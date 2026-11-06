//! The switcher's state and keys. No terminal, no threads and no clock
//! here: keys come in, [`Action`]s go out, and the event loop
//! (`tui/mod.rs`) does the I/O and says what time it is. Only the screen on
//! top of the stack gets keys, so list keys can never act on the task
//! behind a dialog. The same state runs a dialog alone (the CLI's): then
//! there is no list, and the loop ends when the last screen closes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use clap::ValueEnum;
use jiff::tz::TimeZone;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;
use regex_lite::Regex;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use super::finder::Saved;
use super::form::{Editing, TaskForm};
use super::help::Help;
use super::projects::ProjectsScreen;
use super::screen::{Context, Need, Outcome, Screen, SettingsScreen, SnoozeDialog};
use crate::config::{Setting, Settings};
use crate::error::Error;
use crate::folders::Listing;
use crate::herdr::Sessions;
use crate::itm::Itm;
use crate::paths::Paths;
use crate::projects::{Check, CloneChoice, RemovalNote, RemovalPlan, resolve_project};
use crate::snooze::{self, Until};
use crate::status::Status;
use crate::store::{Data, Project, ProjectKind, State, Task};
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
#[derive(Debug, Clone)]
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
    /// Each saved folder, its links resolved, and its project: the finder
    /// knows a saved folder reached through a link (R1.6).
    pub folders: BTreeMap<PathBuf, String>,
    /// The end of each task's notes (R9.5), by session.
    pub notes: BTreeMap<String, NotesTail>,
    /// The settings (R10.4), or why `config.toml` could not be read.
    pub settings: Result<Settings, String>,
}

impl Default for Snapshot {
    fn default() -> Snapshot {
        Snapshot {
            data: Data::default(),
            sessions: Sessions::default(),
            status: Status::default(),
            current: None,
            switch_problem: None,
            listed: false,
            missing: BTreeSet::new(),
            folders: BTreeMap::new(),
            notes: BTreeMap::new(),
            settings: Ok(Settings::default()),
        }
    }
}

impl Snapshot {
    /// The saved projects as the form and its finder show them.
    pub fn saved(&self) -> Saved {
        Saved { projects: self.data.projects.clone(), missing: self.missing.clone(), folders: self.folders.clone() }
    }
}

/// When a task's notes last changed (epoch second) and their last lines.
#[derive(Debug, Clone, PartialEq)]
pub struct NotesTail {
    pub modified: i64,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Header(&'static str),
    Task(String),
    /// An `@name` filter that names no project (R6.2).
    UnknownProject(String),
    /// "Create task" when nothing matches the search.
    Create,
}

impl Row {
    fn selectable(&self) -> bool {
        matches!(self, Row::Task(_) | Row::Create)
    }
}

/// A search as typed (R6.2): `@name` or `@"a name"` shows one project's
/// tasks; the rest searches titles.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub projects: Vec<String>,
    pub text: String,
}

