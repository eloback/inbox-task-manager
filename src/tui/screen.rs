//! Screens that open over the task list: dialogs now, and later the New
//! task form, Projects, Settings and help. They are a stack: only the top
//! one gets keys, so list keys never act on the task behind a dialog
//! (DESIGN.md section 4, rules for the TUI), and Esc closes only the top.
//! The app pushes them; a screen that opens another (the form's project
//! finder) will say so in its [`Outcome`].
//! A new screen is a new variant; every `match` that must handle it then
//! fails to compile until it does.

use jiff::tz::TimeZone;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use super::app::{Action, Job};
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
}

/// What a key did to the screen on top.
#[derive(Debug)]
pub enum Outcome {
    Stay,
    Close,
    /// Close, then act.
    CloseAnd(Action),
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
        }
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
