//! The form's project finder (R1.6): typing narrows the saved projects,
//! most recently used first, by their letters in order in any case, over
//! names and where they are (URLs, folders). Enter adds the highlighted
//! one, or removes it when the task has it already (✓). A typed path lists
//! folders to add (Tab goes into one); a typed git URL offers to clone it,
//! or picks the saved project of that repository. Adding goes through the
//! add dialog, so names and duplicates are checked in one place; "+ New
//! project" (a name only) is the last row, so Enter on a typo saves nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use ratatui::Frame;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use super::add::AddDialog;
use super::app::Action;
use super::completion::Folders;
use super::draw::{ACCENT, PROJECT, SUCCESS, WIDE, bold, colored, dialog_box, key_hints, muted, project_line};
use super::screen::{Field, Need, Outcome, Screen, moves, step};
use crate::folders::{Listing, folder_parent, looks_like_path};
use crate::fuzzy::fuzzy_match;
use crate::paths::Paths;
use crate::projects::{looks_like_git_url, projects_by_recent, saved_repository};
use crate::store::{Project, ProjectKind};

/// The saved projects as the form and the finder show them.
#[derive(Debug, Clone, Default)]
pub struct Saved {
    pub projects: BTreeMap<String, Project>,
    /// Ids of those whose folder is set and not there.
    pub missing: BTreeSet<String>,
    /// Each saved folder, its links resolved, and its project.
    pub folders: BTreeMap<PathBuf, String>,
}

impl Saved {
    /// The saved projects of `data`, their folders looked at (the CLI's forms).
    pub fn read(data: &crate::store::Data) -> Saved {
        let (missing, folders) = (crate::projects::missing_folders(data), crate::projects::saved_folders(data));
        Saved { projects: data.projects.clone(), missing, folders }
    }

    /// A project just saved (its folder is there).
    pub fn add(&mut self, id: String, project: Project) {
        if let Some(folder) = project.folder() {
            self.folders.insert(folder.to_path_buf(), id.clone());
        }
        self.projects.insert(id, project);
    }
}

