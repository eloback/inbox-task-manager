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
            false => {
                // One width for where they are, as wide as the longest end of a row lets it be.
                let tail = ids.iter().map(|id| self.used(id, now).chars().count()).max().unwrap_or(0);
                let room = (list.width as usize).saturating_sub(14 + 5 + 1 + tail).clamp(12, 45);
                ids.iter().map(|id| ListItem::new(Line::from(self.row(id, paths, now, room)))).collect()
            }
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

    /// Its tasks (how many are open), when it was last used, and whether its folder is missing.
    fn used(&self, id: &str, now: i64) -> String {
        let tasks: Vec<_> = self.data.tasks.values().filter(|t| t.projects.iter().any(|p| p == id)).collect();
        let open = tasks.iter().filter(|t| t.state != State::Archived).count();
        let plural = if tasks.len() == 1 { "" } else { "s" };
        let mut count = format!("{} task{plural}", tasks.len());
        if !tasks.is_empty() {
            count.push_str(&format!(" · {open} open"));
        }
        let missing = if self.missing.contains(id) { " · folder missing" } else { "" };
        format!("{count:<16} used {}{missing}", rel(self.data.projects[id].last_used_at, now))
    }

    /// Its name, kind, where it is in `room` columns (its end kept in a
    /// narrow popup), its tasks and when it was last used, which is never cut.
    fn row(&self, id: &str, paths: &Paths, now: i64, room: usize) -> Vec<Span<'static>> {
        let project = &self.data.projects[id];
        let used = self.used(id, now);
        let place = paths.place(project);
        let chars: Vec<char> = place.chars().collect();
        let place = match chars.len() >= room {
            true => format!("…{}", chars[chars.len() + 2 - room..].iter().collect::<String>()),
            false => place,
        };
        let mut spans = name_badge(&project.name, 14, &[]);
        spans.extend([
            muted(format!("{:<4} ", project.kind().name())),
            Span::raw(format!("{place:<room$} ")),
            muted(used),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projects::RemovalPlan;
    use crate::tui::app::Moment;
    use crate::tui::app::tests::{NOW, ctrl, key, with_projects};
    use jiff::tz::TimeZone;
    use std::time::Instant;

    fn cx_paths() -> Paths {
        Paths::under("/r".as_ref())
    }

    fn with_cx<T>(f: impl FnOnce(&Context) -> T) -> T {
        let paths = cx_paths();
        f(&Context { tz: &TimeZone::UTC, now: Moment { instant: Instant::now(), epoch: NOW }, paths: &paths })
    }

    /// R8.3, after Python's test_projects_rename_remove_and_filter: a
    /// project a task has cannot be removed (nothing to choose); a rename
    /// to a name taken in any case waits; Enter shows the project's tasks.
    #[test]
    fn r8_3_rename_remove_and_show_its_tasks() {
        let mut screen = ProjectsScreen::new(&with_projects());
        assert_eq!(screen.selected.as_deref(), Some("p-api"), "the most recently used first");
        let plan = Job::RemovalPlan { id: "p-api".into() };
        assert!(matches!(screen.on_key(ctrl('d')), Outcome::Act(Action::Run(ref job)) if *job == plan));
        let users = RemovalPlan {
            id: "p-api".into(),
            project: screen.data.projects["p-api"].clone(),
            users: vec!["Fix login".into()],
            clone: None,
        };
        let opened = with_cx(|cx| screen.done(&plan, Ok(Reply::Plan(users)), cx));
        let Outcome::Open(dialog) = opened else { panic!("{opened:?}") };
        assert!(
            matches!(&*dialog, Screen::RemoveProject(d) if d.choices.is_empty()),
            "nothing to choose while a task has it"
        );

        let Outcome::Open(rename) = screen.on_key(ctrl('r')) else { panic!("no rename dialog") };
        let Screen::Rename(mut rename) = *rename else { panic!() };
        let typed = |dialog: &mut RenameDialog, keys: Vec<KeyEvent>| {
            keys.into_iter().fold(Outcome::Stay, |_, k| with_cx(|cx| dialog.on_key(k, cx)))
        };
        let clear: Vec<KeyEvent> = (0..3).map(|_| key(KeyCode::Backspace)).collect();
        typed(&mut rename, clear.clone());
        typed(&mut rename, "WEB".chars().map(|c| key(KeyCode::Char(c))).collect());
        assert!(
            rename.problem.as_deref().unwrap().starts_with("project already exists: web (~/code/web)"),
            "taken, any case"
        );
        assert!(matches!(typed(&mut rename, vec![key(KeyCode::Enter)]), Outcome::Stay));
        typed(&mut rename, clear);
        typed(&mut rename, "backend2".chars().map(|c| key(KeyCode::Char(c))).collect());
        let Outcome::CloseAnd(action) = typed(&mut rename, vec![key(KeyCode::Enter)]) else { panic!() };
        let job = Job::RenameProject { id: "p-api".into(), name: "backend2".into() };
        assert_eq!(action, Action::Run(job.clone()));
        assert!(matches!(screen.returned(action), Outcome::Act(Action::Run(_))), "the switcher runs it");
        with_cx(|cx| screen.done(&job, Ok(Reply::Renamed("api".into(), "backend2".into(), None)), cx));
        assert_eq!(screen.message, Some(("Renamed api to backend2.".into(), false)));
        assert!(matches!(screen.on_key(key(KeyCode::Enter)), Outcome::CloseAnd(Action::Filter(name)) if name == "api"));
    }

    /// R8.3: the list follows each refresh, keeping the highlighted project;
    /// ↑↓ and Ctrl+J/K move; a name only says how to give it a folder, and
    /// that goes with it; an added project is said and highlighted.
    #[test]
    fn r8_3_the_list_follows_refreshes() {
        let mut snap = with_projects();
        let mut screen = ProjectsScreen::new(&snap);
        screen.on_key(ctrl('j'));
        screen.on_key(key(KeyCode::Down));
        screen.on_key(ctrl('k'));
        assert_eq!(screen.selected.as_deref(), Some("p-web"));
        screen.reread(&snap);
        assert_eq!(screen.selected.as_deref(), Some("p-web"));
        screen.selected = Some("p-x".into());
        assert!(screen.hint().unwrap().starts_with(
            "Client X has no folder: press prefix+shift+a in its workspace, or run itm project set 'Client X' --dir"
        ));
        snap.data.projects.remove("p-x");
        screen.reread(&snap);
        assert_eq!(
            (screen.selected.as_deref(), screen.hint()),
            (Some("p-api"), None),
            "gone: the first one, and its hint went"
        );
        let added = crate::tui::app::tests::project("tool", Some("/r/data/projects/x/tool"), NOW);
        screen.returned(Action::ProjectAdded("p-tool".into(), added));
        assert_eq!(
            (screen.selected.as_deref(), screen.message.clone()),
            (Some("p-tool"), Some(("Project added.".into(), false)))
        );
        let removed =
            Job::RemoveProject { id: "p-web".into(), name: "web".into(), choice: crate::projects::CloneChoice::Keep };
        let notes = vec![RemovalNote::Done("project web removed".into()), RemovalNote::Kept("kept ~/x: y".into())];
        with_cx(|cx| screen.done(&removed, Ok(Reply::Removed(notes)), cx));
        assert_eq!(screen.message, Some(("project web removed · kept ~/x: y".into(), false)));
        with_cx(|cx| screen.done(&removed, Err(Error::Other("no".into())), cx));
        assert_eq!(screen.message, Some(("no".into(), true)));
    }
}
