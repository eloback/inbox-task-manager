//! `itm daemon` (R3, R4.3–R4.5, R7, R10.3): every 2 s it reads the agents
//! and workspaces of every running task session, writes the status cache,
//! notifies once per blocked agent, wakes snoozed tasks, and has running
//! tasks' projects follow their workspaces.
//!
//! A pass is split in two: [`Itm::observe`] asks Herdr (one
//! `session.snapshot` per running task session, over its socket), and
//! [`Itm::poll_once`] decides and records, so tests can describe what Herdr
//! shows.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::ops::ControlFlow;
use std::panic::{self, AssertUnwindSafe};
use std::process::Command;
use std::sync::{Arc, Once};
use std::thread;
use std::time::Duration;

use crate::config::Settings;
use crate::error::{Context, Error, Result};
use crate::herdr::{Agent, Sessions, Workspace};
use crate::itm::Itm;
use crate::status::{AgentState, AgentStatus, Cache, SessionStatus};
use crate::store::{State, to_json, write_atomic};
use crate::system::{NotifyCommand, notify_command, output_within};
use crate::tasks::{self, is_task_session};
use crate::times;
use crate::workspaces::project_roots;

pub const POLL: Duration = Duration::from_secs(2);
/// How long a desktop notification offering Open is waited for.
const NOTIFY_WAIT: Duration = Duration::from_secs(3600);

/// Blocked agents already reported: wait key → when the daemon first saw
/// the wait (`notified.json`). A key is a session, pane and state change
/// (`session|pane|seq`), so one wait is reported once (R3.4).
pub type Notified = BTreeMap<String, i64>;

/// What Herdr showed on one pass.
pub struct Observation {
    pub sessions: Sessions,
    /// The agents of each running task session; None when it did not answer.
    pub agents: BTreeMap<String, Option<Vec<Agent>>>,
    /// The workspaces of each running task session, while projects follow
    /// workspaces. A session that did not answer, or has no workspace yet
    /// (it is starting or being restored), is not here.
    pub workspaces: BTreeMap<String, Vec<Workspace>>,
    /// Sessions an `itm attach` client is attached to.
    pub attached: BTreeSet<String>,
}

/// A notification to deliver, about `task`.
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub title: String,
    pub body: String,
    pub task: String,
}

/// The command that shows `notice` on the desktop, or the line the daemon
/// logs instead when there is none or `$ITM_NOTIFY_COMMAND` cannot be read
/// (R3.2): the daemon goes on either way.
fn desktop_command(
    command: Result<Option<NotifyCommand>>,
    notice: &Notice,
) -> std::result::Result<NotifyCommand, String> {
    match command {
        Ok(Some(command)) => Ok(command),
        Ok(None) => Err(format!("no way to show desktop notifications; set ITM_NOTIFY_COMMAND: {}", notice.title)),
        Err(e) => Err(format!("{e}: {}", notice.title)),
    }
}

fn wait_key(session: &str, agent: &Agent) -> String {
    let seq = match &agent.state_change_seq {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    };
    format!("{session}|{}|{seq}", agent.pane_id.as_deref().unwrap_or_default())
}

impl Itm {
    /// What Herdr shows; the workspaces only when projects `follow` them.
    pub fn observe(&self, follow: bool) -> Result<Observation> {
        let sessions = self.herdr.sessions()?;
        let (mut agents, mut workspaces) = (BTreeMap::new(), BTreeMap::new());
        for session in sessions.values().filter(|s| is_task_session(&s.name) && s.running) {
            let view = self.herdr.snapshot(&session.socket).ok();
            let open = view.as_ref().and_then(|v| v.workspaces.clone()).filter(|w| follow && !w.is_empty());
            if let Some(open) = open {
                workspaces.insert(session.name.clone(), open);
            }
            agents.insert(session.name.clone(), view.map(|v| v.agents));
        }
        let attached = self.live_clients()?.into_values().collect();
        Ok(Observation { sessions, agents, workspaces, attached })
    }

