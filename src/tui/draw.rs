//! Drawing the switcher (DESIGN.md section 3): views with counts, the
//! inbox list, a preview of the highlighted task, at most seven keys, and
//! the small dialogs; the helpers the other screens draw with. Pure: what
//! is drawn depends only on the app and `now`.

use jiff::tz::TimeZone;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Wrap};

use super::app::{App, Query, Row, Snapshot, View};
use super::screen::{RemoveDialog, Screen, SettingsScreen, SnoozeDialog};
use crate::config::Setting;
use crate::paths::Paths;
use crate::projects::{CloneChoice, CloneState};
use crate::snooze::Until;
use crate::status::AgentState;
use crate::store::{Project, ProjectKind, State, Task};
use crate::times::{age, day_and_clock, rel};

pub const ACCENT: Color = Color::Cyan;
pub const PROJECT: Color = Color::Magenta;
pub const ERROR: Color = Color::Red;
pub const WARNING: Color = Color::Yellow;
pub const SUCCESS: Color = Color::Green;
pub const MUTED: Color = Color::DarkGray;

/// Dialogs are this wide, or as wide as the popup when it is narrower.
pub const DIALOG: u16 = 64;
/// The forms and their dialogs.
pub const WIDE: u16 = 84;

/// At most seven keys (UX principle 5); arrows move, as everywhere; the
/// rest are in the help (`?`).
const KEYS: [(&str, &str); 7] = [
    ("enter", "open"),
    ("tab", "view"),
    ("^n", "new"),
    ("^e", "edit"),
    ("^z", "snooze"),
    ("^p", "projects"),
    ("?", "all keys"),
];

/// How a line of a screen is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Muted,
    Error,
    Warning,
}

impl Tone {
    pub fn span(self, text: impl Into<String>) -> Span<'static> {
        match self {
            Tone::Muted => muted(text),
            Tone::Error => colored(text, ERROR),
            Tone::Warning => colored(text, WARNING),
        }
    }
}

pub fn muted(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::new().fg(MUTED))
}

pub fn colored(text: impl Into<String>, color: Color) -> Span<'static> {
    Span::styled(text.into(), Style::new().fg(color))
}

pub fn bold(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::new().bold())
}

pub fn key_hints(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![];
    for (key, what) in keys {
        spans.push(Span::styled(key.to_string(), Style::new().fg(ACCENT).bold()));
        spans.push(muted(format!(" {what}  ")));
    }
    Line::from(spans)
}

/// Key hints on as many lines as `width` needs, never cutting one (UX principle 5).
pub fn wrapped_key_hints(keys: &[(&str, &str)], width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Vec<(&str, &str)>> = vec![vec![]];
    let mut used = 0;
    for &(key, what) in keys {
        let hint = key.chars().count() + what.chars().count() + 3;
        if used > 0 && used + hint > width as usize + 2 {
            lines.push(vec![]);
            used = 0;
        }
        lines.last_mut().expect("one line at least").push((key, what));
        used += hint;
    }
    lines.iter().map(|line| key_hints(line)).collect()
}

/// At most `max` characters, with "…" when cut.
pub fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max { text.into() } else { text.chars().take(max - 1).chain(['…']).collect() }
}

pub fn draw(frame: &mut Frame, app: &mut App, now: i64) {
    if app.alone {
        for screen in &app.screens {
            draw_screen(frame, screen, frame.area(), &app.tz, now, &app.paths);
        }
        return;
    }
    let area = frame.area().inner(Margin { horizontal: 1, vertical: 0 });
    let hints = wrapped_key_hints(&KEYS, area.width);
    let [top, _, search, _, body, message, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(hints.len() as u16),
    ])
    .areas(area);

    draw_views(frame, app, top);
    draw_search(frame, app, search);
    let [list, side] = Layout::horizontal([Constraint::Percentage(63), Constraint::Fill(1)]).areas(body);
    draw_list(frame, app, list, now);
    draw_preview(frame, app, side, now);
    if let Some(m) = &app.message {
        frame.render_widget(Paragraph::new(colored(m.text.clone(), if m.error { ERROR } else { SUCCESS })), message);
    }
    frame.render_widget(Paragraph::new(hints), keys);
    for screen in &app.screens {
        draw_screen(frame, screen, frame.area(), &app.tz, now, &app.paths);
    }
}

/// Views are always shown with their counts (R6.1), and the inbox says
/// when waits are unknown instead of showing none (R7.3).
fn draw_views(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![];
    for view in View::ALL {
        let label = format!(" {} {} ", view.label(), app.count(view));
        spans.push(if view == app.view {
            Span::styled(label, Style::new().fg(ACCENT).bold().reversed())
        } else {
            muted(label)
        });
        spans.push(Span::raw(" "));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    let Some(snap) = &app.snapshot else { return };
    let state = if !snap.listed {
        let daemon = if snap.status.fresh { "" } else { " · daemon not running" };
        colored(format!("Herdr not answering · sessions unknown{daemon}"), WARNING)
    } else if !snap.status.fresh {
        colored("daemon not running · waits unknown", WARNING)
    } else {
        let tasks = snap.data.tasks.iter();
        let waiting = tasks.filter(|(name, t)| t.state != State::Archived && snap.status.waiting(name) > 0).count();
        if waiting == 0 {
            return;
        }
        Span::styled(format!("⚑ {waiting} needs you"), Style::new().fg(ERROR).bold())
    };
    frame.render_widget(Paragraph::new(state).right_aligned(), area);
}

/// Creating is discoverable from the search box (UX principle 6).
fn draw_search(frame: &mut Frame, app: &App, area: Rect) {
    let typed = app.search.value();
    let text = if typed.is_empty() {
        muted("Search tasks · @project filters · or type a new title")
    } else {
        Span::raw(typed.to_string())
    };
    frame.render_widget(Paragraph::new(Line::from(vec![colored("› ", ACCENT), text])), area);
    if app.screens.is_empty() {
        frame.set_cursor_position((area.x + 2 + app.search.visual_cursor() as u16, area.y));
    }
}

fn draw_list(frame: &mut Frame, app: &mut App, area: Rect, now: i64) {
    let Some(snap) = &app.snapshot else { return };
    let statuses: Vec<Option<Status>> = (app.rows.iter())
        .map(|row| match row {
            Row::Task(name) => Some(status(snap, name, &snap.data.tasks[name], now)),
            _ => None,
        })
        .collect();
    let widest = statuses.iter().flatten().map(Status::width).max().unwrap_or(0);
    let columns = Columns::fitting(area.width as usize - 1, widest); // one column from the preview
    let items: Vec<ListItem> = (app.rows.iter().zip(statuses))
        .map(|(row, status)| match (row, status) {
            (Row::Task(name), Some(status)) => ListItem::new(task_line(snap, &snap.data.tasks[name], status, columns)),
            (Row::Header(header), _) => {
                let color = if *header == "NEEDS YOU" { ERROR } else { MUTED };
                ListItem::new(Span::styled(*header, Style::new().fg(color).bold()))
            }
            (Row::UnknownProject(name), _) => ListItem::new(colored(format!("No project named “{name}”"), WARNING)),
            (Row::Create, _) => {
                let mut line = vec![
                    colored("+ ", ACCENT),
                    Span::raw(format!("Create task “{}”", Query::parse(app.search.value()).text)),
                ];
                // R6.2: in the filter's project (or the current task's first).
                if let Some(project) = app.default_projects().first().and_then(|id| snap.data.projects.get(id)) {
                    line.extend([Span::raw(" in "), colored(format!("◆ {}", project.name), PROJECT)]);
                }
                ListItem::new(Line::from(line))
            }
            (Row::Task(_), None) => unreachable!("every task row has a status"),
        })
        .collect();
    let list = List::new(items).highlight_style(Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED));
    frame.render_stateful_widget(list, area, &mut app.list);
}

