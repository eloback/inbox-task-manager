//! The switcher's state and keys. No terminal, no threads and no clock
//! here: keys come in, [`Action`]s go out, and the event loop
//! (`tui/mod.rs`) does the I/O and says what time it is. Only the screen on
//! top of the stack gets keys, so list keys can never act on the task
//! behind a dialog.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use clap::ValueEnum;
use jiff::tz::TimeZone;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use super::screen::{Context, Outcome, Screen, SnoozeDialog};
use crate::error::Error;
use crate::herdr::Sessions;
use crate::itm::Itm;
use crate::paths::Paths;
use crate::snooze::{self, Until};
use crate::status::Status;
use crate::store::{Data, State, Task};
use crate::tasks::inbox_order;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum View {
    Inbox,
    Snoozed,
    Archived,
    All,
}

impl View {
    pub const ALL: [View; 4] = [View::Inbox, View::Snoozed, View::Archived, View::All];

    pub fn label(self) -> &'static str {
        match self {
            View::Inbox => "Inbox",
            View::Snoozed => "Snoozed",
            View::Archived => "Archived",
            View::All => "All",
        }
    }

    pub fn shows(self, task: &Task) -> bool {
        match self {
            View::Inbox => task.state == State::Active,
            View::Snoozed => task.state == State::Snoozed,
            View::Archived => task.state == State::Archived,
            View::All => true,
        }
    }
}

/// Everything the switcher shows, loaded in one go by a worker.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub data: Data,
    pub sessions: Sessions,
    pub status: Status,
    /// The session this switcher runs in (inside Herdr).
    pub current: Option<String>,
    /// Why this client cannot be switched in place, if it cannot.
    pub switch_problem: Option<String>,
    /// False when Herdr's sessions could not be listed: they are unknown, not absent.
    pub listed: bool,
    /// Ids of the projects whose folder is set and not there. A project
    /// without a folder is never missing (DESIGN.md section 2, Projects).
    pub missing: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Header(&'static str),
    Task(String),
    /// "Create task" when nothing matches the search.
    Create,
}

/// When the switcher acts: an instant for its timers, and the epoch second
/// for what it shows ("in 2h", snooze presets).
#[derive(Debug, Clone, Copy)]
pub struct Moment {
    pub instant: Instant,
    pub epoch: i64,
}

impl Moment {
    pub fn now() -> Moment {
        Moment { instant: Instant::now(), epoch: crate::times::now() }
    }
}

/// Blocking work for a worker thread (Herdr calls, the store lock, hand-offs).
#[derive(Debug, Clone, PartialEq)]
pub enum Job {
    Wake { session: String, title: String },
    Archive { session: String, title: String },
    Create { title: String },
    Snooze { session: String, title: String, until: Until },
}

#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    Quit,
    /// Leave, then switch to this session (after the terminal is restored).
    Switch(String),
    Run(Job),
}

pub struct Message {
    pub text: String,
    pub error: bool,
    pub until: Instant,
}

pub struct App {
    pub view: View,
    pub search: Input,
    /// None until the first load arrives.
    pub snapshot: Option<Snapshot>,
    pub rows: Vec<Row>,
    pub list: ListState,
    /// Screens over the list, the top one last.
    pub screens: Vec<Screen>,
    pub message: Option<Message>,
    pub paths: Paths,
    pub tz: TimeZone,
    /// The time the event loop last said it is.
    pub now: Moment,
}

impl App {
    pub fn new(view: View, paths: Paths, tz: TimeZone, now: Moment) -> App {
        App {
            view,
            search: Input::default(),
            snapshot: None,
            rows: vec![],
            list: ListState::default(),
            screens: vec![],
            message: None,
            paths,
            tz,
            now,
        }
    }

    pub fn task(&self, session: &str) -> Option<&Task> {
        self.snapshot.as_ref()?.data.tasks.get(session)
    }

    pub fn highlighted(&self) -> Option<&Row> {
        self.list.selected().and_then(|i| self.rows.get(i))
    }

    fn highlighted_task(&self) -> Option<(String, &Task)> {
        match self.highlighted()? {
            Row::Task(session) => Some((session.clone(), self.task(session)?)),
            _ => None,
        }
    }

    pub fn count(&self, view: View) -> usize {
        self.snapshot.as_ref().map_or(0, |s| s.data.tasks.values().filter(|t| view.shows(t)).count())
    }

    pub fn flash(&mut self, text: impl Into<String>, error: bool) {
        let shown_for = Duration::from_secs(if error { 8 } else { 4 });
        self.message = Some(Message { text: text.into(), error, until: self.now.instant + shown_for });
    }

