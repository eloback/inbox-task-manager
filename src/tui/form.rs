//! The New task form (R1.1) and the edit form (R5.4): one form whose first
//! field is enough (UX principle 13). Enter in the title saves with the
//! defaults; Tab reaches the description, the projects and (for a new task)
//! the snooze; Ctrl+S saves from any field; Esc asks first when something
//! was typed. Below the fields it says what saving will do: where the
//! session starts, which projects get no workspace and why, and for an
//! edit what an added project does to the session and which projects keep
//! their open workspace.

use std::collections::BTreeSet;

use jiff::tz::TimeZone;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_textarea::{TextArea, WrapMode};
use tui_input::Input;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::app::{Action, Job, Reply, Snapshot};
use super::draw::{
    FieldFocus, Tone, WIDE, bold, colored, dialog_box, faint, field_line, filled, muted, name_badge, project_line,
    wrapped_key_hints, wrapped_rows,
};
use super::finder::{Finder, Saved};
use super::screen::{Context, Outcome, Screen, selects, type_into};
use super::theme::theme;
use crate::error::Error;
use crate::herdr::Session;
use crate::paths::Paths;
use crate::snooze::Until;
use crate::store::{Project, Task};
use crate::tasks::slugify;

/// A form's fields, in Tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Title,
    Description,
    Projects,
    Snooze,
    /// The time typed for "Other time…".
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Has {
    Folder,
    Missing,
    Nothing,
}

/// A new task's snooze as the form holds it.
#[derive(Debug, Clone, PartialEq)]
enum Chosen {
    /// Read already; None: not snoozed.
    Read(Option<Until>),
    /// A time typed that only GNU `date` reads (`next week`): a worker reads it on save (R4.1).
    ForDate(String),
}

/// A snooze for a new task, which has no agents yet, so none could wake it.
fn new_snooze(until: Option<Until>) -> Result<Option<Until>, String> {
    match until {
        Some(Until::Agent) => Err("a new task has no agents yet, so none could wake it; pick a time".into()),
        until => Ok(until),
    }
}

/// What adding a project does to a task's session (R5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// It never started: it will start with a workspace in each project.
    Starts,
    /// It runs: a workspace opens in each project added.
    Running,
    /// It stopped and keeps its workspaces.
    Stopped,
}

impl Effect {
    pub fn of(session: Option<&Session>) -> Effect {
        match session {
            None => Effect::Starts,
            Some(s) if s.running => Effect::Running,
            Some(_) => Effect::Stopped,
        }
    }
}

/// What the edit form knows about the task it edits.
#[derive(Debug, Clone)]
pub struct Editing {
    pub session: String,
    /// None: Herdr did not answer, so whether the session runs is unknown.
    pub effect: Option<Effect>,
    /// Projects whose workspace is open while projects follow workspaces
    /// and the daemon runs: the edit would keep them, so removing one is refused here.
    pub pinned: BTreeSet<String>,
    /// Why the settings are ignored (a running session's projects then follow no workspace).
    pub config_error: Option<String>,
}

impl Editing {
    /// What the edit form shows of the session from the switcher's last
    /// refresh: whether it runs, and which projects keep their workspace
    /// (R5.4: while projects follow workspaces and the daemon runs).
    pub fn new(session: String, task: &Task, snap: &Snapshot) -> Editing {
        let effect = snap.listed.then(|| Effect::of(snap.sessions.get(&session)));
        let follows = snap.settings.as_ref().is_ok_and(|s| s.projects_from_workspaces) && snap.status.fresh;
        Editing::with(session, task, effect, follows, snap.settings.as_ref().err().cloned())
    }

    /// `follows`: projects follow workspaces and the daemon runs.
    pub fn with(
        session: String,
        task: &Task,
        effect: Option<Effect>,
        follows: bool,
        config_error: Option<String>,
    ) -> Editing {
        let running = effect == Some(Effect::Running);
        let pinned = match running && follows {
            true => task.workspace_projects.iter().cloned().collect(),
            false => BTreeSet::new(),
        };
        Editing { session, effect, pinned, config_error: config_error.filter(|_| running) }
    }
}