    /// One pass over what Herdr showed; returns the notifications to deliver.
    /// A wait is marked notified only after the store update succeeded, is
    /// forgotten once its agent is no longer blocked, and is kept for
    /// sessions that could not be asked.
    ///
    /// Running tasks' projects follow their workspaces (R10.3). `settled`,
    /// the daemon's memory, holds the sessions whose workspaces it read on
    /// the pass before: only those can lose a project, so a session just
    /// started or restored, or first seen by a daemon that just started, is
    /// never mistaken for one whose workspaces closed.
    pub fn poll_once(
        &self,
        notified: &mut Notified,
        seen: &Observation,
        settings: &Settings,
        settled: &mut BTreeSet<String>,
        now: i64,
    ) -> Result<Vec<Notice>> {
        let mut cache = BTreeMap::new();
        let mut blocked_now: BTreeMap<String, (&str, &Agent)> = BTreeMap::new();
        for (session, agents) in &seen.agents {
            let Some(agents) = agents else { continue };
            let entries: Vec<AgentStatus> = agents
                .iter()
                .map(|agent| {
                    let mut since = None;
                    if agent.agent_status == AgentState::Blocked {
                        let key = wait_key(session, agent);
                        since = Some(notified.get(&key).copied().unwrap_or(now)); // when the wait started
                        blocked_now.insert(key, (session, agent));
                    }
                    AgentStatus {
                        agent: agent.agent.clone().unwrap_or_else(|| "?".into()),
                        status: agent.agent_status,
                        title: agent.terminal_title_stripped.clone().unwrap_or_default(),
                        pane_id: agent.pane_id.clone(),
                        since,
                    }
                })
                .collect();
            let blocked = entries.iter().filter(|e| e.status == AgentState::Blocked).count();
            cache.insert(session.clone(), SessionStatus { blocked, agents: entries });
        }
        let status_file = self.paths.status_file()?;
        write_atomic(&status_file, &to_json(&Cache { updated_at: now, sessions: cache }, false))
            .context(|| format!("cannot write {}", status_file.display()))?;

        let new_waits: Vec<(&str, &Agent)> =
            blocked_now.iter().filter(|(key, _)| !notified.contains_key(*key)).map(|(_, wait)| *wait).collect();
        let waiting: BTreeSet<&str> = blocked_now.values().map(|(session, _)| *session).collect();
        let running = |name: &str| seen.sessions.get(name).is_some_and(|s| s.running);

        // Folders are matched to projects outside the store lock (resolving them reads the disk; git may run).
        let open: BTreeMap<&String, (Vec<String>, bool)> = match seen.workspaces.is_empty() {
            true => BTreeMap::new(),
            false => {
                let projects = self.store().load()?.projects;
                let roots = project_roots(&projects); // each project's folder resolved once per pass
                let open_in = |found| self.workspace_projects(&projects, &roots, found);
                seen.workspaces.iter().map(|(name, found)| (name, open_in(found))).collect()
            }
        };
        let trusted: BTreeSet<&String> = seen.workspaces.keys().filter(|name| settled.contains(*name)).collect();
        *settled = seen.workspaces.keys().cloned().collect();

        let notices = self.store().edit(|data| {
            tasks::adopt_sessions(data, seen.sessions.keys().map(String::as_str), now);
            for (name, task) in data.tasks.iter_mut() {
                // A stopped session has no open workspaces, and with sync off nothing keeps the record
                // current: an old one must not keep a project or count as closed later.
                if !settings.projects_from_workspaces || !running(name) {
                    task.workspace_projects.clear();
                }
            }
            for (name, (ids, complete)) in &open {
                let Some(task) = data.tasks.get_mut(*name).filter(|t| t.state != State::Archived) else { continue };
                // A project removed meanwhile is not added back.
                let ids: Vec<String> = ids.iter().filter(|id| data.projects.contains_key(*id)).cloned().collect();
                let trusted = *complete && trusted.contains(name);
                task.sync_projects(&ids, settings.remove_projects_without_workspace, trusted);
            }
            let mut notices = vec![];
            let mut notice =
                |title: String, body: String, task: &str| notices.push(Notice { title, body, task: task.into() });
            for &(name, agent) in &new_waits {
                let Some(task) = data.tasks.get_mut(name).filter(|t| t.state != State::Archived) else { continue };
                let woke = task.state == State::Snoozed;
                if woke {
                    task.activate();
                }
                if seen.attached.contains(name) && !woke {
                    continue; // Herdr already toasts blocked agents in the attached session
                }
                let label = agent.terminal_title_stripped.as_deref().filter(|t| !t.is_empty());
                let label = label.or(agent.pane_id.as_deref()).unwrap_or_default();
                let prefix = if woke { "Woke · " } else { "" };
                let who = agent.agent.as_deref().unwrap_or("agent");
                notice(format!("{prefix}Needs input · {}", task.title), format!("{who}: {label}"), name);
            }
            for (name, task) in data.tasks.iter_mut().filter(|(_, t)| t.state == State::Snoozed) {
                let (title, body) = match task.snoozed_until {
                    // Snoozed until an agent needs you: any wait wakes it, also one already reported.
                    None if waiting.contains(name.as_str()) => ("Woke · Needs input", "An agent is waiting for you"),
                    // Its session stopped (Herdr restart, reboot): no agent can ask any more.
                    None if !running(name) => ("Task back", "Its session stopped, so no agent can ask"),
                    Some(until) if until <= now => ("Task back", "Snooze ended"),
                    _ => continue,
                };
                task.activate();
                notice(format!("{title} · {}", task.title), body.into(), name);
            }
            Ok(notices)
        })?;

        // Only now that the store is updated: remember current waits, keep
        // the ones of sessions that could not be asked, forget the rest.
        let asked = |key: &str| seen.agents.get(key.split('|').next().unwrap_or_default()).is_some_and(Option::is_some);
        notified.retain(|key, _| blocked_now.contains_key(key) || !asked(key));
        for key in blocked_now.keys() {
            notified.entry(key.clone()).or_insert(now);
        }
        write_atomic(&self.paths.notified_file(), &to_json(&*notified, false))
            .context(|| "cannot write notified.json".into())?;
        Ok(notices)
    }