    pub fn expire_message(&mut self) {
        self.message.take_if(|m| self.now.instant >= m.until);
    }

    /// A refresh arrived: show it, keeping the highlighted task (N3).
    pub fn loaded(&mut self, snapshot: Result<Snapshot, Error>) {
        match snapshot {
            Ok(snapshot) => {
                self.snapshot = Some(snapshot);
                self.rebuild();
            }
            Err(e) => self.flash(e.to_string(), true),
        }
    }

    /// The rows for the view and search, keeping the highlighted task if it is still there.
    fn rebuild(&mut self) {
        let keep = self.highlighted().cloned();
        self.rows = self.visible_rows();
        let selectable = |row: &Row| !matches!(row, Row::Header(_));
        let at = keep.and_then(|k| self.rows.iter().position(|r| *r == k && selectable(r)));
        self.list.select(at.or_else(|| self.rows.iter().position(selectable)));
    }

    fn visible_rows(&self) -> Vec<Row> {
        let Some(snap) = &self.snapshot else { return vec![] };
        let query = self.search.value().trim().to_lowercase();
        let mut tasks: Vec<(&String, &Task)> = (snap.data.tasks.iter())
            .filter(|(_, task)| self.view.shows(task) && task.title.to_lowercase().contains(&query))
            .collect();
        tasks.sort_by_key(|(name, task)| inbox_order(&snap.status, name, task));
        let mut rows = vec![];
        if self.view == View::Inbox {
            // Inbox first: the tasks waiting for you, with what they wait for (R2.3).
            let (needs, rest): (Vec<_>, Vec<_>) =
                tasks.into_iter().partition(|(name, _)| snap.status.waiting(name) > 0);
            for (header, group) in [("NEEDS YOU", needs), ("ACTIVE", rest)] {
                if !group.is_empty() {
                    rows.push(Row::Header(header));
                    rows.extend(group.into_iter().map(|(name, _)| Row::Task(name.clone())));
                }
            }
        } else {
            rows.extend(tasks.into_iter().map(|(name, _)| Row::Task(name.clone())));
        }
        if rows.is_empty() && !query.is_empty() {
            rows.push(Row::Create);
        }
        rows
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        if let Some(top) = self.screens.last_mut() {
            let cx = Context { tz: &self.tz, now: self.now.epoch };
            match top.on_key(key, &cx) {
                Outcome::Stay => {}
                Outcome::Close => drop(self.screens.pop()),
                Outcome::CloseAnd(action) => {
                    self.screens.pop();
                    return action;
                }
            }
            return Action::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => return Action::Quit,
            KeyCode::Char('c') if ctrl => return Action::Quit,
            KeyCode::Enter => return self.open(),
            KeyCode::Tab => self.change_view(1),
            KeyCode::BackTab => self.change_view(-1),
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') if ctrl => self.move_by(-1),
            KeyCode::Char('j') if ctrl => self.move_by(1),
            KeyCode::Char(digit @ '1'..='9') if alt => return self.open_row(digit as usize - '0' as usize),
            KeyCode::Char('z') if ctrl => self.snooze(),
            KeyCode::Char('u') if ctrl => return self.wake(),
            KeyCode::Char('a') if ctrl => return self.archive(),
            _ if alt => {}
            _ => {
                if self.search.handle_event(&Event::Key(key)).is_some_and(|change| change.value) {
                    self.rebuild();
                }
            }
        }
        Action::None
    }

    /// Tab / Shift+Tab: the next view; the search text stays (R6.1).
    fn change_view(&mut self, step: isize) {
        let at = View::ALL.iter().position(|v| *v == self.view).unwrap_or(0) as isize;
        self.view = View::ALL[(at + step).rem_euclid(View::ALL.len() as isize) as usize];
        self.rebuild();
    }

    /// Up and down skip the section headers.
    fn move_by(&mut self, step: isize) {
        let selectable: Vec<usize> =
            (0..self.rows.len()).filter(|&i| !matches!(self.rows[i], Row::Header(_))).collect();
        let Some(at) = self.list.selected().and_then(|s| selectable.iter().position(|&i| i == s)) else { return };
        let next = (at as isize + step).clamp(0, selectable.len() as isize - 1) as usize;
        self.list.select(Some(selectable[next]));
    }