#[derive(Debug, Clone)]
pub struct TaskForm {
    /// None for a new task.
    pub editing: Option<Editing>,
    pub title: Input,
    pub description: TextArea<'static>,
    /// Project ids, the first where the session starts.
    pub projects: Vec<String>,
    pub saved: Saved,
    /// Fixed while the form is open, so a choice stays what it said (a time
    /// passed meanwhile is refused when saving). Never "until an agent": a
    /// new task has no agents yet.
    pub presets: Vec<(&'static str, Until)>,
    /// 0: don't snooze; 1 to the number of presets: that preset; one more: a time typed.
    pub snooze: usize,
    pub other: Input,
    pub focus: Part,
    /// The focused field's text is selected: what is typed replaces it.
    pub selected: bool,
    pub error: Option<String>,
    /// A time typed that only GNU `date` reads is being read by a worker
    /// (R4.1): saving waits for it, and keys wait too.
    pub reading: bool,
    /// Why saving cannot switch to the task (a plain Herdr client, R1.4).
    pub cannot_switch: Option<String>,
    /// Where a task without a project folder starts, as shown.
    pub task_dir: String,
    initial: (String, String, Vec<String>),
}

impl TaskForm {
    pub fn new_task(
        title: &str,
        projects: Vec<String>,
        saved: Saved,
        presets: Vec<(&'static str, Until)>,
        paths: &Paths,
    ) -> TaskForm {
        let projects: Vec<String> = projects.into_iter().filter(|id| saved.projects.contains_key(id)).collect();
        let mut form = TaskForm {
            editing: None,
            title: Input::new(title.into()),
            description: description(""),
            initial: (title.into(), String::new(), projects.clone()),
            projects,
            saved,
            presets,
            snooze: 0,
            other: Input::default(),
            focus: Part::Title,
            selected: false,
            error: None,
            reading: false,
            cannot_switch: None,
            task_dir: paths.display(&paths.task_dir),
        };
        form.selected = selects(&mut form.title);
        form
    }

    pub fn edit(task: &Task, saved: Saved, editing: Editing) -> TaskForm {
        let projects: Vec<String> =
            task.projects.iter().filter(|id| saved.projects.contains_key(*id)).cloned().collect();
        let text = task.description.clone().unwrap_or_default();
        let mut form = TaskForm {
            editing: Some(editing),
            title: Input::new(task.title.clone()),
            description: description(&text),
            initial: (task.title.clone(), text, projects.clone()),
            projects,
            saved,
            presets: vec![],
            snooze: 0,
            other: Input::default(),
            focus: Part::Title,
            selected: false,
            error: None,
            reading: false,
            cannot_switch: None,
            task_dir: String::new(),
        };
        form.selected = selects(&mut form.title);
        form
    }

    /// Focus `part`. Moving to a one-line field selects its text, as in the
    /// Python itm; focus already there leaves what is being typed as it is.
    fn focus_on(&mut self, part: Part) {
        if self.focus == part {
            return;
        }
        self.focus = part;
        self.selected = match part {
            Part::Title => selects(&mut self.title),
            Part::Other => selects(&mut self.other),
            _ => false,
        };
    }

    pub fn cannot_switch(self, problem: Option<String>) -> TaskForm {
        TaskForm { cannot_switch: problem, ..self }
    }

    /// A snooze given on the command line, shown as a time typed (never
    /// "until an agent": a new task has no agents yet).
    pub fn until(self, until: Option<Until>, tz: &TimeZone) -> TaskForm {
        let Some(Until::Time(ts)) = until else { return self };
        let Ok(when) = jiff::Timestamp::from_second(ts) else { return self };
        let typed = when.to_zoned(tz.clone()).strftime("%Y-%m-%d %H:%M").to_string();
        TaskForm { snooze: self.presets.len() + 1, other: Input::new(typed), ..self }
    }

    fn other_chosen(&self) -> bool {
        self.editing.is_none() && self.snooze == self.presets.len() + 1
    }

    /// The fields in Tab order: an edit has no snooze, and the time typed shows only for "Other time…".
    pub fn parts(&self) -> Vec<Part> {
        let mut parts = vec![Part::Title, Part::Description, Part::Projects];
        if self.editing.is_none() {
            parts.push(Part::Snooze);
            if self.other_chosen() {
                parts.push(Part::Other);
            }
        }
        parts
    }

    fn move_focus(&mut self, by: isize) {
        let parts = self.parts();
        let at = parts.iter().position(|p| *p == self.focus).unwrap_or(0) as isize;
        self.focus_on(parts[(at + by).rem_euclid(parts.len() as isize) as usize]);
    }

    pub fn description_text(&self) -> String {
        self.description.lines().join("\n")
    }

    /// Whether Esc would lose something: text, or (when editing) the projects changed.
    pub fn typed(&self) -> bool {
        let (title, description, projects) = &self.initial;
        let changed_projects = self.editing.is_some() && self.projects != *projects;
        changed_projects
            || self.title.value().trim() != title.trim()
            || self.description_text().trim() != description.trim()
    }

    pub fn on_key(&mut self, key: KeyEvent, cx: &Context) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if self.reading {
            return Outcome::Stay; // for the few milliseconds `date` takes
        }
        match key.code {
            KeyCode::Esc if self.typed() => return Outcome::Open(Box::new(Screen::ConfirmDiscard)),
            KeyCode::Esc => return Outcome::Close,
            KeyCode::Char('s') if ctrl => return self.save(cx),
            KeyCode::Tab => self.move_focus(1),
            KeyCode::BackTab => self.move_focus(-1),
            _ => match self.focus {
                Part::Title | Part::Other if key.code == KeyCode::Enter => return self.save(cx),
                Part::Title => drop(type_into(&mut self.title, &mut self.selected, key)),
                Part::Other => drop(type_into(&mut self.other, &mut self.selected, key)),
                Part::Description => drop(self.description.input(key)),
                Part::Projects => match key.code {
                    KeyCode::Enter | KeyCode::Char(' ') => return self.find("", cx),
                    KeyCode::Backspace => drop(self.projects.pop()),
                    KeyCode::Char(c) if !ctrl && !alt => return self.find(&c.to_string(), cx),
                    _ => {}
                },
                Part::Snooze => match key.code {
                    KeyCode::Enter => return self.save(cx),
                    KeyCode::Left | KeyCode::Up => self.snooze = self.snooze.saturating_sub(1),
                    KeyCode::Right | KeyCode::Down => self.snooze = (self.snooze + 1).min(self.presets.len() + 1),
                    // Typing on the snooze is typing a time.
                    KeyCode::Char(c) if !ctrl && !alt => {
                        self.snooze = self.presets.len() + 1;
                        self.focus_on(Part::Other);
                        let typed = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
                        type_into(&mut self.other, &mut self.selected, typed);
                    }
                    _ => {}
                },
            },
        }
        Outcome::Stay
    }

    /// Text pasted into the description, newlines kept; false elsewhere (it is typed then).
    pub fn paste(&mut self, text: &str) -> bool {
        let here = self.focus == Part::Description;
        if here {
            self.description.insert_str(text);
        }
        here
    }

