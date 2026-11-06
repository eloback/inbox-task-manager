//! `Itm`: the folders, Herdr and process table one run of itm works with,
//! and the task operations the CLI, the switcher and the daemon share.

use std::collections::BTreeSet;
use std::env;
use std::path::PathBuf;

use jiff::tz::TimeZone;

use crate::error::{Error, Result, bail};
use crate::herdr::{Herdr, Sessions};
use crate::paths::Paths;
use crate::projects::resolve_project;
use crate::snooze::Until;
use crate::status::Status;
use crate::store::{Data, State, Store, Task};
use crate::system::{Proc, Processes};
use crate::tasks;
use crate::times;

pub struct Itm {
    pub paths: Paths,
    pub herdr: Herdr,
    pub procs: Box<dyn Processes>,
    /// `HERDR_SOCKET_PATH`: the session this terminal is in, when inside Herdr.
    pub client_socket: Option<PathBuf>,
    /// `ITM_NOTIFY_COMMAND`.
    pub notify_command: Option<String>,
    pub tz: TimeZone,
}

impl Itm {
    pub fn from_env() -> Itm {
        let paths = Paths::from_env();
        Itm {
            herdr: Herdr::new(paths.herdr_config.clone()),
            paths,
            procs: Box::new(Proc),
            client_socket: env::var_os("HERDR_SOCKET_PATH").filter(|v| !v.is_empty()).map(PathBuf::from),
            notify_command: env::var("ITM_NOTIFY_COMMAND").ok(),
            tz: TimeZone::system(),
        }
    }

    /// An itm whose folders are all under `root`, outside Herdr.
    #[cfg(test)]
    pub fn under(root: &std::path::Path, procs: impl Processes + 'static) -> Itm {
        let paths = Paths::under(root);
        Itm {
            herdr: Herdr::new(paths.herdr_config.clone()),
            paths,
            procs: Box::new(procs),
            client_socket: None,
            notify_command: None,
            tz: TimeZone::UTC,
        }
    }

    pub fn store(&self) -> Store {
        Store::new(self.paths.tasks_file())
    }

    /// The daemon's view of waiting agents; unknown (not fresh) without a runtime folder.
    pub fn status(&self) -> Status {
        self.paths.status_file().map(|path| Status::read(&path, times::now())).unwrap_or_default()
    }

    pub fn inside_herdr(&self) -> bool {
        self.client_socket.is_some()
    }

    /// The session this terminal is in, by its socket.
    pub fn current_session(&self, sessions: &Sessions) -> Option<String> {
        let socket = self.client_socket.as_ref()?;
        sessions.values().find(|s| &s.socket == socket).map(|s| s.name.clone())
    }

    /// The task `query` names, else the task this terminal is in.
    pub fn target_or_current(&self, data: &Data, query: Option<&str>) -> Result<String> {
        if let Some(query) = query {
            return tasks::resolve_task(data, query);
        }
        if self.inside_herdr()
            && let Some(current) = self.current_session(&self.herdr.sessions()?)
            && data.tasks.contains_key(&current)
        {
            return Ok(current);
        }
        bail!("not inside a task; name the task")
    }

    /// Save a new task and return its session name. `projects` are names or
    /// ids; the session starts with a workspace in each. Invalid input writes nothing.
    pub fn create_task(
        &self,
        title: &str,
        description: Option<&str>,
        projects: &[String],
        until: Option<Until>,
    ) -> Result<String> {
        let title = title.trim();
        if title.is_empty() {
            bail!("task title cannot be empty");
        }
        if until == Some(Until::Agent) {
            bail!("a new task has no agents yet, so none could wake it; snooze it until a time");
        }
        let sessions = self.herdr.sessions()?;
        let notes = self.notes_taken();
        let now = times::now();
        self.store()
            .edit(|data| {
                // Not the name of a session, a task, a tombstone, or notes left by a deleted task.
                let taken: BTreeSet<String> = (sessions.keys().chain(data.tasks.keys()).chain(data.deleted.keys()))
                    .chain(notes.iter())
                    .cloned()
                    .collect();
                let name = tasks::unique_session_name(title, &taken);
                let mut task = Task::new(title, now);
                task.description = description.map(str::trim).filter(|d| !d.is_empty()).map(Into::into);
                for query in projects {
                    let id = resolve_project(data, query)?;
                    data.projects.get_mut(&id).expect("resolved").last_used_at = now;
                    if !task.projects.contains(&id) {
                        task.projects.push(id);
                    }
                }
                if let Some(until) = until {
                    task.snooze(until, now)?;
                }
                data.tasks.insert(name.clone(), task);
                Ok((name, data.clone()))
            })
            .map(|(name, data)| {
                // Best effort, after the store write: notes missing now start when first opened.
                let _ = self.ensure_notes(&name, &data);
                name
            })
    }

