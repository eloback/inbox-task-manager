//! `itm daemon` (R3, R4.3–R4.5, R7): every 2 s it reads the agents of every
//! running task session, writes the status cache, notifies once per blocked
//! agent and wakes snoozed tasks.
//!
//! A pass is split in two: [`Itm::observe`] asks Herdr (one `agent.list`
//! per running task session, over its socket), and [`Itm::poll_once`]
//! decides and records, so tests can describe what Herdr shows.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::ops::ControlFlow;
use std::panic::{self, AssertUnwindSafe};
use std::process::Command;
use std::sync::{Arc, Once};
use std::thread;
use std::time::Duration;

use crate::error::{Context, Error, Result};
use crate::herdr::{Agent, Sessions};
use crate::itm::Itm;
use crate::status::{AgentState, AgentStatus, Cache, SessionStatus};
use crate::store::{State, python_json, write_atomic};
use crate::system::{notify_command, output_within};
use crate::tasks::{self, is_task_session};
use crate::times;

pub const POLL: Duration = Duration::from_secs(2);
/// How long a desktop notification offering Open is waited for (Python's limit).
const NOTIFY_WAIT: Duration = Duration::from_secs(3600);

/// Blocked agents already reported: wait key → when the daemon first saw
/// the wait (`notified.json`). A key is a session, pane and state change,
/// as the Python itm writes it, so one wait is reported once (R3.4).
pub type Notified = BTreeMap<String, i64>;