    /// The project finder over the form, its search started with what was typed on the field.
    fn find(&self, typed: &str, cx: &Context) -> Outcome {
        let finder = Finder::new(self.projects.clone(), typed, self.saved.clone(), cx.paths);
        Outcome::Open(Box::new(Screen::Finder(Box::new(finder))))
    }

    pub fn returned(&mut self, action: Action) -> Outcome {
        match action {
            Action::Picked(projects, added) => {
                if let Some((id, project)) = added {
                    self.saved.add(id, project);
                }
                self.projects = projects;
                Outcome::Stay
            }
            Action::Answer(true) => Outcome::Close, // discard what was typed
            action => Outcome::Act(action),
        }
    }

    /// The snooze chosen; Err for a time typed that does not read.
    fn snooze_value(&self, now: i64, tz: &TimeZone) -> Result<Chosen, String> {
        match self.snooze {
            0 => Ok(Chosen::Read(None)),
            n if n <= self.presets.len() => Ok(Chosen::Read(Some(self.presets[n - 1].1))),
            _ => match self.other.value().trim() {
                "" => Ok(Chosen::Read(None)),
                // Refused here, while what was typed is still on screen.
                text => match Until::read(text, now, tz) {
                    Ok(None) => Ok(Chosen::ForDate(text.into())),
                    Ok(until) => new_snooze(until).map(Chosen::Read),
                    Err(e) => Err(e.to_string()),
                },
            },
        }
    }

    /// The time typed was read by a worker (R4.1): save with it, or say why not.
    pub fn read(&mut self, result: Result<Reply, Error>) -> Outcome {
        self.reading = false;
        match result.map_err(|e| e.to_string()).and_then(|reply| match reply {
            Reply::Until(until) => new_snooze(Some(until)),
            _ => Err("the time was not read".into()),
        }) {
            Ok(until) => self.create(until),
            Err(e) => {
                // Back on the time to fix it, the cursor where it was: typing goes on.
                (self.error, self.focus, self.selected) = (Some(e), Part::Other, false);
                Outcome::Stay
            }
        }
    }

    /// Names of removed projects whose workspace is open: the edit would keep them.
    fn blocked_removals(&self) -> Vec<String> {
        let Some(editing) = &self.editing else { return vec![] };
        (self.initial.2.iter())
            .filter(|id| !self.projects.contains(id) && editing.pinned.contains(*id))
            .filter_map(|id| self.saved.projects.get(id).map(|p| p.name.clone()))
            .collect()
    }

    fn save(&mut self, cx: &Context) -> Outcome {
        let title = self.title.value().trim().to_string();
        if title.is_empty() {
            self.error = Some("The task needs a title.".into());
            self.focus_on(Part::Title);
            return Outcome::Stay;
        }
        let description = self.description_text().trim().to_string();
        let job = match &self.editing {
            Some(editing) => {
                let blocked = self.blocked_removals();
                if !blocked.is_empty() {
                    self.error = Some(format!(
                        "{} still has a workspace in this task; close it in Herdr first, or turn off “projects follow workspaces” in the settings (ctrl+s in the switcher).",
                        blocked.join(", ")
                    ));
                    return Outcome::Stay;
                }
                let initial = &self.initial.2;
                let changed = self.projects != *initial;
                Job::Edit {
                    session: editing.session.clone(),
                    title,
                    description,
                    add: self.projects.iter().filter(|id| !initial.contains(id)).cloned().collect(),
                    remove: initial.iter().filter(|id| !self.projects.contains(id)).cloned().collect(),
                    order: if changed { self.projects.clone() } else { vec![] },
                }
            }
            None => match self.snooze_value(cx.now.epoch, cx.tz) {
                Err(e) => {
                    self.error = Some(e);
                    return Outcome::Stay;
                }
                Ok(Chosen::ForDate(text)) => {
                    (self.reading, self.error) = (true, None);
                    return Outcome::Act(Action::Run(Job::SnoozeTime { text }));
                }
                Ok(Chosen::Read(until)) => return self.create(until),
            },
        };
        Outcome::CloseAnd(Action::Run(job))
    }

    /// Save the new task, snoozed `until` (its title was checked).
    fn create(&self, until: Option<Until>) -> Outcome {
        let description = self.description_text().trim().to_string();
        Outcome::CloseAnd(Action::Run(Job::Create {
            title: self.title.value().trim().to_string(),
            description: Some(description).filter(|d| !d.is_empty()),
            projects: self.projects.clone(),
            until,
        }))
    }

    fn project(&self, id: &str) -> Option<&Project> {
        self.saved.projects.get(id)
    }

    /// Whether a project gets a workspace: a folder that is there, one that is missing, or none.
    fn has(&self, id: &str) -> Has {
        match self.project(id).and_then(Project::folder) {
            None => Has::Nothing,
            Some(_) if self.saved.missing.contains(id) => Has::Missing,
            Some(_) => Has::Folder,
        }
    }

