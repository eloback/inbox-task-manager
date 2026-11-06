//! Tasks and their states (REQUIREMENTS.md, "Task states"): what can happen
//! to a task, independent of where it is stored or how it is shown.

use std::collections::{BTreeMap, BTreeSet};

use unicode_normalization::{UnicodeNormalization, char::canonical_combining_class};

use crate::error::{Error, Result, bail};
use crate::snooze::{MAX_SNOOZE, Until};
use crate::status::Status;
use crate::store::{Data, State, Task};

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
        self.extra.remove("workspace_projects"); // its workspaces close with the session
    }
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
