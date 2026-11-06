//! The add dialog (R8.3, R8.5, R8.8, R10.5): a git repository (cloned by a
//! child `itm project add git`, which shows git's progress and stops on
//! Esc, keeping nothing), a local folder (completed as it is typed, R1.6),
//! or a name only. What is typed is checked once typing pauses: where the
//! project would go, and whether it is saved already or its name taken; a
//! problem with the name puts the cursor on the name. Enter adds it, and
//! adding checks again.

use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use tui_input::Input;

use super::app::{Action, Job, Reply};
use super::completion::Folders;
use super::draw::{
    ACCENT, FieldFocus, Tone, WIDE, bold, dialog_box, field_line, muted, wrapped_key_hints, wrapped_rows,
};
use super::screen::{Context, Field, Need, Outcome, selects, type_into};
use crate::error::Error;
use crate::paths::Paths;
use crate::projects::Check;
use crate::store::ProjectKind;

/// Typing pauses this long before what was typed is checked (the check
/// reads the store and may ask git how it rewrites the URL).
const CHECK_AFTER: Duration = Duration::from_millis(250);

/// The kinds in the order of the dialog's tabs.
pub const KINDS: [ProjectKind; 3] = [ProjectKind::Git, ProjectKind::Dir, ProjectKind::Name];

/// What a check was about.
#[derive(Debug, Clone, PartialEq)]
pub struct Typed {
    pub kind: ProjectKind,
    pub source: String,
    pub name: Option<String>,
}

/// Work the dialog waits for.
#[derive(Debug, Clone, PartialEq)]
pub enum Busy {
    /// A folder or a name being saved.
    Saving,
    /// A clone: what git said last, and whether Esc asked it to stop.
    Cloning { last: Option<String>, stopping: bool },
}

/// How the last try ended, shown until a field changes.
#[derive(Debug, Clone, PartialEq)]
pub enum Said {
    Failed(String),
    /// Esc stopped the clone.
    Stopped,
}

#[derive(Debug, Clone)]
pub struct AddDialog {
    pub kind: ProjectKind,
    pub source: Input,
    pub name: Input,
    pub focus: Field,
    /// The focused field's text is selected: what is typed replaces it.
    pub selected: bool,
    pub folders: Folders,
    /// The last check of what was typed: what it checked, and what it found
    /// (Err: the source is no folder or git URL).
    pub check: Option<(Typed, Result<Check, String>)>,
    check_wanted: bool,
    /// Not before then: typing has to pause first.
    check_after: Option<Instant>,
    pub busy: Option<Busy>,
    pub said: Option<Said>,
}

impl AddDialog {
    pub fn new(kind: ProjectKind, source: &str, name: &str, focus: Field) -> AddDialog {
        let mut dialog = AddDialog {
            kind,
            source: Input::new(source.into()),
            name: Input::new(name.into()),
            focus: if kind == ProjectKind::Name { Field::Name } else { focus },
            selected: false,
            folders: Folders::default(),
            check: None,
            check_wanted: true,
            check_after: None,
            busy: None,
            said: None,
        };
        dialog.selected = dialog.select_focused();
        dialog
    }

    /// Focus `field`. Moving there selects its text, as in the Python itm;
    /// focus already there leaves what is being typed as it is.
    fn focus_on(&mut self, field: Field) {
        if self.focus != field {
            self.focus = field;
            self.selected = self.select_focused();
        }
    }

    fn select_focused(&mut self) -> bool {
        selects(if self.focus == Field::Source { &mut self.source } else { &mut self.name })
    }

    /// Filled with a workspace's folder and name (R10.5), ready on the name.
    pub fn folder(folder: &str, name: &str) -> AddDialog {
        AddDialog::new(ProjectKind::Dir, folder, name, Field::Name)
    }

    pub fn typed(&self) -> Typed {
        let name = Some(self.name.value().trim().to_string()).filter(|n| !n.is_empty());
        match self.kind {
            // A name only is named by what is typed as its name.
            ProjectKind::Name => Typed { kind: self.kind, source: name.unwrap_or_default(), name: None },
            kind => Typed { kind, source: self.source.value().trim().into(), name },
        }
    }

    /// Ghost text completing a folder being typed (a folder project only).
    pub fn ghost(&self) -> Option<String> {
        (self.kind == ProjectKind::Dir).then(|| self.folders.ghost(self.source.value())).flatten()
    }

