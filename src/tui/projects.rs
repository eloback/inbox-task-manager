//! The Projects screen (Ctrl+P, R8.3): each saved project, the most
//! recently used first, with where it is, its tasks and its last use.
//! Enter shows its tasks (an `@name` search, R6.2); Ctrl+N adds one (the
//! add dialog), Ctrl+R renames it (its tasks keep it), Ctrl+D removes it
//! (the remove dialog, after reading its clone in a worker, R8.7).

use std::collections::BTreeSet;

use ratatui::Frame;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, List, ListItem, ListState, Paragraph, Wrap};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use super::add::AddDialog;
use super::app::{Action, Job, Reply, Snapshot, renamed};
use super::draw::{Tone, WIDE, bold, field_line, key_hints, muted, name_badge, wrapped_dialog};
use super::screen::{Context, Field, Outcome, RemoveDialog, Screen, moves, step};
use crate::error::{Error, shell_quote};
use crate::paths::Paths;
use crate::projects::{RemovalNote, name_problem, projects_by_recent};
use crate::store::{Data, Project, ProjectKind, State};
use crate::times::rel;

#[derive(Debug, Clone)]
pub struct ProjectsScreen {
    /// The tasks and projects as the switcher's last refresh read them.
    pub data: Data,
    pub missing: BTreeSet<String>,
    /// The highlighted project's id, kept across refreshes.
    pub selected: Option<String>,
    /// What the last action said, and whether it went wrong.
    pub message: Option<(String, bool)>,
}

impl ProjectsScreen {
    pub fn new(snap: &Snapshot) -> ProjectsScreen {
        let mut screen =
            ProjectsScreen { data: snap.data.clone(), missing: snap.missing.clone(), selected: None, message: None };
        screen.selected = screen.ids().first().cloned();
        screen
    }

    /// The projects' ids in the order shown.
    pub fn ids(&self) -> Vec<String> {
        projects_by_recent(&self.data.projects).into_iter().map(|(id, _)| id.clone()).collect()
    }

    pub fn highlighted(&self) -> Option<(&String, &Project)> {
        let id = self.selected.as_ref()?;
        self.data.projects.get_key_value(id)
    }

    /// A refresh: the highlighted project stays, unless it is gone.
    pub fn reread(&mut self, snap: &Snapshot) {
        (self.data, self.missing) = (snap.data.clone(), snap.missing.clone());
        let ids = self.ids();
        if self.selected.as_ref().is_none_or(|id| !ids.contains(id)) {
            self.selected = ids.first().cloned();
        }
    }

    fn say(&mut self, text: impl Into<String>, error: bool) {
        self.message = Some((text.into(), error));
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let ids = self.ids();
        let at = self.selected.as_ref().and_then(|id| ids.iter().position(|i| i == id));
        match key.code {
            KeyCode::Esc => return Outcome::Close,
            _ if moves(key) != 0 => {
                if let Some(at) = at {
                    self.selected = Some(ids[step(at, moves(key), ids.len() - 1)].clone());
                }
            }
            KeyCode::Char('n') if ctrl => {
                let dialog = AddDialog::new(ProjectKind::Git, "", "", Field::Source);
                return Outcome::Open(Box::new(Screen::AddProject(Box::new(dialog))));
            }
            _ => {
                let Some((id, project)) = self.highlighted() else { return Outcome::Stay };
                let (id, name) = (id.clone(), project.name.clone());
                match key.code {
                    KeyCode::Enter => return Outcome::CloseAnd(Action::Filter(name)),
                    KeyCode::Char('r') if ctrl => {
                        let dialog = RenameDialog::new(id, name, self.data.clone());
                        return Outcome::Open(Box::new(Screen::Rename(dialog)));
                    }
                    KeyCode::Char('d') if ctrl => {
                        self.message = None;
                        return Outcome::Act(Action::Run(Job::RemovalPlan { id }));
                    }
                    _ => {}
                }
            }
        }
        Outcome::Stay
    }

    pub fn returned(&mut self, action: Action) -> Outcome {
        match action {
            Action::ProjectAdded(id, project) => {
                self.data.projects.insert(id.clone(), project); // shown before the refresh brings it
                self.selected = Some(id);
                self.say("Project added.", false);
                Outcome::Stay
            }
            action => Outcome::Act(action), // a rename or a removal to run
        }
    }

    pub fn done(&mut self, job: &Job, result: Result<Reply, Error>, cx: &Context) -> Outcome {
        match (job, result) {
            (_, Err(e)) => self.say(e.to_string(), true),
            (Job::RemovalPlan { .. }, Ok(Reply::Plan(plan))) => {
                return Outcome::Open(Box::new(Screen::RemoveProject(RemoveDialog::new(&plan, cx.paths))));
            }
            (Job::RemoveProject { .. }, Ok(Reply::Removed(notes))) => {
                let lines: Vec<String> = notes.into_iter().map(RemovalNote::line).collect();
                self.say(lines.join(" · "), false);
            }
            (Job::RenameProject { .. }, Ok(Reply::Renamed(old, new, note))) => {
                self.say(renamed(&old, &new, note), false)
            }
            _ => {}
        }
        Outcome::Stay
    }
}

/// Renaming a project (R8.2): its tasks keep it, since they store its id.
/// A name another project has, in any case, is said while typing, and
/// Enter waits for one that is free.
#[derive(Debug, Clone)]
pub struct RenameDialog {
    pub id: String,
    pub old: String,
    pub name: Input,
    pub data: Data,
    pub problem: Option<String>,
}