/// A row of the finder: what Enter on it does.
#[derive(Debug, Clone, PartialEq)]
pub enum Pick {
    /// A saved project; `matched` are the letters of its name the search
    /// matched; `saved` marks the saved repository of a URL typed.
    Project {
        id: String,
        matched: Vec<usize>,
        saved: bool,
    },
    /// "Remove all projects" (only with an empty search, and never highlighted first).
    RemoveAll,
    /// Clone the URL typed.
    Clone(String),
    AddGit,
    AddFolder,
    /// A project that is only a name: the text typed, through the add dialog.
    NewName(String),
    /// A folder as typed (ending in "/"); `named`: the folder the text
    /// names itself ("Use …"); the saved project with that folder.
    Folder {
        path: String,
        named: bool,
        project: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct Finder {
    /// The task's projects (ids) when the finder opened.
    pub chosen: Vec<String>,
    pub query: Input,
    pub saved: Saved,
    pub folders: Folders,
    /// None while the folder typed is read: there is nothing to act on until
    /// it arrives, so Enter and Tab never pick a row left from earlier text.
    pub rows: Option<Vec<Pick>>,
    pub selected: usize,
    /// Where projects are is searched as it is shown (`~/…`).
    pub paths: Paths,
}

impl Finder {
    pub fn new(chosen: Vec<String>, typed: &str, saved: Saved, paths: &Paths) -> Finder {
        let mut finder = Finder {
            chosen,
            query: Input::new(typed.into()),
            saved,
            folders: Folders::default(),
            rows: None,
            selected: 0,
            paths: paths.clone(),
        };
        finder.rebuild();
        finder
    }

    pub fn text(&self) -> &str {
        self.query.value().trim()
    }

    /// A folder being typed, as it is listed: `~`, `.` and `..` are the folder itself.
    fn folder_text(&self) -> Option<String> {
        let text = self.text();
        looks_like_path(text).then(|| if matches!(text, "~" | "." | "..") { format!("{text}/") } else { text.into() })
    }

    pub fn rebuild(&mut self) {
        self.rows = match self.folder_text() {
            Some(text) => self.folder_rows(&text),
            None => Some(self.project_rows()),
        };
        let rows = self.rows.as_deref().unwrap_or_default();
        self.selected = if self.text().is_empty() {
            // The most recent project not chosen yet: adding one is the common case; never "remove all".
            let new = |p: &Pick| matches!(p, Pick::Project { id, .. } if !self.chosen.contains(id));
            let add = |p: &Pick| matches!(p, Pick::AddGit);
            rows.iter().position(new).or_else(|| rows.iter().position(add)).unwrap_or(0)
        } else {
            0
        };
    }

    fn project_rows(&self) -> Vec<Pick> {
        let text = self.text();
        let recent = projects_by_recent(&self.saved.projects);
        let mut rows = vec![];
        if text.is_empty() {
            if !self.chosen.is_empty() {
                rows.push(Pick::RemoveAll);
            }
            rows.extend(recent.iter().map(|(id, _)| Pick::Project {
                id: id.to_string(),
                matched: vec![],
                saved: false,
            }));
        } else {
            let mut scored = vec![];
            for (rank, (id, project)) in recent.iter().enumerate() {
                let name = fuzzy_match(text, &project.name);
                // Repositories by URL, folders by where they are; "no folder" is display text, never searched.
                let has_place = project.url.is_some() || project.path.is_some();
                let place = has_place.then(|| fuzzy_match(text, &self.paths.place(project))).flatten();
                let by_name = name.as_ref().map_or(0, |m| m.0);
                let score = by_name.max(place.map_or(0, |m| m.0 * 4 / 5));
                if score > 0 {
                    let matched = name.filter(|_| by_name >= score).map(|m| m.1).unwrap_or_default();
                    scored.push((score, rank, Pick::Project { id: id.to_string(), matched, saved: false }));
                }
            }
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            rows.extend(scored.into_iter().map(|(_, _, pick)| pick));
            if looks_like_git_url(text) {
                rows.insert(
                    0,
                    match saved_repository(&self.saved.projects, text) {
                        Some(id) => Pick::Project { id: id.clone(), matched: vec![], saved: true },
                        None => Pick::Clone(text.into()),
                    },
                );
            }
        }
        rows.extend([Pick::AddGit, Pick::AddFolder]);
        if !text.is_empty() && !looks_like_git_url(text) {
            rows.push(Pick::NewName(text.into())); // last: Enter on a typo never saves a project
        }
        rows
    }

    /// Rows for a folder being typed; None while its parent is read.
    fn folder_rows(&self, text: &str) -> Option<Vec<Pick>> {
        let completions = self.folders.complete(text)?;
        let Some((parent, segment)) = folder_parent(text) else { return Some(vec![]) };
        let Some(Some(listing)) = self.folders.get(parent) else { return Some(vec![]) };
        let saved = |folder: &PathBuf| self.saved.folders.get(folder).cloned();
        // The folder the text names, when it exists (its parent's listing says so).
        let named = match segment {
            "" => Some(listing.real.clone()),
            name if listing.names.iter().any(|n| n == name) => Some(listing.target(name)),
            _ => None,
        };
        let mut rows = vec![];
        if let Some(named) = &named {
            rows.push(Pick::Folder { path: text.into(), named: true, project: saved(named) });
        }
        for path in completions {
            let target = listing.target(path[parent.len()..].trim_end_matches('/'));
            if Some(&target) != named.as_ref() {
                rows.push(Pick::Folder { path, named: false, project: saved(&target) });
            }
        }
        Some(rows)
    }

    pub fn highlighted(&self) -> Option<&Pick> {
        self.rows.as_ref()?.get(self.selected)
    }

    /// The folder to read for what is typed, when it was not read yet.
    pub fn needs(&mut self) -> Option<Need> {
        let text = self.folder_text()?;
        self.folders.wanted(&text).map(Need::Listing)
    }

    pub fn listed(&mut self, parent: &str, listing: &Option<Listing>) {
        self.folders.arrived(parent, listing);
        if self.folder_text().is_some_and(|text| folder_parent(&text).is_some_and(|(p, _)| p == parent)) {
            self.rebuild(); // what is typed now, not when it was asked for
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => return Outcome::Close, // back: the task's projects stay
            _ if moves(key) != 0 => {
                let last = self.rows.as_ref().map_or(0, |r| r.len().saturating_sub(1));
                self.selected = step(self.selected, moves(key), last);
            }
            KeyCode::Tab | KeyCode::BackTab => self.go_into(),
            KeyCode::Enter => return self.highlighted().cloned().map_or(Outcome::Stay, |pick| self.pick(pick)),
            _ => {
                if self.query.handle_event(&Event::Key(key)).is_some_and(|c| c.value) {
                    self.rebuild();
                }
            }
        }
        Outcome::Stay
    }

    /// Tab: into the highlighted folder, whose subfolders are listed next. Nothing for projects.
    fn go_into(&mut self) {
        if let Some(Pick::Folder { path, .. }) = self.highlighted() {
            let path = if path.ends_with('/') { path.clone() } else { format!("{path}/") };
            self.query = Input::new(path);
            self.rebuild();
        }
    }

    /// The projects with `id` added, or removed when chosen already.
    fn toggled(&self, id: &str) -> Vec<String> {
        match self.chosen.iter().any(|c| c == id) {
            true => self.chosen.iter().filter(|c| *c != id).cloned().collect(),
            false => self.chosen.iter().cloned().chain([id.to_string()]).collect(),
        }
    }

    fn pick(&self, pick: Pick) -> Outcome {
        let add = |kind, source: &str, name: &str, focus| {
            Outcome::Open(Box::new(Screen::AddProject(Box::new(AddDialog::new(kind, source, name, focus)))))
        };
        match pick {
            Pick::Project { id, .. } | Pick::Folder { project: Some(id), .. } => {
                Outcome::CloseAnd(Action::Picked(self.toggled(&id), None))
            }
            Pick::RemoveAll => Outcome::CloseAnd(Action::Picked(vec![], None)),
            Pick::Folder { path, project: None, .. } => add(ProjectKind::Dir, &path, "", Field::Name),
            Pick::Clone(url) => add(ProjectKind::Git, &url, "", Field::Name),
            Pick::AddGit => add(ProjectKind::Git, "", "", Field::Source),
            Pick::AddFolder => add(ProjectKind::Dir, "", "", Field::Source),
            // The text is the name; the dialog shows it before anything is saved.
            Pick::NewName(name) => add(ProjectKind::Name, "", &name, Field::Name),
        }
    }

    /// The add dialog saved a project: the task gets it.
    pub fn returned(&mut self, action: Action) -> Outcome {
        match action {
            Action::ProjectAdded(id, project) => {
                let projects = self.chosen.iter().cloned().chain([id.clone()]).collect();
                Outcome::CloseAnd(Action::Picked(projects, Some((id, project))))
            }
            action => Outcome::Act(action),
        }
    }

    /// What the line under the rows says.
    pub fn status(&self) -> Option<String> {
        let text = self.text();
        let Some(rows) = &self.rows else { return Some("Reading the folder…".into()) };
        if self.folder_text().is_some() {
            return Some(match rows.is_empty() {
                true => "No folder there matches.".into(),
                false => "Enter adds the folder (or picks it when saved); tab goes into it.".into(),
            });
        }
        let found = rows.iter().any(|r| matches!(r, Pick::Project { .. } | Pick::Clone(_)));
        (!text.is_empty() && !found).then(|| {
            format!(
                "No project matches “{text}”. Type a path (~/…, /…) to add a folder, or pick “New project” for a name without a folder."
            )
        })
    }
}

/// At most this many rows show at a time.
const ROWS: usize = 12;

impl Finder {
    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let rows = self.rows.as_deref().unwrap_or_default();
        let shown = rows.len().min(ROWS) as u16;
        let status = self.status();
        let inside = dialog_box(frame, area, WIDE, 4 + shown + 1 + u16::from(status.is_some()) * 2 + 2 + 2);
        let mut head = vec![Line::from(bold("Projects")), Line::default()];
        let typed = match self.query.value() {
            "" => muted("Search projects, or type ~/folder, /folder or a git URL"),
            value => Span::raw(value.to_string()),
        };
        head.push(Line::from(vec![colored("› ", ACCENT), typed]));
        frame.render_widget(Paragraph::new(head), inside);
        frame.set_cursor_position((inside.x + 2 + self.query.visual_cursor() as u16, inside.y + 2));
        let list = Rect { y: inside.y + 4, height: shown, ..inside }.intersection(inside);
        let items: Vec<ListItem> = rows.iter().map(|pick| ListItem::new(Line::from(self.row(pick)))).collect();
        let mut state = ListState::default().with_selected((!rows.is_empty()).then_some(self.selected));
        let highlight = Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED);
        frame.render_stateful_widget(List::new(items).highlight_style(highlight), list, &mut state);
        let mut tail = vec![];
        if let Some(status) = status {
            tail.extend([Line::default(), Line::from(muted(status))]);
        }
        tail.push(Line::default());
        tail.push(key_hints(&[
            ("enter", "add · remove a ✓ one"),
            ("tab", "complete a folder"),
            ("↑↓", "move"),
            ("esc", "back"),
        ]));
        let below = Rect { y: list.bottom(), height: inside.bottom().saturating_sub(list.bottom()), ..inside };
        frame.render_widget(Paragraph::new(tail).wrap(Wrap { trim: false }), below);
    }

