//! Drawing the switcher (DESIGN.md section 3): views with counts, the
//! inbox list, a preview of the highlighted task, at most seven keys.
//! Pure: what is drawn depends only on the app and `now`.

use jiff::tz::TimeZone;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Wrap};

use super::app::{App, Row, Snapshot, View};
use super::screen::{Screen, SnoozeDialog};
use crate::snooze::Until;
use crate::status::AgentState;
use crate::store::{State, Task};
use crate::times::{age, day_and_clock, rel};

const ACCENT: Color = Color::Cyan;
const PROJECT: Color = Color::Magenta;
const ERROR: Color = Color::Red;
const WARNING: Color = Color::Yellow;
const SUCCESS: Color = Color::Green;
const MUTED: Color = Color::DarkGray;

/// At most seven keys (UX principle 5); arrows move, as everywhere.
const KEYS: [(&str, &str); 7] = [
    ("enter", "open"),
    ("tab", "view"),
    ("alt+1…9", "row"),
    ("^z", "snooze"),
    ("^u", "wake"),
    ("^a", "archive"),
    ("esc", "close"),
];

fn muted(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::new().fg(MUTED))
}

fn colored(text: impl Into<String>, color: Color) -> Span<'static> {
    Span::styled(text.into(), Style::new().fg(color))
}

fn key_hints(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![];
    for (key, what) in keys {
        spans.push(Span::styled(key.to_string(), Style::new().fg(ACCENT).bold()));
        spans.push(muted(format!(" {what}  ")));
    }
    Line::from(spans)
}

/// Key hints on as many lines as `width` needs, never cutting one (UX principle 5).
fn wrapped_key_hints(keys: &[(&str, &str)], width: u16) -> Vec<Line<'static>> {
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
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max { text.into() } else { text.chars().take(max - 1).chain(['…']).collect() }
}

pub fn draw(frame: &mut Frame, app: &mut App, now: i64) {
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
        draw_screen(frame, screen, area, &app.tz, now);
    }
}

/// A screen with nothing behind it (`itm snooze` without a time).
pub fn draw_alone(frame: &mut Frame, screen: &Screen, tz: &TimeZone, now: i64) {
    draw_screen(frame, screen, frame.area(), tz, now);
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
    let text =
        if typed.is_empty() { muted("Search tasks · or type a new title") } else { Span::raw(typed.to_string()) };
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
            (Row::Create, _) => ListItem::new(Line::from(vec![
                colored("+ ", ACCENT),
                Span::raw(format!("Create task “{}”", app.search.value().trim())),
            ])),
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
    let [text, keys] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);
    let (lines, enter) = preview(app, now);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), text);
    if let Some(enter) = enter {
        frame.render_widget(Paragraph::new(key_hints(&[("enter", enter)])), keys);
    }
}

fn preview(app: &App, now: i64) -> (Vec<Line<'static>>, Option<&'static str>) {
    let Some(snap) = &app.snapshot else { return (vec![Line::from(muted("Loading…"))], None) };
    let (name, task) = match app.highlighted() {
        Some(Row::Task(name)) => (name, &snap.data.tasks[name]),
        Some(Row::Create) => {
            let title = app.search.value().trim();
            return (
                vec![Line::from(muted(format!("No task matches. Press enter to create “{title}”.")))],
                Some("create"),
            );
        }
        _ if snap.data.tasks.is_empty() => {
            return (vec![Line::from(muted("No tasks yet. Type a title and press enter."))], None);
        }
        _ => return (vec![], None),
    };
    let mut lines = vec![Line::from(Span::styled(task.title.clone(), Style::new().bold()))];
    for (id, project) in task.projects.iter().filter_map(|id| Some((id, snap.data.projects.get(id)?))) {
        lines.push(Line::from(vec![
            colored(format!("◆ {}", project.name), PROJECT),
            muted(format!("  {}", project.kind())),
        ]));
        lines.push(Line::from(muted(format!("  {}", app.paths.place(project)))));
        if snap.missing.contains(id) {
            lines.push(Line::from(colored("  Its folder is missing, so it gets no workspace.", ERROR)));
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
    lines.extend([Line::default(), Line::from(muted(format!("used {}", rel(task.last_used_at, now))))]);
    let enter = match task.state {
        State::Active => "open",
        State::Snoozed => "wake & open",
        State::Archived => "reopen",
    };
    (lines, Some(enter))
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

/// A box of `height` rows in the middle of `area`, at most 64 columns wide;
/// returns the area inside its border.
fn dialog_box(frame: &mut Frame, area: Rect, height: u16) -> Rect {
    let [_, row, _] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(height.min(area.height)), Constraint::Fill(1)])
            .areas(area);
    let [_, rect, _] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(area.width.min(64)), Constraint::Fill(1)])
            .areas(row);
    let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(ACCENT));
    frame.render_widget(Clear, rect);
    let inside = block.inner(rect).inner(Margin { horizontal: 1, vertical: 0 });
    frame.render_widget(block, rect);
    inside
}

fn draw_screen(frame: &mut Frame, screen: &Screen, area: Rect, tz: &TimeZone, now: i64) {
    match screen {
        Screen::ConfirmArchive { title, .. } => draw_archive(frame, title, area),
        Screen::Snooze(snooze) => draw_snooze(frame, snooze, area, tz, now),
    }
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
    let inside = dialog_box(frame, area, 9);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inside);
}

/// Presets before free text, sorted by time; free text keeps working (UX principle 9).
fn draw_snooze(frame: &mut Frame, dialog: &SnoozeDialog, area: Rect, tz: &TimeZone, now: i64) {
    let inside = dialog_box(frame, area, dialog.presets.len() as u16 + 9);
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
    use crate::tui::app::tests::{NOW, app, ctrl, key, snapshot};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyCode;

    fn screen(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 22)).unwrap();
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
}
