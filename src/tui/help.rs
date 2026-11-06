//! Every key (`?` while the search is empty, or F1): the footer shows at
//! most seven (UX principle 5), the rest are here. It scrolls when the
//! popup is shorter than the list; the close hint stays visible.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::draw::{ACCENT, WIDE, bold, dialog_box, key_hints};
use super::screen::{Outcome, moves};

/// Sections of (keys, what they do).
pub const HELP: [(&str, &[(&str, &str)]); 5] = [
    (
        "Task list",
        &[
            ("typing", "search titles; @name or @\"a name\" shows one project's tasks"),
            ("↑ ↓ / ctrl+k ctrl+j", "move"),
            ("enter", "open · wake & open · reopen · create"),
            ("tab / shift+tab", "next / previous view"),
            ("alt+1…9", "open row 1–9"),
            ("ctrl+n", "new task form"),
            ("ctrl+e", "edit title, description, projects"),
            ("ctrl+z", "snooze"),
            ("ctrl+u", "wake a snoozed task"),
            ("ctrl+a", "archive (asks when running)"),
            ("ctrl+d", "delete"),
            ("ctrl+o", "the task's notes, in your editor"),
            ("ctrl+p", "projects"),
            ("ctrl+s", "settings (Herdr integration)"),
            ("? / F1", "this help (? while the search is empty)"),
            ("esc", "close"),
        ],
    ),
    (
        "Task form",
        &[
            ("enter", "create (in Title)"),
            ("tab", "next field"),
            ("ctrl+s", "save from any field"),
            ("enter / typing", "on Projects: open the project finder"),
            ("backspace", "on Projects: remove the last one"),
            ("← →", "on Snooze: the next time; typing there types one"),
            ("esc", "cancel; asks when you typed something"),
        ],
    ),
    (
        "Project finder",
        &[
            ("typing", "find a saved project (letters in order, any case)"),
            ("enter", "add it; on a ✓ project, remove it"),
            ("~/… or /…", "list folders; enter adds one as a project"),
            ("tab", "go into the highlighted folder"),
            ("a git URL", "clone it, or pick it when saved"),
            ("+ New project", "a project that is only a name (last row; opens the add dialog)"),
            ("↑ ↓ / ctrl+k ctrl+j", "move"),
            ("esc", "back, projects unchanged"),
        ],
    ),
    (
        "Add project",
        &[
            ("tab / →", "complete a folder path"),
            ("ctrl+← ctrl+→", "git / folder / name only"),
            ("← →", "the same, in an empty source"),
            ("enter", "add (clone)"),
            ("esc", "back; stops a clone"),
        ],
    ),
    (
        "Projects",
        &[
            ("enter", "show its tasks"),
            ("↑ ↓ / ctrl+k ctrl+j", "move"),
            ("ctrl+n", "add"),
            ("ctrl+r", "rename"),
            ("ctrl+d", "remove"),
            ("esc", "back"),
        ],
    ),
];

/// The number of lines the help has: a title line per section, its keys, and a blank line between sections.
pub fn lines() -> usize {
    HELP.iter().map(|(_, keys)| keys.len() + 2).sum::<usize>() - 1
}

#[derive(Debug, Clone, Default)]
pub struct Help {
    /// The first line shown.
    pub scroll: usize,
}

impl Help {
    pub fn on_key(&mut self, key: KeyEvent) -> Outcome {
        let by = match key.code {
            KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('q' | '?') => return Outcome::Close,
            KeyCode::PageUp => -10,
            KeyCode::PageDown => 10,
            _ => moves(key),
        };
        self.scroll = self.scroll.saturating_add_signed(by).min(lines().saturating_sub(1));
        Outcome::Stay
    }
}

impl Help {
    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![];
        for (i, (section, keys)) in HELP.iter().enumerate() {
            if i > 0 {
                lines.push(Line::default());
            }
            lines.push(Line::from(bold(*section)));
            for (key, what) in keys.iter() {
                let key = Span::styled(format!("  {key:<20}"), Style::new().fg(ACCENT).bold());
                lines.push(Line::from(vec![key, Span::raw(format!(" {what}"))]));
            }
        }
        let inside = dialog_box(frame, area, WIDE, lines.len() as u16 + 4);
        let body = Rect { height: inside.height.saturating_sub(2), ..inside };
        // Scrolled no further than the last line at the bottom.
        let scroll = self.scroll.min(lines.len().saturating_sub(body.height as usize));
        frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), body);
        let keys = Rect { y: inside.bottom().saturating_sub(1), height: 1, ..inside };
        frame.render_widget(Paragraph::new(key_hints(&[("esc", "close"), ("↑ ↓", "scroll")])), keys);
    }
}