    /// Ctrl+←/→ cycle the kinds; plain ←/→ (`wrap` false) stop at the ends.
    /// Never while a clone or a save runs.
    fn step_kind(&mut self, by: isize, wrap: bool) {
        let at = KINDS.iter().position(|k| *k == self.kind).unwrap_or(0) as isize + by;
        if self.busy.is_some() || (!wrap && !(0..KINDS.len() as isize).contains(&at)) {
            return;
        }
        self.kind = KINDS[at.rem_euclid(KINDS.len() as isize) as usize];
        if self.kind == ProjectKind::Name {
            self.focus_on(Field::Name); // a name only has no source
        }
        (self.check_wanted, self.check_after, self.said) = (true, None, None);
    }

    pub fn on_key(&mut self, key: KeyEvent, cx: &Context) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let in_source = self.focus == Field::Source;
        match key.code {
            KeyCode::Esc => {
                return match &mut self.busy {
                    // Stopped with SIGTERM: the child removes what it copied; the dialog says when it is gone.
                    Some(Busy::Cloning { stopping, .. }) if !*stopping => {
                        *stopping = true;
                        Outcome::Act(Action::StopClone)
                    }
                    Some(_) => Outcome::Stay,
                    None => Outcome::Close,
                };
            }
            KeyCode::Left if ctrl => self.step_kind(-1, true),
            KeyCode::Right if ctrl => self.step_kind(1, true),
            // In an empty source ←/→ change the kind; in a field with text they move the cursor.
            KeyCode::Left if in_source && self.source.value().is_empty() => self.step_kind(-1, false),
            KeyCode::Right if in_source && self.source.value().is_empty() => self.step_kind(1, false),
            // Tab, or → at the end, takes the folder suggested.
            KeyCode::Tab | KeyCode::Right
                if in_source
                    && (key.code == KeyCode::Tab || self.source.cursor() == self.source.value().chars().count())
                    && self.ghost().is_some() =>
            {
                (self.source, self.selected) = (Input::new(self.ghost().expect("checked")), false);
                self.changed(cx);
            }
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                if self.kind != ProjectKind::Name {
                    self.focus_on(if in_source { Field::Name } else { Field::Source });
                }
            }
            KeyCode::Enter => return self.submit(),
            _ if self.busy.is_some() => {} // what is being added stays as it is
            _ => {
                let field = if in_source { &mut self.source } else { &mut self.name };
                if type_into(field, &mut self.selected, key) {
                    self.changed(cx);
                }
            }
        }
        Outcome::Stay
    }

    /// A field changed: what the last try said goes, and a check follows once typing pauses.
    fn changed(&mut self, cx: &Context) {
        (self.said, self.check_wanted, self.check_after) = (None, true, Some(cx.now.instant + CHECK_AFTER));
    }

    fn submit(&mut self) -> Outcome {
        if self.busy.is_some() {
            return Outcome::Stay; // also before the clone's process exists: a second Enter there
        }
        let Typed { kind, source, name } = self.typed();
        if source.is_empty() {
            return Outcome::Stay;
        }
        self.said = None;
        let job = match kind {
            ProjectKind::Git => {
                self.busy = Some(Busy::Cloning { last: None, stopping: false });
                Job::Clone { url: source, name }
            }
            kind => {
                self.busy = Some(Busy::Saving);
                Job::AddProject { kind, source, name }
            }
        };
        Outcome::Act(Action::Run(job))
    }

    /// A folder to list for completion, else a check once typing paused.
    pub fn needs(&mut self, now: Instant) -> Option<Need> {
        if self.kind == ProjectKind::Dir
            && let Some(parent) = self.folders.wanted(self.source.value())
        {
            return Some(Need::Listing(parent));
        }
        if !self.check_wanted || self.check_after.is_some_and(|after| now < after) || self.busy.is_some() {
            return None;
        }
        self.check_wanted = false;
        let typed = self.typed();
        if typed.source.is_empty() {
            self.check = None; // nothing typed: nothing to say
            return None;
        }
        let Typed { kind, source, name } = typed;
        Some(Need::Job(Job::CheckProject { kind, source, name }))
    }

    pub fn due(&self) -> Option<Instant> {
        self.check_after.filter(|_| self.check_wanted)
    }

    /// What git said last during the clone.
    pub fn cloning(&mut self, line: String) {
        if let Some(Busy::Cloning { last, stopping: false }) = &mut self.busy {
            *last = Some(line);
        }
    }

    pub fn done(&mut self, job: &Job, result: Result<Reply, Error>) -> Outcome {
        if let Job::CheckProject { kind, source, name } = job {
            let typed = Typed { kind: *kind, source: source.clone(), name: name.clone() };
            if typed == self.typed() {
                let found = match result {
                    Ok(Reply::Checked(check)) => Ok(check),
                    Ok(_) => return Outcome::Stay,
                    Err(e) => Err(e.to_string()),
                };
                self.check = Some((typed, found));
            }
            return Outcome::Stay; // a check of what was typed before: a newer one follows
        }
        let stopping = matches!(self.busy, Some(Busy::Cloning { stopping: true, .. }));
        self.busy = None;
        match result {
            // Also when Esc came too late to stop the clone: it is there.
            Ok(Reply::Added(id, project)) => Outcome::CloseAnd(Action::ProjectAdded(id, project)),
            Ok(_) => Outcome::Stay,
            Err(_) if stopping => {
                self.said = Some(Said::Stopped);
                Outcome::Stay
            }
            Err(e) => {
                if e.is_about_the_name() {
                    self.focus_on(Field::Name); // another name will do (R8.5)
                }
                self.said = Some(Said::Failed(e.to_string()));
                Outcome::Stay
            }
        }
    }

    /// The check that is about what is typed now, or the last one while a newer is on its way.
    pub fn shown_check(&self) -> Option<&Result<Check, String>> {
        self.check.as_ref().map(|(_, found)| found)
    }
}