    /// Enter: open the task (the attach wakes or reopens it), or create one.
    fn open(&mut self) -> Action {
        match self.highlighted() {
            Some(Row::Create) => Action::Run(Job::Create { title: self.search.value().trim().to_string() }),
            Some(Row::Task(session)) => self.switch(session.clone()),
            _ => Action::None,
        }
    }

    /// Alt+1…9: open row n.
    fn open_row(&mut self, n: usize) -> Action {
        let tasks: Vec<usize> = (0..self.rows.len()).filter(|&i| matches!(self.rows[i], Row::Task(_))).collect();
        let Some(&at) = tasks.get(n - 1) else { return Action::None };
        self.list.select(Some(at));
        self.open()
    }

    fn switch(&mut self, session: String) -> Action {
        match self.snapshot.as_ref().and_then(|s| s.switch_problem.clone()) {
            Some(problem) => {
                self.flash(problem, true);
                Action::None
            }
            None => Action::Switch(session),
        }
    }

    /// Ctrl+Z: "Until an agent needs me" is offered only when an agent could ever wake the task.
    fn snooze(&mut self) {
        let (Some(snap), Some((session, task))) = (&self.snapshot, self.highlighted_task()) else { return };
        let agent = Itm::agent_snooze_problem(&session, &snap.sessions, &snap.status).is_none();
        let presets = snooze::presets(self.now.epoch, &self.tz, agent);
        let title = task.title.clone();
        self.screens.push(Screen::Snooze(SnoozeDialog::new(session, title, presets)));
    }

    fn wake(&mut self) -> Action {
        match self.highlighted_task() {
            Some((session, task)) if task.state == State::Snoozed => {
                Action::Run(Job::Wake { session, title: task.title.clone() })
            }
            _ => Action::None,
        }
    }

    fn archive(&mut self) -> Action {
        let Some((session, task)) = self.highlighted_task().filter(|(_, t)| t.state != State::Archived) else {
            return Action::None;
        };
        let title = task.title.clone();
        let running = self.snapshot.as_ref().and_then(|s| s.sessions.get(&session)).is_some_and(|s| s.running);
        if running {
            self.screens.push(Screen::ConfirmArchive { session, title });
            return Action::None;
        }
        Action::Run(Job::Archive { session, title })
    }

