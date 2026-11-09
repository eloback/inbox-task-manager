//! Screens that open over the task list, or alone (a dialog the CLI opens):
//! the dialogs, the New task and edit form, its project finder, the add,
//! rename and remove dialogs, Projects, Settings and help. They are a
//! stack: only the top one gets keys, so list keys never act on the task
//! behind a dialog (DESIGN.md section 4), and Esc closes only the top.
//!
//! A screen says what a key did in its [`Outcome`]: it stays, closes (with
//! an action for the screen below, which gets it in [`Screen::returned`],
//! like a dialog's result), has work done while it stays open (the result
//! comes back in [`Screen::done`]), or opens another screen over itself.
//! Screens do no I/O: what they need read (a folder listing, a check of
//! what was typed) they say in [`Screen::needs`], and the event loop
//! reads it in a thread and hands it back.
//!
//! A new screen is a new variant; every `match` that must handle it then
//! fails to compile until it does.

use std::time::Instant;

use jiff::tz::TimeZone;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use super::add::AddDialog;
use super::app::{Action, Job, Moment, Reply, Snapshot};
use super::finder::Finder;
use super::form::TaskForm;
use super::help::Help;
use super::projects::{ProjectsScreen, RenameDialog};
use crate::config::{Setting, Settings};
use crate::error::Error;
use crate::folders::Listing;
use crate::paths::Paths;
use crate::projects::{CloneChoice, CloneState, RemovalPlan};
use crate::snooze::Until;
use crate::store::ProjectKind;

#[derive(Debug, Clone)]
pub enum Screen {
    /// Archiving stops a running session, and Ctrl+A is also a common Herdr
    /// prefix: ask first (R5.1, UX principle 10).
    ConfirmArchive {
        session: String,
        title: String,
    },
    /// Deleting cannot be undone: ask first, saying what goes (R5.3).
    ConfirmDelete {
        session: String,
        title: String,
    },
    /// Esc in a form with something typed in it (R1.1): y discards it.
    ConfirmDiscard,
    Snooze(SnoozeDialog),
    RemoveProject(RemoveDialog),
    /// The workspace hotkey asks before giving a project without a folder
    /// the workspace's folder: it changes which workspaces count as the
    /// project in every task (R10.5).
    ConfirmFolder {
        project: String,
        /// As shown, `~` for the home folder.
        folder: String,
    },
    AddProject(Box<AddDialog>),
    Settings(SettingsScreen),
    Form(Box<TaskForm>),
    Finder(Box<Finder>),
    Projects(ProjectsScreen),
    Rename(RenameDialog),
    Help(Help),
}

/// What a key did to the screen on top.
#[derive(Debug)]
pub enum Outcome {
    Stay,
    Close,
    /// Close, then the screen below gets the action ([`Screen::returned`]);
    /// with none below, the switcher acts on it (or the CLI that opened the screen alone gets it).
    CloseAnd(Action),
    /// Act and stay: a job's result comes back to this screen ([`Screen::done`]).
    Act(Action),
    /// Open a screen over this one.
    Open(Box<Screen>),
}

/// What a screen may need besides its own state.
pub struct Context<'a> {
    pub tz: &'a TimeZone,
    /// When the key arrived.
    pub now: Moment,
    pub paths: &'a Paths,
}

/// Something a screen needs read to show what it should.
#[derive(Debug, Clone, PartialEq)]
pub enum Need {
    /// The listing of a folder as typed (folder completion, R1.6).
    Listing(String),
    /// A job whose result comes back to the screen (the add dialog's check of what was typed).
    Job(Job),
}

impl Screen {
    pub fn on_key(&mut self, key: KeyEvent, cx: &Context) -> Outcome {
        match self {
            Screen::ConfirmArchive { session, title } => match key.code {
                KeyCode::Char('y') => {
                    Outcome::CloseAnd(Action::Run(Job::Archive { session: session.clone(), title: title.clone() }))
                }
                KeyCode::Char('n') | KeyCode::Esc => Outcome::Close,
                _ => Outcome::Stay,
            },
            Screen::ConfirmDelete { session, title } => match key.code {
                KeyCode::Char('y') => {
                    Outcome::CloseAnd(Action::Run(Job::Delete { session: session.clone(), title: title.clone() }))
                }
                KeyCode::Char('n') | KeyCode::Esc => Outcome::Close,
                _ => Outcome::Stay,
            },
            Screen::ConfirmDiscard => match key.code {
                KeyCode::Char('y') => Outcome::CloseAnd(Action::Answer(true)),
                KeyCode::Char('n') | KeyCode::Esc => Outcome::Close, // keep editing
                _ => Outcome::Stay,
            },
            Screen::Snooze(dialog) => dialog.on_key(key, cx),
            Screen::RemoveProject(dialog) => dialog.on_key(key),
            Screen::ConfirmFolder { .. } => match key.code {
                KeyCode::Char('y') => Outcome::CloseAnd(Action::Answer(true)),
                KeyCode::Char('n') => Outcome::CloseAnd(Action::Answer(false)),
                KeyCode::Esc => Outcome::Close,
                _ => Outcome::Stay,
            },
            Screen::AddProject(dialog) => dialog.on_key(key, cx),
            Screen::Settings(screen) => screen.on_key(key),
            Screen::Form(form) => form.on_key(key, cx),
            Screen::Finder(finder) => finder.on_key(key),
            Screen::Projects(screen) => screen.on_key(key),
            Screen::Rename(dialog) => dialog.on_key(key, cx),
            Screen::Help(help) => help.on_key(key),
        }
    }