/// The labels' column.
const LABEL: usize = 8;

impl AddDialog {
    /// What the dialog says under the fields: the work it waits for, how the
    /// last try ended, else what adding would do or why it cannot.
    pub fn status(&self, paths: &Paths) -> Vec<(String, Tone)> {
        let muted = |text: &str| vec![(text.to_string(), Tone::Muted)];
        match (&self.busy, &self.said, self.shown_check()) {
            (Some(Busy::Cloning { stopping: true, .. }), _, _) => vec![("Stopping the clone…".into(), Tone::Warning)],
            (Some(Busy::Cloning { last: Some(last), .. }), _, _) => muted(&format!("Cloning… {last}")),
            (Some(Busy::Cloning { last: None, .. }), _, _) => muted("Starting the clone…"),
            (Some(Busy::Saving), _, _) => muted("Saving…"),
            (None, Some(Said::Stopped), _) => vec![("Stopped. Nothing of the clone was kept.".into(), Tone::Warning)],
            // The first line is what went wrong; what follows (a hint) is how to go on.
            (None, Some(Said::Failed(e)), _) => (e.lines().enumerate())
                .map(|(i, line)| (line.to_string(), if i == 0 { Tone::Error } else { Tone::Muted }))
                .collect(),
            (None, None, None) => vec![],
            (None, None, Some(Err(e))) => vec![(e.clone(), Tone::Error)],
            (None, None, Some(Ok(Check { problem: Some(problem), .. }))) => vec![(problem.to_string(), Tone::Error)],
            (None, None, Some(Ok(check))) => match self.kind {
                ProjectKind::Name => muted(
                    "Saves the name; the project has no folder, so it gets no workspace. Give it one later with prefix+shift+a in its workspace, or `itm project set`.",
                ),
                ProjectKind::Git => vec![
                    (
                        format!("Clones to {}", check.folder.as_deref().map(|f| paths.display(f)).unwrap_or_default()),
                        Tone::Muted,
                    ),
                    ("Esc stops the clone and removes what it copied.".into(), Tone::Muted),
                ],
                ProjectKind::Dir => muted("Saves the folder; nothing is copied."),
            },
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, paths: &Paths) {
        let status = self.status(paths);
        let fields: u16 = if self.kind == ProjectKind::Name { 1 } else { 2 };
        let width = area.width.min(WIDE).saturating_sub(4) as usize; // inside the border and margin
        let rows: usize = status.iter().map(|(text, _)| wrapped_rows(&Line::from(text.as_str()), width)).sum();
        let keys = [("enter", "add"), ("tab", "complete"), ("←→", "kind (empty source)"), ("esc", "back")];
        let keys = wrapped_key_hints(&keys, width as u16);
        let inside = dialog_box(frame, area, WIDE, 3 + fields + 1 + rows as u16 + 1 + keys.len() as u16 + 2);
        let mut tabs = vec![];
        for kind in KINDS {
            let label = match kind {
                ProjectKind::Git => " Git repository ",
                ProjectKind::Dir => " Local folder ",
                ProjectKind::Name => " Name only ",
            };
            tabs.push(match kind == self.kind {
                true => Span::styled(label, Style::new().fg(ACCENT).bold().reversed()),
                false => muted(label),
            });
            tabs.push(Span::raw(" "));
        }
        let mut lines = vec![Line::from(bold("Add project")), Line::from(tabs), Line::default()];
        let room = (inside.width as usize).saturating_sub(LABEL);
        let mut source_cursor = 0;
        if self.kind != ProjectKind::Name {
            let placeholder = match self.kind {
                ProjectKind::Git => "git@host:group/repo.git or https://host/group/repo",
                _ => "~/code/project",
            };
            let ghost = self.ghost();
            let focus = FieldFocus::of(self.focus == Field::Source, self.selected);
            let (line, cursor) = field_line("Source", LABEL, &self.source, placeholder, ghost.as_deref(), focus, room);
            source_cursor = cursor;
            lines.push(line);
        }
        let default = self.shown_check().and_then(|c| c.as_ref().ok()).map(|c| c.default_name.as_str());
        let placeholder = match (self.kind, default) {
            (ProjectKind::Name, _) => "the project's name",
            (_, Some(default)) if !default.is_empty() => default,
            _ => "defaults to the repository or folder name",
        };
        let focus = FieldFocus::of(self.focus == Field::Name, self.selected);
        let (line, name_cursor) = field_line("Name", LABEL, &self.name, placeholder, None, focus, room);
        lines.push(line);
        lines.push(Line::default());
        lines.extend(status.into_iter().map(|(text, tone)| Line::from(tone.span(text))));
        lines.push(Line::default());
        lines.extend(keys);
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inside);
        let (row, column) = match (self.kind, self.focus) {
            (ProjectKind::Name, _) => (3, name_cursor),
            (_, Field::Source) => (3, source_cursor),
            (_, Field::Name) => (4, name_cursor),
        };
        frame.set_cursor_position(((inside.x + column).min(inside.right().saturating_sub(1)), inside.y + row));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;
    use crate::tui::app::Moment;
    use jiff::tz::TimeZone;
    use ratatui::crossterm::event::KeyCode;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(text: &str) -> Vec<KeyEvent> {
        text.chars().map(|c| key(KeyCode::Char(c))).collect()
    }

    fn press(dialog: &mut AddDialog, keys: &[KeyEvent], at: Instant) -> Outcome {
        let paths = Paths::under("/r".as_ref());
        let cx = Context { tz: &TimeZone::UTC, now: Moment { instant: at, epoch: 0 }, paths: &paths };
        let mut last = Outcome::Stay;
        for key in keys {
            last = dialog.on_key(*key, &cx);
        }
        last
    }

    /// After Python's test_kind_keys and test_the_name_only_tab: plain
    /// arrows stop at the ends and only in an empty source; Ctrl cycles;
    /// nothing changes the kind while a clone runs.
    #[test]
    fn r8_3_the_kind_keys() {
        let now = Instant::now();
        let mut dialog = AddDialog::new(ProjectKind::Git, "", "", Field::Source);
        press(&mut dialog, &[key(KeyCode::Left)], now);
        assert_eq!(dialog.kind, ProjectKind::Git, "no jump from git to Name only");
        press(&mut dialog, &[key(KeyCode::Right), key(KeyCode::Right)], now);
        assert_eq!((dialog.kind, dialog.focus), (ProjectKind::Name, Field::Name));
        let ctrl = |code| KeyEvent::new(code, KeyModifiers::CONTROL);
        press(&mut dialog, &[ctrl(KeyCode::Right)], now);
        assert_eq!(dialog.kind, ProjectKind::Git);
        dialog.busy = Some(Busy::Cloning { last: None, stopping: false });
        press(&mut dialog, &[ctrl(KeyCode::Left)], now);
        assert_eq!(dialog.kind, ProjectKind::Git, "a clone being started: the kind stays");

        let mut named = AddDialog::new(ProjectKind::Name, "", "Client X", Field::Name);
        press(&mut named, &[key(KeyCode::Left), key(KeyCode::Right), key(KeyCode::Right)], now);
        // Left on the selected name goes to its start (as in the Python itm), then the cursor moves.
        assert_eq!((named.kind, named.name.cursor()), (ProjectKind::Name, 2), "the cursor moves in the name");
    }

    /// R8.5: what was typed is checked once typing pauses, and only the
    /// check of what is typed now is kept.
    #[test]
    fn r8_5_typing_is_checked_once_it_pauses() {
        let start = Instant::now();
        let mut dialog = AddDialog::new(ProjectKind::Dir, "", "", Field::Source);
        assert_eq!(dialog.needs(start), None, "nothing typed: nothing to check");
        press(&mut dialog, &[key(KeyCode::Char('~'))], start);
        assert_eq!(dialog.needs(start), None, "not before typing pauses; ~ completes to ~/ without a listing");
        assert_eq!(dialog.ghost().as_deref(), Some("~/"));
        assert_eq!(dialog.due(), Some(start + CHECK_AFTER));
        let check = Job::CheckProject { kind: ProjectKind::Dir, source: "~".into(), name: None };
        assert_eq!(dialog.needs(start + CHECK_AFTER), Some(Need::Job(check.clone())));
        assert_eq!(dialog.needs(start + CHECK_AFTER), None, "asked once");
        press(&mut dialog, &[key(KeyCode::Char('/'))], start);
        assert_eq!(dialog.needs(start), Some(Need::Listing("~/".into())));
        let found = Check { folder: Some("/r/home".into()), default_name: "home".into(), problem: None };
        dialog.done(&check, Ok(Reply::Checked(found)));
        assert_eq!(dialog.check, None, "a check of what was typed before is dropped");
    }

    /// R8.5: focus that does not move leaves the name being typed as it is
    /// (Textual's focus on the focused input does nothing): a name taken,
    /// said after Enter on the name, and a switch to a name only while
    /// typing it keep it unselected. Moving to the name selects it.
    #[test]
    fn r8_5_focus_that_stays_keeps_what_is_typed() {
        let now = Instant::now();
        let mut dialog = AddDialog::new(ProjectKind::Dir, "~/code/api", "", Field::Name);
        press(&mut dialog, &typed("api"), now);
        let Outcome::Act(Action::Run(add)) = press(&mut dialog, &[key(KeyCode::Enter)], now) else { panic!() };
        dialog.done(&add, Err(Error::ProjectNameTaken { name: "api".into(), place: "~/code/old".into() }));
        press(&mut dialog, &typed("2"), now);
        assert_eq!(dialog.name.value(), "api2");

        let mut dialog = AddDialog::new(ProjectKind::Git, "", "", Field::Name);
        press(&mut dialog, &typed("ap"), now);
        press(&mut dialog, &[KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL)], now);
        press(&mut dialog, &typed("i"), now);
        assert_eq!((dialog.kind, dialog.name.value()), (ProjectKind::Name, "api"));

        let mut dialog = AddDialog::new(ProjectKind::Dir, "~/code/api", "api", Field::Source);
        press(&mut dialog, &[key(KeyCode::Tab)], now);
        press(&mut dialog, &typed("web"), now);
        assert_eq!(dialog.name.value(), "web", "Tab to the name selected it");
    }

