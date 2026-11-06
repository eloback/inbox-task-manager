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
use crate::snooze::Until;

#[derive(Debug, Clone)]
pub enum Screen {
    /// Archiving stops a running session, and Ctrl+A is also a common Herdr
    /// prefix: ask first (R5.1, UX principle 10).
    ConfirmArchive {
        session: String,
        title: String,
    },
    Snooze(SnoozeDialog),
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
            Screen::Snooze(dialog) => dialog.on_key(key, cx),
        }
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