    /// Why "until an agent needs me" would hide `name` for good, if it would (R4.5).
    pub fn agent_snooze_problem(name: &str, sessions: &Sessions, status: &Status) -> Option<&'static str> {
        if !sessions.get(name).is_some_and(|s| s.running) {
            return Some("its session is not running, so no agent can ask; snooze it until a time");
        }
        if status.waiting(name) > 0 {
            return Some("an agent in it is already waiting for you");
        }
        None
    }

    /// Snooze a task and move every itm client on it to the next task (R4.7).
    pub fn snooze_task(&self, name: &str, until: Until) -> Result<()> {
        if until == Until::Agent
            && let Some(problem) = Self::agent_snooze_problem(name, &self.herdr.sessions()?, &self.status())
        {
            bail!("cannot snooze until an agent needs you: {problem}");
        }
        let data = self.store().edit(|data| {
            existing(data, name)?.snooze(until, times::now())?;
            Ok(data.clone())
        })?;
        self.move_clients_off(name, &data)
    }

    pub fn wake_task(&self, name: &str) -> Result<()> {
        self.store().edit(|data| {
            let task = existing(data, name)?;
            if task.state != State::Snoozed {
                bail!("{name} is not snoozed");
            }
            task.activate();
            Ok(())
        })
    }

    /// Archive a task: mark it, move itm clients off it, then stop its
    /// session (after the hand-off: the session may host the caller).
    pub fn archive_task(&self, name: &str) -> Result<()> {
        let data = self.store().edit(|data| {
            existing(data, name)?.archive(times::now());
            Ok(data.clone())
        })?;
        self.move_clients_off(name, &data)?;
        match self.herdr.sessions()?.get(name) {
            Some(session) if session.running => self
                .herdr
                .stop(&session.socket)
                .map_err(|e| Error(format!("{name} archived, but its session did not stop: {e}"))),
            _ => Ok(()),
        }
    }

    /// Make an archived (or snoozed) task active again; returns its session name.
    pub fn reopen_task(&self, query: &str) -> Result<String> {
        self.store().edit(|data| {
            let name = tasks::resolve_task(data, query)?;
            existing(data, &name)?.activate();
            Ok(name)
        })
    }

    /// Mark a task used and active, adopting a `task-*` session itm did not
    /// make. Returns the store as saved; None for sessions that are not tasks.
    pub fn touch_task(&self, name: &str) -> Result<Option<Data>> {
        if !tasks::is_task_session(name) {
            return Ok(None);
        }
        let now = times::now();
        self.store()
            .edit(|data| {
                data.deleted.remove(name);
                let task = data.tasks.entry(name.into()).or_insert_with(|| Task::adopted(name, now));
                task.activate();
                task.last_used_at = now;
                Ok(data.clone())
            })
            .map(Some)
    }

    /// Switch every itm client attached to `name` to the next active task.
    fn move_clients_off(&self, name: &str, data: &Data) -> Result<()> {
        let next = tasks::next_active(data, &self.status(), Some(name));
        self.handoff(&next, Some(name), false).map(drop)
    }
}