    /// A Herdr toast in a session an itm client is attached to, else a desktop notification (R3.2).
    fn deliver(self: &Arc<Self>, notice: &Notice, seen: &Observation) {
        for session in seen.attached.iter().filter_map(|name| seen.sessions.get(name)) {
            if self.herdr.toast(&session.socket, &notice.title, &notice.body).unwrap_or(false) {
                return;
            }
        }
        self.desktop_notify(notice);
    }

    /// Runs in a thread: `notify-send --wait` blocks until the notification
    /// closes, and its Open action switches the newest itm client to the task.
    fn desktop_notify(self: &Arc<Self>, notice: &Notice) {
        let open = self.live_clients().is_ok_and(|c| !c.is_empty());
        let command = match desktop_command(
            notify_command(self.notify_command.as_deref(), &notice.title, &notice.body, open),
            notice,
        ) {
            Ok(command) => command,
            Err(line) => return eprintln!("{line}"),
        };
        let (itm, task) = (Arc::clone(self), notice.task.clone());
        thread::spawn(move || {
            let mut run = Command::new(&command.argv[0]);
            match output_within(run.args(&command.argv[1..]), NOTIFY_WAIT) {
                Ok(Some((_, said))) if command.offers_open && said.trim() == "open" => {
                    if let Err(e) = itm.handoff(&task, None, true) {
                        eprintln!("could not open {task}: {e}");
                    }
                }
                Ok(_) => {} // closed, or still open after NOTIFY_WAIT
                Err(e) => eprintln!("notification failed: {e}"),
            }
        });
    }

    /// The settings for one pass. A broken file never stops the daemon: it
    /// is logged once while it stays the same, and the pass runs without
    /// following workspaces (nothing is added or removed) until it is fixed.
    fn pass_settings(&self, last_error: &mut Option<String>, mut log: impl FnMut(&str)) -> Settings {
        match self.settings() {
            Ok(settings) => {
                *last_error = None;
                settings
            }
            Err(e) => {
                let e = e.to_string();
                if last_error.as_ref() != Some(&e) {
                    log(&format!("settings ignored: {e}"));
                }
                *last_error = Some(e);
                Settings::broken()
            }
        }
    }

    /// The daemon's loop: a pass every [`POLL`], forever.
    pub fn run_daemon(self: Arc<Self>) -> Result<()> {
        let mut notified: Notified = fs::read_to_string(self.paths.notified_file())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let (mut settings_error, mut settled) = (None, BTreeSet::new());
        let pass = || {
            let settings = self.pass_settings(&mut settings_error, |line| eprintln!("{line}"));
            let seen = self.observe(settings.projects_from_workspaces)?;
            for notice in self.poll_once(&mut notified, &seen, &settings, &mut settled, times::now())? {
                self.deliver(&notice, &seen);
            }
            Ok(())
        };
        let sleep = || {
            thread::sleep(POLL);
            ControlFlow::Continue(())
        };
        keep_polling(pass, |line| eprintln!("{line}"), sleep);
        Ok(())
    }
}

/// Run `pass`, then `wait`, until `wait` breaks (the daemon's never does).
/// One failed pass never stops the daemon, not even one that panics; an
/// error is logged once while it repeats (DESIGN.md section 2).
fn keep_polling(
    mut pass: impl FnMut() -> Result<()>,
    mut log: impl FnMut(&str),
    mut wait: impl FnMut() -> ControlFlow<()>,
) {
    let mut last_error = None;
    loop {
        match guarded(&mut pass) {
            Ok(()) => last_error = None,
            Err(e) if last_error.as_ref() != Some(&e) => {
                log(&format!("poll failed: {e}"));
                last_error = Some(e);
            }
            Err(_) => {}
        }
        if wait().is_break() {
            return;
        }
    }
}