    /// What saving will do, a line each: (label, text, tone).
    pub fn effects(&self, paths: &Paths, now: i64, tz: &TimeZone) -> Vec<(&'static str, String, Tone)> {
        let those = |ids: &[String], has: Has| -> Vec<&Project> {
            ids.iter().filter(|id| self.has(id) == has).filter_map(|id| self.project(id)).collect()
        };
        let names =
            |projects: &[&Project], by: &str| projects.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(by);
        let folders = |projects: &[&Project]| {
            projects.iter().filter_map(|p| p.folder()).map(|f| paths.display(f)).collect::<Vec<_>>().join(", ")
        };
        let (with_folder, missing, without) = (Has::Folder, Has::Missing, Has::Nothing);
        let Some(editing) = &self.editing else {
            // As the session will start: in the first project whose folder is there, a workspace for each.
            let present = those(&self.projects, with_folder);
            let starts =
                present.first().and_then(|p| p.folder()).map_or_else(|| self.task_dir.clone(), |f| paths.display(f));
            let mut lines = vec![("Starts in", starts, Tone::Muted)];
            if present.len() > 1 {
                lines.push(("Workspaces", names(&present, " · "), Tone::Muted));
            }
            let gone = those(&self.projects, missing);
            if !gone.is_empty() {
                lines.push(("", format!("Missing, so no workspace: {}", names(&gone, ", ")), Tone::Error));
            }
            let folderless = those(&self.projects, without);
            if !folderless.is_empty() {
                lines.push(("", format!("No folder, so no workspace: {}", names(&folderless, ", ")), Tone::Muted));
            }
            let title = self.title.value();
            let slug = if title.trim().is_empty() { "…".into() } else { slugify(title) };
            lines.push(("Session  ", format!("task-{slug}"), Tone::Muted));
            let then = match (self.snooze_value(now, tz), &self.cannot_switch) {
                (Ok(Chosen::Read(Some(_)) | Chosen::ForDate(_)) | Err(_), _) => {
                    "Saved snoozed; the session starts when you open it".into()
                }
                (_, Some(problem)) => format!("Saved without switching: {problem}"),
                _ => "Then switches you to it".into(),
            };
            lines.push(("", then, Tone::Muted));
            return lines;
        };
        let mut lines = vec![("Session  ", editing.session.clone(), Tone::Muted)];
        let added: Vec<String> = self.projects.iter().filter(|id| !self.initial.2.contains(id)).cloned().collect();
        let folderless = those(&added, without);
        if !folderless.is_empty() {
            lines.push(("", format!("{}: no folder, so no workspace", names(&folderless, ", ")), Tone::Muted));
        }
        let gone = those(&added, missing);
        if !gone.is_empty() {
            lines.push(("", format!("{} missing, so no workspace opens there", folders(&gone)), Tone::Error));
        }
        let present = those(&added, with_folder);
        if !present.is_empty() {
            let places = folders(&present);
            let text = match editing.effect {
                Some(Effect::Starts) => format!("Its session will start with a workspace in {places}"),
                Some(Effect::Running) => format!("A workspace opens in {places}, in its running session"),
                Some(Effect::Stopped) => {
                    format!("Its stopped session keeps its workspaces; open one in {places} when you need it")
                }
                None => "Herdr did not answer, so it is unknown whether its session runs".into(),
            };
            lines.push(("", text, Tone::Muted));
        }
        let blocked = self.blocked_removals();
        if !blocked.is_empty() {
            let text = format!("{} still has a workspace in this task; close it in Herdr first", blocked.join(", "));
            lines.push(("", text, Tone::Error));
        }
        if let Some(e) = &editing.config_error {
            lines.push(("", format!("Settings ignored, projects do not follow workspaces: {e}"), Tone::Warning));
        }
        lines
    }
}

/// The label column.
const LABEL: usize = 13;

impl TaskForm {
    /// The form in a wide dialog: the fields, what saving does, why it did not, the keys.
    pub fn draw(&self, frame: &mut Frame, area: Rect, paths: &Paths, now: i64, tz: &TimeZone) {
        let effects = self.effects(paths, now, tz);
        let keys: &[(&str, &str)] = match self.editing {
            Some(_) => &[("^s", "save"), ("esc", "cancel")],
            None => &[("enter", "create"), ("tab", "next field"), ("^s", "create from any field"), ("esc", "cancel")],
        };
        // The fields have a row each (the description three); what follows wraps.
        let fields = 2 + self.parts().len() as u16 + 2;
        let width = (area.width.min(WIDE).saturating_sub(4)) as usize; // inside the border and margin
        let mut tail = vec![Line::default()];
        for (label, text, tone) in effects {
            tail.push(Line::from(match label {
                "" => vec![tone.span(text)],
                label => vec![faint(format!("{label} ")), muted(text)],
            }));
        }
        if let Some(e) = &self.error {
            tail.push(Line::from(Tone::Error.span(e.clone())));
        } else if self.reading {
            tail.push(Line::from(muted("Reading the time…")));
        }
        tail.push(Line::default());
        tail.extend(wrapped_key_hints(keys, width as u16));
        let rows: usize = tail.iter().map(|line| wrapped_rows(line, width)).sum();
        let inside = dialog_box(frame, area, WIDE, fields + rows as u16 + 2);
        let room = (inside.width as usize).saturating_sub(LABEL);
        let mut lines =
            vec![Line::from(bold(if self.editing.is_some() { "Edit task" } else { "New task" })), Line::default()];
        let label = |text: &str, part: Part| {
            let style = if self.focus == part { theme().accent.bold() } else { theme().sub };
            Span::styled(format!("{text:<LABEL$}"), style)
        };
        let focused = |part| FieldFocus::of(self.focus == part, self.selected);
        let (title, title_cursor) =
            field_line("Title", LABEL, &self.title, "What is this task about?", None, focused(Part::Title), room);
        lines.push(title);
        let description_row = lines.len();
        lines.extend([Line::from(label("Description", Part::Description)), Line::default(), Line::default()]);
        let mut projects = vec![label("Projects", Part::Projects)];
        projects.extend(filled(self.project_field(room, paths), room));
        lines.push(Line::from(projects));
        let mut other_cursor = 0;
        if self.editing.is_none() {
            let mut snooze = vec![label("Snooze", Part::Snooze)];
            snooze.extend(filled(self.snooze_choice(now, tz), room));
            lines.push(Line::from(snooze));
            if self.other_chosen() {
                let placeholder = "2h, 14:30, tomorrow 9:00, fri 9:00";
                let (other, cursor) = field_line("", LABEL, &self.other, placeholder, None, focused(Part::Other), room);
                other_cursor = cursor;
                lines.push(other);
            }
        }
        frame.render_widget(Paragraph::new(lines), Rect { height: fields, ..inside });
        let below = Rect { y: inside.y + fields, height: inside.height.saturating_sub(fields), ..inside };
        frame.render_widget(Paragraph::new(tail).wrap(Wrap { trim: false }), below);
        // The description is a widget of its own, over its three rows. Empty
        // and elsewhere, its placeholder is drawn here: the widget draws its
        // cursor before it.
        let rows = Rect {
            x: inside.x + LABEL as u16,
            y: inside.y + description_row as u16,
            width: inside.width.saturating_sub(LABEL as u16),
            height: 3,
        }
        .intersection(inside);
        // The field filled over its three rows, whatever its text covers (V4).
        frame.render_widget(ratatui::widgets::Block::new().style(theme().field), rows);
        if self.focus == Part::Description || !self.description.is_empty() {
            let mut description = self.description.clone();
            // The cursor is a block of the text's own colors, swapped: the one reversal left.
            let cursor = if self.focus == Part::Description { theme().text.reversed() } else { theme().field };
            description.set_style(theme().field);
            description.set_cursor_style(cursor);
            description.set_placeholder_style(theme().field.patch(theme().sub));
            frame.render_widget(&description, rows);
        } else {
            frame.render_widget(Paragraph::new(muted("Optional. Enter adds a line.")).style(theme().field), rows);
        }
        let typing = match self.focus {
            Part::Title => Some((2, title_cursor)),
            Part::Other => Some((self.parts().len() as u16 + 3, other_cursor)),
            _ => None,
        };
        if let Some((row, column)) = typing {
            frame.set_cursor_position(((inside.x + column).min(inside.right().saturating_sub(1)), inside.y + row));
        }
    }