/// The task named `name`, re-resolved inside a locked edit: it may have been deleted meanwhile.
fn existing<'a>(data: &'a mut Data, name: &str) -> Result<&'a mut Task> {
    data.tasks.get_mut(name).ok_or_else(|| Error(format!("task {name} no longer exists")))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::herdr::{Session, fake};
    use crate::status::{AgentState, AgentStatus, Cache, SessionStatus};
    use crate::store::python_json;
    use crate::switching::tests::FakeProcs;
    use serde_json::json;
    use std::fs;

    pub fn itm() -> (tempfile::TempDir, Itm) {
        // Socket paths must stay under ~108 bytes: /tmp, or the Nix build's /build.
        let base = if std::path::Path::new("/tmp").is_dir() { "/tmp".into() } else { std::env::temp_dir() };
        let tmp = tempfile::Builder::new().prefix("itm").tempdir_in(base).unwrap();
        let itm = Itm::under(tmp.path(), FakeProcs::default());
        (tmp, itm)
    }

    pub fn write_status(itm: &Itm, sessions: &[(&str, usize)]) {
        let blocked = |n| SessionStatus {
            blocked: n,
            agents: (0..n)
                .map(|_| AgentStatus {
                    agent: "claude".into(),
                    status: AgentState::Blocked,
                    title: String::new(),
                    pane_id: None,
                    since: Some(1),
                })
                .collect(),
        };
        let sessions = sessions.iter().map(|&(name, n)| (name.to_string(), blocked(n))).collect();
        let cache = Cache { updated_at: times::now(), sessions };
        crate::store::write_atomic(&itm.paths.status_file().unwrap(), &python_json(&cache, false)).unwrap();
    }

    #[test]
    fn a_new_task_is_active_with_a_unique_name_and_a_trimmed_description() {
        let (_tmp, itm) = itm();
        assert_eq!(itm.create_task(" Fix bug ", Some("  token expires  "), &[], None).unwrap(), "task-fix-bug");
        assert_eq!(itm.create_task("Fix bug", Some("   "), &[], None).unwrap(), "task-fix-bug-2");
        let data = itm.store().load().unwrap();
        let (first, second) = (&data.tasks["task-fix-bug"], &data.tasks["task-fix-bug-2"]);
        assert_eq!((first.title.as_str(), first.description.as_deref()), ("Fix bug", Some("token expires")));
        assert_eq!((second.state, second.description.as_deref()), (State::Active, None));
    }

    #[test]
    fn a_name_is_not_reused_while_its_session_or_notes_exist() {
        let (_tmp, itm) = itm();
        fs::create_dir_all(itm.paths.herdr_config.join("sessions/task-a")).unwrap();
        fs::create_dir_all(itm.paths.data.join("tasks/task-a-2")).unwrap();
        assert_eq!(itm.create_task("A", None, &[], None).unwrap(), "task-a-3");
    }

    #[test]
    fn invalid_input_writes_nothing() {
        let (_tmp, itm) = itm();
        assert!(itm.create_task("  ", None, &[], None).is_err());
        let err = itm.create_task("Asked", None, &[], Some(Until::Agent)).unwrap_err();
        assert!(err.0.contains("new task has no agents"));
        assert!(itm.create_task("Late", None, &[], Some(Until::Time(times::now() - 5))).is_err());
        assert!(itm.store().load().unwrap().tasks.is_empty());
    }

    #[test]
    fn snoozing_until_an_agent_needs_a_way_to_wake() {
        let (_tmp, itm) = itm();
        itm.create_task("Run", None, &[], None).unwrap();
        let err = itm.snooze_task("task-run", Until::Agent).unwrap_err();
        assert!(err.0.contains("not running"), "{err}");

        fake::serve(&itm.paths.herdr_config, "task-run", |_, _| json!({}));
        write_status(&itm, &[("task-run", 1)]);
        assert!(itm.snooze_task("task-run", Until::Agent).unwrap_err().0.contains("already waiting"));

        write_status(&itm, &[]);
        itm.snooze_task("task-run", Until::Agent).unwrap();
        let task = &itm.store().load().unwrap().tasks["task-run"];
        assert_eq!((task.state, task.snoozed_until), (State::Snoozed, None));
    }

    #[test]
    fn wake_archive_and_reopen() {
        let (_tmp, itm) = itm();
        itm.create_task("A", None, &[], Some(Until::Time(times::now() + 600))).unwrap();
        itm.wake_task("task-a").unwrap();
        assert_eq!(itm.wake_task("task-a").unwrap_err().0, "task-a is not snoozed");

        let stops = fake::serve(&itm.paths.herdr_config, "task-a", |_, _| json!({"type": "ok"}));
        itm.archive_task("task-a").unwrap();
        let task = itm.store().load().unwrap().tasks["task-a"].clone();
        assert!(task.state == State::Archived && task.archived_at.is_some());
        assert_eq!(stops.lock().unwrap().iter().map(|(m, _)| m.as_str()).collect::<Vec<_>>(), ["server.stop"]);

        assert_eq!(itm.reopen_task("a").unwrap(), "task-a");
        assert_eq!(itm.store().load().unwrap().tasks["task-a"].state, State::Active);
        assert_eq!(itm.archive_task("task-gone").unwrap_err().0, "task task-gone no longer exists");
    }

    #[test]
    fn the_current_task_is_found_by_its_socket() {
        let (_tmp, mut itm) = itm();
        itm.create_task("Here", None, &[], None).unwrap();
        let data = itm.store().load().unwrap();
        assert_eq!(itm.target_or_current(&data, None).unwrap_err().0, "not inside a task; name the task");
        fake::serve(&itm.paths.herdr_config, "task-here", |_, _| json!({}));
        itm.client_socket = Some(itm.herdr.socket("task-here"));
        assert_eq!(itm.target_or_current(&data, None).unwrap(), "task-here");
        let sessions: Sessions =
            [("x".to_string(), Session { name: "x".into(), running: true, socket: "/s".into() })].into();
        assert_eq!(itm.current_session(&sessions), None);
    }
}