/// A row's icon and what the task is doing ("claude waiting · 4m", "wakes in 2h").
struct Status {
    icon: Span<'static>,
    detail: Vec<Span<'static>>,
}

impl Status {
    fn width(&self) -> usize {
        self.detail.iter().map(Span::width).sum()
    }
}

/// Where a row's parts go: the icon and title, then the first project, then
/// the status. The status is never cut: in a narrow list the title gets
/// shorter first, and the project goes last.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Columns {
    /// Icon, title and the gap after them.
    title: usize,
    /// The first project and the gap after it; 0 when it does not fit.
    project: usize,
}

impl Columns {
    const WIDE: Columns = Columns { title: 31, project: 12 };
    /// The narrowest title column kept next to a project.
    const MIN_TITLE: usize = 12;

    fn fitting(width: usize, status: usize) -> Columns {
        let lead = width.saturating_sub(status).min(Columns::WIDE.title + Columns::WIDE.project);
        match lead.checked_sub(Columns::WIDE.project) {
            Some(title) if title >= Columns::MIN_TITLE => Columns { title, project: Columns::WIDE.project },
            _ => Columns { title: lead, project: 0 },
        }
    }
}

fn status(snap: &Snapshot, name: &str, task: &Task, now: i64) -> Status {
    let session = snap.sessions.get(name);
    let longest_wait = snap.status.blocked(name).min_by_key(|a| a.since.unwrap_or(now));
    let (icon, detail) = match (task.state, longest_wait) {
        (State::Snoozed, _) => match task.snoozed_until {
            Some(until) => (colored("◔", WARNING), colored(format!("wakes {}", rel(until, now)), WARNING)),
            None => (colored("◔", WARNING), colored("until an agent asks", WARNING)),
        },
        (State::Archived, _) => (muted("▣"), muted(format!("archived {}", rel(task.archived_at.unwrap_or(now), now)))),
        (State::Active, Some(wait)) => {
            let waited = age(wait.since.unwrap_or(now), now);
            (colored("⚑", ERROR), colored(format!("{} waiting · {waited}", wait.agent), ERROR))
        }
        _ if !snap.listed => (muted("?"), muted("session unknown")),
        _ if session.is_none() => (muted("○"), muted("not started")),
        _ if !session.is_some_and(|s| s.running) => {
            (muted("○"), muted(format!("stopped · {}", rel(task.last_used_at, now))))
        }
        _ if snap.status.agents(name).iter().any(|a| a.status == AgentState::Working) => {
            (colored("●", SUCCESS), colored("working", SUCCESS))
        }
        _ => (colored("●", SUCCESS), muted("idle")),
    };
    let mut detail = vec![detail];
    if snap.current.as_deref() == Some(name) {
        detail.push(colored(" · here", ACCENT));
    }
    Status { icon, detail }
}

/// Icon and title, the first project, and what the task is doing.
fn task_line(snap: &Snapshot, task: &Task, status: Status, columns: Columns) -> Line<'static> {
    let room = columns.title.saturating_sub(3); // the icon, a space, and one gap after
    let title = if room == 0 { String::new() } else { format!(" {}", cut(&task.title, room.min(28))) };
    let mut spans = vec![status.icon, Span::raw(title)];
    let used: usize = spans.iter().map(Span::width).sum();
    spans.push(Span::raw(" ".repeat(columns.title.saturating_sub(used).max(1))));
    if columns.project > 0 {
        let projects = project_names(snap, task);
        let badge = match projects.split_first() {
            Some((first, rest)) => {
                let more = if rest.is_empty() { String::new() } else { format!(" +{}", rest.len()) };
                format!("◆ {}{more}", cut(first, 9usize.saturating_sub(more.len()).max(3)))
            }
            None => String::new(),
        };
        let pad = columns.project.saturating_sub(badge.chars().count()).max(1);
        spans.push(colored(badge + &" ".repeat(pad), PROJECT));
    }
    spans.extend(status.detail);
    Line::from(spans)
}

fn project_names(snap: &Snapshot, task: &Task) -> Vec<String> {
    task.projects.iter().filter_map(|id| snap.data.projects.get(id)).map(|p| p.name.clone()).collect()
}

/// The highlighted task's context: projects, description, state, agents
/// (UX principle 12), and the key that opens it, named for its state.
fn draw_preview(frame: &mut Frame, app: &App, area: Rect, now: i64) {
    let block = Block::new().borders(Borders::LEFT).border_style(Style::new().fg(MUTED));
    let inner = block.inner(area).inner(Margin { horizontal: 2, vertical: 0 });
    frame.render_widget(block, area);
    let (lines, enter) = preview(app, now);
    // Pinned: a long preview never hides the keys, and they are never cut.
    let hints: &[(&str, &str)] = match enter {
        Some("create") => &[("enter", "create")],
        Some(enter) => &[("enter", enter), ("ctrl+o", "notes")],
        None => &[],
    };
    let hints = if hints.is_empty() { vec![] } else { wrapped_key_hints(hints, inner.width) };
    let [text, keys] = Layout::vertical([Constraint::Fill(1), Constraint::Length(hints.len() as u16)]).areas(inner);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), text);
    frame.render_widget(Paragraph::new(hints), keys);
}