    /// A job finished in its worker.
    pub fn done(&mut self, job: Job, result: Result<String, Error>) -> Action {
        let current = self.snapshot.as_ref().and_then(|s| s.current.clone());
        let problem = self.snapshot.as_ref().and_then(|s| s.switch_problem.clone());
        match (job, result) {
            (_, Err(e)) => self.flash(e.to_string(), true),
            (Job::Wake { title, .. }, Ok(_)) => self.flash(format!("“{title}” is active."), false),
            (Job::Archive { session, title }, Ok(_)) => {
                // Its clients moved to another task: this one included, so leave.
                if current.as_deref() == Some(&session) {
                    return Action::Quit;
                }
                self.flash(format!("Archived “{title}”."), false);
            }
            (Job::Snooze { session, title, until }, Ok(_)) => {
                if current.as_deref() == Some(&session) {
                    return Action::Quit; // its clients moved to the next task, this one included
                }
                self.flash(format!("Snoozed “{title}” {}.", until.describe(self.now.epoch, &self.tz)), false);
            }
            (Job::Create { title }, Ok(session)) => match problem {
                None => return Action::Switch(session),
                Some(problem) => {
                    self.search.reset();
                    self.rebuild();
                    self.flash(format!("Saved “{title}” without switching: {problem}"), true);
                }
            },
        }
        Action::None
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::herdr::Session;
    use crate::status::{AgentState, AgentStatus, SessionStatus};

    pub const NOW: i64 = 1_800_000_000;

    pub fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    pub fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    /// Fix login (an agent waiting 4 min, running), Write docs (running,
    /// working), Old idea (never started), Later (snoozed 2 h), Shipped (archived).
    pub fn snapshot() -> Snapshot {
        let mut snap = Snapshot { listed: true, ..Snapshot::default() };
        let mut add = |session: &str, title: &str, used: i64, f: &dyn Fn(&mut Task)| {
            let mut task = Task::new(title, NOW - 86400);
            task.last_used_at = NOW - used;
            f(&mut task);
            snap.data.tasks.insert(session.into(), task);
        };
        add("task-fix-login", "Fix login", 3600, &|t| t.description = Some("Users get logged out\nafter 5 min".into()));
        add("task-write-docs", "Write docs", 60, &|_| {});
        add("task-old-idea", "Old idea", 7200, &|_| {});
        add("task-later", "Later", 600, &|t| t.snooze(Until::Time(NOW + 7200), NOW).unwrap());
        add("task-shipped", "Shipped", 100, &|t| t.archive(NOW - 2 * 86400));
        for name in ["task-fix-login", "task-write-docs", "task-later"] {
            let session = Session { name: name.into(), running: true, socket: "/s".into() };
            snap.sessions.insert(name.into(), session);
        }
        let agent = |name: &str, status, since| AgentStatus {
            agent: name.into(),
            status,
            title: "Refactor auth".into(),
            pane_id: Some("w1:p1".into()),
            since,
        };
        let waiting = SessionStatus { blocked: 1, agents: vec![agent("claude", AgentState::Blocked, Some(NOW - 240))] };
        let working = SessionStatus { blocked: 0, agents: vec![agent("codex", AgentState::Working, None)] };
        snap.status.sessions = [("task-fix-login".into(), waiting), ("task-write-docs".into(), working)].into();
        snap.status.fresh = true;
        snap
    }

    pub fn app(snap: Snapshot) -> App {
        let now = Moment { instant: Instant::now(), epoch: NOW };
        let mut app = App::new(View::Inbox, Paths::under(std::path::Path::new("/r")), TimeZone::UTC, now);
        app.loaded(Ok(snap));
        app
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn the_inbox_lists_tasks_needing_you_first_under_headers() {
        let app = app(snapshot());
        assert_eq!(
            app.rows,
            [
                Row::Header("NEEDS YOU"),
                Row::Task("task-fix-login".into()),
                Row::Header("ACTIVE"),
                Row::Task("task-write-docs".into()),
                Row::Task("task-old-idea".into()),
            ]
        );
        assert_eq!(app.highlighted(), Some(&Row::Task("task-fix-login".into())));
        let counts: Vec<usize> = View::ALL.iter().map(|v| app.count(*v)).collect();
        assert_eq!(counts, [3, 1, 1, 5]);
    }

    #[test]
    fn moving_skips_headers_and_a_refresh_keeps_the_highlighted_task() {
        let mut app = app(snapshot());
        app.on_key(key(KeyCode::Down));
        app.on_key(ctrl('j'));
        assert_eq!(app.highlighted(), Some(&Row::Task("task-old-idea".into())));
        app.on_key(key(KeyCode::Down)); // at the end: stays
        app.on_key(ctrl('k'));
        assert_eq!(app.highlighted(), Some(&Row::Task("task-write-docs".into())));

        let mut snap = snapshot();
        snap.status.sessions.clear(); // the wait ended: rows reorder
        app.loaded(Ok(snap));
        assert_eq!(app.highlighted(), Some(&Row::Task("task-write-docs".into())));
    }

    #[test]
    fn views_change_with_tab_and_keep_the_search() {
        let mut app = app(snapshot());
        typed(&mut app, "La");
        assert_eq!(app.rows, [Row::Create]); // "Later" is snoozed
        app.on_key(key(KeyCode::Tab));
        assert_eq!((app.view, app.search.value()), (View::Snoozed, "La"));
        assert_eq!(app.rows, [Row::Task("task-later".into())]);
        app.on_key(key(KeyCode::BackTab));
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(app.view, View::All);
    }

    #[test]
    fn enter_opens_and_says_nothing_else_happens_here() {
        let mut app = app(snapshot());
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Switch("task-fix-login".into()));
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Switch("task-later".into())); // the attach wakes it
        assert_eq!(app.on_key(alt('1')), Action::Switch("task-later".into()));
        assert_eq!(app.on_key(alt('2')), Action::None);
    }

    #[test]
    fn a_client_that_cannot_switch_says_why_instead() {
        let mut snap = snapshot();
        snap.switch_problem = Some("this Herdr client was not started by `itm attach`".into());
        let mut app = app(snap);
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
        assert!(app.message.as_ref().is_some_and(|m| m.error && m.text.contains("itm attach")));
    }