impl RenameDialog {
    pub fn new(id: String, old: String, data: Data) -> RenameDialog {
        RenameDialog { id, name: Input::new(old.clone()), old, data, problem: None }
    }

    pub fn on_key(&mut self, key: KeyEvent, cx: &Context) -> Outcome {
        match key.code {
            KeyCode::Esc => Outcome::Close,
            KeyCode::Enter => {
                let new = self.name.value().trim();
                let problem = name_problem(&self.data, cx.paths, new, &self.id);
                if new.is_empty() || new == self.old || problem.is_some() {
                    self.problem = problem.map(|e| e.to_string());
                    return Outcome::Stay;
                }
                Outcome::CloseAnd(Action::Run(Job::RenameProject { id: self.id.clone(), name: new.into() }))
            }
            _ => {
                if self.name.handle_event(&Event::Key(key)).is_some_and(|c| c.value) {
                    let problem = name_problem(&self.data, cx.paths, self.name.value().trim(), &self.id);
                    self.problem = problem.map(|e| e.to_string());
                }
                Outcome::Stay
            }
        }
    }
}

impl ProjectsScreen {
    /// A whole screen over the list: a row per project, how to give a
    /// name-only project a folder, what the last action said, the keys.
    pub fn draw(&self, frame: &mut Frame, area: Rect, paths: &Paths, now: i64) {
        frame.render_widget(Clear, area);
        let area = area.inner(Margin { horizontal: 1, vertical: 1 });
        let [title, _, list, hint, message, keys] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(if self.hint().is_some() { 2 } else { 0 }),
            Constraint::Length(u16::from(self.message.is_some()) * 2),
            Constraint::Length(1),
        ])
        .areas(area);
        frame.render_widget(
            Paragraph::new(Line::from(vec![bold("Projects"), muted("  repositories and folders tasks start in")])),
            title,
        );
        let ids = self.ids();
        let items: Vec<ListItem> = match ids.is_empty() {
            true => vec![ListItem::new(muted("No projects yet. Ctrl+N adds a repository, a folder or a name."))],
            false => ids.iter().map(|id| ListItem::new(Line::from(self.row(id, paths, now)))).collect(),
        };
        let at = self.selected.as_ref().and_then(|id| ids.iter().position(|i| i == id));
        let mut state = ListState::default().with_selected(at);
        let highlight = Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED);
        frame.render_stateful_widget(List::new(items).highlight_style(highlight), list, &mut state);
        if let Some(text) = self.hint() {
            frame.render_widget(Paragraph::new(muted(text)).wrap(Wrap { trim: false }), hint);
        }
        if let Some((text, error)) = &self.message {
            let tone = if *error { Tone::Error } else { Tone::Muted };
            frame.render_widget(Paragraph::new(tone.span(text.clone())).wrap(Wrap { trim: false }), message);
        }
        let hints = [("enter", "show its tasks"), ("^n", "add"), ("^r", "rename"), ("^d", "remove"), ("esc", "back")];
        frame.render_widget(Paragraph::new(key_hints(&hints)), keys);
    }

    /// How to give the highlighted project a folder when it has none (the screen has no action for it).
    fn hint(&self) -> Option<String> {
        let (_, project) = self.highlighted().filter(|(_, p)| p.kind() == ProjectKind::Name)?;
        Some(format!(
            "{} has no folder: press prefix+shift+a in its workspace, or run itm project set {} --dir <folder>",
            project.name,
            shell_quote(&project.name)
        ))
    }

    /// Its name, kind, where it is, its tasks (how many are open) and when it was last used.
    fn row(&self, id: &str, paths: &Paths, now: i64) -> Vec<Span<'static>> {
        let project = &self.data.projects[id];
        let tasks: Vec<_> = self.data.tasks.values().filter(|t| t.projects.iter().any(|p| p == id)).collect();
        let open = tasks.iter().filter(|t| t.state != State::Archived).count();
        let plural = if tasks.len() == 1 { "" } else { "s" };
        let mut count = format!("{} task{plural}", tasks.len());
        if !tasks.is_empty() {
            count.push_str(&format!(" · {open} open"));
        }
        let place = paths.place(project);
        let chars: Vec<char> = place.chars().collect();
        let place = match chars.len() > 44 {
            true => format!("…{}", chars[chars.len() - 43..].iter().collect::<String>()),
            false => place,
        };
        let missing = if self.missing.contains(id) { " · folder missing" } else { "" };
        let mut spans = name_badge(&project.name, 14, &[]);
        spans.extend([
            muted(format!("{:<4} ", project.kind().name())),
            Span::raw(format!("{place:<45} ")),
            muted(format!("{count:<16} used {}{missing}", rel(project.last_used_at, now))),
        ]);
        spans
    }
}

impl RenameDialog {
    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let status = match &self.problem {
            Some(problem) => Tone::Error.span(problem.clone()),
            None => muted("Its tasks keep it: they refer to the project's id, not its name."),
        };
        let lines = vec![
            Line::from(bold(format!("Rename project “{}”", self.old))),
            Line::default(),
            field_line("New name", 10, &self.name, "", None, true),
            Line::default(),
            Line::from(status),
            Line::default(),
            key_hints(&[("enter", "rename"), ("esc", "cancel")]),
        ];
        let inside = wrapped_dialog(frame, area, WIDE, lines);
        let x = (inside.x + 10 + self.name.visual_cursor() as u16).min(inside.right().saturating_sub(1));
        frame.set_cursor_position((x, inside.y + 2));
    }
}