    /// The projects on one line, the first where the session starts. The
    /// key hint is never cut off: the projects give way, counted as "+n".
    fn project_field(&self, width: usize, paths: &Paths) -> Vec<Span<'static>> {
        let chosen: Vec<&Project> = self.projects.iter().filter_map(|id| self.project(id)).collect();
        let hint = match self.focus == Part::Projects {
            true => format!("  enter or type to add{}", if chosen.is_empty() { "" } else { " · ⌫ removes the last" }),
            false => String::new(),
        };
        let hint_len = hint.chars().count();
        let mut spans = match chosen.as_slice() {
            [] => vec![muted("No project")],
            [one] if line_width(&project_line(one, &[], false, paths)) + hint_len <= width => {
                project_line(one, &[], false, paths)
            }
            _ => {
                let (mut spans, mut used, mut shown) = (vec![], 0, 0);
                for (i, project) in chosen.iter().enumerate() {
                    let rest = chosen.len() - i - 1;
                    let more = if rest > 0 { format!("+{rest}").len() } else { 0 };
                    let room = width.saturating_sub(hint_len + used + more);
                    let mut size = project.name.chars().count().min(20) + 4;
                    if size > room {
                        if shown > 0 || room < 6 {
                            break;
                        }
                        size = room; // the first one, shortened
                    }
                    spans.extend(name_badge(&project.name, size, &[]));
                    (used, shown) = (used + size, shown + 1);
                }
                if shown < chosen.len() {
                    let count = if shown > 0 {
                        format!("+{}", chosen.len() - shown)
                    } else {
                        format!("{} projects", chosen.len())
                    };
                    spans.push(muted(count));
                }
                spans
            }
        };
        if !hint.is_empty() {
            spans.push(muted(hint));
        }
        spans
    }

    /// The snooze chosen, with ‹ › to say ←→ change it.
    fn snooze_choice(&self, now: i64, tz: &TimeZone) -> Vec<Span<'static>> {
        let focused = self.focus == Part::Snooze;
        let mut spans = vec![];
        if focused {
            spans.push(colored("‹ ", theme().accent));
        }
        match self.snooze {
            0 => spans.push(Span::raw("Don't snooze")),
            n if n <= self.presets.len() => {
                let (label, until) = self.presets[n - 1];
                spans.push(Span::raw(label.to_string()));
                if let Until::Time(ts) = until {
                    spans.push(muted(format!("  {}", crate::times::day_and_clock(ts, now, tz))));
                }
            }
            _ => spans.push(colored("Other time…", theme().accent)),
        }
        if focused {
            spans.push(colored(" ›", theme().accent));
            spans.push(muted("  ←→ change, or type a time"));
        }
        spans
    }
}

fn line_width(spans: &[Span]) -> usize {
    spans.iter().map(Span::width).sum()
}