    #[test]
    fn no_match_offers_to_create_the_task() {
        let mut app = app(snapshot());
        typed(&mut app, "Brand new");
        assert_eq!(app.rows, [Row::Create]);
        let create = Job::Create { title: "Brand new".into() };
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Run(create.clone()));
        assert_eq!(app.done(create, Ok("task-brand-new".into())), Action::Switch("task-brand-new".into()));
    }

    #[test]
    fn archiving_a_running_task_asks_first_and_the_dialog_takes_every_key() {
        let mut app = app(snapshot());
        assert_eq!(app.on_key(ctrl('a')), Action::None);
        assert!(matches!(app.screens[..], [Screen::ConfirmArchive { .. }]));
        // List keys do nothing behind the dialog.
        for k in [key(KeyCode::Enter), key(KeyCode::Down), ctrl('a'), key(KeyCode::Tab)] {
            assert_eq!(app.on_key(k), Action::None);
        }
        assert_eq!(app.view, View::Inbox);
        app.on_key(key(KeyCode::Esc));
        assert!(app.screens.is_empty());
        app.on_key(ctrl('a'));
        let archive = Job::Archive { session: "task-fix-login".into(), title: "Fix login".into() };
        assert_eq!(app.on_key(key(KeyCode::Char('y'))), Action::Run(archive));

        // Not running: nothing to lose, no question.
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        let archive = Job::Archive { session: "task-old-idea".into(), title: "Old idea".into() };
        assert_eq!(app.on_key(ctrl('a')), Action::Run(archive));
    }

    #[test]
    fn only_the_top_screen_gets_keys_and_esc_closes_only_it() {
        let mut app = app(snapshot());
        app.on_key(ctrl('a')); // the archive question, over the list
        let presets = vec![("1 hour", Until::Time(NOW + 3600))];
        app.screens.push(Screen::Snooze(SnoozeDialog::new("task-x".into(), "X".into(), presets)));
        assert_eq!(app.on_key(key(KeyCode::Char('y'))), Action::None, "y is typed into the snooze dialog");
        app.on_key(key(KeyCode::Esc));
        assert!(matches!(app.screens[..], [Screen::ConfirmArchive { .. }]));
        let archive = Job::Archive { session: "task-fix-login".into(), title: "Fix login".into() };
        assert_eq!(app.on_key(key(KeyCode::Char('y'))), Action::Run(archive));
        assert!(app.screens.is_empty());
    }

    #[test]
    fn the_snooze_dialog_takes_a_preset_or_a_typed_time() {
        let mut app = app(snapshot());
        let agent_preset = |app: &App| match &app.screens[..] {
            [Screen::Snooze(dialog)] => dialog.presets.iter().any(|p| p.1 == Until::Agent),
            _ => panic!("no snooze dialog"),
        };
        app.on_key(ctrl('z'));
        assert!(!agent_preset(&app), "an agent waits already: it could not wake the task");
        app.on_key(key(KeyCode::Esc));
        app.on_key(key(KeyCode::Down));
        app.on_key(ctrl('z'));
        assert!(agent_preset(&app), "a running session with no wait");
        app.on_key(key(KeyCode::Down));
        let Action::Run(Job::Snooze { session, until: Until::Time(_), .. }) = app.on_key(key(KeyCode::Enter)) else {
            panic!("no snooze")
        };
        assert_eq!(session, "task-write-docs");
        assert!(app.screens.is_empty());

        app.on_key(ctrl('z'));
        typed(&mut app, "soonish");
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
        let [Screen::Snooze(dialog)] = &app.screens[..] else { panic!("the dialog stays open") };
        assert_eq!(dialog.error.as_deref(), Some("cannot understand snooze time: soonish"));
        for _ in 0..7 {
            app.on_key(key(KeyCode::Backspace));
        }
        typed(&mut app, "agent");
        let action = app.on_key(key(KeyCode::Enter));
        assert!(matches!(action, Action::Run(Job::Snooze { until: Until::Agent, .. })), "{action:?}");
        assert_eq!(app.search.value(), "", "typing went to the dialog, not the search");
    }

    #[test]
    fn archiving_the_task_this_switcher_runs_in_closes_it() {
        let mut snap = snapshot();
        snap.current = Some("task-fix-login".into());
        let mut app = app(snap);
        let job = Job::Archive { session: "task-fix-login".into(), title: "Fix login".into() };
        assert_eq!(app.done(job, Ok(String::new())), Action::Quit);
    }

    #[test]
    fn ctrl_u_wakes_only_snoozed_tasks_and_errors_stay_visible() {
        let mut app = app(snapshot());
        assert_eq!(app.on_key(ctrl('u')), Action::None);
        app.on_key(key(KeyCode::Tab));
        let wake = Job::Wake { session: "task-later".into(), title: "Later".into() };
        assert_eq!(app.on_key(ctrl('u')), Action::Run(wake.clone()));
        app.done(wake, Err(Error::Other("task-later is not snoozed".into())));
        assert!(app.message.as_ref().is_some_and(|m| m.error && m.text == "task-later is not snoozed"));
    }
}
