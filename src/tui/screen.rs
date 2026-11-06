//! Screens that open over the task list: dialogs and Settings now, and
//! later the New task form, Projects and help. They are a stack: only the
//! top one gets keys, so list keys never act on the task behind a dialog
//! (DESIGN.md section 4, rules for the TUI), and Esc closes only the top.
//! The app pushes them; a screen that opens another (the form's project
//! finder) will say so in its [`Outcome`]. A screen that has work done and
//! stays open (saving a setting, adding a project) gets the result back in
//! [`Screen::done`].
//! A new screen is a new variant; every `match` that must handle it then
//! fails to compile until it does.

use jiff::tz::TimeZone;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use super::app::{Action, Job};
use crate::config::{Setting, Settings};
use crate::error::Error;
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
    AddProject(AddDialog),
    Settings(SettingsScreen),
}

/// What a key did to the screen on top.
#[derive(Debug)]
pub enum Outcome {
    Stay,
    Close,
    /// Close, then act.
    CloseAnd(Action),
    /// Act and stay: the result comes back to this screen ([`Screen::done`]).
    Act(Action),
}

/// What a screen may need besides its own state.
pub struct Context<'a> {
    pub tz: &'a TimeZone,
    /// The epoch second the key arrived.
    pub now: i64,
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
            Screen::Snooze(dialog) => dialog.on_key(key, cx),
            Screen::RemoveProject(dialog) => dialog.on_key(key),
            Screen::ConfirmFolder { .. } => match key.code {
                KeyCode::Char('y') => Outcome::CloseAnd(Action::Answer(true)),
                KeyCode::Char('n') => Outcome::CloseAnd(Action::Answer(false)),
                KeyCode::Esc => Outcome::Close,
                _ => Outcome::Stay,
            },
            Screen::AddProject(dialog) => dialog.on_key(key),
            Screen::Settings(screen) => screen.on_key(key),
        }
    }

    /// The result of the work this screen asked for ([`Outcome::Act`]).
    pub fn done(&mut self, job: &Job, result: Result<String, Error>) -> Outcome {
        match (self, job) {
            (Screen::AddProject(dialog), Job::AddProject { .. }) => dialog.done(result),
            (Screen::Settings(screen), Job::Setting { setting, on }) => screen.done(*setting, *on, result),
            _ => Outcome::Stay, // not this screen's: it was closed meanwhile
        }
    }
}

/// Which field of a dialog has the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Source,
    Name,
}

/// Adding a project. For now a folder only, as the workspace hotkey adds
/// it (R10.5): the folder and the name, filled in. Enter saves it; a
/// problem with the name keeps the cursor on the name (R8.5). The Projects
/// screen (R8.3) will add the other kinds.
#[derive(Debug, Clone)]
pub struct AddDialog {
    pub kind: ProjectKind,
    pub source: Input,
    pub name: Input,
    pub focus: Field,
    /// Why the last try did not save it.
    pub error: Option<String>,
    /// A save runs: Enter waits for it.
    pub saving: bool,
}

impl AddDialog {
    pub fn folder(folder: &str, name: &str) -> AddDialog {
        AddDialog {
            kind: ProjectKind::Dir,
            source: Input::new(folder.into()),
            name: Input::new(name.into()),
            focus: Field::Source,
            error: None,
            saving: false,
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => return Outcome::Close,
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                self.focus = if self.focus == Field::Source { Field::Name } else { Field::Source };
            }
            KeyCode::Enter if !self.saving => {
                self.saving = true;
                let name = Some(self.name.value().trim().to_string()).filter(|n| !n.is_empty());
                let job = Job::AddProject { kind: self.kind, source: self.source.value().trim().into(), name };
                return Outcome::Act(Action::Run(job));
            }
            KeyCode::Enter => {}
            _ => {
                let field = if self.focus == Field::Source { &mut self.source } else { &mut self.name };
                if field.handle_event(&Event::Key(key)).is_some_and(|change| change.value) {
                    self.error = None;
                }
            }
        }
        Outcome::Stay
    }

    /// Saved: close with the project's id. Not saved: say why, on the name
    /// when the name is the problem.
    fn done(&mut self, result: Result<String, Error>) -> Outcome {
        self.saving = false;
        match result {
            Ok(id) => Outcome::CloseAnd(Action::ProjectAdded(id)),
            Err(e) => {
                if e.is_about_the_name() {
                    self.focus = Field::Name;
                }
                self.error = Some(e.to_string());
                Outcome::Stay
            }
        }
    }
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
    /// An index into [`Setting::ALL`].
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
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let last = Setting::ALL.len() - 1;
        match key.code {
            KeyCode::Esc => return Outcome::Close,
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('k') if ctrl => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(last),
            KeyCode::Char('j') if ctrl => self.selected = (self.selected + 1).min(last),
            KeyCode::Enter | KeyCode::Char(' ') if !self.saving => {
                if let Ok(settings) = &self.settings {
                    let setting = Setting::ALL[self.selected];
                    (self.saving, self.failed) = (true, None);
                    return Outcome::Act(Action::Run(Job::Setting { setting, on: !settings.get(setting) }));
                }
            }
            _ => {}
        }
        Outcome::Stay
    }

    fn done(&mut self, setting: Setting, on: bool, result: Result<String, Error>) -> Outcome {
        self.saving = false;
        match (result, &mut self.settings) {
            (Ok(_), Ok(settings)) => settings.set(setting, on),
            (Ok(_), Err(_)) => {}
            (Err(e), _) => self.failed = Some(e.to_string()),
        }
        Outcome::Stay
    }
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
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let last = self.choices.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => return Outcome::Close,
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('k') if ctrl => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(last),
            KeyCode::Char('j') if ctrl => self.selected = (self.selected + 1).min(last),
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
}

impl SnoozeDialog {
    pub fn new(session: String, title: String, presets: Vec<(&'static str, Until)>) -> SnoozeDialog {
        SnoozeDialog { session, title, typed: Input::default(), presets, selected: 0, error: None }
    }

    fn on_key(&mut self, key: KeyEvent, cx: &Context) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let last = self.presets.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => return Outcome::Close,
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('k') if ctrl => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(last),
            KeyCode::Char('j') if ctrl => self.selected = (self.selected + 1).min(last),
            KeyCode::Enter => {
                let until = match self.typed.value().trim() {
                    "" => match self.presets.get(self.selected) {
                        Some(&(_, until)) => Ok(until),
                        None => return Outcome::Stay, // nothing offered and nothing typed
                    },
                    typed => Until::parse(typed, cx.now, cx.tz),
                };
                match until {
                    Ok(until) => {
                        let job = Job::Snooze { session: self.session.clone(), title: self.title.clone(), until };
                        return Outcome::CloseAnd(Action::Run(job));
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
}
