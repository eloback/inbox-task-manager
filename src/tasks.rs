//! Tasks and their states (REQUIREMENTS.md, "Task states"): what can happen
//! to a task, independent of where it is stored or how it is shown.

use std::collections::{BTreeMap, BTreeSet};

use unicode_normalization::{UnicodeNormalization, char::canonical_combining_class};

use crate::error::{Error, Result, bail};
use crate::notes::Entry;
use crate::projects::resolve_project;
use crate::snooze::{MAX_SNOOZE, Until};
use crate::status::Status;
use crate::store::{Data, Project, State, Task};

/// Only sessions with this prefix are tasks; itm never adopts, stops or
/// deletes any other session.
pub const PREFIX: &str = "task-";
/// Where a client goes when it leaves a task and no other task is active.
pub const DEFAULT_SESSION: &str = "default";

pub fn is_task_session(name: &str) -> bool {
    name.starts_with(PREFIX)
}

impl Task {
    pub fn new(title: &str, now: i64) -> Task {
        Task {
            title: title.into(),
            state: State::Active,
            created_at: now,
            last_used_at: now,
            description: None,
            projects: vec![],
            found_projects: vec![],
            workspace_projects: vec![],
            snoozed_until: None,
            archived_at: None,
            extra: BTreeMap::new(),
        }
    }

    /// A task for a `task-*` session made outside itm, titled after its name.
    pub fn adopted(session: &str, now: i64) -> Task {
        Task::new(&session[PREFIX.len()..].replace('-', " "), now)
    }

    pub fn activate(&mut self) {
        self.state = State::Active;
        self.snoozed_until = None;
        self.archived_at = None;
    }

    /// Snooze this task. A time must be in the future and at most 10 years
    /// away, checked here for every caller (a preset may pass while a dialog is open).
    pub fn snooze(&mut self, until: Until, now: i64) -> Result<()> {
        if let Until::Time(ts) = until
            && !(now < ts && ts <= now + MAX_SNOOZE)
        {
            bail!("snooze time must be in the future");
        }
        self.state = State::Snoozed;
        self.archived_at = None;
        self.snoozed_until = match until {
            Until::Time(ts) => Some(ts),
            Until::Agent => None, // no timer: only a blocked agent wakes it
        };
        Ok(())
    }

    pub fn archive(&mut self, now: i64) {
        self.state = State::Archived;
        self.archived_at = Some(now);
        self.snoozed_until = None;
        self.workspace_projects.clear(); // its workspaces close with the session
    }

    /// Set the projects, once each, in order. `found` are the ones workspace
    /// sync added (R10.1); None keeps those it added before that remain.
    pub fn set_projects(&mut self, projects: Vec<String>, found: Option<Vec<String>>) {
        let mut seen = BTreeSet::new();
        self.projects = projects.into_iter().filter(|id| seen.insert(id.clone())).collect();
        let found = found.unwrap_or_else(|| std::mem::take(&mut self.found_projects));
        self.found_projects = found.into_iter().filter(|id| self.projects.contains(id)).collect();
    }
}

impl Task {
    /// Workspace sync (R10.3): add the projects the running session's
    /// workspaces are in (`open`), and (`remove`) drop the ones whose
    /// workspace closed. Only a project this sync added (`found_projects`)
    /// is removed, and only once its workspace was open on an earlier pass
    /// (`workspace_projects`) and is gone now: one being opened is not
    /// removed. A pass that cannot be trusted (a session just started or
    /// restored, a daemon just started, a workspace whose folder is unknown)
    /// only adds, and keeps the record of what was open, so a workspace
    /// that closed meanwhile is noticed on the next trusted pass.
    pub fn sync_projects(&mut self, open: &[String], remove: bool, trusted: bool) {
        let closed = |id: &String| {
            remove
                && trusted
                && self.found_projects.contains(id)
                && self.workspace_projects.contains(id)
                && !open.contains(id)
        };
        let kept: Vec<String> = self.projects.iter().filter(|id| !closed(id)).cloned().collect();
        let new: Vec<String> = open.iter().filter(|id| !kept.contains(id)).cloned().collect();
        let found = kept.iter().filter(|id| self.found_projects.contains(id)).chain(&new).cloned().collect();
        let mut record = open.to_vec();
        if !trusted {
            record.extend(self.workspace_projects.iter().filter(|id| !open.contains(id)).cloned());
        }
        self.set_projects(kept.into_iter().chain(new).collect(), Some(found));
        record.retain(|id| self.projects.contains(id));
        let mut seen = BTreeSet::new();
        record.retain(|id| seen.insert(id.clone()));
        self.workspace_projects = record;
    }
}