/// The description field: wrapped, three rows tall, Enter adds a line.
fn description(text: &str) -> TextArea<'static> {
    let mut area = TextArea::new(text.lines().map(String::from).collect());
    area.set_wrap_mode(WrapMode::WordOrGlyph);
    area.set_placeholder_text("Optional. Enter adds a line.");
    area.set_cursor_line_style(ratatui::style::Style::new());
    area
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::Moment;
    use crate::tui::app::tests::{NOW, ctrl, key, with_projects};
    use std::time::Instant;

    fn paths() -> Paths {
        Paths::under("/r".as_ref())
    }

    fn press(form: &mut TaskForm, keys: &[KeyEvent]) -> Outcome {
        let paths = paths();
        let cx = Context { tz: &TimeZone::UTC, now: Moment { instant: Instant::now(), epoch: NOW }, paths: &paths };
        keys.iter().fold(Outcome::Stay, |_, key| form.on_key(*key, &cx))
    }

    fn typed(text: &str) -> Vec<KeyEvent> {
        text.chars().map(|c| key(KeyCode::Char(c))).collect()
    }

    fn new_form(projects: &[&str]) -> TaskForm {
        let presets = vec![("1 hour", Until::Time(NOW + 3600)), ("Tomorrow morning", Until::Time(NOW + 86400))];
        let projects = projects.iter().map(|p| p.to_string()).collect();
        TaskForm::new_task("", projects, with_projects().saved(), presets, &paths())
    }

    fn edit_form(effect: Option<Effect>, follows: bool, broken: Option<&str>) -> TaskForm {
        let snap = with_projects();
        let mut task = snap.data.tasks["task-fix-login"].clone();
        (task.projects, task.workspace_projects) = (vec!["p-api".into(), "p-web".into()], vec!["p-api".into()]);
        let editing = Editing::with("task-fix-login".into(), &task, effect, follows, broken.map(Into::into));
        TaskForm::edit(&task, snap.saved(), editing)
    }

    fn effects(form: &TaskForm) -> Vec<String> {
        form.effects(&paths(), NOW, &TimeZone::UTC)
            .into_iter()
            .map(|(l, t, _)| format!("{l}{}{t}", if l.is_empty() { "" } else { " " }))
            .collect()
    }

    fn saved_job(outcome: Outcome) -> Job {
        match outcome {
            Outcome::CloseAnd(Action::Run(job)) => job,
            other => panic!("not saved: {other:?}"),
        }
    }

    /// R5.4, R1.1: a title the form opens with is selected, as in the
    /// Python itm: what is typed replaces it. Coming back to it with Tab
    /// selects it again; an arrow keeps it and moves the cursor.
    #[test]
    fn typing_replaces_the_title_the_form_opens_with() {
        let mut form = edit_form(None, false, None);
        assert!(form.selected);
        press(&mut form, &typed("Fix login now"));
        assert_eq!(form.title.value(), "Fix login now");
        press(&mut form, &[key(KeyCode::Tab), key(KeyCode::BackTab)]);
        press(&mut form, &[key(KeyCode::Left)]);
        press(&mut form, &typed("! "));
        assert_eq!(form.title.value(), "! Fix login now", "Left went to its start");

        let mut form = TaskForm::new_task("Plan", vec![], with_projects().saved(), vec![], &paths());
        press(&mut form, &[key(KeyCode::Backspace)]);
        assert_eq!(form.title.value(), "", "itm new -i \"Plan\": Backspace clears it");
        assert!(!new_form(&[]).selected, "an empty title has nothing to select");
    }

    /// R1.1: Esc closes an empty form at once, and asks first once something was typed.
    #[test]
    fn r1_1_esc_asks_only_after_typing() {
        let mut form = new_form(&[]);
        assert!(matches!(press(&mut form, &[key(KeyCode::Esc)]), Outcome::Close));
        press(&mut form, &typed("x"));
        assert!(
            matches!(press(&mut form, &[key(KeyCode::Esc)]), Outcome::Open(s) if matches!(*s, Screen::ConfirmDiscard))
        );
        assert!(matches!(form.returned(Action::Answer(true)), Outcome::Close), "y discards");
        let mut edit = edit_form(Some(Effect::Running), false, None);
        assert!(matches!(press(&mut edit, &[key(KeyCode::Esc)]), Outcome::Close));
        edit.returned(Action::Picked(vec!["p-api".into()], None));
        assert!(edit.typed(), "only the projects changed: still asks");
    }

    /// R1.1: Enter in the title creates with the defaults; Tab reaches the
    /// description (Enter adds a line there), Ctrl+S saves from any field,
    /// and a title is needed.
    #[test]
    fn r1_1_the_form_creates_from_any_field() {
        let mut form = new_form(&[]);
        let outcome = press(&mut form, &[key(KeyCode::Enter)]);
        assert!(matches!(outcome, Outcome::Stay));
        assert_eq!((form.error.as_deref(), form.focus), (Some("The task needs a title."), Part::Title));
        press(&mut form, &typed("Plan launch"));
        press(&mut form, &[ctrl('a'), key(KeyCode::Tab)]); // ctrl+a moves the cursor, it stays in the form
        press(&mut form, &typed("one"));
        press(&mut form, &[key(KeyCode::Enter)]);
        press(&mut form, &typed("two"));
        let job = saved_job(press(&mut form, &[ctrl('s')]));
        let create = Job::Create {
            title: "Plan launch".into(),
            description: Some("one\ntwo".into()),
            projects: vec![],
            until: None,
        };
        assert_eq!(job, create);
        let mut quick = new_form(&["p-api"]);
        press(&mut quick, &typed("Quick"));
        let job = saved_job(press(&mut quick, &[key(KeyCode::Enter)]));
        assert!(matches!(job, Job::Create { projects, until: None, .. } if projects == ["p-api"]));
    }

    /// R1.1, R4.1: the snooze field: ←→ through the presets (time order, no
    /// "until an agent" for a new task), typing a time on it, and what
    /// does not read (or "agent") refused while it is on screen.
    #[test]
    fn r1_1_snooze_presets_and_a_time_typed() {
        let mut form = new_form(&[]);
        press(&mut form, &typed("Later"));
        press(&mut form, &[key(KeyCode::Tab), key(KeyCode::Tab), key(KeyCode::Tab)]);
        assert_eq!(form.focus, Part::Snooze);
        press(&mut form, &[key(KeyCode::Right)]);
        assert!(effects(&form).contains(&"Saved snoozed; the session starts when you open it".to_string()));
        let job = saved_job(press(&mut form, &[ctrl('s')]));
        assert!(matches!(job, Job::Create { until: Some(Until::Time(t)), .. } if t == NOW + 3600));

        let mut form = new_form(&[]);
        press(&mut form, &typed("Soon"));
        press(&mut form, &[key(KeyCode::Tab), key(KeyCode::Tab), key(KeyCode::Tab)]);
        press(&mut form, &typed("25:00")); // typing on the snooze is typing a time
        assert_eq!((form.focus, form.other.value()), (Part::Other, "25:00"));
        assert!(matches!(press(&mut form, &[key(KeyCode::Enter)]), Outcome::Stay));
        assert_eq!(form.error.as_deref(), Some("invalid time: 25:00"));
        form.other = Input::new("agent".into());
        assert!(matches!(press(&mut form, &[key(KeyCode::Enter)]), Outcome::Stay));
        assert_eq!(form.error.as_deref(), Some("a new task has no agents yet, so none could wake it; pick a time"));
        assert_eq!(form.title.value(), "Soon", "nothing typed is lost");
        form.other = Input::new("2h".into());
        let job = saved_job(press(&mut form, &[key(KeyCode::Enter)]));
        assert!(matches!(job, Job::Create { until: Some(Until::Time(t)), .. } if t == NOW + 7200));
        assert!(form.presets.iter().all(|p| p.1 != Until::Agent));
    }

    /// R4.1: a time only GNU `date` reads (`next week`) is read by a worker
    /// before saving; keys wait meanwhile, and what it says stays on screen.
    #[test]
    fn r4_1_a_time_only_gnu_date_reads_is_read_before_saving() {
        let mut form = new_form(&[]);
        press(&mut form, &typed("Later"));
        press(&mut form, &[key(KeyCode::Tab), key(KeyCode::Tab), key(KeyCode::Tab)]);
        press(&mut form, &typed("next week"));
        assert!(effects(&form).contains(&"Saved snoozed; the session starts when you open it".to_string()));
        let outcome = press(&mut form, &[ctrl('s')]);
        let Outcome::Act(Action::Run(job)) = outcome else { panic!("not read first: {outcome:?}") };
        assert_eq!(job, Job::SnoozeTime { text: "next week".into() });
        assert!(form.reading);
        press(&mut form, &[key(KeyCode::Char('!')), key(KeyCode::Esc)]);
        assert_eq!((form.other.value(), form.reading), ("next week", true), "keys wait for the time");
        let week = NOW + 7 * 86400;
        let job = saved_job(form.read(Ok(Reply::Until(Until::Time(week)))));
        assert_eq!(
            job,
            Job::Create { title: "Later".into(), description: None, projects: vec![], until: Some(Until::Time(week)) }
        );

        let mut form = new_form(&[]);
        press(&mut form, &typed("Later"));
        press(&mut form, &[key(KeyCode::Tab), key(KeyCode::Tab), key(KeyCode::Tab)]);
        press(&mut form, &typed("monkey"));
        press(&mut form, &[key(KeyCode::Tab), ctrl('s')]); // saved from another field
        let failed = form.read(Err(Error::Other("cannot understand snooze time: monkey".into())));
        assert!(matches!(failed, Outcome::Stay));
        assert_eq!((form.error.as_deref(), form.focus), (Some("cannot understand snooze time: monkey"), Part::Other));
        assert!(!form.reading);
        press(&mut form, &[key(KeyCode::Backspace)]);
        assert_eq!(form.other.value(), "monke", "typing goes on");
    }

    /// R1.6: on the projects field Enter or Space opens the finder empty, a
    /// key typed opens it with that key, and Backspace removes the last project.
    #[test]
    fn r1_6_the_project_field_opens_the_finder() {
        let mut form = new_form(&["p-api", "p-web"]);
        press(&mut form, &[key(KeyCode::Tab), key(KeyCode::Tab)]);
        let finder = |outcome: Outcome| match outcome {
            Outcome::Open(screen) => match *screen {
                Screen::Finder(finder) => finder,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        };
        assert_eq!(finder(press(&mut form, &[key(KeyCode::Char(' '))])).text(), "");
        let opened = finder(press(&mut form, &[key(KeyCode::Char('b'))]));
        assert_eq!((opened.text(), opened.chosen.clone()), ("b", vec!["p-api".to_string(), "p-web".to_string()]));
        press(&mut form, &[key(KeyCode::Backspace)]);
        assert_eq!(form.projects, ["p-api"]);
        let added = project_named("new", Some("/r/home/new"));
        form.returned(Action::Picked(vec!["p-api".into(), "p-new".into()], Some(("p-new".into(), added))));
        assert_eq!(form.projects, ["p-api", "p-new"]);
        assert_eq!(form.saved.folders.get(std::path::Path::new("/r/home/new")).map(String::as_str), Some("p-new"));
    }

    fn project_named(name: &str, path: Option<&str>) -> Project {
        crate::tui::app::tests::project(name, path, NOW)
    }

    /// R1.2, R1.6: the form says where the session really starts: the first
    /// project whose folder is there, a workspace each; what gets none and why.
    #[test]
    fn r1_6_the_new_task_form_says_where_it_starts() {
        let lines = effects(&new_form(&["p-gone", "p-api"]));
        assert_eq!(lines[0], "Starts in ~/code/api");
        assert!(lines.contains(&"Missing, so no workspace: gone".to_string()), "{lines:?}");
        let lines = effects(&new_form(&["p-x"]));
        assert_eq!(lines[0], "Starts in ~");
        assert!(lines.contains(&"No folder, so no workspace: Client X".to_string()), "{lines:?}");
        let mut form = new_form(&["p-api", "p-web"]);
        assert!(effects(&form).contains(&"Workspaces api · web".to_string()));
        press(&mut form, &typed("Fix: the API"));
        assert!(effects(&form).contains(&"Session   task-fix-the-api".to_string()));
        assert_eq!(effects(&form).last().unwrap(), "Then switches you to it");
        let cannot = form.cannot_switch(Some("this Herdr client was not started by `itm attach`".into()));
        assert!(effects(&cannot).last().unwrap().starts_with("Saved without switching: this Herdr client"));
    }

    /// R5.4: the edit form says what a project added does to the session.
    #[test]
    fn r5_4_the_edit_form_says_what_a_new_project_does() {
        let cases = [
            (Some(Effect::Starts), "Its session will start with a workspace in ~/code/backend"),
            (Some(Effect::Running), "A workspace opens in ~/code/backend, in its running session"),
            (
                Some(Effect::Stopped),
                "Its stopped session keeps its workspaces; open one in ~/code/backend when you need it",
            ),
            (None, "Herdr did not answer, so it is unknown whether its session runs"),
        ];
        for (effect, said) in cases {
            let mut form = edit_form(effect, false, None);
            let mut projects = form.projects.clone();
            projects.extend(["p-new".into(), "p-x".into(), "p-gone".into()]);
            form.returned(Action::Picked(
                projects,
                Some(("p-new".into(), project_named("backend", Some("/r/home/code/backend")))),
            ));
            let lines = effects(&form);
            assert_eq!(lines[0], "Session   task-fix-login");
            assert!(lines.contains(&said.to_string()), "{effect:?}: {lines:?}");
            assert!(lines.contains(&"Client X: no folder, so no workspace".to_string()), "{lines:?}");
            assert!(lines.contains(&"~/code/gone missing, so no workspace opens there".to_string()), "{lines:?}");
        }
    }

    /// R5.4: while projects follow workspaces and the daemon runs, a project
    /// whose workspace is open in the running session cannot be removed
    /// here; with that setting off it can. A broken settings file is said
    /// and counts as off (R10.4).
    #[test]
    fn r5_4_a_project_with_an_open_workspace_stays() {
        let mut form = edit_form(Some(Effect::Running), true, None);
        form.returned(Action::Picked(vec!["p-web".into()], None));
        assert!(
            effects(&form).contains(&"api still has a workspace in this task; close it in Herdr first".to_string())
        );
        assert!(matches!(press(&mut form, &[ctrl('s')]), Outcome::Stay));
        assert!(
            form.error
                .as_deref()
                .unwrap()
                .starts_with("api still has a workspace in this task; close it in Herdr first, or turn off")
        );
        let mut off = edit_form(Some(Effect::Running), false, None);
        off.returned(Action::Picked(vec!["p-web".into()], None));
        let job = saved_job(press(&mut off, &[ctrl('s')]));
        assert!(matches!(job, Job::Edit { ref remove, .. } if remove == &["p-api"]), "{job:?}");
        let mut broken = edit_form(Some(Effect::Running), false, Some("~/c.toml is not valid TOML: x"));
        broken.returned(Action::Picked(vec!["p-web".into()], None));
        let lines = effects(&broken);
        assert!(lines.contains(
            &"Settings ignored, projects do not follow workspaces: ~/c.toml is not valid TOML: x".to_string()
        ));
        assert!(!lines.iter().any(|l| l.contains("still has a workspace")), "off, as for the daemon");
        let stopped = edit_form(Some(Effect::Stopped), true, Some("x"));
        assert!(
            stopped.editing.as_ref().unwrap().pinned.is_empty()
                && stopped.editing.as_ref().unwrap().config_error.is_none()
        );
    }

    /// R5.4: the edit saves the title, the description and the projects in
    /// the form's order; unchanged projects send no order.
    #[test]
    fn r5_4_the_edit_keeps_the_order() {
        let mut form = edit_form(Some(Effect::Running), false, None);
        let job = saved_job(press(&mut form, &[ctrl('s')]));
        assert!(
            matches!(job, Job::Edit { ref order, ref add, ref remove, .. } if order.is_empty() && add.is_empty() && remove.is_empty())
        );
        form.returned(Action::Picked(vec!["p-web".into(), "p-api".into()], None));
        form.title = Input::new("Fix login fast".into());
        let job = saved_job(press(&mut form, &[ctrl('s')]));
        let edit = Job::Edit {
            session: "task-fix-login".into(),
            title: "Fix login fast".into(),
            description: "Users get logged out\nafter 5 min".into(),
            add: vec![],
            remove: vec![],
            order: vec!["p-web".into(), "p-api".into()],
        };
        assert_eq!(job, edit);
    }

    /// R1.1: a paste into the description keeps its lines; anywhere else it is typed.
    #[test]
    fn r1_1_a_paste_keeps_the_descriptions_lines() {
        let mut form = new_form(&[]);
        assert!(!form.paste("Plan\nlaunch"), "the title types it, one line");
        press(&mut form, &[key(KeyCode::Tab)]);
        assert!(form.paste("one\ntwo"));
        assert_eq!(form.description_text(), "one\ntwo");
    }

    /// Keys that edit text stay in the form: Home then Ctrl+K clears the title.
    #[test]
    fn line_editing_keys_stay_in_the_form() {
        let mut form = new_form(&[]);
        press(&mut form, &typed("abc"));
        press(&mut form, &[key(KeyCode::Home), ctrl('k')]);
        assert_eq!(form.title.value(), "");
    }
}