fn preview(app: &App, now: i64) -> (Vec<Line<'static>>, Option<&'static str>) {
    let Some(snap) = &app.snapshot else { return (vec![Line::from(muted("Loading…"))], None) };
    let (name, task) = match app.highlighted() {
        Some(Row::Task(name)) => (name, &snap.data.tasks[name]),
        Some(Row::Create) => {
            let title = Query::parse(app.search.value()).text;
            return (
                vec![Line::from(muted(format!("No task matches. Press enter to create “{title}”.")))],
                Some("create"),
            );
        }
        _ if snap.data.tasks.is_empty() => {
            return (vec![Line::from(muted("No tasks yet. Type a title and press enter, or ctrl+n."))], None);
        }
        _ => return (vec![], None),
    };
    let mut lines = vec![Line::from(Span::styled(task.title.clone(), Style::new().bold()))];
    // The daemon's record of open workspaces holds only while the session runs (R10.3).
    let running = snap.sessions.get(name).is_some_and(|s| s.running);
    for (id, project) in task.projects.iter().filter_map(|id| Some((id, snap.data.projects.get(id)?))) {
        let mut head =
            vec![colored(format!("◆ {}", project.name), PROJECT), muted(format!("  {}", project.kind().name()))];
        if running && task.workspace_projects.contains(id) {
            head.push(colored("  workspace open", SUCCESS));
        }
        lines.push(Line::from(head));
        lines.push(Line::from(muted(format!("  {}", app.paths.place(project)))));
        if snap.missing.contains(id) {
            lines.push(Line::from(colored("  Its folder is missing, so it gets no workspace.", ERROR)));
        } else if project.kind() == ProjectKind::Name {
            lines.push(Line::from(muted("  give it one: prefix+shift+a in its workspace, or itm project set")));
        }
    }
    if let Some(description) = &task.description {
        lines.push(Line::default());
        let all: Vec<&str> = description.lines().collect();
        lines.extend(all.iter().take(5).map(|l| Line::from(muted(l.to_string()))));
        if all.len() > 5 {
            lines.push(Line::from(muted("…")));
        }
    }
    lines.push(Line::default());
    match task.state {
        State::Snoozed => {
            let text = match task.snoozed_until {
                Some(until) => format!("◔ snoozed until {} ({})", day_and_clock(until, now, &app.tz), rel(until, now)),
                None => "◔ snoozed until an agent needs you".into(),
            };
            lines.extend([Line::from(colored(text, WARNING)), Line::default()]);
        }
        State::Archived => {
            let when = rel(task.archived_at.unwrap_or(now), now);
            lines.extend([Line::from(muted(format!("▣ archived {when} · layout and agents saved"))), Line::default()]);
        }
        State::Active => {}
    }
    let agents = snap.status.agents(name);
    for agent in agents {
        let label = format!("{:<7}", agent.agent);
        lines.push(Line::from(match agent.status {
            AgentState::Blocked => {
                let waited = age(agent.since.unwrap_or(now), now);
                vec![colored("⚑ ", ERROR), Span::raw(label), colored(format!(" waiting {waited}"), ERROR)]
            }
            AgentState::Working => vec![colored("● ", SUCCESS), Span::raw(label), muted(" working")],
            other => vec![muted("○ "), Span::raw(label), muted(format!(" {}", other.name()))],
        }));
        if !agent.title.is_empty() {
            lines.push(Line::from(muted(format!("  {}", agent.title))));
        }
    }
    if agents.is_empty() {
        let session = snap.sessions.get(name);
        lines.push(Line::from(match session {
            _ if !snap.listed => colored("Session unknown: Herdr is not answering", WARNING),
            None => muted(not_started(app, snap, task)),
            Some(s) if !s.running => muted("Session stopped · opens where you left it"),
            Some(_) if snap.status.fresh => muted("No agents running"),
            Some(_) => colored("Agents unknown: the daemon is not running", WARNING),
        }));
    }
    lines.push(Line::default());
    lines.extend(notes_preview(snap, name, task, now));
    lines.extend([Line::default(), Line::from(muted(format!("used {}", rel(task.last_used_at, now))))]);
    let enter = match task.state {
        State::Active => "open",
        State::Snoozed => "wake & open",
        State::Archived => "reopen",
    };
    (lines, Some(enter))
}

/// When the task's notes were edited and their last lines (R9.5), without
/// what itm started them with (the preview shows those already).
fn notes_preview(snap: &Snapshot, name: &str, task: &Task, now: i64) -> Vec<Line<'static>> {
    let Some(tail) = snap.notes.get(name) else {
        return vec![Line::from(muted("Notes · none yet · ctrl+o writes some"))];
    };
    let known: Vec<&str> = task.description.as_deref().unwrap_or("").lines().map(str::trim).collect();
    let started = |line: &str| line.starts_with("# ") || (line.starts_with("*Created ") && line.ends_with('*'));
    let written: Vec<&String> = (tail.lines.iter()).filter(|l| !started(l) && !known.contains(&l.trim())).collect();
    if written.is_empty() {
        return vec![Line::from(muted("Notes · nothing written yet · ctrl+o"))];
    }
    let mut lines = vec![Line::from(muted(format!("Notes · edited {}", rel(tail.modified, now))))];
    lines.extend(
        written[written.len().saturating_sub(3)..].iter().map(|l| Line::from(muted(format!("  {}", cut(l, 80))))),
    );
    lines
}

/// Where a session that never ran will start (R1.2): in its first project
/// whose folder is there, with a workspace per such project, else in the
/// task folder.
fn not_started(app: &App, snap: &Snapshot, task: &Task) -> String {
    let folders: Vec<&str> = (task.projects.iter())
        .filter(|id| !snap.missing.contains(*id))
        .filter_map(|id| snap.data.projects.get(id)?.path.as_deref())
        .collect();
    let folder = folders.first().map_or_else(|| app.paths.task_dir.clone(), std::path::PathBuf::from);
    let more = if folders.len() > 1 { format!(", with {} workspaces", folders.len()) } else { String::new() };
    format!("Not started yet · starts in {}{more}", app.paths.display(&folder))
}

/// A box of `height` rows in the middle of `area`, at most `width` columns
/// wide (Herdr's popup border is the only outer border, so a dialog alone
/// fills the popup when it is smaller); returns the area inside its border.
pub fn dialog_box(frame: &mut Frame, area: Rect, width: u16, height: u16) -> Rect {
    let [_, row, _] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(height.min(area.height)), Constraint::Fill(1)])
            .areas(area);
    let [_, rect, _] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(area.width.min(width)), Constraint::Fill(1)])
            .areas(row);
    let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(ACCENT));
    frame.render_widget(Clear, rect);
    let inside = block.inner(rect).inner(Margin { horizontal: 1, vertical: 0 });
    frame.render_widget(block, rect);
    inside
}

fn draw_screen(frame: &mut Frame, screen: &Screen, area: Rect, tz: &TimeZone, now: i64, paths: &Paths) {
    match screen {
        Screen::ConfirmArchive { title, .. } => draw_archive(frame, title, area),
        Screen::ConfirmDelete { title, .. } => draw_delete(frame, title, area),
        Screen::ConfirmDiscard => draw_discard(frame, area),
        Screen::Snooze(snooze) => draw_snooze(frame, snooze, area, tz, now),
        Screen::RemoveProject(dialog) => draw_remove(frame, dialog, area),
        Screen::ConfirmFolder { project, folder } => draw_confirm_folder(frame, project, folder, area),
        Screen::AddProject(dialog) => dialog.draw(frame, area, paths),
        Screen::Settings(screen) => draw_settings(frame, screen, area),
        Screen::Form(form) => form.draw(frame, area, paths, now, tz),
        Screen::Finder(finder) => finder.draw(frame, area),
        Screen::Projects(screen) => screen.draw(frame, area, paths, now),
        Screen::Rename(dialog) => dialog.draw(frame, area),
        Screen::Help(help) => help.draw(frame, area),
    }
}