/// A change to a task by hand (R5.4): `itm edit`, and later the form.
#[derive(Debug, Clone, Default)]
pub struct Edit {
    pub title: Option<String>,
    /// An empty one clears it.
    pub description: Option<String>,
    /// The projects (names or ids) instead of the task's; None keeps them.
    pub projects: Option<Vec<String>>,
    pub add: Vec<String>,
    /// Removing a project the task does not have does nothing.
    pub remove: Vec<String>,
    /// Ids to put first, in this order (the form keeps the order of its projects).
    pub order: Vec<String>,
    /// The projects added are found in a workspace (R10.1): the workspace
    /// hotkey while projects follow workspaces; closing it removes them.
    pub found: bool,
    /// The project has a workspace in the session already (the workspace
    /// hotkey): none is opened for it.
    pub has_workspace: bool,
}

/// What an edit changed.
#[derive(Debug)]
pub struct Edited {
    /// The task as it was, for notes that do not exist yet (they start from it).
    pub before: Task,
    /// Title and description changes, for the task's notes (R9.3).
    pub entries: Vec<Entry>,
    /// The projects added, as saved.
    pub added: Vec<Project>,
    /// The names of the projects removed (the id of one no longer saved).
    pub removed: Vec<String>,
    /// The names of projects kept because their workspace is open.
    pub kept: Vec<String>,
}

/// Apply `edit` to the task `session`. A project named here becomes the
/// user's: closing its workspace no longer removes it (R10.1). `keep_open`
/// (projects follow workspaces and the daemon runs): a project whose
/// workspace is open in the session stays, since the daemon would add it
/// back; the rest of the edit is applied.
pub fn edit_task(data: &mut Data, session: &str, edit: &Edit, keep_open: bool, now: i64) -> Result<Edited> {
    if edit.title.as_deref().is_some_and(|t| t.trim().is_empty()) {
        bail!("task title cannot be empty");
    }
    let current = data.tasks.get(session).ok_or_else(|| Error::TaskGone(session.into()))?.projects.clone();
    let resolve_all = |queries: &[String]| -> Result<Vec<String>> {
        let mut ids: Vec<String> = vec![];
        for query in queries {
            let id = resolve_project(data, query)?;
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        Ok(ids)
    };
    let (mut wanted, claimed) = match &edit.projects {
        Some(queries) => {
            let ids = resolve_all(queries)?;
            (ids.clone(), ids)
        }
        None => {
            let claimed = resolve_all(&edit.add)?;
            // An id the task holds counts, also of a project gone from a hand-edited file.
            let mut gone = BTreeSet::new();
            for query in &edit.remove {
                gone.insert(if current.contains(query) { query.clone() } else { resolve_project(data, query)? });
            }
            let mut wanted = current.clone();
            wanted.extend(claimed.iter().filter(|id| !current.contains(id)).cloned());
            wanted.retain(|id| !gone.contains(id));
            (wanted, claimed)
        }
    };
    let Data { tasks, projects, .. } = data;
    let task = tasks.get_mut(session).expect("found above");
    let before = task.clone();
    let mut entries = vec![];
    if let Some(title) = &edit.title {
        let title = title.trim();
        if title != task.title {
            entries.push(("Title changed", Some(title.to_string())));
        }
        task.title = title.into();
    }
    if let Some(description) = &edit.description {
        let description = description.trim();
        if description != task.description.as_deref().unwrap_or("") {
            entries.push(match description {
                "" => ("Description cleared", None),
                text => ("Description changed", Some(text.to_string())),
            });
        }
        task.description = Some(description.to_string()).filter(|d| !d.is_empty());
    }
    let kept: Vec<String> = match keep_open {
        true => {
            current.iter().filter(|id| !wanted.contains(id) && task.workspace_projects.contains(id)).cloned().collect()
        }
        false => vec![],
    };
    wanted.extend(kept.iter().cloned());
    if !edit.order.is_empty() {
        let (first, rest): (Vec<String>, Vec<String>) = wanted.into_iter().partition(|id| edit.order.contains(id));
        wanted = edit.order.iter().filter(|id| first.contains(id)).cloned().chain(rest).collect();
    }
    let added: Vec<String> = wanted.iter().filter(|id| !current.contains(id)).cloned().collect();
    let removed: Vec<String> = current.iter().filter(|id| !wanted.contains(id)).cloned().collect();
    // Projects named here are the user's, unless they are found in a workspace.
    let previously = task.found_projects.iter().filter(|id| edit.found || !claimed.contains(id));
    let found = previously.chain(added.iter().filter(|_| edit.found)).cloned().collect();
    task.set_projects(wanted, Some(found));
    let label =
        |projects: &BTreeMap<String, Project>, id: &String| projects.get(id).map_or(id.clone(), |p| p.name.clone());
    let added = added
        .iter()
        .map(|id| {
            let project = projects.get_mut(id).expect("resolved");
            project.last_used_at = now;
            project.clone()
        })
        .collect();
    Ok(Edited {
        before,
        entries,
        added,
        removed: removed.iter().map(|id| label(projects, id)).collect(),
        kept: kept.iter().map(|id| label(projects, id)).collect(),
    })
}

/// Track `task-*` sessions made outside itm, and forget tombstones whose
/// session is gone. Only call this with a real session list: without one,
/// every tombstone would look expired and a session being deleted would come back.
pub fn adopt_sessions<'a>(data: &mut Data, sessions: impl IntoIterator<Item = &'a str> + Clone, now: i64) {
    let running: BTreeSet<&str> = sessions.clone().into_iter().collect();
    data.deleted.retain(|name, _| running.contains(name.as_str()));
    for name in sessions {
        if is_task_session(name) && !data.tasks.contains_key(name) && !data.deleted.contains_key(name) {
            data.tasks.insert(name.into(), Task::adopted(name, now));
        }
    }
}