impl Query {
    pub fn parse(typed: &str) -> Query {
        static FILTER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"@"([^"]*)"?|@(\S+)"#).unwrap());
        let projects = (FILTER.captures_iter(typed))
            .filter_map(|c| c.get(1).or(c.get(2)).map(|m| m.as_str().to_string()))
            .filter(|token| !token.trim().is_empty())
            .collect();
        let text = FILTER.replace_all(typed, " ").split_whitespace().collect::<Vec<_>>().join(" ");
        Query { projects, text }
    }
}

/// The search token that shows a project's tasks (the Projects screen's Enter).
pub fn filter_token(name: &str) -> String {
    if name.contains(' ') { format!("@\"{name}\"") } else { format!("@{name}") }
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

/// Blocking work for a worker thread (Herdr calls, the store lock, git,
/// hand-offs).
#[derive(Debug, Clone, PartialEq)]
pub enum Job {
    Wake {
        session: String,
        title: String,
    },
    Archive {
        session: String,
        title: String,
    },
    Delete {
        session: String,
        title: String,
    },
    /// A new task (R1.1); done, it is [`Reply::Created`].
    Create {
        title: String,
        description: Option<String>,
        /// Project ids, the first where the session starts.
        projects: Vec<String>,
        until: Option<Until>,
    },
    /// The edit form saved (R5.4); done, it says what happened besides.
    Edit {
        session: String,
        title: String,
        /// Empty clears it.
        description: String,
        add: Vec<String>,
        remove: Vec<String>,
        /// The projects in the form's order, when it changed them.
        order: Vec<String>,
    },
    Snooze {
        session: String,
        title: String,
        until: Until,
    },
    /// What removing a project would do (it reads its clone with git), for the remove dialog.
    RemovalPlan {
        id: String,
    },
    RemoveProject {
        id: String,
        name: String,
        choice: CloneChoice,
    },
    RenameProject {
        id: String,
        name: String,
    },
    /// Save a folder or a name; done, it is [`Reply::Added`].
    AddProject {
        kind: ProjectKind,
        source: String,
        name: Option<String>,
    },
    /// What adding a project would do, as typed so far (R8.5): nothing is saved.
    CheckProject {
        kind: ProjectKind,
        source: String,
        name: Option<String>,
    },
    /// Clone a repository in a child `itm project add git` (R8.8); what git
    /// says on the way comes as [`super::Input::Cloning`].
    Clone {
        url: String,
        name: Option<String>,
    },
    /// Change a setting in `config.toml` (R10.4).
    Setting {
        setting: Setting,
        on: bool,
    },
}

impl Job {
    /// Whether the switcher's list may show something else once it is done (a refresh follows).
    pub fn changes_tasks(&self) -> bool {
        !matches!(self, Job::CheckProject { .. } | Job::RemovalPlan { .. })
    }
}

/// What a job that worked gives back.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Done,
    /// A task was saved: its session.
    Created(String),
    /// What happened besides, a line each (an edit's notes).
    Said(Vec<String>),
    /// A project was saved: its id, and the project as saved.
    Added(String, Project),
    Checked(Check),
    Plan(RemovalPlan),
    Removed(Vec<RemovalNote>),
    /// (old name, new name, a note).
    Renamed(String, String, Option<String>),
}

#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    Quit,
    /// Leave, then switch to this session (after the terminal is restored).
    Switch(String),
    Run(Job),
    /// Open this task's notes in the editor, which gets the terminal meanwhile (R9.4).
    Notes(String),
    /// A question was answered yes or no.
    Answer(bool),
    /// A project was saved: its id, and the project. What opened the add dialog takes it.
    ProjectAdded(String, Project),
    /// The finder's choice: the task's projects now (ids), and a project saved on the way.
    Picked(Vec<String>, Option<(String, Project)>),
    /// Show this project's tasks (`@name` in the search, R6.2).
    Filter(String),
    /// Esc during a clone: stop it (SIGTERM), which keeps nothing of it.
    StopClone,
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
    /// A dialog alone (the CLI's): no list behind it; done once its last screen closes.
    pub alone: bool,
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
            alone: false,
        }
    }

    /// `screen` with nothing behind it.
    pub fn alone(screen: Screen, paths: Paths, tz: TimeZone, now: Moment) -> App {
        App { screens: vec![screen], alone: true, ..App::new(View::Inbox, paths, tz, now) }
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
                for screen in &mut self.screens {
                    screen.reread(&snapshot);
                }
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
        let at = keep.and_then(|k| self.rows.iter().position(|r| *r == k && r.selectable()));
        self.list.select(at.or_else(|| self.rows.iter().position(Row::selectable)));
    }

    /// The project an `@name` filter names (Ok), or the name when it names none (Err).
    pub fn filter_project(&self, query: &Query) -> Option<Result<String, String>> {
        let token = query.projects.first()?;
        let data = &self.snapshot.as_ref()?.data;
        Some(resolve_project(data, token).map_err(|_| token.clone()))
    }

    /// A new task's projects: the `@name` filter's, else the first of the
    /// task this switcher runs in (R1.6).
    pub fn default_projects(&self) -> Vec<String> {
        match self.filter_project(&Query::parse(self.search.value())) {
            Some(Ok(id)) => vec![id],
            Some(Err(_)) => vec![],
            None => (self.snapshot.as_ref())
                .and_then(|s| s.data.tasks.get(s.current.as_deref()?))
                .map_or(vec![], |task| task.projects.iter().take(1).cloned().collect()),
        }
    }

    fn visible_rows(&self) -> Vec<Row> {
        let Some(snap) = &self.snapshot else { return vec![] };
        let query = Query::parse(self.search.value());
        let filter = self.filter_project(&query);
        let text = query.text.to_lowercase();
        let in_project = |task: &Task| match &filter {
            None => true,
            Some(Ok(id)) => task.projects.contains(id),
            Some(Err(_)) => false, // a project that does not exist has no tasks
        };
        let mut tasks: Vec<(&String, &Task)> = (snap.data.tasks.iter())
            .filter(|(_, task)| self.view.shows(task) && task.title.to_lowercase().contains(&text) && in_project(task))
            .collect();
        tasks.sort_by_key(|(name, task)| inbox_order(&snap.status, name, task));
        let mut rows = vec![];
        let none = tasks.is_empty();
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
        if let Some(Err(unknown)) = filter {
            rows.push(Row::UnknownProject(unknown));
        }
        if none && !query.text.is_empty() {
            rows.push(Row::Create);
        }
        rows
    }

    /// What the screens' keys see besides their own state.
    fn context(&self) -> Context<'_> {
        Context { tz: &self.tz, now: self.now, paths: &self.paths }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        if let Some(mut top) = self.screens.pop() {
            let outcome = top.on_key(key, &self.context());
            self.screens.push(top);
            return self.apply(outcome);
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
            KeyCode::Char('n') if ctrl => self.new_task(),
            KeyCode::Char('e') if ctrl => self.edit_task(),
            KeyCode::Char('z') if ctrl => self.snooze(),
            KeyCode::Char('u') if ctrl => return self.wake(),
            KeyCode::Char('a') if ctrl => return self.archive(),
            KeyCode::Char('d') if ctrl => self.delete(),
            KeyCode::Char('p') if ctrl => self.open_projects(),
            KeyCode::Char('s') if ctrl => self.open_settings(),
            KeyCode::Char('o') if ctrl => {
                return self.highlighted_task().map_or(Action::None, |(s, _)| Action::Notes(s));
            }
            KeyCode::F(1) => self.screens.push(Screen::Help(Help::default())),
            // "?" is help only while the search is empty; otherwise it is typed.
            KeyCode::Char('?') if self.search.value().is_empty() => self.screens.push(Screen::Help(Help::default())),
            _ if alt => {}
            _ => {
                if self.search.handle_event(&Event::Key(key)).is_some_and(|change| change.value) {
                    self.rebuild();
                }
            }
        }
        Action::None
    }

    /// Pasted text goes where typing goes, its whitespace (newlines too) one space each.
    pub fn on_paste(&mut self, text: &str) -> Action {
        let mut action = Action::None;
        for (i, word) in text.split_whitespace().enumerate() {
            for c in (i > 0).then_some(' ').into_iter().chain(word.chars()) {
                action = self.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
                if action != Action::None {
                    return action;
                }
            }
        }
        action
    }

    /// What a screen's outcome does to the stack; returns what the event loop does next.
    fn apply(&mut self, outcome: Outcome) -> Action {
        let mut outcome = outcome;
        loop {
            match outcome {
                Outcome::Stay => return Action::None,
                Outcome::Act(action) => return self.reached(action),
                Outcome::Open(screen) => {
                    self.screens.push(*screen);
                    return Action::None;
                }
                Outcome::Close => {
                    self.screens.pop();
                    return Action::None;
                }
                Outcome::CloseAnd(action) => {
                    self.screens.pop();
                    match self.screens.last_mut() {
                        Some(below) => outcome = below.returned(action),
                        None => return self.reached(action),
                    }
                }
            }
        }
    }

    /// An action no screen took: the switcher's own, else for the event loop.
    fn reached(&mut self, action: Action) -> Action {
        match action {
            Action::Filter(name) if !self.alone => {
                self.search = Input::new(format!("{} ", filter_token(&name)));
                self.rebuild();
                Action::None
            }
            action => action,
        }
    }

    /// What the screen on top needs read now (a folder listing, a check).
    pub fn needs(&mut self) -> Option<Need> {
        let now = self.now.instant;
        self.screens.last_mut()?.needs(now)
    }

    /// When the screen on top will need something.
    pub fn due(&self) -> Option<Instant> {
        self.screens.last()?.due()
    }

    /// A folder listing arrived: every screen that asked for it keeps it.
    pub fn listed(&mut self, parent: &str, listing: &Option<Listing>) {
        for screen in &mut self.screens {
            screen.listed(parent, listing);
        }
    }

    /// What a clone child said last, for the add dialog that runs it.
    pub fn cloning(&mut self, line: String) {
        if let Some(Screen::AddProject(dialog)) =
            self.screens.iter_mut().rev().find(|s| matches!(s, Screen::AddProject(_)))
        {
            dialog.cloning(line);
        }
    }

    /// Tab / Shift+Tab: the next view; the search text stays (R6.1).
    fn change_view(&mut self, step: isize) {
        let at = View::ALL.iter().position(|v| *v == self.view).unwrap_or(0) as isize;
        self.view = View::ALL[(at + step).rem_euclid(View::ALL.len() as isize) as usize];
        self.rebuild();
    }

    /// Up and down skip the section headers.
    fn move_by(&mut self, step: isize) {
        let selectable: Vec<usize> = (0..self.rows.len()).filter(|&i| self.rows[i].selectable()).collect();
        let Some(at) = self.list.selected().and_then(|s| selectable.iter().position(|&i| i == s)) else { return };
        let next = (at as isize + step).clamp(0, selectable.len() as isize - 1) as usize;
        self.list.select(Some(selectable[next]));
    }

    /// Enter: open the task (the attach wakes or reopens it), or create one
    /// in the filter's project (or the current task's first).
    fn open(&mut self) -> Action {
        match self.highlighted() {
            Some(Row::Create) => Action::Run(Job::Create {
                title: Query::parse(self.search.value()).text,
                description: None,
                projects: self.default_projects(),
                until: None,
            }),
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

    /// Ctrl+N: the New task form (R1.1), with the search's title and the
    /// filter's project (or the current task's first).
    fn new_task(&mut self) {
        let Some(snap) = &self.snapshot else { return };
        let title = Query::parse(self.search.value()).text;
        let presets = snooze::presets(self.now.epoch, &self.tz, false); // a new task has no agents yet
        let form = TaskForm::new_task(&title, self.default_projects(), snap.saved(), presets, &self.paths)
            .cannot_switch(snap.switch_problem.clone());
        self.screens.push(Screen::Form(Box::new(form)));
    }

    /// Ctrl+E: the edit form (R5.4), saying what a project added does to the session.
    fn edit_task(&mut self) {
        let (Some(snap), Some((session, task))) = (&self.snapshot, self.highlighted_task()) else { return };
        let editing = Editing::new(session, task, snap);
        let form = TaskForm::edit(task, snap.saved(), editing);
        self.screens.push(Screen::Form(Box::new(form)));
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

    /// Ctrl+D: deleting cannot be undone, so it always asks (R5.3).
    fn delete(&mut self) {
        if let Some((session, task)) = self.highlighted_task() {
            let title = task.title.clone();
            self.screens.push(Screen::ConfirmDelete { session, title });
        }
    }

    /// Ctrl+P: the saved projects (R8.3).
    fn open_projects(&mut self) {
        if let Some(snap) = &self.snapshot {
            self.screens.push(Screen::Projects(ProjectsScreen::new(snap)));
        }
    }

    /// Ctrl+S: the settings (R10.4), as the last refresh read them.
    fn open_settings(&mut self) {
        let settings = self.snapshot.as_ref().map_or(Ok(Settings::default()), |s| s.settings.clone());
        let file = self.paths.display(&self.paths.config_file());
        self.screens.push(Screen::Settings(SettingsScreen::new(settings, file)));
    }

    /// A job finished in its worker. The screen that asked for it gets the
    /// result; when the user went on to another screen meanwhile, that one
    /// stays where it is.
    pub fn done(&mut self, job: Job, result: Result<Reply, Error>) -> Action {
        if let Some(at) = self.screens.iter().rposition(|s| s.takes(&job)) {
            let mut screen = self.screens.remove(at);
            let outcome = screen.done(&job, result, &self.context());
            self.screens.insert(at, screen);
            return match outcome {
                outcome if at + 1 == self.screens.len() => self.apply(outcome),
                Outcome::Act(action) => self.reached(action),
                _ => Action::None,
            };
        }
        let current = self.snapshot.as_ref().and_then(|s| s.current.clone());
        let problem = self.snapshot.as_ref().and_then(|s| s.switch_problem.clone());
        // Its clients moved to another task: this one's too, so leave (R4.7).
        let here = |session: &str| current.as_deref() == Some(session);
        match (job, result) {
            (
                Job::CheckProject { .. } | Job::RemovalPlan { .. } | Job::AddProject { .. } | Job::Clone { .. },
                Ok(_),
            ) => {} // its screen closed meanwhile
            (_, Err(e)) => self.flash(e.to_string(), true),
            (Job::Wake { title, .. }, Ok(_)) => self.flash(format!("“{title}” is active."), false),
            (Job::Archive { session, .. } | Job::Delete { session, .. }, Ok(_)) if here(&session) => {
                return Action::Quit;
            }
            (Job::Archive { title, .. }, Ok(_)) => self.flash(format!("Archived “{title}”."), false),
            (Job::Delete { title, .. }, Ok(_)) => self.flash(format!("Deleted “{title}”."), false),
            (Job::Snooze { session, .. }, Ok(_)) if here(&session) => return Action::Quit,
            (Job::Snooze { title, until, .. }, Ok(_)) => {
                self.flash(format!("Snoozed “{title}” {}.", until.describe(self.now.epoch, &self.tz)), false)
            }
            (Job::RemoveProject { .. }, Ok(Reply::Removed(notes))) => {
                let kept = notes.iter().any(|n| matches!(n, RemovalNote::Kept(_)));
                let lines: Vec<String> = notes.into_iter().map(RemovalNote::line).collect();
                self.flash(lines.join("; "), kept);
            }
            (Job::RenameProject { .. }, Ok(Reply::Renamed(old, new, note))) => {
                self.flash(renamed(&old, &new, note), false)
            }
            (Job::Edit { .. }, Ok(Reply::Said(notes))) => {
                // A workspace that could not be opened, or a folder missing, is worth reading.
                let failed = notes.iter().any(|n| n.starts_with("could not") || n.contains(" is missing"));
                self.flash(format!("Saved{}.", notes.iter().map(|n| format!("; {n}")).collect::<String>()), failed);
            }
            (Job::Create { title, until: Some(until), .. }, Ok(_)) => {
                self.search.reset();
                self.rebuild();
                self.flash(format!("Saved “{title}”, snoozed {}.", until.describe(self.now.epoch, &self.tz)), false);
            }
            (Job::Create { title, .. }, Ok(Reply::Created(session))) => match problem {
                None => return Action::Switch(session),
                Some(problem) => {
                    self.search.reset();
                    self.rebuild();
                    self.flash(format!("Saved “{title}” without switching: {problem}"), true);
                }
            },
            (_, Ok(_)) => {}
        }
        Action::None
    }
}

/// What a rename says (R8.2), as the Python itm's Projects screen says it.
pub fn renamed(old: &str, new: &str, note: Option<String>) -> String {
    format!("Renamed {old} to {new}.{}", note.map(|n| format!(" {n}.")).unwrap_or_default())
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

    pub fn project(name: &str, path: Option<&str>, used: i64) -> Project {
        Project {
            name: name.into(),
            path: path.map(Into::into),
            url: None,
            added_at: 1,
            last_used_at: used,
            extra: <_>::default(),
        }
    }

    /// [`snapshot`] with saved projects: api (Fix login has it), web,
    /// backend (a clone), gone (its folder is missing) and Client X (a name only).
    pub fn with_projects() -> Snapshot {
        let mut snap = snapshot();
        let mut backend = project("backend", Some("/r/data/projects/github.com/acme/backend"), NOW - 50);
        backend.url = Some("git@github.com:acme/backend.git".into());
        for (id, project) in [
            ("p-api", project("api", Some("/r/home/code/api"), NOW - 10)),
            ("p-web", project("web", Some("/r/home/code/web"), NOW - 20)),
            ("p-backend", backend),
            ("p-gone", project("gone", Some("/r/home/code/gone"), NOW - 60)),
            ("p-x", project("Client X", None, NOW - 70)),
        ] {
            if let Some(folder) = project.folder() {
                snap.folders.insert(folder.to_path_buf(), id.into());
            }
            snap.data.projects.insert(id.into(), project);
        }
        snap.missing.insert("p-gone".into());
        snap.data.tasks.get_mut("task-fix-login").unwrap().projects = vec!["p-api".into()];
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
        let create = Job::Create { title: "Brand new".into(), description: None, projects: vec![], until: None };
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Run(create.clone()));
        assert_eq!(
            app.done(create, Ok(Reply::Created("task-brand-new".into()))),
            Action::Switch("task-brand-new".into())
        );
    }

    /// R1.4: in a client that cannot be switched, the switcher saves the task without switching and says why.
    #[test]
    fn a_task_created_where_switching_cannot_work_is_saved_and_says_why() {
        let mut snap = snapshot();
        snap.switch_problem = Some("this Herdr client was not started by `itm attach`".into());
        let mut app = app(snap);
        typed(&mut app, "Brand new");
        let create = Job::Create { title: "Brand new".into(), description: None, projects: vec![], until: None };
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Run(create.clone()));
        assert_eq!(app.done(create, Ok(Reply::Created("task-brand-new".into()))), Action::None);
        let message = app.message.as_ref().expect("a message");
        assert!(message.error && message.text.starts_with("Saved “Brand new” without switching: this Herdr client"));
        assert_eq!(app.search.value(), "", "the search is cleared for the next one");
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
        assert_eq!(app.done(job, Ok(Reply::Done)), Action::Quit);
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

    /// R5.3: Ctrl+D always asks; only y deletes, and deleting the task this
    /// switcher runs in closes it (R4.7).
    #[test]
    fn ctrl_d_asks_before_deleting() {
        let mut snap = snapshot();
        snap.current = Some("task-write-docs".into());
        let mut app = app(snap);
        assert_eq!(app.on_key(ctrl('d')), Action::None);
        assert!(matches!(&app.screens[..], [Screen::ConfirmDelete { session, .. }] if session == "task-fix-login"));
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None, "only y deletes");
        assert_eq!(app.on_key(key(KeyCode::Char('n'))), Action::None);
        assert!(app.screens.is_empty());
        app.on_key(ctrl('d'));
        let delete = Job::Delete { session: "task-fix-login".into(), title: "Fix login".into() };
        assert_eq!(app.on_key(key(KeyCode::Char('y'))), Action::Run(delete.clone()));
        assert_eq!(app.done(delete, Ok(Reply::Done)), Action::None);
        assert_eq!(app.message.as_ref().map(|m| m.text.as_str()), Some("Deleted “Fix login”."));
        let here = Job::Delete { session: "task-write-docs".into(), title: "Write docs".into() };
        assert_eq!(app.done(here, Ok(Reply::Done)), Action::Quit);
    }

    /// R10.4: Ctrl+S opens the settings; a toggle saves one at a time, and
    /// says when it was not saved. A file with a mistake toggles nothing.
    #[test]
    fn r10_4_ctrl_s_toggles_the_settings_one_at_a_time() {
        use crate::config::Setting::{ProjectsFromWorkspaces, RemoveProjectsWithoutWorkspace};
        use crate::tui::screen::SettingsScreen;
        let mut app = app(snapshot());
        assert_eq!(app.on_key(ctrl('s')), Action::None);
        let shown = |app: &App| match &app.screens[..] {
            [Screen::Settings(screen)] => screen.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(shown(&app).file, "/r/config/inbox-task-manager/config.toml");
        app.on_key(key(KeyCode::Down));
        let off = Job::Setting { setting: RemoveProjectsWithoutWorkspace, on: false };
        assert_eq!(app.on_key(key(KeyCode::Char(' '))), Action::Run(off.clone()));
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None, "one save at a time");
        app.loaded(Ok(snapshot())); // a refresh while saving keeps what is being saved
        assert!(shown(&app).saving);
        assert_eq!(app.done(off, Ok(Reply::Done)), Action::None);
        let screen: SettingsScreen = shown(&app);
        assert_eq!((screen.saving, screen.settings.unwrap().remove_projects_without_workspace), (false, false));
        let on = Job::Setting { setting: RemoveProjectsWithoutWorkspace, on: true };
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Run(on.clone()));
        app.done(on, Err(Error::Other("cannot write ~/c.toml: Permission denied".into())));
        assert_eq!(shown(&app).failed.as_deref(), Some("cannot write ~/c.toml: Permission denied"));
        app.on_key(ctrl('k'));
        let first = Job::Setting { setting: ProjectsFromWorkspaces, on: false };
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Run(first));

        let mut broken = snapshot();
        broken.settings = Err("~/c.toml: unknown setting herdr.nope".into());
        let mut app = super::tests::app(broken.clone());
        app.on_key(ctrl('s'));
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None, "nothing toggles until the file is fixed");
        app.loaded(Ok(snapshot())); // fixed meanwhile
        assert!(matches!(app.on_key(key(KeyCode::Enter)), Action::Run(Job::Setting { .. })));
        assert_eq!(app.on_key(key(KeyCode::Esc)), Action::None);
        assert!(app.screens.is_empty(), "Esc closes it, also while it saves");
    }

    /// R9.4: Ctrl+O opens the highlighted task's notes, in any view.
    #[test]
    fn ctrl_o_opens_the_highlighted_tasks_notes_in_any_view() {
        let mut app = app(snapshot());
        assert_eq!(app.on_key(ctrl('o')), Action::Notes("task-fix-login".into()));
        app.on_key(key(KeyCode::BackTab)); // All
        app.on_key(key(KeyCode::BackTab)); // Archived
        assert_eq!(app.view, View::Archived);
        assert_eq!(app.on_key(ctrl('o')), Action::Notes("task-shipped".into()));
        typed(&mut app, "nothing like it");
        assert_eq!(app.on_key(ctrl('o')), Action::None, "the Create row has no notes");
    }

    /// R8.7: the remove dialog offers deleting only a clean clone of itm's own, keeping it first.
    #[test]
    fn the_remove_dialog_offers_only_what_may_happen() {
        use crate::projects::{CloneState, RemovalPlan};
        use crate::store::Project;
        use crate::tui::screen::RemoveDialog;
        let plan = |clone: Option<CloneState>, users: &[&str]| RemovalPlan {
            id: "p1".into(),
            project: Project {
                name: "api".into(),
                path: Some("/r/home/src/api".into()),
                url: clone.as_ref().map(|_| "git@github.com:acme/api.git".into()),
                added_at: 1,
                last_used_at: 1,
                extra: Default::default(),
            },
            users: users.iter().map(|u| u.to_string()).collect(),
            clone,
        };
        let paths = Paths::under(std::path::Path::new("/r"));
        let dialog = |plan: RemovalPlan| RemoveDialog::new(&plan, &paths);
        let clean = || Some(CloneState::Deletable { local_work: None });
        assert_eq!(dialog(plan(clean(), &[])).choices, [CloneChoice::Keep, CloneChoice::IfClean]);
        let busy = Some(CloneState::Deletable { local_work: Some("stashes".into()) });
        assert_eq!(dialog(plan(busy, &[])).choices, [CloneChoice::Keep]);
        assert_eq!(dialog(plan(Some(CloneState::Foreign), &[])).choices, [CloneChoice::Keep]);
        assert_eq!(dialog(plan(None, &[])).choices, [CloneChoice::Keep], "a folder is only forgotten");
        assert_eq!(dialog(plan(clean(), &["A task"])).choices, [], "nothing while a task has it");
        assert_eq!(dialog(plan(clean(), &[])).folder.as_deref(), Some("~/src/api"));

        let mut app = app(snapshot());
        let removed = |choice| Action::Run(Job::RemoveProject { id: "p1".into(), name: "api".into(), choice });
        app.screens.push(Screen::RemoveProject(dialog(plan(clean(), &[]))));
        assert_eq!(app.on_key(key(KeyCode::Enter)), removed(CloneChoice::Keep), "keeping is the default");
        app.screens.push(Screen::RemoveProject(dialog(plan(clean(), &[]))));
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.on_key(key(KeyCode::Enter)), removed(CloneChoice::IfClean));
        app.screens.push(Screen::RemoveProject(dialog(plan(clean(), &["A task"]))));
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None, "nothing to choose");
        assert_eq!(app.on_key(key(KeyCode::Esc)), Action::None);
        assert!(app.screens.is_empty(), "esc changes nothing");
    }

    /// R6.2: `@name` and `@"a name"` filter by project; the rest searches titles.
    #[test]
    fn r6_2_queries_read_project_filters() {
        let query = Query::parse(r#"@"my app" fix @api"#);
        assert_eq!((query.projects, query.text.as_str()), (vec!["my app".to_string(), "api".to_string()], "fix"));
        assert_eq!(Query::parse("@ x @\"  \"").projects, Vec::<String>::new(), "blank tokens are none");
        assert_eq!((filter_token("api"), filter_token("Client X")), ("@api".to_string(), "@\"Client X\"".to_string()));
    }

    /// R6.2, after test_create_row_uses_the_filtered_project and
    /// test_unknown_project_filter: a filter shows the tasks that have the
    /// project among any of theirs, a task created from it gets the project,
    /// and one that names no project says so and creates without one.
    #[test]
    fn r6_2_project_filters_and_the_create_row() {
        let mut snap = with_projects();
        snap.data.tasks.get_mut("task-write-docs").unwrap().projects = vec!["p-api".into(), "p-web".into()];
        let mut app = app(snap);
        typed(&mut app, "@web");
        assert_eq!(app.rows, [Row::Header("ACTIVE"), Row::Task("task-write-docs".into())], "any of its projects");
        app.search = Input::new("@API Rate limits".into());
        app.rebuild();
        assert_eq!(app.rows, [Row::Create]);
        let create =
            Job::Create { title: "Rate limits".into(), description: None, projects: vec!["p-api".into()], until: None };
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Run(create));
        app.search = Input::new("@nope Fix".into());
        app.rebuild();
        assert_eq!(app.rows, [Row::UnknownProject("nope".into()), Row::Create]);
        assert_eq!(app.highlighted(), Some(&Row::Create));
        assert_eq!(app.default_projects(), Vec::<String>::new());
        app.on_key(ctrl('n'));
        let [Screen::Form(form)] = &app.screens[..] else { panic!("no form") };
        assert_eq!((form.title.value(), form.projects.clone()), ("Fix", vec![]));
    }

    /// R1.6: without a filter, a new task starts with the first project of the task this switcher runs in.
    #[test]
    fn r1_6_a_new_task_starts_with_the_current_tasks_first_project() {
        let mut snap = with_projects();
        snap.current = Some("task-fix-login".into());
        let mut app = app(snap);
        assert_eq!(app.default_projects(), ["p-api"]);
        app.on_key(ctrl('n'));
        assert!(matches!(&app.screens[..], [Screen::Form(form)] if form.projects == ["p-api"]));
    }

    /// UX principle 5, after test_vim_keys_and_help: `?` opens the help only
    /// while the search is empty (then it is typed), F1 always; the help
    /// closes with `?`, F1 or Esc; list keys do nothing behind a screen.
    #[test]
    fn help_opens_with_a_question_mark_on_an_empty_search_or_f1() {
        let mut app = app(snapshot());
        app.on_key(key(KeyCode::Char('?')));
        assert!(matches!(app.screens[..], [Screen::Help(_)]));
        app.on_key(key(KeyCode::Char('?')));
        assert!(app.screens.is_empty());
        typed(&mut app, "why?");
        assert_eq!((app.search.value(), app.screens.len()), ("why?", 0));
        app.on_key(key(KeyCode::F(1)));
        assert!(matches!(app.screens[..], [Screen::Help(_)]));
        app.on_key(key(KeyCode::Esc));
        app.on_key(ctrl('p'));
        assert_eq!(app.on_key(ctrl('z')), Action::None, "snooze is no Projects key");
        assert!(matches!(app.screens[..], [Screen::Projects(_)]));
        app.on_key(ctrl('s'));
        assert!(matches!(app.screens[..], [Screen::Projects(_)]), "nor are the settings");
    }

    /// R5.4: Ctrl+E edits the highlighted task, knowing its session runs;
    /// what the edit did is said, as an error when something failed.
    #[test]
    fn r5_4_ctrl_e_edits_and_says_what_happened() {
        let mut app = app(with_projects());
        app.on_key(ctrl('e'));
        let [Screen::Form(form)] = &app.screens[..] else { panic!("no form") };
        let editing = form.editing.as_ref().unwrap();
        assert_eq!(
            (editing.session.as_str(), editing.effect),
            ("task-fix-login", Some(super::super::form::Effect::Running))
        );
        assert_eq!(form.projects, ["p-api"]);
        let edit = |notes: &[&str]| {
            let job = Job::Edit {
                session: "task-fix-login".into(),
                title: "Fix login".into(),
                description: String::new(),
                add: vec![],
                remove: vec![],
                order: vec![],
            };
            (job, Reply::Said(notes.iter().map(|n| n.to_string()).collect()))
        };
        app.screens.clear();
        let (job, said) = edit(&["opened a workspace in ~/code/web"]);
        app.done(job, Ok(said));
        let message = app.message.as_ref().unwrap();
        assert_eq!((message.text.as_str(), message.error), ("Saved; opened a workspace in ~/code/web.", false));
        let (job, said) = edit(&["could not open a workspace in ~/code/web: no answer"]);
        app.done(job, Ok(said));
        assert!(app.message.as_ref().unwrap().error);
    }

    /// R1.1, R1.7: a task the form created snoozed is said, not switched to.
    #[test]
    fn r1_7_a_task_created_snoozed_is_said_not_switched_to() {
        let mut app = app(snapshot());
        typed(&mut app, "Plan");
        let create = Job::Create {
            title: "Plan".into(),
            description: None,
            projects: vec![],
            until: Some(Until::Time(NOW + 3600)),
        };
        assert_eq!(app.done(create, Ok(Reply::Created("task-plan".into()))), Action::None);
        assert_eq!(app.message.as_ref().unwrap().text, "Saved “Plan”, snoozed until 09:00.");
        assert_eq!(app.search.value(), "");
    }

    /// R8.3: Enter on the Projects screen shows the project's tasks.
    #[test]
    fn r8_3_enter_on_a_project_filters_the_list_by_it() {
        let mut app = app(with_projects());
        app.on_key(ctrl('p'));
        app.on_key(key(KeyCode::Enter));
        assert!(app.screens.is_empty());
        assert_eq!(app.search.value(), "@api ");
        assert_eq!(app.rows, [Row::Header("NEEDS YOU"), Row::Task("task-fix-login".into())]);
    }
}