    /// The screen above closed with `action` (a dialog's result). What is
    /// not for this screen goes on to the switcher.
    pub fn returned(&mut self, action: Action) -> Outcome {
        match self {
            Screen::Form(form) => form.returned(action),
            Screen::Finder(finder) => finder.returned(action),
            Screen::Projects(screen) => screen.returned(action),
            _ => Outcome::Act(action),
        }
    }

    /// Whether this screen asked for `job`, so its result comes back here.
    pub fn takes(&self, job: &Job) -> bool {
        match self {
            Screen::AddProject(_) => {
                matches!(job, Job::AddProject { .. } | Job::Clone { .. } | Job::CheckProject { .. })
            }
            Screen::Settings(_) => matches!(job, Job::Setting { .. } | Job::Theme { .. }),
            Screen::Snooze(_) | Screen::Form(_) => matches!(job, Job::SnoozeTime { .. }),
            Screen::Projects(_) => {
                matches!(job, Job::RenameProject { .. } | Job::RemovalPlan { .. } | Job::RemoveProject { .. })
            }
            _ => false,
        }
    }

    /// The result of a job this screen asked for ([`Screen::takes`]).
    pub fn done(&mut self, job: &Job, result: Result<Reply, Error>, cx: &Context) -> Outcome {
        match self {
            Screen::AddProject(dialog) => dialog.done(job, result),
            Screen::Settings(screen) => match job {
                Job::Setting { setting, on } => screen.done(|s| s.set(*setting, *on), result),
                Job::Theme { theme } => screen.done(|s| s.theme = *theme, result),
                _ => Outcome::Stay,
            },
            Screen::Projects(screen) => screen.done(job, result, cx),
            Screen::Snooze(dialog) => dialog.read(result),
            Screen::Form(form) => form.read(result),
            _ => Outcome::Stay,
        }
    }

    /// What this screen needs read now, asked for once each.
    pub fn needs(&mut self, now: Instant) -> Option<Need> {
        match self {
            Screen::AddProject(dialog) => dialog.needs(now),
            Screen::Finder(finder) => finder.needs(),
            _ => None,
        }
    }

    /// When this screen will need something (a check once typing pauses).
    pub fn due(&self) -> Option<Instant> {
        match self {
            Screen::AddProject(dialog) => dialog.due(),
            _ => None,
        }
    }

    /// A folder listing some screen asked for arrived.
    pub fn listed(&mut self, parent: &str, listing: &Option<Listing>) {
        match self {
            Screen::AddProject(dialog) => dialog.folders.arrived(parent, listing),
            Screen::Finder(finder) => finder.listed(parent, listing),
            _ => {}
        }
    }

    /// The switcher refreshed: screens that show its state follow it.
    pub fn reread(&mut self, snapshot: &Snapshot) {
        match self {
            Screen::Settings(screen) => screen.reread(&snapshot.settings),
            Screen::Projects(screen) => screen.reread(snapshot),
            _ => {}
        }
    }
}

/// Which field of a two-field dialog has the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Source,
    Name,
}

/// The settings in `config.toml` (R10.4), saved one toggle at a time as
/// they are toggled. A file with a mistake is shown, and nothing toggles
/// until it is fixed.
#[derive(Debug, Clone)]
pub struct SettingsScreen {
    /// As last read, or why they could not be.
    pub settings: Result<Settings, String>,
    /// Where they are saved, as shown.
    pub file: String,
    /// An index into [`Setting::ALL`], or past its end the theme.
    pub selected: usize,
    /// A save runs: one at a time, since a toggle reads the value the last one left.
    pub saving: bool,
    /// Why the last save failed.
    pub failed: Option<String>,
}