/// Whether [`adopt_sessions`] would change anything (readers write only then).
pub fn needs_adoption<'a>(data: &Data, sessions: impl IntoIterator<Item = &'a str> + Clone) -> bool {
    let running: BTreeSet<&str> = sessions.into_iter().collect();
    data.deleted.keys().any(|name| !running.contains(name.as_str()))
        || running.iter().any(|n| is_task_session(n) && !data.tasks.contains_key(*n) && !data.deleted.contains_key(*n))
}

/// The session name of the task `query` names: its session, its session
/// without the prefix, or its title (ignoring case).
pub fn resolve_task(data: &Data, query: &str) -> Result<String> {
    if data.tasks.contains_key(query) {
        return Ok(query.into());
    }
    let prefixed = format!("{PREFIX}{query}");
    if data.tasks.contains_key(&prefixed) {
        return Ok(prefixed);
    }
    let lowered = query.to_lowercase();
    let mut matches: Vec<String> =
        data.tasks.iter().filter(|(_, t)| t.title.to_lowercase() == lowered).map(|(n, _)| n.clone()).collect();
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(Error::NoSuchTask(query.into())),
        _ => Err(Error::AmbiguousTask { query: query.into(), sessions: matches }),
    }
}

/// `Café crème: résumé #42!` → `cafe-creme-resume-42`, at most 40 characters.
pub fn slugify(title: &str) -> String {
    // Like Python's unicodedata: decompose, then drop what combines (accents).
    let ascii: String = title.nfkd().filter(|c| canonical_combining_class(*c) == 0).collect::<String>().to_lowercase();
    let mut slug = String::new();
    for c in ascii.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_matches('-').chars().take(40).collect();
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() { "task".into() } else { slug.into() }
}

/// `task-<slug>`, with a numeric suffix when the name is taken (ignoring case).
pub fn unique_session_name(title: &str, taken: &BTreeSet<String>) -> String {
    let base = format!("{PREFIX}{}", slugify(title));
    let taken: BTreeSet<String> = taken.iter().map(|n| n.to_lowercase()).collect();
    let mut name = base.clone();
    let mut suffix = 2;
    while taken.contains(&name.to_lowercase()) {
        name = format!("{base}-{suffix}");
        suffix += 1;
    }
    name
}

/// Where clients go when they leave `exclude`: an active task with waiting
/// agents first, then the most recently used, else `default` (R4.7).
pub fn next_active(data: &Data, status: &Status, exclude: Option<&str>) -> String {
    data.tasks
        .iter()
        .filter(|(name, task)| task.state == State::Active && Some(name.as_str()) != exclude)
        .max_by_key(|(name, task)| (status.waiting(name), task.last_used_at, name.as_str()))
        .map_or_else(|| DEFAULT_SESSION.into(), |(name, _)| name.clone())
}

/// `itm jump`: the task whose agent has waited longest, archived ones aside.
pub fn longest_waiting(data: &Data, status: &Status, now: i64) -> Option<String> {
    (data.tasks.iter().filter(|(_, task)| task.state != State::Archived))
        .filter_map(|(name, _)| Some((status.waiting_since(name, now)?, name)))
        .min()
        .map(|(_, name)| name.clone())
}