    fn row(&self, pick: &Pick) -> Vec<Span<'static>> {
        match pick {
            Pick::Project { id, matched, saved } => {
                let Some(project) = self.saved.projects.get(id) else { return vec![] };
                let chosen = self.chosen.contains(id);
                let mut spans = vec![if chosen { colored("✓ ", SUCCESS) } else { Span::raw("  ") }];
                spans.extend(project_line(project, matched, self.saved.missing.contains(id), &self.paths));
                if *saved {
                    spans.push(muted("  saved"));
                }
                spans
            }
            Pick::RemoveAll => vec![muted("  Remove all projects")],
            Pick::Clone(url) => vec![colored("+ Clone", ACCENT), Span::raw(format!(" {url}"))],
            Pick::AddGit => vec![colored("+ Add git repository…", ACCENT)],
            Pick::AddFolder => vec![colored("+ Add local folder…", ACCENT), muted("  or type ~/ or /")],
            Pick::NewName(name) => {
                vec![colored("+ New project", ACCENT), Span::raw(format!(" “{name}”…  ")), muted("no folder")]
            }
            Pick::Folder { path, named, project } => {
                let mut spans =
                    vec![if *named { colored("Use ", ACCENT) } else { Span::raw("  ") }, Span::raw(path.clone())];
                if let Some(project) = project.as_ref().and_then(|id| Some((id, self.saved.projects.get(id)?))) {
                    let state = if self.chosen.contains(project.0) { "✓ chosen" } else { "saved" };
                    spans.extend([colored(format!("  ◆ {}", project.1.name), PROJECT), muted(format!(" {state}"))]);
                }
                spans
            }
        }
    }
}