/// Lines in a dialog box as tall as they wrap to.
pub fn wrapped_dialog(frame: &mut Frame, area: Rect, width: u16, lines: Vec<Line<'static>>) -> Rect {
    let inner = (area.width.min(width).saturating_sub(4)).max(1) as usize; // less border and margin
    let rows: usize = lines.iter().map(|line| wrapped_rows(line, inner)).sum();
    let inside = dialog_box(frame, area, width, rows as u16 + 2);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inside);
    inside
}

/// How many rows `line` takes wrapped at words in `width` columns, as a
/// paragraph wraps it (a word wider than a row takes several).
pub fn wrapped_rows(line: &Line, width: usize) -> usize {
    let width = width.max(1);
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let (mut rows, mut used) = (1, 0);
    for word in text.split(' ') {
        let len = word.chars().count();
        match used {
            0 if len <= width => used = len,
            _ if used > 0 && used + 1 + len <= width => used += 1 + len,
            _ => {
                rows += usize::from(used > 0) + len.saturating_sub(1) / width;
                used = len - len.saturating_sub(1) / width * width;
            }
        }
    }
    rows
}

/// `text` wrapped at words in `width` columns, every line after `indent` spaces.
pub fn indented(text: &str, width: usize, indent: usize) -> Vec<String> {
    let room = width.saturating_sub(indent).max(1);
    let mut lines: Vec<String> = vec![];
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > room {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    lines.push(line);
    lines.into_iter().map(|line| format!("{}{line}", " ".repeat(indent))).collect()
}

/// A project's name as a badge in `width` cells (`◆ name`, cut with "…"),
/// the letters a search matched underlined (R1.6).
pub fn name_badge(name: &str, width: usize, matched: &[usize]) -> Vec<Span<'static>> {
    let chars: Vec<char> = name.chars().collect();
    let room = width.saturating_sub(4).max(1);
    let cut = chars.len() > width.saturating_sub(3);
    let shown: Vec<char> = if cut { chars[..room.min(chars.len())].to_vec() } else { chars };
    let mut spans = vec![colored("◆ ", PROJECT)];
    let style = Style::new().fg(PROJECT);
    for (i, c) in shown.iter().enumerate() {
        let style =
            if matched.contains(&i) { style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED) } else { style };
        spans.push(Span::styled(c.to_string(), style));
    }
    if cut {
        spans.push(colored("…", PROJECT));
    }
    let used: usize = spans.iter().map(Span::width).sum();
    spans.push(Span::raw(" ".repeat(width.saturating_sub(used).max(1))));
    spans
}

/// A project on one line: its badge, its kind and where it is.
pub fn project_line(project: &Project, matched: &[usize], missing: bool, paths: &Paths) -> Vec<Span<'static>> {
    let mut spans = name_badge(&project.name, 14, matched);
    let gone = if missing { " (folder missing)" } else { "" };
    spans.push(muted(format!("{:<4} {}{gone}", project.kind().name(), paths.place(project))));
    spans
}

/// A one-line text field: its label (`width` columns), then what was typed
/// (or the placeholder), and ghost text after it. A value longer than
/// `room` columns scrolls to keep the cursor in view. Returns the line, and
/// the column the cursor goes in, from the start of the line.
pub fn field_line<'a>(
    label: &str,
    width: usize,
    input: &tui_input::Input,
    placeholder: &str,
    ghost: Option<&str>,
    focused: bool,
    room: usize,
) -> (Line<'a>, u16) {
    let style = if focused { Style::new().fg(ACCENT).bold() } else { Style::new().fg(MUTED) };
    let mut spans = vec![Span::styled(format!("{label:<width$}"), style)];
    let scroll = input.visual_scroll(room.saturating_sub(1).max(1)); // a column for the cursor at the end
    match input.value() {
        "" => spans.push(muted(placeholder.to_string())),
        value => {
            spans.push(Span::raw(skip_columns(value, scroll)));
            if let Some(rest) = ghost.and_then(|g| g.strip_prefix(value)) {
                spans.push(muted(rest.to_string()));
            }
        }
    }
    (Line::from(spans), (width + input.visual_cursor() - scroll) as u16)
}

/// `text` without its first `columns` display columns.
fn skip_columns(text: &str, columns: usize) -> String {
    let mut skipped = 0;
    let rest = text.char_indices().find(|(_, c)| {
        let past = skipped >= columns;
        skipped += Span::raw(c.to_string()).width();
        past
    });
    rest.map_or(String::new(), |(at, _)| text[at..].to_string())
}

/// Giving a folder changes which workspaces count as the project in every
/// task, so the question says so (R10.5, UX principle 10).
fn draw_confirm_folder(frame: &mut Frame, project: &str, folder: &str, area: Rect) {
    let lines = vec![
        Line::from(Span::styled(format!("Give {folder} to {project}?"), Style::new().bold())),
        Line::default(),
        Line::from(muted(format!(
            "{project} has no folder. With this one, workspaces in it count as {project} in every task, and new tasks with {project} start there."
        ))),
        Line::default(),
        key_hints(&[("y", "give it"), ("n", "keep it without a folder"), ("esc", "cancel")]),
    ];
    wrapped_dialog(frame, area, DIALOG, lines);
}

/// The settings, each with what it does; a file with a mistake is shown
/// instead of toggles that would not save (R10.4).
fn draw_settings(frame: &mut Frame, screen: &SettingsScreen, area: Rect) {
    let mut lines = vec![
        Line::from(Span::styled("Settings", Style::new().bold())),
        Line::default(),
        Line::from(Span::styled("Herdr integration", Style::new().bold())),
    ];
    // A file with a mistake: the defaults are shown, and nothing can be toggled.
    let shown = screen.settings.clone().unwrap_or_default();
    for (i, setting) in Setting::ALL.into_iter().enumerate() {
        let mark = match shown.get(setting) {
            true if screen.settings.is_ok() => colored("● ", SUCCESS),
            true => muted("● "),
            false => muted("○ "),
        };
        let style = match screen.settings {
            Ok(_) if i == screen.selected => Style::new().add_modifier(Modifier::REVERSED),
            Ok(_) => Style::new(),
            Err(_) => Style::new().fg(MUTED),
        };
        lines.push(Line::from(vec![mark, Span::styled(setting.label(), style)]));
        let width = area.width.min(WIDE).saturating_sub(4) as usize; // inside the border and margin
        lines.extend(indented(setting.help(), width, 2).into_iter().map(|line| Line::from(muted(line))));
    }
    lines.push(Line::default());
    lines.push(Line::from(match (&screen.settings, &screen.failed) {
        (Err(e), _) => colored(e.clone(), ERROR),
        (_, Some(e)) => colored(format!("Not saved: {e}"), ERROR),
        _ if screen.saving => muted("Saving…"),
        _ => muted(format!("Saved in {}; the daemon reads it on every pass.", screen.file)),
    }));
    lines.push(Line::default());
    lines.push(key_hints(&[("enter / space", "toggle"), ("esc", "close")]));
    wrapped_dialog(frame, area, WIDE, lines);
}