/// `itm last`: the most recently used active task other than `current`.
pub fn previous_task(data: &Data, current: Option<&str>) -> Option<String> {
    (data.tasks.iter().filter(|(name, task)| task.state == State::Active && Some(name.as_str()) != current))
        .max_by_key(|(name, task)| (task.last_used_at, name.as_str()))
        .map(|(name, _)| name.clone())
}

/// Tasks per state, and tasks with an agent waiting (archived ones aside).
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct Counts {
    pub active: usize,
    pub snoozed: usize,
    pub archived: usize,
    pub waiting: usize,
}

pub fn counts(data: &Data, status: &Status) -> Counts {
    let tasks = || data.tasks.iter();
    let count = |state: State| tasks().filter(|(_, t)| t.state == state).count();
    Counts {
        active: count(State::Active),
        snoozed: count(State::Snoozed),
        archived: count(State::Archived),
        waiting: tasks().filter(|(name, t)| t.state != State::Archived && status.waiting(name) > 0).count(),
    }
}

/// Inbox order: tasks with waiting agents first, then active, snoozed,
/// archived, each most recently used first.
pub fn inbox_order(status: &Status, name: &str, task: &Task) -> (bool, u8, i64) {
    let state = match task.state {
        State::Active => 0,
        State::Snoozed => 1,
        State::Archived => 2,
    };
    (status.waiting(name) == 0, state, -task.last_used_at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{AgentState, AgentStatus, SessionStatus};

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    /// R10.1, R10.3: projects found in workspaces come and go with them; one added by hand stays.
    #[test]
    fn r10_3_found_projects_come_and_go_with_their_workspaces() {
        let mut task = Task::new("T", 1);
        task.projects = ids(&["api"]); // by hand
        task.sync_projects(&ids(&["api", "web"]), true, true);
        assert_eq!((&task.projects, &task.found_projects), (&ids(&["api", "web"]), &ids(&["web"])));
        assert_eq!(task.workspace_projects, ids(&["api", "web"]));
        task.sync_projects(&ids(&["api"]), true, true); // web's workspace closed
        assert_eq!((&task.projects, &task.found_projects), (&ids(&["api"]), &vec![]));
        task.sync_projects(&[], true, true); // api was added by hand: it stays
        assert_eq!((&task.projects, &task.workspace_projects), (&ids(&["api"]), &vec![]));
    }

    /// R10.3: with "remove a project when its workspace closes" off, projects are only added.
    #[test]
    fn r10_3_without_removal_projects_are_only_added() {
        let mut task = Task::new("T", 1);
        task.sync_projects(&ids(&["web"]), false, true);
        task.sync_projects(&[], false, true);
        assert_eq!(task.projects, ids(&["web"]));
    }

    /// R10.3: a project never seen open (a daemon restarted with the session
    /// rebuilt) is not removed, and an untrusted pass only adds, keeping the record.
    #[test]
    fn r10_3_untrusted_and_unseen_never_remove() {
        let mut task = Task::new("T", 1);
        task.sync_projects(&ids(&["web"]), true, true);
        task.workspace_projects.clear();
        task.sync_projects(&[], true, true);
        assert_eq!(task.projects, ids(&["web"]), "never seen closing: kept");

        let mut task = Task::new("T", 1);
        task.sync_projects(&ids(&["api", "web"]), true, true);
        task.sync_projects(&ids(&["api"]), true, false);
        assert_eq!(task.projects, ids(&["api", "web"]), "an untrusted pass only adds");
        assert_eq!(task.workspace_projects, ids(&["api", "web"]), "and keeps what was open");
        task.sync_projects(&ids(&["api"]), true, true);
        assert_eq!(task.projects, ids(&["api"]), "the next trusted pass notices");
    }

    fn data() -> Data {
        let mut data = Data::default();
        let mut add = |name: &str, f: &dyn Fn(&mut Task)| {
            let mut task = Task::new(name, 1);
            f(&mut task);
            data.tasks.insert(format!("task-{}", name.to_lowercase()), task);
        };
        add("Old", &|t| t.last_used_at = 10);
        add("New", &|t| t.last_used_at = 20);
        add("Zz", &|t| (t.state, t.snoozed_until) = (State::Snoozed, Some(99)));
        add("Done", &|t| (t.state, t.archived_at) = (State::Archived, Some(5)));
        data
    }

    pub fn waiting(sessions: &[(&str, i64)]) -> Status {
        let blocked = |since| AgentStatus {
            agent: "claude".into(),
            status: AgentState::Blocked,
            title: String::new(),
            pane_id: None,
            since: Some(since),
        };
        let sessions = sessions
            .iter()
            .map(|&(name, since)| (name.to_string(), SessionStatus { blocked: 1, agents: vec![blocked(since)] }))
            .collect();
        Status { sessions, fresh: true }
    }

    #[test]
    fn slugs() {
        assert_eq!(slugify("Café crème: résumé #42!"), "cafe-creme-resume-42");
        assert_eq!(slugify("!!!"), "task");
        let long = slugify(&format!("{} b", "a".repeat(39)));
        assert!(long.len() <= 40 && !long.ends_with('-'), "{long}");
    }

    #[test]
    fn session_names_get_a_suffix_when_taken_ignoring_case() {
        let taken = ["task-fix-bug".to_string(), "Task-Fix-Bug-2".to_string()].into();
        assert_eq!(unique_session_name("Fix bug", &taken), "task-fix-bug-3");
    }

    #[test]
    fn next_active_prefers_waiting_then_recent_else_default() {
        let data = data();
        assert_eq!(next_active(&data, &Status::default(), None), "task-new");
        assert_eq!(next_active(&data, &waiting(&[("task-old", 5)]), None), "task-old");
        assert_eq!(next_active(&data, &Status::default(), Some("task-new")), "task-old");
        assert_eq!(next_active(&Data::default(), &Status::default(), None), DEFAULT_SESSION);
    }

    #[test]
    fn resolve_by_session_name_or_title() {
        let data = data();
        assert_eq!(resolve_task(&data, "task-old").unwrap(), "task-old");
        assert_eq!(resolve_task(&data, "old").unwrap(), "task-old");
        assert_eq!(resolve_task(&data, "NEW").unwrap(), "task-new");
        assert_eq!(resolve_task(&data, "missing").unwrap_err(), Error::NoSuchTask("missing".into()));
        assert_eq!(resolve_task(&data, "missing").unwrap_err().to_string(), "no task named 'missing'");
    }

    #[test]
    fn jump_goes_to_the_longest_wait_and_last_to_the_previous_task() {
        let data = data();
        assert_eq!(longest_waiting(&data, &Status::default(), 100), None);
        let status = waiting(&[("task-new", 50), ("task-old", 40), ("task-done", 1)]);
        assert_eq!(longest_waiting(&data, &status, 100).as_deref(), Some("task-old"));
        assert_eq!(previous_task(&data, Some("task-new")).as_deref(), Some("task-old"));
        assert_eq!(previous_task(&data, None).as_deref(), Some("task-new"));
        assert_eq!(previous_task(&Data::default(), None), None);
    }

    #[test]
    fn counts_leave_archived_waits_out() {
        let status = waiting(&[("task-zz", 1), ("task-done", 1)]);
        assert_eq!(counts(&data(), &status), Counts { active: 2, snoozed: 1, archived: 1, waiting: 1 });
    }

    #[test]
    fn snoozing_until_an_agent_has_no_timer_and_times_must_be_ahead() {
        let mut task = Task::new("A", 1);
        task.snoozed_until = Some(5);
        task.snooze(Until::Agent, 100).unwrap();
        assert_eq!((task.state, task.snoozed_until), (State::Snoozed, None));

        let mut task = Task::new("A", 1);
        assert!(task.snooze(Until::Time(95), 100).is_err()); // a preset that passed while a dialog was open
        assert!(task.snooze(Until::Time(100 + MAX_SNOOZE + 1), 100).is_err());
        assert_eq!(task.state, State::Active);
    }

    #[test]
    fn a_tombstone_blocks_adoption_until_its_session_is_gone() {
        let mut data = Data::default();
        data.deleted.insert("task-gone".into(), 1);
        adopt_sessions(&mut data, ["task-gone"], 5);
        assert!(data.tasks.is_empty());
        assert!(!needs_adoption(&data, ["task-gone"]));
        adopt_sessions(&mut data, [], 5);
        assert!(data.deleted.is_empty());
    }

    #[test]
    fn only_prefixed_sessions_are_adopted() {
        let mut data = Data::default();
        assert!(needs_adoption(&data, ["default", "task-from-cli"]));
        adopt_sessions(&mut data, ["default", "task-from-cli"], 5);
        assert_eq!(data.tasks.keys().collect::<Vec<_>>(), ["task-from-cli"]);
        assert_eq!(data.tasks["task-from-cli"].title, "from cli");
    }
}