impl SettingsScreen {
    pub fn new(settings: Result<Settings, String>, file: String) -> SettingsScreen {
        SettingsScreen { settings, file, selected: 0, saving: false, failed: None }
    }

    /// The settings as read again (a refresh), unless a save is on its way.
    pub fn reread(&mut self, settings: &Result<Settings, String>) {
        if !self.saving {
            self.settings = settings.clone();
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Outcome {
        let last = Setting::ALL.len(); // the theme
        match key.code {
            KeyCode::Esc => return Outcome::Close,
            _ if moves(key) != 0 => self.selected = step(self.selected, moves(key), last),
            KeyCode::Enter | KeyCode::Char(' ') if !self.saving => {
                if let Ok(settings) = &self.settings {
                    (self.saving, self.failed) = (true, None);
                    let job = match Setting::ALL.get(self.selected) {
                        Some(&setting) => Job::Setting { setting, on: !settings.get(setting) },
                        None => Job::Theme { theme: settings.theme.next() },
                    };
                    return Outcome::Act(Action::Run(job));
                }
            }
            _ => {}
        }
        Outcome::Stay
    }

    fn done(&mut self, change: impl FnOnce(&mut Settings), result: Result<Reply, Error>) -> Outcome {
        self.saving = false;
        match (result, &mut self.settings) {
            (Ok(_), Ok(settings)) => change(settings),
            (Ok(_), Err(_)) => {}
            (Err(e), _) => self.failed = Some(e.to_string()),
        }
        Outcome::Stay
    }
}

/// Typing into a one-line field, selected on focus: while `selected`,
/// which focusing a field with text in it sets ([`selects`]), a character
/// replaces the text and Backspace or Delete clears it, so a field that
/// opens filled in (a rename, an edit's title) takes a new value without
/// being cleared first. Any other key unselects it, Left and Home going to
/// its start. Returns whether the value changed.
pub fn type_into(input: &mut Input, selected: &mut bool, key: KeyEvent) -> bool {
    let plain = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
    if std::mem::take(selected) {
        match key.code {
            KeyCode::Char(_) if plain => input.reset(),
            KeyCode::Backspace | KeyCode::Delete => {
                let had = !input.value().is_empty();
                input.reset();
                return had;
            }
            KeyCode::Left | KeyCode::Home => {
                *input = std::mem::take(input).with_cursor(0);
                return false;
            }
            _ => {}
        }
    }
    input.handle_event(&Event::Key(key)).is_some_and(|change| change.value)
}

/// What focusing a field selects: its text, if it has any, with the cursor
/// at its end (see [`type_into`]).
pub fn selects(input: &mut Input) -> bool {
    *input = std::mem::take(input).with_cursor(usize::MAX);
    !input.value().is_empty()
}

/// Up and down in a list: ↑↓, and Ctrl+K / Ctrl+J as in vim; 0 for any other key.
pub fn moves(key: KeyEvent) -> isize {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Up => -1,
        KeyCode::Down => 1,
        KeyCode::Char('k') if ctrl => -1,
        KeyCode::Char('j') if ctrl => 1,
        _ => 0,
    }
}

/// `at` moved by `by`, within `0..=last`.
pub fn step(at: usize, by: isize, last: usize) -> usize {
    at.saturating_add_signed(by).min(last)
}

/// Removing a project (R8.7), following `itm project remove`: a clone is
/// deleted only when it is itm's own and clean, and only when chosen; any
/// other is only ever kept. Esc changes nothing.
#[derive(Debug, Clone)]
pub struct RemoveDialog {
    pub id: String,
    pub name: String,
    pub kind: ProjectKind,
    /// Its folder, `~` for the home folder.
    pub folder: Option<String>,
    /// Titles of the tasks that have it: then it cannot be removed.
    pub users: Vec<String>,
    pub clone: Option<CloneState>,
    /// What Enter can do, the default first; none while tasks have it.
    pub choices: Vec<CloneChoice>,
    pub selected: usize,
}

impl RemoveDialog {
    pub fn new(plan: &RemovalPlan, paths: &Paths) -> RemoveDialog {
        let clean = matches!(plan.clone, Some(CloneState::Deletable { local_work: None }));
        let choices = match () {
            _ if !plan.users.is_empty() => vec![],
            _ if clean => vec![CloneChoice::Keep, CloneChoice::IfClean],
            _ => vec![CloneChoice::Keep],
        };
        RemoveDialog {
            id: plan.id.clone(),
            name: plan.project.name.clone(),
            kind: plan.project.kind(),
            folder: plan.project.folder().map(|f| paths.display(f)),
            users: plan.users.clone(),
            clone: plan.clone.clone(),
            choices,
            selected: 0,
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => return Outcome::Close,
            _ if moves(key) != 0 => {
                self.selected = step(self.selected, moves(key), self.choices.len().saturating_sub(1))
            }
            KeyCode::Enter => {
                if let Some(&choice) = self.choices.get(self.selected) {
                    let job = Job::RemoveProject { id: self.id.clone(), name: self.name.clone(), choice };
                    return Outcome::CloseAnd(Action::Run(job));
                }
            }
            _ => {}
        }
        Outcome::Stay
    }
}

/// Presets in time order, or a time typed (R4.1, UX principle 9).
#[derive(Debug, Clone)]
pub struct SnoozeDialog {
    pub session: String,
    pub title: String,
    pub typed: Input,
    pub presets: Vec<(&'static str, Until)>,
    pub selected: usize,
    pub error: Option<String>,
    /// A time typed that only GNU `date` reads is being read by a worker
    /// (R4.1); keys wait for it.
    pub reading: bool,
}

impl SnoozeDialog {
    pub fn new(session: String, title: String, presets: Vec<(&'static str, Until)>) -> SnoozeDialog {
        SnoozeDialog { session, title, typed: Input::default(), presets, selected: 0, error: None, reading: false }
    }

    fn on_key(&mut self, key: KeyEvent, cx: &Context) -> Outcome {
        match key.code {
            _ if self.reading => {} // for the few milliseconds `date` takes
            KeyCode::Esc => return Outcome::Close,
            _ if moves(key) != 0 => {
                self.selected = step(self.selected, moves(key), self.presets.len().saturating_sub(1))
            }
            KeyCode::Enter => {
                let until = match self.typed.value().trim() {
                    "" => match self.presets.get(self.selected) {
                        Some(&(_, until)) => Ok(Some(until)),
                        None => return Outcome::Stay, // nothing offered and nothing typed
                    },
                    typed => Until::read(typed, cx.now.epoch, cx.tz),
                };
                match until {
                    Ok(Some(until)) => return self.snooze(until),
                    Ok(None) => {
                        self.reading = true;
                        return Outcome::Act(Action::Run(Job::SnoozeTime { text: self.typed.value().trim().into() }));
                    }
                    Err(e) => self.error = Some(e.to_string()), // free text keeps working; say why not
                }
            }
            _ => {
                self.typed.handle_event(&Event::Key(key));
                self.error = None;
            }
        }
        Outcome::Stay
    }

    fn snooze(&self, until: Until) -> Outcome {
        Outcome::CloseAnd(Action::Run(Job::Snooze { session: self.session.clone(), title: self.title.clone(), until }))
    }

    /// The time typed was read by a worker (R4.1).
    fn read(&mut self, result: Result<Reply, Error>) -> Outcome {
        self.reading = false;
        match result {
            Ok(Reply::Until(until)) => return self.snooze(until),
            Ok(_) => {}
            Err(e) => self.error = Some(e.to_string()),
        }
        Outcome::Stay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// A field focused with text in it has that text selected; a character
    /// replaces it, Backspace and Delete clear it, Left and Home go to its
    /// start, any other key keeps it.
    #[test]
    fn typing_into_a_selected_field_replaces_its_text() {
        let field = || {
            let mut input = Input::new("api".into()).with_cursor(1);
            let selected = selects(&mut input);
            (input, selected)
        };
        let (mut input, mut selected) = field();
        assert!(selected && input.cursor() == 3, "selected, the cursor at its end");
        assert!(type_into(&mut input, &mut selected, key(KeyCode::Char('w'))));
        assert!(type_into(&mut input, &mut selected, key(KeyCode::Char('e'))));
        assert_eq!((input.value(), selected), ("we", false));
        for code in [KeyCode::Backspace, KeyCode::Delete] {
            let (mut input, mut selected) = field();
            assert!(type_into(&mut input, &mut selected, key(code)));
            assert_eq!(input.value(), "");
        }
        let (mut input, mut selected) = field();
        assert!(!type_into(&mut input, &mut selected, key(KeyCode::Home)));
        type_into(&mut input, &mut selected, key(KeyCode::Char('x')));
        assert_eq!(input.value(), "xapi");
        let (mut input, mut selected) = field();
        type_into(&mut input, &mut selected, key(KeyCode::Right));
        type_into(&mut input, &mut selected, key(KeyCode::Char('s')));
        assert_eq!(input.value(), "apis", "unselected at its end");
        let (mut input, mut selected) = field();
        type_into(&mut input, &mut selected, KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(!selected && input.value() == "api", "a Ctrl key is no character typed");
        let mut empty = Input::default();
        assert!(!selects(&mut empty), "nothing to select");
    }
}