/// Esc in a form with something typed in it (R1.1).
fn draw_discard(frame: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(bold("Discard what you typed?")),
        Line::default(),
        Line::from(vec![
            Span::styled("y", Style::new().fg(ERROR).bold()),
            muted(" discard   "),
            Span::styled("n", Style::new().fg(ACCENT).bold()),
            muted(" keep editing"),
        ]),
    ];
    wrapped_dialog(frame, area, DIALOG, lines);
}

/// Deleting cannot be undone, and the question says what goes (UX principle 10).
fn draw_delete(frame: &mut Frame, title: &str, area: Rect) {
    let lines = vec![
        Line::from(Span::styled(format!("Delete “{title}”?"), Style::new().bold())),
        Line::default(),
        Line::from(muted(
            "Stops its session and removes its layout, agents and notes. Its project stays. This cannot be undone.",
        )),
        Line::default(),
        Line::from(vec![
            Span::styled("y", Style::new().fg(ERROR).bold()),
            muted(" delete   "),
            Span::styled("n", Style::new().fg(ACCENT).bold()),
            muted(" keep"),
        ]),
    ];
    wrapped_dialog(frame, area, DIALOG, lines);
}

/// What removing a project does to its clone (R8.7): only a clean clone of
/// itm's own is offered for deleting, and keeping it comes first.
fn draw_remove(frame: &mut Frame, dialog: &RemoveDialog, area: Rect) {
    let folder = dialog.folder.clone().unwrap_or_default();
    let mut lines = vec![Line::from(Span::styled(format!("Remove project “{}”?", dialog.name), Style::new().bold()))];
    lines.push(Line::default());
    if !dialog.users.is_empty() {
        lines.push(Line::from(colored(
            format!("Used by {} task(s), archived ones included:", dialog.users.len()),
            ERROR,
        )));
        lines.extend(dialog.users.iter().map(|title| Line::from(format!("  · {title}"))));
        lines.push(Line::default());
        lines.push(Line::from(muted(
            "Remove it from those tasks (ctrl+e) or delete them first. Removing a project never touches a task.",
        )));
    } else {
        match (&dialog.kind, &dialog.clone) {
            (ProjectKind::Name, _) => lines.push(Line::from(muted("It has no folder; nothing is deleted."))),
            (ProjectKind::Dir, _) => {
                lines.push(Line::from(muted(format!("Forgets the folder {folder}. Nothing is deleted."))))
            }
            (_, Some(CloneState::Missing)) => lines.push(Line::from(muted("Its clone is already gone."))),
            (_, Some(CloneState::Deletable { local_work: Some(work) })) => {
                lines.push(Line::from(vec![muted("Clone: "), Span::raw(folder)]));
                lines.push(Line::from(colored(format!("It has {work}, so the clone is kept."), WARNING)));
                let force = format!("To delete it anyway: itm project remove {} --delete-clone", dialog.name);
                lines.push(Line::from(muted(force)));
            }
            (_, Some(CloneState::Deletable { local_work: None })) => {
                lines.push(Line::from(vec![muted("Clone: "), Span::raw(folder)]));
            }
            _ => {
                lines.push(Line::from(muted(format!("Its folder {folder} is kept: it is not a clone itm may delete."))))
            }
        }
        lines.push(Line::default());
        let clean = dialog.choices.len() > 1;
        let with_work = matches!(dialog.clone, Some(CloneState::Deletable { local_work: Some(_) }));
        for (i, choice) in dialog.choices.iter().enumerate() {
            let (text, color) = match choice {
                CloneChoice::Keep if clean => ("Keep the clone  (default)", Color::Reset),
                CloneChoice::Keep if with_work => ("Forget the project, keep the clone", Color::Reset),
                CloneChoice::Keep => ("Forget the project", Color::Reset),
                CloneChoice::IfClean | CloneChoice::Delete => ("Delete the clone", ERROR),
            };
            let style = Style::new().fg(color);
            let style = if i == dialog.selected { style.add_modifier(Modifier::REVERSED) } else { style };
            lines.push(Line::from(Span::styled(format!(" {text} "), style)));
        }
    }
    lines.push(Line::default());
    let keys: &[(&str, &str)] = match dialog.choices.is_empty() {
        true => &[("esc", "back")],
        false => &[("enter", "remove"), ("esc", "cancel, nothing changes")],
    };
    lines.push(key_hints(keys));
    wrapped_dialog(frame, area, WIDE, lines);
}

/// Destructive actions confirm and say what will be lost (UX principle 10).
fn draw_archive(frame: &mut Frame, title: &str, area: Rect) {
    let lines = vec![
        Line::from(Span::styled(format!("Archive “{title}”?"), Style::new().bold())),
        Line::default(),
        Line::from(muted(
            "Stops its session and the programs running in it. Reopening restores the layout and resumes agents that support it.",
        )),
        Line::default(),
        Line::from(vec![
            Span::styled("y", Style::new().fg(WARNING).bold()),
            muted(" archive   "),
            Span::styled("n", Style::new().fg(ACCENT).bold()),
            muted(" keep"),
        ]),
    ];
    wrapped_dialog(frame, area, DIALOG, lines);
}

