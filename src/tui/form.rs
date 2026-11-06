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
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui_textarea::{TextArea, WrapMode};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::app::{Action, Job};
use super::draw::{
    ACCENT, MUTED, Tone, WIDE, bold, colored, dialog_box, field_line, key_hints, muted, name_badge, project_line,
};
use super::finder::{Finder, Saved};
use super::screen::{Context, Outcome, Screen};
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
    pub error: Option<String>,
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
        TaskForm {
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
            error: None,
            cannot_switch: None,
            task_dir: paths.display(&paths.task_dir),
        }
    }

    pub fn edit(task: &Task, saved: Saved, editing: Editing) -> TaskForm {
        let projects: Vec<String> =
            task.projects.iter().filter(|id| saved.projects.contains_key(*id)).cloned().collect();
        let text = task.description.clone().unwrap_or_default();
        TaskForm {
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
            error: None,
            cannot_switch: None,
            task_dir: String::new(),
        }
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
        self.focus = parts[(at + by).rem_euclid(parts.len() as isize) as usize];
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
        match key.code {
            KeyCode::Esc if self.typed() => return Outcome::Open(Box::new(Screen::ConfirmDiscard)),
            KeyCode::Esc => return Outcome::Close,
            KeyCode::Char('s') if ctrl => return self.save(cx),
            KeyCode::Tab => self.move_focus(1),
            KeyCode::BackTab => self.move_focus(-1),
            _ => match self.focus {
                Part::Title | Part::Other if key.code == KeyCode::Enter => return self.save(cx),
                Part::Title => drop(self.title.handle_event(&Event::Key(key))),
                Part::Other => drop(self.other.handle_event(&Event::Key(key))),
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
                        (self.snooze, self.focus) = (self.presets.len() + 1, Part::Other);
                        self.other.handle_event(&Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)));
                    }
                    _ => {}
                },
            },
        }
        Outcome::Stay
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
    fn snooze_value(&self, now: i64, tz: &TimeZone) -> Result<Option<Until>, String> {
        match self.snooze {
            0 => Ok(None),
            n if n <= self.presets.len() => Ok(Some(self.presets[n - 1].1)),
            _ => match self.other.value().trim() {
                "" => Ok(None),
                // Refused here, while what was typed is still on screen.
                text => match Until::parse(text, now, tz) {
                    Ok(Until::Agent) => Err("a new task has no agents yet, so none could wake it; pick a time".into()),
                    Ok(until) => Ok(Some(until)),
                    Err(e) => Err(e.to_string()),
                },
            },
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
            (self.error, self.focus) = (Some("The task needs a title.".into()), Part::Title);
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
                Ok(until) => Job::Create {
                    title,
                    description: Some(description).filter(|d| !d.is_empty()),
                    projects: self.projects.clone(),
                    until,
                },
            },
        };
        Outcome::CloseAnd(Action::Run(job))
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
                (Ok(Some(_)) | Err(_), _) => "Saved snoozed; the session starts when you open it".into(),
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
        let fields = self.parts().len() as u16 + 2; // the description has three rows
        let height = 2 + fields + 1 + effects.len() as u16 + u16::from(self.error.is_some()) + 2 + 2;
        let inside = dialog_box(frame, area, WIDE, height);
        let width = inside.width as usize;
        let mut lines =
            vec![Line::from(bold(if self.editing.is_some() { "Edit task" } else { "New task" })), Line::default()];
        let label = |text: &str, part: Part| {
            let style = if self.focus == part { Style::new().fg(ACCENT).bold() } else { Style::new().fg(MUTED) };
            Span::styled(format!("{text:<LABEL$}"), style)
        };
        lines.push(field_line(
            "Title",
            LABEL,
            &self.title,
            "What is this task about?",
            None,
            self.focus == Part::Title,
        ));
        let description_row = lines.len();
        lines.extend([Line::from(label("Description", Part::Description)), Line::default(), Line::default()]);
        let mut projects = vec![label("Projects", Part::Projects)];
        projects.extend(self.project_field(width - LABEL, paths));
        lines.push(Line::from(projects));
        if self.editing.is_none() {
            let mut snooze = vec![label("Snooze", Part::Snooze)];
            snooze.extend(self.snooze_choice(now, tz));
            lines.push(Line::from(snooze));
            if self.other_chosen() {
                let placeholder = "2h, 14:30, tomorrow 9:00, fri 9:00";
                lines.push(field_line("", LABEL, &self.other, placeholder, None, self.focus == Part::Other));
            }
        }
        lines.push(Line::default());
        for (label, text, tone) in effects {
            let mut line = vec![];
            if !label.is_empty() {
                line.push(muted(format!("{label} ")));
                line.push(Span::raw(text));
            } else {
                line.push(tone.span(text));
            }
            lines.push(Line::from(line));
        }
        if let Some(e) = &self.error {
            lines.push(Line::from(Tone::Error.span(e.clone())));
        }
        lines.push(Line::default());
        lines.push(key_hints(keys));
        frame.render_widget(Paragraph::new(lines), inside);
        // The description is a widget of its own, over its three rows.
        let mut description = self.description.clone();
        let cursor = if self.focus == Part::Description { Style::new().reversed() } else { Style::new() };
        description.set_cursor_style(cursor);
        description.set_placeholder_style(Style::new().fg(MUTED));
        let rows = Rect {
            x: inside.x + LABEL as u16,
            y: inside.y + description_row as u16,
            width: inside.width.saturating_sub(LABEL as u16),
            height: 3,
        };
        frame.render_widget(&description, rows.intersection(inside));
        let typing = match self.focus {
            Part::Title => Some((2, &self.title)),
            Part::Other => Some((self.parts().len() as u16 + 3, &self.other)),
            _ => None,
        };
        if let Some((row, input)) = typing {
            let x = (inside.x + LABEL as u16 + input.visual_cursor() as u16).min(inside.right().saturating_sub(1));
            frame.set_cursor_position((x, inside.y + row));
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
            spans.push(colored(hint, ACCENT));
        }
        spans
    }

    /// The snooze chosen, with ‹ › to say ←→ change it.
    fn snooze_choice(&self, now: i64, tz: &TimeZone) -> Vec<Span<'static>> {
        let focused = self.focus == Part::Snooze;
        let mut spans = vec![];
        if focused {
            spans.push(colored("‹ ", ACCENT));
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
            _ => spans.push(colored("Other time…", ACCENT)),
        }
        if focused {
            spans.push(colored(" ›", ACCENT));
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
