//! `Itm`: the folders, Herdr and process table one run of itm works with,
//! and the task operations the CLI, the switcher and the daemon share.

use std::collections::BTreeSet;
use std::env;
use std::path::PathBuf;

use jiff::tz::TimeZone;

use crate::error::{Error, Result, bail};
use crate::git::Git;
use crate::herdr::{Herdr, Sessions};
use crate::paths::Paths;
use crate::projects::resolve_project;
use crate::snooze::Until;
use crate::status::Status;
use crate::store::{Data, State, Store, Task};
use crate::system::Processes;
use crate::tasks::{self, Edit};
use crate::times;
use crate::workspaces::Repositories;

pub struct Itm {
    pub paths: Paths,
    pub herdr: Herdr,
    pub git: Git,
    /// The git repositories of workspace folders, remembered (R10.2).
    pub repositories: Repositories,
    pub procs: Box<dyn Processes>,
    /// This program, for the children it starts (a TUI's clone): `ITM_SELF`
    /// when set (a wrapper that sets up `PATH`), else this binary.
    pub program: PathBuf,
    /// `HERDR_SOCKET_PATH`: the session this terminal is in, when inside Herdr.
    pub client_socket: Option<PathBuf>,
    /// `ITM_NOTIFY_COMMAND`.
    pub notify_command: Option<String>,
    pub tz: TimeZone,
}