/// Presets before free text, sorted by time; free text keeps working (UX principle 9).
fn draw_snooze(frame: &mut Frame, dialog: &SnoozeDialog, area: Rect, tz: &TimeZone, now: i64) {
    let inside = dialog_box(frame, area, DIALOG, dialog.presets.len() as u16 + 9);
    let [title, _, typed, _, presets, _, error, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(dialog.presets.len() as u16),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inside);
    frame.render_widget(Paragraph::new(Span::styled(format!("Snooze “{}”", dialog.title), Style::new().bold())), title);
    let text = match dialog.typed.value() {
        "" => muted("Type 2h, 14:30, fri 9:00… or pick below"),
        value => Span::raw(value.to_string()),
    };
    frame.render_widget(Paragraph::new(Line::from(vec![colored("› ", ACCENT), text])), typed);
    frame.set_cursor_position((typed.x + 2 + dialog.typed.visual_cursor() as u16, typed.y));
    let rows = dialog.presets.iter().enumerate().map(|(i, (label, until))| {
        let (when, color) = match until {
            Until::Time(ts) => (day_and_clock(*ts, now, tz), Color::Reset),
            Until::Agent => ("no timer".to_string(), WARNING),
        };
        let width = presets.width as usize;
        let line = format!(" {label:<26}{when:>w$} ", w = width.saturating_sub(28));
        let style = Style::new().fg(color);
        Line::from(Span::styled(
            line,
            if i == dialog.selected { style.add_modifier(Modifier::REVERSED) } else { style },
        ))
    });
    frame.render_widget(Paragraph::new(rows.collect::<Vec<_>>()), presets);
    if let Some(e) = &dialog.error {
        frame.render_widget(Paragraph::new(colored(e.clone(), ERROR)), error);
    }
    frame.render_widget(Paragraph::new(key_hints(&[("enter", "snooze"), ("↑↓", "pick"), ("esc", "cancel")])), keys);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::tests::{NOW, app, ctrl, key, snapshot, with_projects};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyCode;

    fn screen(app: &mut App) -> String {
        sized(app, 110, 22)
    }

    fn sized(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app, NOW)).unwrap();
        let buffer = terminal.backend().buffer();
        let rows = buffer.content.chunks(buffer.area.width as usize);
        rows.map(|cells| cells.iter().map(|c| c.symbol()).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_inbox() {
        insta::assert_snapshot!(screen(&mut app(snapshot())));
    }

    #[test]
    fn a_snoozed_task_says_when_it_wakes_and_what_enter_does() {
        let mut app = app(snapshot());
        app.on_key(key(KeyCode::Tab));
        let screen = screen(&mut app);
        assert!(screen.contains("◔ Later") && screen.contains("wakes in 2h"), "{screen}");
        assert!(screen.contains("◔ snoozed until 10:00 (in 2h)"), "{screen}");
        assert!(screen.contains("enter wake & open"), "{screen}");
    }

    #[test]
    fn the_archive_dialog() {
        let mut app = app(snapshot());
        app.on_key(ctrl('a'));
        insta::assert_snapshot!(screen(&mut app));
    }

    #[test]
    fn the_delete_dialog() {
        let mut app = app(snapshot());
        app.on_key(ctrl('d'));
        insta::assert_snapshot!(screen(&mut app));
    }

    /// R10.4: the settings, each with what it does, and where they are saved.
    #[test]
    fn the_settings_screen() {
        let mut app = app(snapshot());
        app.on_key(ctrl('s'));
        insta::assert_snapshot!(screen(&mut app));
    }

    /// R10.4: a file with a mistake is shown, with the defaults and nothing to toggle.
    #[test]
    fn the_settings_screen_with_a_mistake_in_the_file() {
        let mut snap = snapshot();
        snap.settings = Err("~/.config/inbox-task-manager/config.toml: unknown setting herdr.nope".into());
        let mut app = app(snap);
        app.on_key(ctrl('s'));
        insta::assert_snapshot!(screen(&mut app));
    }

    /// R10.5: the add dialog, filled with the workspace's folder and name.
    #[test]
    fn the_add_dialog() {
        use crate::tui::add::AddDialog;
        let mut app = app(snapshot());
        let mut dialog = AddDialog::folder("~/code/new-thing", "My Thing");
        dialog.said = Some(crate::tui::add::Said::Failed(
            "project already exists: My Thing (no folder); choose another name".into(),
        ));
        app.screens.push(Screen::AddProject(Box::new(dialog)));
        insta::assert_snapshot!(screen(&mut app));
    }

    /// R10.5: the folder question says what giving the folder changes.
    #[test]
    fn the_folder_question() {
        let mut app = app(snapshot());
        app.screens.push(Screen::ConfirmFolder { project: "Client X".into(), folder: "~/code/client-x".into() });
        insta::assert_snapshot!(screen(&mut app));
    }

    /// R2.3, R10.3: a running task's preview says which projects have their workspace open.
    #[test]
    fn the_preview_says_which_workspaces_are_open() {
        use crate::store::Project;
        let mut snap = snapshot();
        for (id, name) in [("p1", "api"), ("p2", "web")] {
            let path = Some(format!("/r/home/code/{name}"));
            let project =
                Project { name: name.into(), path, url: None, added_at: 1, last_used_at: 1, extra: <_>::default() };
            snap.data.projects.insert(id.into(), project);
        }
        let task = snap.data.tasks.get_mut("task-fix-login").unwrap();
        (task.projects, task.workspace_projects) = (vec!["p1".into(), "p2".into()], vec!["p2".into()]);
        let text = screen(&mut app(snap.clone()));
        assert!(text.contains("◆ web  dir  workspace open"), "{text}");
        assert!(text.contains("◆ api  dir\n") || text.contains("◆ api  dir "), "{text}");
        assert!(!text.contains("◆ api  dir  workspace open"), "{text}");
        snap.sessions.get_mut("task-fix-login").unwrap().running = false;
        assert!(!screen(&mut app(snap)).contains("workspace open"), "a stopped session has none open");
    }

    /// R8.7: a clean clone is offered for deleting, keeping it first.
    #[test]
    fn the_remove_dialog() {
        use crate::projects::{CloneState, RemovalPlan};
        use crate::store::Project;
        use crate::tui::screen::RemoveDialog;
        let project = Project {
            name: "api".into(),
            path: Some("/r/data/projects/github.com/acme/api".into()),
            url: Some("git@github.com:acme/api.git".into()),
            added_at: 1,
            last_used_at: 1,
            extra: Default::default(),
        };
        let clean = Some(CloneState::Deletable { local_work: None });
        let plan = RemovalPlan { id: "p1".into(), project, users: vec![], clone: clean };
        let mut app = app(snapshot());
        app.screens.push(Screen::RemoveProject(RemoveDialog::new(&plan, &app.paths)));
        insta::assert_snapshot!(screen(&mut app));
        let busy = RemovalPlan { users: vec!["Fix login".into()], ..plan };
        app.screens = vec![Screen::RemoveProject(RemoveDialog::new(&busy, &app.paths))];
        let shown = screen(&mut app);
        assert!(
            shown.contains("Used by 1 task(s), archived ones included:") && shown.contains("· Fix login"),
            "{shown}"
        );
        assert!(shown.contains("esc back") && !shown.contains("enter remove"), "{shown}");
    }

    /// R9.5: the preview says when the notes changed and shows their last
    /// lines, without what itm started them with.
    #[test]
    fn the_preview_shows_the_end_of_the_notes() {
        use crate::tui::app::NotesTail;
        let mut snap = snapshot();
        let lines = [
            "# Fix login",
            "*Created 2026-11-05 10:00 · no project · task-fix-login*",
            "after 5 min",
            "## 2026-11-05 11:00 · Note",
            "tried the cookie flag",
            "Safari drops it",
        ];
        let tail = NotesTail { modified: NOW - 7200, lines: lines.map(String::from).to_vec() };
        snap.notes.insert("task-fix-login".into(), tail);
        let shown = screen(&mut app(snap.clone()));
        assert!(shown.contains("Notes · edited 2h ago"), "{shown}");
        for line in ["## 2026-11-05 11:00 · Note", "tried the cookie flag", "Safari drops it"] {
            assert!(shown.contains(&format!("  {line}")), "{line}: {shown}");
        }
        assert!(!shown.contains("*Created") && !shown.contains("# Fix login"), "{shown}");
        let started = NotesTail { modified: NOW, lines: lines[..3].iter().map(|l| l.to_string()).collect() };
        snap.notes.insert("task-fix-login".into(), started);
        assert!(screen(&mut app(snap)).contains("Notes · nothing written yet"), "the description is no note");
    }

    #[test]
    fn the_snooze_dialog() {
        let mut app = app(snapshot());
        app.on_key(key(KeyCode::Down));
        app.on_key(ctrl('z'));
        let [Screen::Snooze(dialog)] = &mut app.screens[..] else { panic!("no dialog") };
        // Fixed times, so the screen does not depend on the clock.
        dialog.presets = vec![("1 hour", Until::Time(NOW + 3600)), ("Until an agent needs me", Until::Agent)];
        insta::assert_snapshot!(screen(&mut app));
    }

    #[test]
    fn unknown_waits_and_sessions_are_said_not_hidden() {
        let mut snap = snapshot();
        snap.status = Default::default();
        let screen_text = screen(&mut app(snap.clone()));
        assert!(screen_text.contains("daemon not running · waits unknown"), "{screen_text}");
        assert!(screen_text.contains("Agents unknown: the daemon is not"), "{screen_text}");
        snap.listed = false;
        snap.sessions.clear();
        let screen_text = screen(&mut app(snap));
        assert!(screen_text.contains("Herdr not answering · sessions unknown · daemon not running"), "{screen_text}");
    }

    /// A narrow pane (a small Herdr popup) shortens titles and drops the
    /// project before it cuts what a task is doing, and wraps the keys.
    #[test]
    fn a_narrow_pane_keeps_each_status_and_every_key() {
        let mut snap = snapshot();
        snap.current = Some("task-write-docs".into());
        let mut app = app(snap);
        let mut terminal = Terminal::new(TestBackend::new(64, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app, NOW)).unwrap();
        let screen = format!("{}", terminal.backend());
        for status in ["claude waiting · 4m", "working · here", "not started"] {
            assert!(screen.contains(status), "{status} cut:\n{screen}");
        }
        for (key, what) in KEYS {
            assert!(screen.contains(&format!("{key} {what}")), "{key} cut:\n{screen}");
        }
        insta::assert_snapshot!(screen);
    }

    #[test]
    fn columns_give_way_title_first_then_project() {
        assert_eq!(Columns::fitting(120, 19), Columns::WIDE);
        assert_eq!(Columns::fitting(50, 19), Columns { title: 19, project: 12 });
        assert_eq!(Columns::fitting(40, 19), Columns { title: 21, project: 0 });
        assert_eq!(Columns::fitting(10, 19), Columns { title: 0, project: 0 });
    }

    /// R1.2: the preview says where a session that never ran starts: its first
    /// project whose folder is there (not a missing one, not a name only).
    #[test]
    fn a_task_that_never_ran_starts_in_its_first_project_with_a_folder() {
        let mut snap = snapshot();
        let project = |name: &str, path: Option<&str>| crate::store::Project {
            name: name.into(),
            path: path.map(Into::into),
            url: None,
            added_at: 0,
            last_used_at: 0,
            extra: Default::default(),
        };
        snap.data.projects.insert("p1".into(), project("Client X", None));
        snap.data.projects.insert("p2".into(), project("gone", Some("/r/home/gone")));
        snap.data.projects.insert("p3".into(), project("api", Some("/r/home/code/api")));
        snap.data.projects.insert("p4".into(), project("web", Some("/r/home/code/web")));
        snap.missing.insert("p2".into());
        snap.data.tasks.get_mut("task-old-idea").unwrap().projects = vec!["p1".into(), "p2".into(), "p3".into()];
        let mut app = app(snap.clone());
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        let shown = screen(&mut app);
        assert!(shown.contains("◆ Client X  name") && shown.contains("no folder"), "{shown}");
        assert!(shown.contains("Its folder is missing, so it gets"), "{shown}");
        let starts = |snap: &Snapshot| not_started(&app, snap, &snap.data.tasks["task-old-idea"]);
        assert_eq!(starts(&snap), "Not started yet · starts in ~/code/api");

        snap.data.tasks.get_mut("task-old-idea").unwrap().projects = vec!["p3".into(), "p4".into()];
        assert_eq!(starts(&snap), "Not started yet · starts in ~/code/api, with 2 workspaces");
        snap.data.tasks.get_mut("task-old-idea").unwrap().projects = vec!["p1".into()];
        assert_eq!(starts(&snap), "Not started yet · starts in ~");
    }

    #[test]
    fn before_the_first_load() {
        let now = super::super::app::Moment { instant: std::time::Instant::now(), epoch: NOW };
        let mut app = App::new(View::Inbox, crate::paths::Paths::under("/r".as_ref()), TimeZone::UTC, now);
        assert!(screen(&mut app).contains("Loading…"));
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }

    fn top(app: &mut App) -> &mut Screen {
        app.screens.last_mut().expect("a screen")
    }

    /// R1.1, R1.6: the New task form: what saving does under the fields, the
    /// projects (a missing folder, a name only), the focused field's hint.
    #[test]
    fn the_new_task_form() {
        let mut app = app(with_projects());
        app.on_key(ctrl('n'));
        typed(&mut app, "Plan launch");
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Tab));
        let ids = ["p-api", "p-gone", "p-x"].map(String::from).to_vec();
        if let Screen::Form(form) = top(&mut app) {
            form.returned(crate::tui::app::Action::Picked(ids, None));
        }
        insta::assert_snapshot!(sized(&mut app, 100, 30));
    }

    /// R5.4: the edit form says what a project added does to the running session.
    #[test]
    fn the_edit_form() {
        let mut app = app(with_projects());
        app.on_key(ctrl('e'));
        if let Screen::Form(form) = top(&mut app) {
            form.returned(crate::tui::app::Action::Picked(vec!["p-api".into(), "p-web".into(), "p-x".into()], None));
        }
        insta::assert_snapshot!(sized(&mut app, 100, 30));
    }

    /// UX principle 5: in a narrow popup (the form's is 70% wide) no key
    /// hint is cut, and what saving does wraps instead of being cut.
    #[test]
    fn a_narrow_form_cuts_no_key_hint() {
        let mut app = app(with_projects());
        app.alone = true; // as `itm new` in its popup
        app.screens = vec![];
        app.on_key(ctrl('n'));
        let shown = sized(&mut app, 56, 26);
        for hint in ["enter create", "tab next field", "^s create from any field", "esc cancel"] {
            assert!(shown.contains(hint), "{hint} cut:\n{shown}");
        }
        insta::assert_snapshot!(shown);
    }

    /// R10.4: a setting's help keeps its indent on every line it wraps to.
    #[test]
    fn help_text_keeps_its_indent_when_it_wraps() {
        assert_eq!(indented("one two three four", 11, 2), ["  one two", "  three", "  four"]);
        assert_eq!(indented("", 11, 2), ["  "]);
        let mut app = app(snapshot());
        app.on_key(ctrl('s'));
        let shown = screen(&mut app);
        assert!(shown.contains("│   folder Herdr names a workspace after)"), "{shown}");
    }

    /// A value longer than its field scrolls to keep the cursor in view; the
    /// cursor stays inside the field.
    #[test]
    fn a_long_value_scrolls_in_its_field() {
        let input = tui_input::Input::new("git@github.com:organization/a-long-repository-name.git".into());
        let (line, cursor) = field_line("Source", 8, &input, "", None, true, 20);
        let shown: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(shown, "Source  repository-name.git", "its end, where the cursor is");
        assert_eq!(cursor, 8 + 19);
        let (line, cursor) = field_line("Name", 8, &tui_input::Input::new("api".into()), "", None, true, 20);
        assert_eq!((line.spans[1].content.as_ref(), cursor), ("api", 11));
        assert_eq!(skip_columns("日本語abc", 4), "語abc", "wide characters take two columns");
    }

    /// R1.6: many projects give way to the field's hint, counted as "+n".
    #[test]
    fn the_project_field_fits_many_projects_and_its_hint() {
        let mut snap = with_projects();
        let mut ids = vec![];
        for n in 0..5 {
            let id = format!("p-long-{n}");
            let project = crate::tui::app::tests::project(&format!("project-with-a-long-name-{n}"), None, 1);
            snap.data.projects.insert(id.clone(), project);
            ids.push(id);
        }
        let mut app = app(snap);
        app.on_key(ctrl('n'));
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Tab));
        if let Screen::Form(form) = top(&mut app) {
            form.returned(crate::tui::app::Action::Picked(ids, None));
        }
        let shown = sized(&mut app, 90, 30);
        let field = shown.lines().find(|l| l.contains("Projects")).unwrap();
        assert!(field.contains("enter or type to add · ⌫ removes the last"), "the hint is never cut off: {field}");
        assert!(field.contains("+4") || field.contains("+3"), "the others are counted: {field}");
        assert!(field.ends_with('│'), "the field stays in its box: {field}");
    }

    /// R1.6: the finder over the form, a search typed on the field.
    #[test]
    fn the_finder() {
        let mut app = app(with_projects());
        app.on_key(ctrl('n'));
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Tab));
        typed(&mut app, "a");
        insta::assert_snapshot!(sized(&mut app, 100, 26));
    }

    /// R1.6: folders while their parent is read, then listed, a saved one named.
    #[test]
    fn the_finder_with_folders() {
        use crate::folders::Listing;
        let mut app = app(with_projects());
        app.on_key(ctrl('n'));
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Tab));
        typed(&mut app, "~/code/");
        while app.needs().is_some() {}
        let reading = sized(&mut app, 100, 26);
        assert!(reading.contains("Reading the folder…"), "{reading}");
        let names = ["api", "web", "new"].map(String::from).to_vec();
        let listing = Listing { real: "/r/home/code".into(), names, links: Default::default() };
        app.listed("~/code/", &Some(listing));
        insta::assert_snapshot!(sized(&mut app, 100, 26));
    }

    /// R8.3, R8.5: the add dialog's git kind once what was typed was checked.
    #[test]
    fn the_add_dialog_for_a_repository() {
        use crate::projects::Check;
        use crate::tui::add::AddDialog;
        let mut app = app(snapshot());
        let mut dialog =
            AddDialog::new(ProjectKind::Git, "https://github.com/acme/tool.git", "", super::super::screen::Field::Name);
        let folder = Some("/r/data/inbox-task-manager/projects/github.com/acme/tool".into());
        let check = Check { folder, default_name: "tool".into(), problem: None };
        dialog.check = Some((dialog.typed(), Ok(check)));
        app.screens.push(Screen::AddProject(Box::new(dialog)));
        insta::assert_snapshot!(screen(&mut app));
    }

    /// R8.8: a clone shows what git says; a failed one says why and how to go on.
    #[test]
    fn the_add_dialog_while_cloning_and_after_a_failure() {
        use crate::tui::add::{AddDialog, Busy, Said};
        let mut app = app(snapshot());
        let mut dialog =
            AddDialog::new(ProjectKind::Git, "https://github.com/acme/tool.git", "", super::super::screen::Field::Name);
        dialog.busy = Some(Busy::Cloning { last: Some("Receiving objects:  50% (10/20)".into()), stopping: false });
        app.screens.push(Screen::AddProject(Box::new(dialog.clone())));
        insta::assert_snapshot!(screen(&mut app));
        dialog.busy = None;
        dialog.said = Some(Said::Failed(crate::projects::clone_failure(
            "fatal: could not read Username\nitm: could not clone x\n",
        )));
        app.screens = vec![Screen::AddProject(Box::new(dialog))];
        let shown = screen(&mut app);
        assert!(shown.contains("could not clone x") && shown.contains("in a terminal"), "{shown}");
    }

    /// R8.3: the Projects screen, a name-only project highlighted.
    #[test]
    fn the_projects_screen() {
        let mut app = app(with_projects());
        app.on_key(ctrl('p'));
        for _ in 0..4 {
            app.on_key(key(KeyCode::Down));
        }
        if let Screen::Projects(screen) = top(&mut app) {
            screen.message = Some(("Renamed api2 to api.".into(), false));
        }
        insta::assert_snapshot!(sized(&mut app, 110, 16));
    }

    /// R8.3: in a narrow popup where a project is gives way first; when it was used is never cut.
    #[test]
    fn a_narrow_projects_screen_cuts_no_time() {
        let mut app = app(with_projects());
        app.on_key(ctrl('p'));
        for width in [70, 64] {
            let shown = sized(&mut app, width, 16);
            assert!(shown.contains("used 1m ago · folder missing"), "{width}:\n{shown}");
            assert!(shown.contains("^d remove"), "{width}:\n{shown}");
        }
        assert!(sized(&mut app, 100, 16).contains("~/code/api"), "the place, where it fits");
    }

    #[test]
    fn the_rename_dialog() {
        let mut app = app(with_projects());
        app.on_key(ctrl('p'));
        app.on_key(ctrl('r'));
        typed(&mut app, "2");
        insta::assert_snapshot!(screen(&mut app));
    }

    /// UX principle 5: every key, scrolled in a short popup with the close hint kept.
    #[test]
    fn the_help() {
        let mut app = app(snapshot());
        app.on_key(key(KeyCode::Char('?')));
        insta::assert_snapshot!(screen(&mut app));
        for _ in 0..100 {
            app.on_key(key(KeyCode::Down));
        }
        let end = screen(&mut app);
        assert!(
            end.contains("ctrl+d               remove") && end.contains("esc close"),
            "scrolled to the end:\n{end}"
        );
        app.on_key(key(KeyCode::Up));
        let up = screen(&mut app);
        assert_ne!(up, end, "one ↑ moves at once from the end");
    }

    /// R1.1: Esc in a form with something typed in it asks first.
    #[test]
    fn the_discard_question() {
        let mut app = app(snapshot());
        app.on_key(ctrl('n'));
        typed(&mut app, "x");
        app.on_key(key(KeyCode::Esc));
        insta::assert_snapshot!(sized(&mut app, 100, 26));
    }

    /// R6.2: a filter that names no project says so, and offers to create the task without one.
    #[test]
    fn an_unknown_project_filter() {
        let mut app = app(with_projects());
        typed(&mut app, "@nope Fix");
        let shown = screen(&mut app);
        assert!(shown.contains("No project named “nope”") && shown.contains("+ Create task “Fix”"), "{shown}");
        assert!(!shown.contains("“Fix” in"), "{shown}");
        app.search = tui_input::Input::default();
        typed(&mut app, "@api Rate limits");
        assert!(screen(&mut app).contains("+ Create task “Rate limits” in ◆ api"));
    }
}