/// What Herdr showed on one pass.
pub struct Observation {
    pub sessions: Sessions,
    /// The agents of each running task session; None when it did not answer.
    pub agents: BTreeMap<String, Option<Vec<Agent>>>,
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

fn wait_key(session: &str, agent: &Agent) -> String {
    let seq = match &agent.state_change_seq {
        None | Some(serde_json::Value::Null) => "None".to_string(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    };
    format!("{session}|{}|{seq}", agent.pane_id.as_deref().unwrap_or("None"))
}

impl Itm {
    pub fn observe(&self) -> Result<Observation> {
        let sessions = self.herdr.sessions()?;
        let agents = sessions
            .values()
            .filter(|s| is_task_session(&s.name) && s.running)
            .map(|s| (s.name.clone(), self.herdr.agents(&s.socket).ok()))
            .collect();
        let attached = self.live_clients()?.into_values().collect();
        Ok(Observation { sessions, agents, attached })
    }

    /// One pass over what Herdr showed; returns the notifications to deliver.
    /// A wait is marked notified only after the store update succeeded, is
    /// forgotten once its agent is no longer blocked, and is kept for
    /// sessions that could not be asked.
    pub fn poll_once(&self, notified: &mut Notified, seen: &Observation, now: i64) -> Result<Vec<Notice>> {
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
        write_atomic(&status_file, &python_json(&Cache { updated_at: now, sessions: cache }, false))
            .context(|| format!("cannot write {}", status_file.display()))?;

        let new_waits: Vec<(&str, &Agent)> =
            blocked_now.iter().filter(|(key, _)| !notified.contains_key(*key)).map(|(_, wait)| *wait).collect();
        let waiting: BTreeSet<&str> = blocked_now.values().map(|(session, _)| *session).collect();
        let running = |name: &str| seen.sessions.get(name).is_some_and(|s| s.running);

        let notices = self.store().edit(|data| {
            tasks::adopt_sessions(data, seen.sessions.keys().map(String::as_str), now);
            for task in data.tasks.values_mut() {
                // This version does not follow workspaces (R10): as with that
                // setting off, no record of open workspaces is kept.
                task.extra.remove("workspace_projects");
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
        write_atomic(&self.paths.notified_file(), &python_json(&*notified, false))
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
        let command = match notify_command(self.notify_command.as_deref(), &notice.title, &notice.body, open) {
            Ok(Some(command)) => command,
            Ok(None) => {
                return eprintln!("no way to show desktop notifications; set ITM_NOTIFY_COMMAND: {}", notice.title);
            }
            Err(e) => return eprintln!("{e}: {}", notice.title),
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

    /// The daemon's loop: a pass every [`POLL`], forever.
    pub fn run_daemon(self: Arc<Self>) -> Result<()> {
        let mut notified: Notified = fs::read_to_string(self.paths.notified_file())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let pass = || {
            let seen = self.observe()?;
            for notice in self.poll_once(&mut notified, &seen, times::now())? {
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
            attached: BTreeSet::new(),
        };
        (tmp, itm, seen)
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
        let notices = itm.poll_once(&mut notified, &seen, NOW).unwrap();
        assert_eq!(titles(&notices), ["Needs input · A", "Task back · C", "Woke · Needs input · B"]);
        assert_eq!(notices.iter().find(|n| n.task == "task-a").unwrap().body, "claude: p1");
        assert!(states(&itm).iter().all(|(_, s)| *s == State::Active));
        let status = Status::read(&itm.paths.status_file().unwrap(), NOW);
        assert_eq!((status.waiting("task-a"), status.waiting("default")), (1, 0));
        assert_eq!(notified.keys().collect::<Vec<_>>(), ["task-a|p1|7", "task-b|p3|1"]);

        assert_eq!(itm.poll_once(&mut notified, &seen, NOW + 2).unwrap(), []);
    }

    #[test]
    fn a_wait_is_reported_again_after_the_agent_unblocked() {
        let (_tmp, itm, mut seen) = world();
        let mut notified = Notified::new();
        itm.poll_once(&mut notified, &seen, NOW).unwrap();
        let blocked = seen.agents.insert("task-a".into(), Some(vec![])).unwrap();
        itm.poll_once(&mut notified, &seen, NOW + 2).unwrap();
        assert!(!notified.keys().any(|k| k.starts_with("task-a|")));
        seen.agents.insert("task-a".into(), blocked);
        assert_eq!(titles(&itm.poll_once(&mut notified, &seen, NOW + 4).unwrap()), ["Needs input · A"]);
    }

    #[test]
    fn a_session_that_did_not_answer_keeps_its_waits() {
        let (_tmp, itm, mut seen) = world();
        let mut notified = Notified::new();
        itm.poll_once(&mut notified, &seen, NOW).unwrap();
        seen.agents.insert("task-a".into(), None);
        itm.poll_once(&mut notified, &seen, NOW + 2).unwrap();
        assert!(notified.contains_key("task-a|p1|7"));
    }

    #[test]
    fn a_failed_store_update_reports_nothing_as_notified() {
        let (_tmp, itm, seen) = world();
        fs::write(itm.paths.tasks_file(), "{broken").unwrap();
        let mut notified = Notified::new();
        assert!(itm.poll_once(&mut notified, &seen, NOW).is_err());
        assert!(notified.is_empty());
    }

    #[test]
    fn an_attached_session_is_left_to_herdr() {
        let (_tmp, itm, mut seen) = world();
        seen.attached.insert("task-a".into());
        let notices = itm.poll_once(&mut Notified::new(), &seen, NOW).unwrap();
        assert!(!titles(&notices).contains(&"Needs input · A"));
    }

    #[test]
    fn the_status_records_when_a_wait_started() {
        let (_tmp, itm, seen) = world();
        let mut notified = Notified::new();
        itm.poll_once(&mut notified, &seen, NOW).unwrap();
        itm.poll_once(&mut notified, &seen, NOW + 300).unwrap();
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
        itm.poll_once(&mut notified, &seen, NOW).unwrap();
        let tasks = itm.store().load().unwrap().tasks;
        assert_eq!((tasks["task-b"].state, tasks["task-c"].state), (State::Active, State::Snoozed));

        seen.sessions.get_mut("task-c").unwrap().running = false;
        seen.agents.remove("task-c");
        let notices = itm.poll_once(&mut notified, &seen, NOW + 2).unwrap();
        assert_eq!(titles(&notices), ["Task back · C"]);
    }

    #[test]
    fn until_an_agent_wakes_on_a_wait_already_reported() {
        let (_tmp, itm, seen) = world();
        let mut notified = Notified::new();
        itm.poll_once(&mut notified, &seen, NOW).unwrap(); // task-a's wait is now reported
        itm.store().edit(|data| data.tasks.get_mut("task-a").unwrap().snooze(Until::Agent, NOW)).unwrap();
        let notices = itm.poll_once(&mut notified, &seen, NOW + 2).unwrap();
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
        let notices = itm.poll_once(&mut Notified::new(), &seen, NOW).unwrap();
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

    #[test]
    fn notified_json_keys_are_the_ones_python_writes() {
        let none = agent("claude", "blocked", "w1:p1", None);
        assert_eq!(wait_key("task-a", &none), "task-a|w1:p1|None");
        let no_pane: Agent = serde_json::from_value(json!({"agent_status": "blocked", "state_change_seq": 3})).unwrap();
        assert_eq!(wait_key("task-a", &no_pane), "task-a|None|3");
    }
}