    /// R8.8: Esc during a clone stops it once and keeps the dialog; when it
    /// ended, the dialog says it kept nothing, and Esc closes it.
    #[test]
    fn r8_8_esc_stops_a_clone_and_keeps_the_dialog() {
        let now = Instant::now();
        let mut dialog = AddDialog::new(ProjectKind::Git, "https://example.com/acme/slow", "", Field::Name);
        let clone = Job::Clone { url: "https://example.com/acme/slow".into(), name: None };
        assert!(
            matches!(press(&mut dialog, &[key(KeyCode::Enter)], now), Outcome::Act(Action::Run(job)) if job == clone)
        );
        assert!(matches!(press(&mut dialog, &[key(KeyCode::Enter)], now), Outcome::Stay), "one clone at a time");
        dialog.cloning("Receiving objects:  50%".into());
        assert_eq!(dialog.busy, Some(Busy::Cloning { last: Some("Receiving objects:  50%".into()), stopping: false }));
        assert!(matches!(press(&mut dialog, &[key(KeyCode::Esc)], now), Outcome::Act(Action::StopClone)));
        assert!(matches!(press(&mut dialog, &[key(KeyCode::Esc)], now), Outcome::Stay), "stopping already");
        dialog.done(&clone, Err(Error::Exit(143)));
        assert_eq!((dialog.busy.clone(), dialog.said.clone()), (None, Some(Said::Stopped)));
        assert!(matches!(press(&mut dialog, &[key(KeyCode::Esc)], now), Outcome::Close));
    }
}