thread_local! {
    /// Whether this thread runs a pass: its panics are logged by
    /// [`keep_polling`] (once while they repeat), not by the default hook.
    static IN_PASS: Cell<bool> = const { Cell::new(false) };
    /// Where and why the pass panicked, as the panic hook describes it.
    static PANICKED: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// One pass, with a panic turned into an error.
fn guarded(pass: impl FnOnce() -> Result<()>) -> Result<()> {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let default = panic::take_hook();
        panic::set_hook(Box::new(move |info| match IN_PASS.get() {
            true => PANICKED.set(Some(info.to_string().replace('\n', " "))),
            false => default(info),
        }));
    });
    IN_PASS.set(true);
    let outcome = panic::catch_unwind(AssertUnwindSafe(pass));
    IN_PASS.set(false);
    outcome.unwrap_or_else(|payload| {
        let said =
            payload.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| payload.downcast_ref::<String>().cloned());
        let what = PANICKED.take().or_else(|| said.map(|s| format!("panicked: {s}"))).unwrap_or("panicked".into());
        Err(Error::Other(format!("the pass {what}")))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::Session;
    use crate::itm::tests::itm;
    use crate::snooze::Until;
    use crate::status::Status;
    use crate::store::Task;
    use serde_json::json;
    use std::process::Command;

    const NOW: i64 = 1_800_000_000;

    fn agent(name: &str, status: &str, pane: &str, seq: Option<i64>) -> Agent {
        serde_json::from_value(json!({"agent": name, "agent_status": status, "pane_id": pane, "state_change_seq": seq}))
            .unwrap()
    }

    /// task-a active with a blocked and a working agent, task-b snoozed for
    /// an hour with a blocked agent, task-c snoozed until a time that passed;
    /// all running, plus a blocked agent in the `default` session.
    fn world() -> (tempfile::TempDir, Itm, Observation) {
        let (tmp, itm) = itm();
        itm.store()
            .edit(|data| {
                data.tasks.insert("task-a".into(), Task::new("A", NOW));
                let mut b = Task::new("B", NOW);
                b.snooze(Until::Time(NOW + 3600), NOW)?;
                let mut c = Task::new("C", NOW);
                (c.state, c.snoozed_until) = (State::Snoozed, Some(NOW - 1));
                data.tasks.extend([("task-b".into(), b), ("task-c".into(), c)]);
                Ok(())
            })
            .unwrap();
        let session =
            |name: &str| (name.to_string(), Session { name: name.into(), running: true, socket: "/s".into() });
        let seen = Observation {
            sessions: ["default", "task-a", "task-b", "task-c"].map(session).into(),
            agents: [
                (
                    "task-a".into(),
                    Some(vec![agent("claude", "blocked", "p1", Some(7)), agent("codex", "working", "p2", None)]),
                ),
                ("task-b".into(), Some(vec![agent("claude", "blocked", "p3", Some(1))])),
                ("task-c".into(), Some(vec![])),
            ]
            .into(),
            workspaces: BTreeMap::new(),
            attached: BTreeSet::new(),
        };
        (tmp, itm, seen)
    }

    /// A pass with the default settings, every session read on the pass before.
    fn pass(itm: &Itm, notified: &mut Notified, seen: &Observation, now: i64) -> Result<Vec<Notice>> {
        let mut settled = seen.workspaces.keys().cloned().collect();
        itm.poll_once(notified, seen, &Settings::default(), &mut settled, now)
    }

    fn titles(notices: &[Notice]) -> Vec<&str> {
        let mut titles: Vec<&str> = notices.iter().map(|n| n.title.as_str()).collect();
        titles.sort();
        titles
    }

    fn states(itm: &Itm) -> Vec<(String, State)> {
        itm.store().load().unwrap().tasks.into_iter().map(|(n, t)| (n, t.state)).collect()
    }

    #[test]
    fn notifies_wakes_and_reports_a_wait_once() {
        let (_tmp, itm, seen) = world();
        let mut notified = Notified::new();
        let notices = pass(&itm, &mut notified, &seen, NOW).unwrap();
        assert_eq!(titles(&notices), ["Needs input · A", "Task back · C", "Woke · Needs input · B"]);
        assert_eq!(notices.iter().find(|n| n.task == "task-a").unwrap().body, "claude: p1");
        assert!(states(&itm).iter().all(|(_, s)| *s == State::Active));
        let status = Status::read(&itm.paths.status_file().unwrap(), NOW);
        assert_eq!((status.waiting("task-a"), status.waiting("default")), (1, 0));
        assert_eq!(notified.keys().collect::<Vec<_>>(), ["task-a|p1|7", "task-b|p3|1"]);

        assert_eq!(pass(&itm, &mut notified, &seen, NOW + 2).unwrap(), []);
    }

    #[test]
    fn a_wait_is_reported_again_after_the_agent_unblocked() {
        let (_tmp, itm, mut seen) = world();
        let mut notified = Notified::new();
        pass(&itm, &mut notified, &seen, NOW).unwrap();
        let blocked = seen.agents.insert("task-a".into(), Some(vec![])).unwrap();
        pass(&itm, &mut notified, &seen, NOW + 2).unwrap();
        assert!(!notified.keys().any(|k| k.starts_with("task-a|")));
        seen.agents.insert("task-a".into(), blocked);
        assert_eq!(titles(&pass(&itm, &mut notified, &seen, NOW + 4).unwrap()), ["Needs input · A"]);
    }

    #[test]
    fn a_session_that_did_not_answer_keeps_its_waits() {
        let (_tmp, itm, mut seen) = world();
        let mut notified = Notified::new();
        pass(&itm, &mut notified, &seen, NOW).unwrap();
        seen.agents.insert("task-a".into(), None);
        pass(&itm, &mut notified, &seen, NOW + 2).unwrap();
        assert!(notified.contains_key("task-a|p1|7"));
    }

    #[test]
    fn a_failed_store_update_reports_nothing_as_notified() {
        let (_tmp, itm, seen) = world();
        fs::write(itm.paths.tasks_file(), "{broken").unwrap();
        let mut notified = Notified::new();
        assert!(pass(&itm, &mut notified, &seen, NOW).is_err());
        assert!(notified.is_empty());
    }

    /// R3.2: without a way to notify, or with an `$ITM_NOTIFY_COMMAND` that
    /// does not read, the daemon logs the notification and goes on.
    #[test]
    fn a_notification_with_no_way_to_show_it_is_logged() {
        let notice = Notice { title: "Needs input · A".into(), body: "claude: p1".into(), task: "task-a".into() };
        let broken = notify_command(Some("ntfy 'unclosed"), &notice.title, &notice.body, false);
        assert_eq!(
            desktop_command(broken, &notice).unwrap_err(),
            "cannot read $ITM_NOTIFY_COMMAND: ntfy 'unclosed: Needs input · A"
        );
        assert_eq!(
            desktop_command(Ok(None), &notice).unwrap_err(),
            "no way to show desktop notifications; set ITM_NOTIFY_COMMAND: Needs input · A"
        );
        let custom = notify_command(Some("ntfy"), &notice.title, &notice.body, false);
        assert_eq!(desktop_command(custom, &notice).unwrap().argv, ["ntfy", "Needs input · A", "claude: p1"]);
    }

    #[test]
    fn an_attached_session_is_left_to_herdr() {
        let (_tmp, itm, mut seen) = world();
        seen.attached.insert("task-a".into());
        let notices = pass(&itm, &mut Notified::new(), &seen, NOW).unwrap();
        assert!(!titles(&notices).contains(&"Needs input · A"));
    }

    #[test]
    fn the_status_records_when_a_wait_started() {
        let (_tmp, itm, seen) = world();
        let mut notified = Notified::new();
        pass(&itm, &mut notified, &seen, NOW).unwrap();
        pass(&itm, &mut notified, &seen, NOW + 300).unwrap();
        let status = Status::read(&itm.paths.status_file().unwrap(), NOW + 300);
        assert_eq!(status.waiting_since("task-a", NOW + 300), Some(NOW));
        assert!(!Status::read(&itm.paths.status_file().unwrap(), NOW + 300 + 16).fresh);
    }

    #[test]
    fn snoozed_until_an_agent_wakes_for_an_agent_or_a_stopped_session_only() {
        let (_tmp, itm, mut seen) = world();
        itm.store()
            .edit(|data| {
                for name in ["task-b", "task-c"] {
                    data.tasks.get_mut(name).unwrap().snooze(Until::Agent, NOW)?;
                }
                Ok(())
            })
            .unwrap();
        let mut notified = Notified::new();
        pass(&itm, &mut notified, &seen, NOW).unwrap();
        let tasks = itm.store().load().unwrap().tasks;
        assert_eq!((tasks["task-b"].state, tasks["task-c"].state), (State::Active, State::Snoozed));

        seen.sessions.get_mut("task-c").unwrap().running = false;
        seen.agents.remove("task-c");
        let notices = pass(&itm, &mut notified, &seen, NOW + 2).unwrap();
        assert_eq!(titles(&notices), ["Task back · C"]);
    }

    #[test]
    fn until_an_agent_wakes_on_a_wait_already_reported() {
        let (_tmp, itm, seen) = world();
        let mut notified = Notified::new();
        pass(&itm, &mut notified, &seen, NOW).unwrap(); // task-a's wait is now reported
        itm.store().edit(|data| data.tasks.get_mut("task-a").unwrap().snooze(Until::Agent, NOW)).unwrap();
        let notices = pass(&itm, &mut notified, &seen, NOW + 2).unwrap();
        assert_eq!(titles(&notices), ["Woke · Needs input · A"]);
    }

    #[test]
    fn task_sessions_made_elsewhere_are_adopted_and_archived_tasks_stay_quiet() {
        let (_tmp, itm, mut seen) = world();
        itm.store()
            .edit(|data| {
                data.tasks.get_mut("task-a").unwrap().archive(NOW);
                Ok(())
            })
            .unwrap();
        let session = Session { name: "task-new".into(), running: false, socket: "/s".into() };
        seen.sessions.insert("task-new".into(), session);
        let notices = pass(&itm, &mut Notified::new(), &seen, NOW).unwrap();
        assert!(!titles(&notices).contains(&"Needs input · A"));
        assert_eq!(itm.store().load().unwrap().tasks["task-new"].title, "new");
    }

    #[test]
    fn a_failed_or_panicking_pass_never_stops_the_daemon_and_is_logged_once() {
        let (mut passes, mut logged) = (0, vec![]);
        let pass = || {
            passes += 1;
            match passes {
                1 | 2 => panic!("boom"),
                4 | 5 => Err(Error::Other("herdr not found".into())),
                _ => Ok(()),
            }
        };
        let mut waits = 0;
        let wait = || {
            waits += 1;
            if waits < 6 { ControlFlow::Continue(()) } else { ControlFlow::Break(()) }
        };
        keep_polling(pass, |line| logged.push(line.to_string()), wait);
        assert_eq!(passes, 6);
        assert_eq!(logged.len(), 2, "{logged:?}");
        assert!(
            logged[0].starts_with("poll failed: the pass panicked at src/daemon.rs:") && logged[0].ends_with("boom")
        );
        assert_eq!(logged[1], "poll failed: herdr not found");
    }

    // ------------------------------------------------------------ projects follow workspaces (R10.3)

    use crate::config::Setting;
    use crate::projects::tests::folder;
    use crate::store::ProjectKind;
    use std::path::Path;

    /// Projects api (~/code/api) and web (~/code/web), and the task Run with
    /// its session running and no projects; what Herdr shows of it has no
    /// agents and no workspaces yet.
    fn running() -> (tempfile::TempDir, Itm, [String; 2], Observation) {
        let (tmp, itm) = itm();
        let ids = ["api", "web"].map(|name| {
            itm.add_project(ProjectKind::Dir, &folder(&itm, &format!("code/{name}")).to_string_lossy(), None).unwrap()
        });
        itm.create_task("Run", None, &[], None).unwrap();
        let session = Session { name: "task-run".into(), running: true, socket: "/s".into() };
        let seen = Observation {
            sessions: [("task-run".to_string(), session)].into(),
            agents: [("task-run".to_string(), Some(vec![]))].into(),
            workspaces: BTreeMap::new(),
            attached: BTreeSet::new(),
        };
        (tmp, itm, ids, seen)
    }

    /// Unnamed workspaces in these folders (None: Herdr does not report it), in order.
    fn open_in(seen: &mut Observation, folders: &[Option<&Path>]) {
        let workspace = |(i, folder): (usize, &Option<&Path>)| Workspace {
            id: format!("w{i}"),
            label: String::new(),
            focused: i == 0,
            folder: folder.map(Path::to_path_buf),
        };
        seen.workspaces.insert("task-run".into(), folders.iter().enumerate().map(workspace).collect());
    }

    fn projects_of(itm: &Itm) -> Vec<String> {
        itm.store().load().unwrap().tasks["task-run"].projects.clone()
    }

    /// R10.3: the daemon adds the projects a running task's workspaces are
    /// in, and removes them once their workspace closed; a session without
    /// workspaces (starting) changes nothing; an archived task is left alone.
    #[test]
    fn r10_3_the_daemon_follows_workspaces() {
        let (_tmp, itm, [api, web], mut seen) = running();
        itm.edit_task("task-run", &crate::tasks::Edit { add: vec![api.clone()], ..Default::default() }).unwrap();
        let (code_api, web_src) = (folder(&itm, "code/api"), folder(&itm, "code/web/src"));
        let mut settled = BTreeSet::new();
        let mut pass = |seen: &Observation, settings: &Settings| {
            itm.poll_once(&mut Notified::new(), seen, settings, &mut settled, NOW).unwrap();
        };
        open_in(&mut seen, &[Some(&code_api), Some(&web_src)]);
        pass(&seen, &Settings::default());
        assert_eq!(projects_of(&itm), [api.clone(), web.clone()]);
        let task = itm.store().load().unwrap().tasks["task-run"].clone();
        assert_eq!((task.found_projects, task.workspace_projects), (vec![web.clone()], vec![api.clone(), web.clone()]));
        seen.workspaces.clear(); // starting or restoring: nothing to follow yet
        pass(&seen, &Settings::default());
        assert_eq!(projects_of(&itm), [api.clone(), web.clone()]);
        open_in(&mut seen, &[Some(&code_api)]);
        pass(&seen, &Settings::default()); // the session was not read on the pass before: only adds
        assert_eq!(projects_of(&itm), [api.clone(), web.clone()]);
        pass(&seen, &Settings::default());
        assert_eq!(projects_of(&itm), std::slice::from_ref(&api), "web's workspace closed");
        // Off: nothing changes, and no record is kept.
        open_in(&mut seen, &[Some(&folder(&itm, "code/web"))]);
        let off = Settings { projects_from_workspaces: false, ..Settings::default() };
        seen.workspaces.clear(); // observe reads none while off
        pass(&seen, &off);
        let task = itm.store().load().unwrap().tasks["task-run"].clone();
        assert_eq!((task.projects, task.workspace_projects), (vec![api.clone()], vec![]));
        // An archived task is left alone.
        itm.store()
            .edit(|data| {
                data.tasks.get_mut("task-run").unwrap().archive(NOW);
                Ok(())
            })
            .unwrap();
        open_in(&mut seen, &[Some(&folder(&itm, "code/web"))]);
        pass(&seen, &Settings::default());
        pass(&seen, &Settings::default());
        assert_eq!(projects_of(&itm), [api]);
    }

    /// R10.3: a workspace whose folder Herdr does not report, or a daemon
    /// that just started, never removes anything on that pass.
    #[test]
    fn r10_3_untrusted_passes_only_add() {
        let (_tmp, itm, [api, web], mut seen) = running();
        let (code_api, code_web) = (folder(&itm, "code/api"), folder(&itm, "code/web"));
        let mut settled = BTreeSet::new();
        let pass = |seen: &Observation, settled: &mut BTreeSet<String>| {
            itm.poll_once(&mut Notified::new(), seen, &Settings::default(), settled, NOW).unwrap();
        };
        open_in(&mut seen, &[Some(&code_api), Some(&code_web)]);
        pass(&seen, &mut settled);
        assert_eq!(settled, BTreeSet::from(["task-run".to_string()]));
        open_in(&mut seen, &[Some(&code_api), None]); // web's folder unknown
        pass(&seen, &mut settled);
        assert_eq!(projects_of(&itm), [api.clone(), web.clone()]);
        open_in(&mut seen, &[Some(&code_api)]);
        pass(&seen, &mut BTreeSet::new()); // a daemon that just started has not read the session before
        assert_eq!(projects_of(&itm), [api.clone(), web.clone()]);
        pass(&seen, &mut settled);
        assert_eq!(projects_of(&itm), [api]);
    }

    /// R10.3: the record of open workspaces holds only while the daemon
    /// reads the session: a stopped session's goes, as does every one while
    /// sync is off; archiving drops it at once.
    #[test]
    fn r10_3_the_record_of_open_workspaces_ends_with_the_session() {
        let (_tmp, itm, [api, web], mut seen) = running();
        itm.create_task("Other", None, &[], None).unwrap();
        itm.store()
            .edit(|data| {
                for (name, id) in [("task-run", &api), ("task-other", &web)] {
                    let task = data.tasks.get_mut(name).unwrap();
                    (task.projects, task.workspace_projects) = (vec![id.clone()], vec![id.clone()]);
                }
                Ok(())
            })
            .unwrap();
        seen.sessions.get_mut("task-run").unwrap().running = false;
        seen.agents.clear();
        pass(&itm, &mut Notified::new(), &seen, NOW).unwrap();
        let tasks = itm.store().load().unwrap().tasks;
        assert!(tasks["task-run"].workspace_projects.is_empty(), "stopped");
        assert!(tasks["task-other"].workspace_projects.is_empty(), "never started");
        assert_eq!(tasks["task-run"].projects, [api], "the projects stay");
    }

    /// R10.2: only saved projects are added; a removed project's folder is in none.
    #[test]
    fn r10_3_only_saved_projects_are_added() {
        let (_tmp, itm, [api, _], mut seen) = running();
        open_in(&mut seen, &[Some(&folder(&itm, "code/api"))]);
        itm.remove_project(&api, crate::projects::CloneChoice::Keep).unwrap();
        pass(&itm, &mut Notified::new(), &seen, NOW).unwrap();
        assert!(projects_of(&itm).is_empty());
    }

    /// R10.2, R10.3: a linked worktree's workspace brings its checkout's project.
    #[test]
    fn r10_3_a_worktree_brings_its_checkouts_project() {
        let (_tmp, itm, _, mut seen) = running();
        let git = |dir: &Path, args: &[&str]| {
            let ok = Command::new("git")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-C"])
                .arg(dir)
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        let lib = folder(&itm, "src/lib");
        git(&lib, &["init", "-q"]);
        git(&lib, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let id = itm.add_project(ProjectKind::Dir, &lib.to_string_lossy(), None).unwrap();
        let tree = itm.paths.home.join("worktrees/fix");
        git(&lib, &["worktree", "add", "-q", &tree.to_string_lossy(), "-b", "fix"]);
        open_in(&mut seen, &[Some(&tree)]);
        pass(&itm, &mut Notified::new(), &seen, NOW).unwrap();
        assert_eq!(projects_of(&itm), [id]);
    }

    /// R10.3: observe reads each running task session's workspaces only
    /// while projects follow workspaces, and leaves out a session with none.
    #[test]
    fn r10_3_observe_reads_workspaces_while_following() {
        let (_tmp, itm) = itm();
        let layout = |label: &str| {
            json!({"snapshot": {"agents": [{"agent": "claude", "agent_status": "working"}],
                "workspaces": [{"workspace_id": "w1", "label": label, "focused": true}],
                "tabs": [{"tab_id": "w1:t1", "workspace_id": "w1"}],
                "layouts": [{"tab_id": "w1:t1", "panes": [{"pane_id": "w1:p1"}]}],
                "panes": [{"pane_id": "w1:p1", "cwd": "/code/api"}]}})
        };
        let starting = json!({"snapshot": {"agents": [], "workspaces": [], "tabs": [], "layouts": [], "panes": []}});
        crate::herdr::fake::serve(&itm.paths.herdr_config, "task-a", move |_, _| layout("api"));
        crate::herdr::fake::serve(&itm.paths.herdr_config, "task-b", move |_, _| starting.clone());
        crate::herdr::fake::serve(&itm.paths.herdr_config, "other", |_, _| json!({}));
        let seen = itm.observe(true).unwrap();
        assert_eq!(seen.workspaces.keys().collect::<Vec<_>>(), ["task-a"]);
        assert_eq!(seen.workspaces["task-a"][0].folder.as_deref(), Some(Path::new("/code/api")));
        assert_eq!(seen.agents["task-a"].as_ref().unwrap().len(), 1);
        assert!(seen.agents.contains_key("task-b") && !seen.agents.contains_key("other"));
        assert!(itm.observe(false).unwrap().workspaces.is_empty());
    }

    /// R10.4: a broken settings file never stops the daemon: it is logged
    /// once while it stays the same, and the pass does not follow workspaces.
    #[test]
    fn r10_4_the_daemon_goes_on_with_a_broken_file() {
        let (_tmp, itm) = itm();
        fs::create_dir_all(&itm.paths.config).unwrap();
        fs::write(itm.paths.config_file(), "[herdr\n").unwrap();
        let (mut last, mut logged) = (None, vec![]);
        for _ in 0..3 {
            let settings = itm.pass_settings(&mut last, |line| logged.push(line.to_string()));
            assert!(!settings.projects_from_workspaces);
        }
        assert_eq!(logged.len(), 1, "{logged:?}");
        assert!(logged[0].starts_with("settings ignored: ") && logged[0].contains("is not valid TOML"), "{logged:?}");
        itm.change_setting(Setting::RemoveProjectsWithoutWorkspace, false).unwrap_err(); // refused: broken
        fs::write(itm.paths.config_file(), "[herdr]\nremove_projects_without_workspace = false\n").unwrap();
        let fixed = itm.pass_settings(&mut last, |line| logged.push(line.to_string()));
        assert!(fixed.projects_from_workspaces && !fixed.remove_projects_without_workspace);
        fs::write(itm.paths.config_file(), "[herdr\n").unwrap();
        itm.pass_settings(&mut last, |line| logged.push(line.to_string()));
        assert_eq!(logged.len(), 2, "broken again: said again");
    }

    #[test]
    fn wait_keys_name_the_session_pane_and_state_change() {
        let none = agent("claude", "blocked", "w1:p1", None);
        assert_eq!(wait_key("task-a", &none), "task-a|w1:p1|");
        let no_pane: Agent = serde_json::from_value(json!({"agent_status": "blocked", "state_change_seq": 3})).unwrap();
        assert_eq!(wait_key("task-a", &no_pane), "task-a||3");
    }
}