impl Itm {
    pub fn from_env() -> Itm {
        let paths = Paths::from_env();
        let program = env::var_os("ITM_SELF").filter(|v| !v.is_empty()).map(PathBuf::from);
        Itm {
            herdr: Herdr::new(paths.herdr_config.clone()),
            git: Git::default(),
            repositories: Repositories::default(),
            paths,
            procs: crate::system::process_table(),
            program: program.or_else(|| env::current_exe().ok()).unwrap_or_else(|| "itm".into()),
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
            git: Git::default(),
            repositories: Repositories::default(),
            paths,
            procs: Box::new(procs),
            program: "itm".into(),
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
                let _ = self.ensure_notes(&name, &data.tasks[&name], &data.projects);
                name
            })
    }

    /// Change a task's title, description or projects by hand (R5.4); returns
    /// what that did besides. The edit is saved first: what follows only
    /// writes the notes (R9.3), reports, or opens a workspace per added
    /// project in a running session, and a failure there never undoes it.
    pub fn edit_task(&self, session: &str, edit: &Edit) -> Result<Vec<String>> {
        // Only a running daemon keeps the record of open workspaces current.
        let keep_open = self.follows_workspaces() && self.status().fresh;
        let now = times::now();
        let (edited, projects) = self.store().edit(|data| {
            let edited = tasks::edit_task(data, session, edit, keep_open, now)?;
            Ok((edited, data.projects.clone()))
        })?;
        let mut entries = edited.entries;
        let place = |p: &crate::store::Project| format!("{} ({})", p.name, self.paths.place(p));
        entries.extend(edited.added.iter().map(|p| ("Project added", Some(place(p)))));
        entries.extend(edited.removed.iter().map(|name| ("Project removed", Some(name.clone()))));
        // Before the Herdr calls below, which can take until their timeouts.
        let notes_failed = match entries.is_empty() {
            true => None,
            false => self
                .ensure_notes(session, &edited.before, &projects) // a task without notes: start them as it was
                .and_then(|_| self.append_notes(session, &entries, now))
                .err()
                .map(|e| format!("could not update its notes: {e}")),
        };
        let mut said = match edited.added.is_empty() || edit.has_workspace {
            true => vec![],
            false => self.project_change_messages(session, &edited.added),
        };
        if !edited.kept.is_empty() {
            said.push(format!(
                "could not remove {}: it still has a workspace in this task; close it in Herdr first, or turn off \"projects follow workspaces\" in the settings",
                edited.kept.join(", ")
            ));
        }
        said.extend(notes_failed);
        Ok(said)
    }

    /// Delete a task for good (R5.3): a tombstone keeps its session from
    /// being adopted again while Herdr deletes it; its notes go too (a later
    /// task may get its session name); its projects stay. Clients on it move
    /// to the next task first, since the caller may be one of them.
    pub fn delete_task(&self, name: &str) -> Result<()> {
        let data = self.store().edit(|data| {
            data.tasks.remove(name).ok_or_else(|| Error::TaskGone(name.into()))?;
            data.deleted.insert(name.into(), times::now());
            Ok(data.clone())
        })?;
        let _ = self.remove_notes(name); // a name that cannot have notes has none
        self.move_clients_off(name, &data)?;
        let Some(session) = self.herdr.sessions()?.remove(name) else { return Ok(()) }; // it never started
        if session.running {
            self.herdr
                .stop(&session.socket)
                .map_err(|e| Error::Other(format!("{name} deleted, but its session did not stop: {e}")))?;
        }
        self.herdr
            .delete_detached(name)
            .map_err(|e| Error::Other(format!("{name} deleted, but Herdr keeps its session: {e}")))
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
                .map_err(|e| Error::Other(format!("{name} archived, but its session did not stop: {e}"))),
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
    data.tasks.get_mut(name).ok_or_else(|| Error::TaskGone(name.into()))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::herdr::{Session, fake};
    use crate::status::{AgentState, AgentStatus, Cache, SessionStatus};
    use crate::store::python_json;
    use crate::switching::tests::FakeProcs;
    use serde_json::{Value, json};
    use std::fs;

    pub fn itm() -> (tempfile::TempDir, Itm) {
        // Socket paths must stay under ~108 bytes: /tmp, or the Nix build's /build.
        let base = if std::path::Path::new("/tmp").is_dir() { "/tmp".into() } else { std::env::temp_dir() };
        // Resolved, as itm stores folders: /tmp may be a link (DESIGN.md section 6).
        let base = base.canonicalize().unwrap_or(base);
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
        assert!(err.to_string().contains("new task has no agents"));
        assert!(itm.create_task("Late", None, &[], Some(Until::Time(times::now() - 5))).is_err());
        assert!(itm.store().load().unwrap().tasks.is_empty());
    }

    #[test]
    fn snoozing_until_an_agent_needs_a_way_to_wake() {
        let (_tmp, itm) = itm();
        itm.create_task("Run", None, &[], None).unwrap();
        let err = itm.snooze_task("task-run", Until::Agent).unwrap_err();
        assert!(err.to_string().contains("not running"), "{err}");

        fake::serve(&itm.paths.herdr_config, "task-run", |_, _| json!({}));
        write_status(&itm, &[("task-run", 1)]);
        assert!(itm.snooze_task("task-run", Until::Agent).unwrap_err().to_string().contains("already waiting"));

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
        assert_eq!(itm.wake_task("task-a").unwrap_err().to_string(), "task-a is not snoozed");

        let stops = fake::serve(&itm.paths.herdr_config, "task-a", |_, _| json!({"type": "ok"}));
        itm.archive_task("task-a").unwrap();
        let task = itm.store().load().unwrap().tasks["task-a"].clone();
        assert!(task.state == State::Archived && task.archived_at.is_some());
        assert_eq!(stops.lock().unwrap().iter().map(|(m, _)| m.as_str()).collect::<Vec<_>>(), ["server.stop"]);

        assert_eq!(itm.reopen_task("a").unwrap(), "task-a");
        assert_eq!(itm.store().load().unwrap().tasks["task-a"].state, State::Active);
        assert_eq!(itm.archive_task("task-gone").unwrap_err(), Error::TaskGone("task-gone".into()));
    }

    #[test]
    fn the_current_task_is_found_by_its_socket() {
        let (_tmp, mut itm) = itm();
        itm.create_task("Here", None, &[], None).unwrap();
        let data = itm.store().load().unwrap();
        assert_eq!(itm.target_or_current(&data, None).unwrap_err().to_string(), "not inside a task; name the task");
        fake::serve(&itm.paths.herdr_config, "task-here", |_, _| json!({}));
        itm.client_socket = Some(itm.herdr.socket("task-here"));
        assert_eq!(itm.target_or_current(&data, None).unwrap(), "task-here");
        let sessions: Sessions =
            [("x".to_string(), Session { name: "x".into(), running: true, socket: "/s".into() })].into();
        assert_eq!(itm.current_session(&sessions), None);
    }

    // ------------------------------------------------------------ edit (R5.4) and delete (R5.3)

    use crate::projects::tests::folder;
    use crate::store::ProjectKind;

    fn edit(itm: &Itm, session: &str, change: impl FnOnce(&mut Edit)) -> Result<Vec<String>> {
        let mut edit = Edit::default();
        change(&mut edit);
        itm.edit_task(session, &edit)
    }

    fn strings(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    /// A change to the store, as a hand edit of tasks.json would make it.
    fn change(itm: &Itm, f: impl FnOnce(&mut Data)) {
        itm.store()
            .edit(|data| {
                f(data);
                Ok(())
            })
            .unwrap()
    }

    fn task(itm: &Itm, session: &str) -> Task {
        itm.store().load().unwrap().tasks[session].clone()
    }

    /// Saves folder projects `~/code/<name>` for each name; their ids.
    fn projects<const N: usize>(itm: &Itm, names: [&str; N]) -> [String; N] {
        names.map(|name| {
            folder(itm, &format!("code/{name}"));
            itm.add_project(ProjectKind::Dir, &format!("~/code/{name}"), None).unwrap()
        })
    }

    fn notes(itm: &Itm, session: &str) -> String {
        fs::read_to_string(itm.notes_path(session).unwrap()).unwrap()
    }

    /// A session socket that takes a request and closes without an answer.
    fn mute_session(itm: &Itm, session: &str) {
        let socket = itm.herdr.socket(session);
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut request = String::new();
                let _ = std::io::BufRead::read_line(&mut std::io::BufReader::new(&stream), &mut request);
            }
        });
    }

    #[test]
    fn r5_4_title_and_description() {
        let (_tmp, itm) = itm();
        itm.create_task("Old", Some("text"), &[], None).unwrap();
        assert_eq!(
            edit(&itm, "task-old", |e| (e.title, e.description) = (Some(" New ".into()), Some("".into()))).unwrap(),
            Vec::<String>::new()
        );
        let edited = task(&itm, "task-old");
        assert_eq!((edited.title.as_str(), edited.description), ("New", None));
        let err = edit(&itm, "task-old", |e| e.title = Some(" ".into())).unwrap_err();
        assert_eq!(err.to_string(), "task title cannot be empty");
        assert_eq!(
            edit(&itm, "task-gone", |e| e.title = Some("x".into())).unwrap_err(),
            Error::TaskGone("task-gone".into())
        );
    }

    /// R5.4: projects are set, added and removed, and the edit says what happens to the session.
    #[test]
    fn r5_4_projects_change_and_say_what_happens_to_the_session() {
        let (_tmp, itm) = itm();
        let [api, web] = projects(&itm, ["api", "web"]);
        itm.create_task("Later", None, &[], Some(Until::Time(times::now() + 3600))).unwrap();
        let starts = |name: &str| vec![format!("its session will start with a workspace in ~/code/{name}")];
        assert_eq!(edit(&itm, "task-later", |e| e.add = strings(&["api"])).unwrap(), starts("api"));
        assert_eq!(edit(&itm, "task-later", |e| e.add = strings(&["web"])).unwrap(), starts("web"));
        assert_eq!(task(&itm, "task-later").projects, [api.clone(), web.clone()]);
        assert_eq!(edit(&itm, "task-later", |e| e.remove = strings(&["API"])).unwrap(), Vec::<String>::new());
        assert_eq!(task(&itm, "task-later").projects, std::slice::from_ref(&web));
        edit(&itm, "task-later", |e| e.remove = strings(&["api"])).unwrap(); // not there: nothing to do
        assert_eq!(task(&itm, "task-later").projects, std::slice::from_ref(&web));
        let err = edit(&itm, "task-later", |e| e.remove = strings(&["nope"])).unwrap_err();
        assert_eq!(err, Error::NoSuchProject("nope".into()));
        assert_eq!(edit(&itm, "task-later", |e| e.projects = Some(vec![])).unwrap(), Vec::<String>::new());
        assert!(task(&itm, "task-later").projects.is_empty());

        fs::create_dir_all(itm.paths.herdr_config.join("sessions/task-later")).unwrap(); // stopped
        let said = edit(&itm, "task-later", |e| {
            (e.title, e.projects) = (Some("Fine".into()), Some(strings(&["web", "api", "WEB"])))
        })
        .unwrap();
        assert_eq!(
            said,
            ["its stopped session keeps its workspaces; open one in ~/code/web, ~/code/api when you need it"]
        );
        let edited = task(&itm, "task-later");
        assert_eq!((edited.title.as_str(), edited.projects), ("Fine", vec![web, api]));
        // Unchanged projects named again (any case) are no change and no error.
        assert_eq!(
            edit(&itm, "task-later", |e| e.projects = Some(strings(&["WEB", "api"]))).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(edit(&itm, "task-later", |e| e.add = strings(&["API"])).unwrap(), Vec::<String>::new());
    }

    /// R5.4: a running session gets a workspace per added project, named
    /// after it; a failure to open one is said and undoes nothing.
    #[test]
    fn r5_4_a_running_session_gets_a_named_workspace_per_new_project() {
        let (_tmp, itm) = itm();
        let ids = projects(&itm, ["api", "web"]);
        itm.create_task("Run", None, &[], None).unwrap();
        let calls = fake::serve(&itm.paths.herdr_config, "task-run", |_, _| json!({}));
        let said = edit(&itm, "task-run", |e| e.add = strings(&["api", "web"])).unwrap();
        assert_eq!(said, ["opened a workspace in ~/code/api", "opened a workspace in ~/code/web"]);
        let home = &itm.paths.home;
        let created: Vec<Value> =
            calls.lock().unwrap().iter().filter(|(m, _)| m == "workspace.create").map(|(_, p)| p.clone()).collect();
        assert_eq!(
            created,
            [
                json!({"cwd": home.join("code/api"), "label": "api", "focus": false}),
                json!({"cwd": home.join("code/web"), "label": "web", "focus": false}),
            ]
        );
        assert_eq!(task(&itm, "task-run").projects, ids);

        itm.create_task("Mute", None, &[], None).unwrap();
        mute_session(&itm, "task-mute");
        let said = edit(&itm, "task-mute", |e| e.add = strings(&["api"])).unwrap();
        assert_eq!(said, ["could not open a workspace in ~/code/api: herdr workspace.create returned invalid JSON"]);
        assert_eq!(task(&itm, "task-mute").projects, ids[..1]);
    }

    /// R5.4: the task this runs in gets the first new workspace focused.
    #[test]
    fn r5_4_editing_the_task_you_are_in_focuses_the_new_workspace() {
        let (_tmp, mut itm) = itm();
        projects(&itm, ["api"]);
        itm.create_task("Here", None, &[], None).unwrap();
        let calls = fake::serve(&itm.paths.herdr_config, "task-here", |_, _| json!({}));
        itm.client_socket = Some(itm.herdr.socket("task-here"));
        edit(&itm, "task-here", |e| e.add = strings(&["api"])).unwrap();
        assert_eq!(calls.lock().unwrap().last().unwrap().1["focus"], json!(true));
    }

    /// R5.4: projects without a folder, or whose folder is missing, get no
    /// workspace, which is said; so is a Herdr that cannot be asked.
    #[test]
    fn r5_4_what_gets_no_workspace_is_said() {
        let (_tmp, itm) = itm();
        projects(&itm, ["api"]);
        itm.add_project(ProjectKind::Name, "Client X", None).unwrap();
        itm.create_task("Run", None, &[], None).unwrap();
        let calls = fake::serve(&itm.paths.herdr_config, "task-run", |_, _| json!({}));
        let said = edit(&itm, "task-run", |e| e.add = strings(&["Client X"])).unwrap();
        assert_eq!(said, ["Client X has no folder, so it gets no workspace"]);
        assert!(calls.lock().unwrap().iter().all(|(m, _)| m != "workspace.create"));
        assert!(notes(&itm, "task-run").contains("Project added\n\nClient X (no folder)\n"));
        fs::remove_dir(itm.paths.home.join("code/api")).unwrap();
        let said = edit(&itm, "task-run", |e| e.add = strings(&["api"])).unwrap();
        assert_eq!(said, ["~/code/api is missing, so it gets no workspace"]);
        assert_eq!(task(&itm, "task-run").projects.len(), 2, "saved anyway");
    }

    #[test]
    fn r5_4_a_herdr_that_cannot_be_asked_is_said_and_the_edit_saved() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, itm) = itm();
        let ids = projects(&itm, ["api"]);
        itm.create_task("Run", None, &[], None).unwrap();
        let sessions = itm.paths.herdr_config.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::set_permissions(&sessions, fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = fs::read_dir(&sessions).is_err(); // root reads it anyway
        // Removing or keeping projects never asks Herdr.
        let quiet = edit(&itm, "task-run", |e| (e.title, e.projects) = (Some("T".into()), Some(vec![])));
        let said = edit(&itm, "task-run", |e| e.add = strings(&["api"]));
        fs::set_permissions(&sessions, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(quiet.unwrap(), Vec::<String>::new());
        assert_eq!(task(&itm, "task-run").projects, ids);
        if unreadable {
            let said = said.unwrap();
            assert!(
                said[0].starts_with("could not tell whether its session runs (cannot list Herdr sessions"),
                "{said:?}"
            );
            assert!(said[0].ends_with("; open a workspace in ~/code/api if it does"), "{said:?}");
        }
    }

    /// R10.1: naming a project by hand makes it the user's: sync no longer removes it.
    #[test]
    fn r5_4_a_project_named_by_hand_is_no_longer_one_sync_found() {
        let (_tmp, itm) = itm();
        let [api, web] = projects(&itm, ["api", "web"]);
        itm.create_task("Run", None, &["api".into(), "web".into()], None).unwrap();
        change(&itm, |data| data.tasks.get_mut("task-run").unwrap().found_projects = vec![api.clone(), web.clone()]);
        edit(&itm, "task-run", |e| e.add = strings(&["web"])).unwrap(); // already there: now the user's
        assert_eq!(task(&itm, "task-run").found_projects, std::slice::from_ref(&api));
        edit(&itm, "task-run", |e| e.remove = strings(&["api"])).unwrap();
        let edited = task(&itm, "task-run");
        assert_eq!((edited.projects, edited.found_projects), (vec![web], vec![]));
    }

    /// R5.4: while projects follow workspaces and the daemon runs, a project
    /// whose workspace is open is kept and said so; the rest is saved.
    #[test]
    fn r5_4_a_project_with_an_open_workspace_is_kept_while_the_daemon_runs() {
        let (_tmp, itm) = itm();
        let [api, _] = projects(&itm, ["api", "web"]);
        itm.create_task("Run", None, &["api".into(), "web".into()], None).unwrap();
        itm.store()
            .edit(|data| {
                let run = data.tasks.get_mut("task-run").unwrap();
                (run.found_projects, run.workspace_projects) = (vec![api.clone()], vec![api.clone()]);
                Ok(())
            })
            .unwrap();
        write_status(&itm, &[]);
        let said = edit(&itm, "task-run", |e| (e.title, e.remove) = (Some("Renamed".into()), strings(&["api", "web"])))
            .unwrap();
        assert_eq!(
            said,
            [
                "could not remove api: it still has a workspace in this task; close it in Herdr first, or turn off \"projects follow workspaces\" in the settings"
            ]
        );
        let edited = task(&itm, "task-run");
        assert_eq!((edited.title.as_str(), edited.projects), ("Renamed", vec![api.clone()]));
        // Without a running daemon nothing keeps the record current: it does not hold the project.
        fs::remove_file(itm.paths.status_file().unwrap()).unwrap();
        assert_eq!(edit(&itm, "task-run", |e| e.remove = strings(&["api"])).unwrap(), Vec::<String>::new());
        assert!(task(&itm, "task-run").projects.is_empty());

        // R10.4: with "projects follow workspaces" off, or a broken settings file, nothing keeps it.
        let open = |itm: &Itm| {
            change(itm, |data| {
                let run = data.tasks.get_mut("task-run").unwrap();
                (run.projects, run.workspace_projects) = (vec![api.clone()], vec![api.clone()]);
            })
        };
        write_status(&itm, &[]);
        itm.change_setting(crate::config::Setting::ProjectsFromWorkspaces, false).unwrap();
        for settings in ["[herdr]\nprojects_from_workspaces = false\n", "[herdr\n"] {
            fs::write(itm.paths.config_file(), settings).unwrap();
            open(&itm);
            assert_eq!(edit(&itm, "task-run", |e| e.remove = strings(&["api"])).unwrap(), Vec::<String>::new());
            assert!(task(&itm, "task-run").projects.is_empty(), "{settings}");
        }
    }

    /// R10.5: the workspace hotkey's edit adds the project as found in the
    /// workspace, and opens none (the workspace is there).
    #[test]
    fn r10_5_an_edit_can_add_a_project_found_in_its_workspace() {
        let (_tmp, itm) = itm();
        let [api] = projects(&itm, ["api"]);
        itm.create_task("Run", None, &[], None).unwrap();
        let calls = fake::serve(&itm.paths.herdr_config, "task-run", |_, _| json!({}));
        let said = edit(&itm, "task-run", |e| (e.add, e.found, e.has_workspace) = (vec![api.clone()], true, true));
        assert_eq!(said.unwrap(), Vec::<String>::new());
        assert!(calls.lock().unwrap().is_empty(), "Herdr is not asked");
        let edited = task(&itm, "task-run");
        assert_eq!((edited.projects, edited.found_projects), (vec![api.clone()], vec![api]));
        assert!(notes(&itm, "task-run").contains("Project added\n\napi (~/code/api)\n"));
    }

    /// The order puts the projects it names first; ids of projects gone from
    /// a hand-edited file can still be removed, and are logged by their id.
    #[test]
    fn r5_4_order_and_projects_no_longer_saved() {
        let (_tmp, itm) = itm();
        let [api, web] = projects(&itm, ["api", "web"]);
        itm.create_task("Run", None, &["api".into(), "web".into()], None).unwrap();
        edit(&itm, "task-run", |e| e.order = vec![web.clone(), "nope".into()]).unwrap();
        assert_eq!(task(&itm, "task-run").projects, [web.clone(), api.clone()]);
        change(&itm, |data| data.tasks.get_mut("task-run").unwrap().projects = strings(&["gone", &api]));
        edit(&itm, "task-run", |e| e.remove = strings(&["gone"])).unwrap();
        assert_eq!(task(&itm, "task-run").projects, [api]);
        change(&itm, |data| data.tasks.get_mut("task-run").unwrap().projects = strings(&["gone"]));
        edit(&itm, "task-run", |e| e.projects = Some(vec![])).unwrap();
        assert!(notes(&itm, "task-run").contains("Project removed\n\ngone\n"));
    }

    /// R9.3: only real changes are logged, with their new value.
    #[test]
    fn r9_3_edits_append_only_what_changed() {
        let (_tmp, itm) = itm();
        projects(&itm, ["api"]);
        itm.create_task("Fix login", Some("old text"), &[], None).unwrap();
        let before = notes(&itm, "task-fix-login");
        edit(&itm, "task-fix-login", |e| {
            (e.title, e.description) = (Some(" Fix login ".into()), Some("old text ".into()))
        })
        .unwrap();
        assert_eq!(notes(&itm, "task-fix-login"), before, "nothing really changed");
        edit(&itm, "task-fix-login", |e| {
            (e.title, e.description) = (Some("Fix login on Safari".into()), Some("ITP drops it".into()))
        })
        .unwrap();
        edit(&itm, "task-fix-login", |e| e.add = strings(&["api"])).unwrap();
        edit(&itm, "task-fix-login", |e| (e.description, e.projects) = (Some("".into()), Some(vec![]))).unwrap();
        let added = notes(&itm, "task-fix-login")[before.len()..].to_string();
        let stamp = |line: &str| line.starts_with("## ") && line.as_bytes()[7] == b'-' && line[13..].starts_with(' ');
        let headings: Vec<&str> = added.lines().filter(|l| stamp(l)).map(|l| &l[20..]).collect();
        assert_eq!(
            headings,
            [
                "· Title changed",
                "· Description changed",
                "· Project added",
                "· Description cleared",
                "· Project removed"
            ]
        );
        let bodies: Vec<&str> = added.lines().filter(|l| !l.is_empty() && !stamp(l)).collect();
        assert_eq!(bodies, ["Fix login on Safari", "ITP drops it", "api (~/code/api)", "api"]);
        assert!(added.starts_with("\n## ") && added.ends_with("api\n\n"), "{added:?}");
    }

    /// R9.3: a task without notes yet gets them from before the edit, then the edit.
    #[test]
    fn r9_3_a_task_without_notes_gets_them_from_before_the_edit() {
        let (_tmp, itm) = itm();
        itm.store()
            .edit(|data| {
                let mut old = Task::new("Old title", 1);
                old.description = Some("old text".into());
                Ok(data.tasks.insert("task-old".into(), old)) // from before notes existed, or adopted
            })
            .unwrap();
        edit(&itm, "task-old", |e| e.title = Some("New title".into())).unwrap();
        let text = notes(&itm, "task-old");
        assert!(text.starts_with("# Old title\n") && text.contains("old text\n\n## "), "{text}");
        assert!(text.contains("Title changed\n\nNew title\n"), "{text}");
    }

    /// DESIGN.md section 2: a failure to write notes never undoes the change; it is said.
    #[test]
    fn r9_3_failing_to_write_notes_keeps_the_edit() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, itm) = itm();
        itm.create_task("Fix", None, &[], None).unwrap();
        let path = itm.notes_path("task-fix").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        let unwritable = fs::OpenOptions::new().append(true).open(&path).is_err(); // root writes anyway
        let said = edit(&itm, "task-fix", |e| e.title = Some("Fixed".into())).unwrap();
        assert_eq!(task(&itm, "task-fix").title, "Fixed");
        if unwritable {
            assert_eq!(said.len(), 1);
            assert!(said[0].starts_with("could not update its notes: "), "{said:?}");
        }
    }

    /// R5.3, R9.1: a deleted task leaves a tombstone, takes its notes (other
    /// files stay) and frees its name; its projects stay.
    #[test]
    fn r5_3_delete_leaves_a_tombstone_and_takes_the_notes() {
        let (_tmp, itm) = itm();
        projects(&itm, ["api"]);
        itm.create_task("Gone", None, &["api".into()], None).unwrap();
        itm.create_task("Kept", None, &[], None).unwrap();
        let screenshot = itm.notes_path("task-kept").unwrap().with_file_name("screenshot.png");
        fs::write(&screenshot, "png").unwrap();
        itm.delete_task("task-gone").unwrap(); // never started: nothing for Herdr to delete
        itm.delete_task("task-kept").unwrap();
        let data = itm.store().load().unwrap();
        assert!(
            data.tasks.is_empty() && data.deleted.contains_key("task-gone") && data.deleted.contains_key("task-kept")
        );
        assert_eq!(data.projects.len(), 1, "its project stays");
        assert!(!itm.notes_path("task-gone").unwrap().parent().unwrap().exists());
        assert!(!itm.notes_path("task-kept").unwrap().exists() && screenshot.exists());
        assert_eq!(itm.delete_task("task-gone").unwrap_err(), Error::TaskGone("task-gone".into()));
        change(&itm, |data| data.deleted.clear()); // its session is gone: the tombstone expired
        assert_eq!(itm.create_task("Gone", None, &[], None).unwrap(), "task-gone", "the same name again");
        assert!(notes(&itm, "task-gone").starts_with("# Gone\n"), "fresh notes");
    }

    /// R9.1: snooze, archive and reopen keep the notes.
    #[test]
    fn r9_1_archive_and_reopen_keep_the_notes() {
        let (_tmp, itm) = itm();
        itm.create_task("Keep me", None, &[], None).unwrap();
        fs::write(itm.notes_path("task-keep-me").unwrap(), "# Keep me\n\ncontext\n").unwrap();
        itm.snooze_task("task-keep-me", Until::Time(times::now() + 60)).unwrap();
        itm.archive_task("task-keep-me").unwrap();
        itm.reopen_task("task-keep-me").unwrap();
        assert_eq!(notes(&itm, "task-keep-me"), "# Keep me\n\ncontext\n");
    }
}
