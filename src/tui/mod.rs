//! The switcher (`itm`, `itm ui`): a terminal UI over ratatui, and the
//! screens the CLI opens alone: the New task and edit forms, snooze
//! without a time, the delete question, what to do with a removed
//! project's clone, and the workspace hotkey's add dialog and folder
//! question.
//!
//! The event loop waits for a key, a worker's result or a deadline (the
//! 2 s refresh, a message expiring, a check once typing pauses). Anything
//! that can block (Herdr calls, the store lock, git, a hand-off, a folder
//! listing) runs in a thread that sends its result over a channel. What
//! the loop needs from the world is injected (where time and keys come
//! from, [`Events`]; what workers do, [`Work`]; the terminal backend), so
//! its tests run it on a test backend with a scripted clock. Choosing a
//! task returns it to the caller, which switches once the terminal is
//! restored.

mod add;
mod app;
mod clones;
mod completion;
mod draw;
mod finder;
mod form;
mod help;
mod projects;
mod screen;

use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use jiff::tz::TimeZone;
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEvent, KeyEventKind};
use ratatui::crossterm::execute;

pub use app::View;
use app::{Action, App, Job, Moment, NotesTail, Reply, Snapshot};
pub use finder::Saved;
pub use form::{Editing, Effect, TaskForm};
use screen::{Need, RemoveDialog, Screen, SnoozeDialog};

use crate::error::{Context, Error, Result};
use crate::folders::{Listing, SCAN_LIMIT, list_folders};
use crate::itm::Itm;
use crate::paths::Paths;
use crate::projects::{CloneChoice, RemovalPlan};
use crate::snooze::{self, Until};
use crate::system::ExitSignals;
use crate::tasks::Edit;
use clones::{Clones, CloseOnDrop};

const REFRESH: Duration = Duration::from_secs(2);
/// While a worker runs, how long to wait for a key before looking for its result.
const BUSY_POLL: Duration = Duration::from_millis(10);
/// While a clone runs, how often the loop looks for a signal (a closing popup) to stop it.
const SIGNAL_POLL: Duration = Duration::from_millis(100);

/// What wakes the loop.
#[derive(Debug)]
pub enum Input {
    Key(KeyEvent),
    /// Text pasted into the terminal (bracketed paste).
    Paste(String),
    /// The terminal changed size: draw again.
    Resized,
    /// The terminal could not be read.
    Failed(Error),
    Loaded(Result<Snapshot>),
    Done(Job, Result<Reply>),
    /// A folder listing a screen asked for (None: it cannot be read).
    Listed(String, Option<Listing>),
    /// What a clone child said last.
    Cloning(String),
}

/// Where the loop's time and keys come from.
pub trait Events {
    fn now(&self) -> Moment;
    /// The next input, a key or what a worker sent on `results`, or None
    /// once `deadline` passed. `busy`: a worker runs, so a result may come.
    fn next(&self, results: &Receiver<Input>, deadline: Instant, busy: bool) -> Option<Input>;
    /// Run `program` with the terminal as it was before the switcher took
    /// it (an editor), then take it back.
    fn suspended(&self, program: &mut dyn FnMut() -> Result<()>) -> Result<()>;
}

/// The terminal and the system clock. Keys are read only while the loop
/// waits, on its own thread: nothing reads the terminal once the switcher
/// has closed, where a reader left running would take keys meant for the
/// `herdr session attach` that follows a switch (and could not be stopped
/// at once, which would delay every switch).
pub struct TerminalEvents;

impl Events for TerminalEvents {
    fn now(&self) -> Moment {
        Moment::now()
    }

    fn next(&self, results: &Receiver<Input>, deadline: Instant, busy: bool) -> Option<Input> {
        loop {
            if let Ok(input) = results.try_recv() {
                return Some(input);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            // Idle, only a key or the deadline can come: wait for them. Busy, look at the results often.
            let wait = if busy { left.min(BUSY_POLL) } else { left };
            match event::poll(wait).and_then(|ready| ready.then(event::read).transpose()) {
                Ok(Some(Event::Key(key))) if key.kind == KeyEventKind::Press => return Some(Input::Key(key)),
                Ok(Some(Event::Paste(text))) => return Some(Input::Paste(text)),
                Ok(Some(Event::Resize(..))) => return Some(Input::Resized),
                Ok(_) => {}
                Err(e) => return Some(Input::Failed(Error::Other(format!("cannot read the terminal: {e}")))),
            }
        }
    }

    fn suspended(&self, program: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        use ratatui::crossterm::terminal;
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
        ratatui::restore();
        let result = program();
        terminal::enable_raw_mode()
            .and_then(|()| execute!(std::io::stdout(), terminal::EnterAlternateScreen, EnableBracketedPaste))
            .context(|| "cannot take the terminal back".into())?;
        result
    }
}

/// What a job can use besides itm: where its children go, so they are
/// stopped when the UI ends, and how it says what it is doing.
pub struct Run {
    pub clones: Clones,
    said: Sender<Input>,
}

impl Run {
    pub fn say(&self, line: String) {
        let _ = self.said.send(Input::Cloning(line)); // nothing to tell once the UI closed
    }
}

/// The blocking work the switcher hands to workers.
pub trait Work: Send + Sync {
    /// Everything the switcher shows.
    fn load(&self) -> Result<Snapshot>;
    fn perform(&self, job: &Job, run: &Run) -> Result<Reply>;
    /// Open a task's notes in the editor, started first when missing (R9.4).
    fn open_notes(&self, session: &str) -> Result<()>;
}

impl Work for Itm {
    /// Adopts `task-*` sessions made elsewhere, writing the store only when
    /// there is something to adopt or expire, and only with a real session list.
    fn load(&self) -> Result<Snapshot> {
        let (sessions, listed) = match self.herdr.sessions() {
            Ok(sessions) => (sessions, true),
            Err(_) => (Default::default(), false),
        };
        let names = sessions.keys().map(String::as_str);
        let mut data = self.store().load()?;
        if listed && crate::tasks::needs_adoption(&data, names.clone()) {
            data = self.store().edit(|data| {
                crate::tasks::adopt_sessions(data, names, crate::times::now());
                Ok(data.clone())
            })?;
        }
        let (current, switch_problem) = match self.inside_herdr() {
            true => (self.current_session(&sessions), self.switchable_session(&sessions).err().map(|e| e.to_string())),
            false => (None, None),
        };
        let missing = crate::projects::missing_folders(&data);
        let folders = crate::projects::saved_folders(&data);
        let notes = (data.tasks.keys())
            .filter_map(|name| {
                let (modified, lines) = self.notes_tail(name, 12)?;
                let modified = modified.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
                Some((name.clone(), NotesTail { modified, lines }))
            })
            .collect();
        let settings = self.settings().map_err(|e| e.to_string());
        let status = self.status();
        Ok(Snapshot { data, sessions, status, current, switch_problem, listed, missing, folders, notes, settings })
    }

    fn perform(&self, job: &Job, run: &Run) -> Result<Reply> {
        let done = |result: Result<()>| result.map(|()| Reply::Done);
        match job {
            Job::Wake { session, .. } => done(self.wake_task(session)),
            Job::Archive { session, .. } => done(self.archive_task(session)),
            Job::Delete { session, .. } => done(self.delete_task(session)),
            Job::Create { title, description, projects, until } => {
                self.create_task(title, description.as_deref(), projects, *until).map(Reply::Created)
            }
            Job::Edit { session, title, description, add, remove, order } => {
                let edit = Edit {
                    title: Some(title.clone()),
                    description: Some(description.clone()),
                    add: add.clone(),
                    remove: remove.clone(),
                    order: order.clone(),
                    ..Edit::default()
                };
                self.edit_task(session, &edit).map(Reply::Said)
            }
            Job::Snooze { session, until, .. } => done(self.snooze_task(session, *until)),
            Job::SnoozeTime { text } => {
                Until::parse(text, crate::times::now(), &self.tz, crate::system::date_reads).map(Reply::Until)
            }
            Job::RemovalPlan { id } => self.removal_plan(id).map(Reply::Plan),
            Job::RemoveProject { id, choice, .. } => self.remove_project(id, *choice).map(Reply::Removed),
            Job::RenameProject { id, name } => {
                self.rename_project(id, name).map(|(old, new, note)| Reply::Renamed(old, new, note))
            }
            Job::AddProject { kind, source, name } => {
                let id = self.add_project(*kind, source, name.as_deref())?;
                let project = self.store().load()?.projects.get(&id).cloned();
                project.map(|p| Reply::Added(id, p)).ok_or_else(|| Error::Other("the project went again".into()))
            }
            Job::CheckProject { kind, source, name } => {
                self.check_project(*kind, source, name.as_deref()).map(Reply::Checked)
            }
            Job::Clone { url, name } => clones::clone_project(self, url, name.as_deref(), &run.clones, &|l| run.say(l)),
            Job::Setting { setting, on } => done(self.change_setting(*setting, *on)),
        }
    }

    fn open_notes(&self, session: &str) -> Result<()> {
        let data = self.store().load()?; // a task deleted meanwhile must not get its notes back
        let task = data.tasks.get(session).ok_or_else(|| Error::Other("this task no longer exists".into()))?;
        let path = self.ensure_notes(session, task, &data.projects)?;
        self.edit_notes(&path)
    }
}

/// Work for a dialog that asks for none (a question).
struct NoWork;

impl Work for NoWork {
    fn load(&self) -> Result<Snapshot> {
        Err(unexpected())
    }

    fn perform(&self, _: &Job, _: &Run) -> Result<Reply> {
        Err(unexpected())
    }

    fn open_notes(&self, _: &str) -> Result<()> {
        Err(unexpected())
    }
}

/// Run the switcher; returns the session to switch to, if one was chosen.
pub fn run(itm: &Arc<Itm>, view: View) -> Result<Option<String>> {
    // A measurement switch (scripts/measure.sh): quit right after the first frame with tasks.
    let exit_after_draw = std::env::var_os("ITM_EXIT_AFTER_DRAW").is_some_and(|v| v == "1");
    let app = App::new(view, itm.paths.clone(), itm.tz.clone(), Moment::now());
    let work: Arc<dyn Work> = Arc::clone(itm) as Arc<dyn Work>;
    let chosen = in_terminal(|terminal| event_loop(terminal, app, work, &TerminalEvents, exit_after_draw))?;
    Ok(match chosen {
        Some(Action::Switch(session)) => Some(session),
        _ => None,
    })
}

/// `screen` alone, with `work` doing what it asks for; the action it closed with.
fn alone(screen: Screen, paths: &Paths, tz: &TimeZone, work: Arc<dyn Work>) -> Result<Option<Action>> {
    let app = App::alone(screen, paths.clone(), tz.clone(), Moment::now());
    in_terminal(|terminal| event_loop(terminal, app, work, &TerminalEvents, false))
}

/// The snooze dialog alone (`itm snooze` without a time, R4.1): the chosen
/// snooze, or None when it was closed. "Until an agent needs me" is offered
/// only when `agent` (an agent could ever wake the task).
pub fn snooze_dialog(session: &str, title: &str, agent: bool, itm: &Arc<Itm>) -> Result<Option<Until>> {
    let presets = snooze::presets(crate::times::now(), &itm.tz, agent);
    let screen = Screen::Snooze(SnoozeDialog::new(session.into(), title.into(), presets));
    // Its work: a time typed that only GNU `date` reads.
    Ok(match alone(screen, &itm.paths, &itm.tz, Arc::clone(itm) as Arc<dyn Work>)? {
        Some(Action::Run(Job::Snooze { until, .. })) => Some(until),
        _ => None,
    })
}

/// The delete question alone (`itm delete`, R5.3): whether the user said yes.
pub fn confirm_delete(session: &str, title: &str, itm: &Itm) -> Result<bool> {
    let screen = Screen::ConfirmDelete { session: session.into(), title: title.into() };
    let chosen = alone(screen, &itm.paths, &itm.tz, Arc::new(NoWork))?;
    Ok(matches!(chosen, Some(Action::Run(Job::Delete { .. }))))
}

/// The workspace hotkey's question (R10.5): whether `project`, which has no
/// folder, gets `folder`; None when it was closed (nothing changes then).
pub fn confirm_folder(project: &str, folder: &str, itm: &Itm) -> Result<Option<bool>> {
    let screen = Screen::ConfirmFolder { project: project.into(), folder: folder.into() };
    Ok(match alone(screen, &itm.paths, &itm.tz, Arc::new(NoWork))? {
        Some(Action::Answer(give)) => Some(give),
        _ => None,
    })
}

/// The add dialog alone, filled with a workspace's folder and name (R10.5):
/// the id of the project it saved, or None when it was closed.
pub fn add_folder_dialog(itm: &Arc<Itm>, folder: &str, name: &str) -> Result<Option<String>> {
    let screen = Screen::AddProject(Box::new(add::AddDialog::folder(folder, name)));
    Ok(match alone(screen, &itm.paths, &itm.tz, Arc::clone(itm) as Arc<dyn Work>)? {
        Some(Action::ProjectAdded(id, _)) => Some(id),
        _ => None,
    })
}

/// The remove dialog alone (`itm project remove`, R8.7): what to do with
/// the clone, or None when it was closed (nothing changes then).
pub fn remove_project_dialog(plan: &RemovalPlan, itm: &Itm) -> Result<Option<CloneChoice>> {
    let screen = Screen::RemoveProject(RemoveDialog::new(plan, &itm.paths));
    Ok(match alone(screen, &itm.paths, &itm.tz, Arc::new(NoWork))? {
        Some(Action::Run(Job::RemoveProject { choice, .. })) => Some(choice),
        _ => None,
    })
}

/// What the New task form saved (R1.1).
#[derive(Debug, Clone, PartialEq)]
pub struct NewTask {
    pub title: String,
    pub description: Option<String>,
    pub projects: Vec<String>,
    pub until: Option<Until>,
}

/// The New task form alone (`itm new` without a title, or `-i`); None when it was closed.
pub fn task_form(itm: &Arc<Itm>, form: TaskForm) -> Result<Option<NewTask>> {
    let screen = Screen::Form(Box::new(form));
    Ok(match alone(screen, &itm.paths, &itm.tz, Arc::clone(itm) as Arc<dyn Work>)? {
        Some(Action::Run(Job::Create { title, description, projects, until })) => {
            Some(NewTask { title, description, projects, until })
        }
        _ => None,
    })
}

/// The edit form alone (`itm edit <task>` without options, R5.4): the edit
/// it saved, or None when it was closed.
pub fn edit_form(itm: &Arc<Itm>, form: TaskForm) -> Result<Option<Edit>> {
    let screen = Screen::Form(Box::new(form));
    Ok(match alone(screen, &itm.paths, &itm.tz, Arc::clone(itm) as Arc<dyn Work>)? {
        Some(Action::Run(Job::Edit { title, description, add, remove, order, .. })) => {
            Some(Edit { title: Some(title), description: Some(description), add, remove, order, ..Edit::default() })
        }
        _ => None,
    })
}

/// Take the terminal while `ui` runs, and give it back.
fn in_terminal<T>(ui: impl FnOnce(&mut ratatui::DefaultTerminal) -> Result<T>) -> Result<T> {
    let mut terminal = ratatui::try_init().context(|| "cannot open the terminal".into())?;
    let _ = execute!(std::io::stdout(), EnableBracketedPaste); // without it, a paste arrives as keys
    let result = ui(&mut terminal);
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

/// Run `task` on a worker thread and send what it returns. A panic becomes
/// `failed`: a refresh must end whatever happens (DESIGN.md section 2), or
/// the switcher would stop refreshing for good.
fn in_worker(inputs: &Sender<Input>, task: impl FnOnce() -> Input + Send + 'static, failed: Input) {
    let inputs = inputs.clone();
    thread::spawn(move || {
        let input = panic::catch_unwind(AssertUnwindSafe(task)).unwrap_or(failed);
        let _ = inputs.send(input); // nothing to tell once the switcher closed
    });
}

fn unexpected() -> Error {
    Error::Other("something went wrong; see the terminal for details".into())
}

/// The loop: refresh every 2 s (the switcher only), one load at a time;
/// keys to the app; jobs, listings and clones to threads, a job followed by
/// a refresh. Returns what ended it: the session to switch to, or (a
/// screen alone) the action its last screen closed with. It returns only
/// once the jobs that must finish have ([`Job::must_finish`]); until then
/// it reads no keys, and a failure among them keeps the switcher open to
/// show it (DESIGN.md section 3: errors stay visible).
fn event_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    mut app: App,
    work: Arc<dyn Work>,
    events: &impl Events,
    exit_after_draw: bool,
) -> Result<Option<Action>> {
    let (tx, rx) = mpsc::channel();
    let clones = Clones::default();
    let _stop_clones = CloseOnDrop(clones.clone()); // on every way out
    // While a clone or a job that must finish runs, a closing popup (SIGHUP)
    // or a kill ends the loop like Esc: the clone stops, the job finishes.
    let mut signals: Option<ExitSignals> = None;
    // One load at a time; a job's result asks for another one right after.
    let (mut loading, mut reload, mut next_refresh) = (false, !app.alone, events.now().instant);
    let (mut running, mut cloning, mut finishing) = (0, 0, 0);
    // How the loop ends, once `finishing` is down to 0.
    let mut leaving: Option<Result<Option<Action>>> = None;
    loop {
        app.now = events.now();
        if leaving.is_none() {
            if !app.alone && !loading && (reload || app.now.instant >= next_refresh) {
                (loading, reload) = (true, false);
                let work = Arc::clone(&work);
                in_worker(&tx, move || Input::Loaded(work.load()), Input::Loaded(Err(unexpected())));
            }
            while let Some(need) = app.needs() {
                running += 1;
                match need {
                    // A plain thread, never joined: a scan blocked on a slow filesystem holds up nothing.
                    Need::Listing(parent) => {
                        let (tx, home) = (tx.clone(), app.paths.home.clone());
                        thread::spawn(move || {
                            let listing = list_folders(&parent, &home, SCAN_LIMIT);
                            let _ = tx.send(Input::Listed(parent, listing));
                        });
                    }
                    Need::Job(job) => {
                        if job.must_finish() {
                            finishing += 1;
                            signals = signals.or_else(|| ExitSignals::catch().ok());
                        }
                        start(job, &work, &tx, &clones)
                    }
                }
            }
        }
        app.expire_message();
        if leaving.is_some() && finishing > 0 {
            app.flash("Finishing first…", false); // for as long as it waits, past a flash's few seconds
        }
        let shown = app.now.epoch;
        let drawn = terminal.draw(|frame| draw::draw(frame, &mut app, shown)).context(|| "cannot draw".into());
        if leaving.is_none()
            && let Err(e) = drawn
        {
            leaving = Some(Err(e)); // the terminal is gone: what must finish still does
        }
        if exit_after_draw && app.snapshot.is_some() {
            return Ok(None);
        }
        // A signal ends the loop, also one that was going to switch: the clones stop on the way out.
        if let Some(signal) = signals.as_mut().and_then(ExitSignals::arrived) {
            leaving = Some(Err(Error::Exit(128 + signal as u8)));
        }
        if let Some(ended) = leaving.take_if(|_| finishing == 0) {
            return ended;
        }
        let input = if leaving.is_some() {
            rx.recv_timeout(SIGNAL_POLL).ok() // only the workers' results now (and signals): no keys, no refresh
        } else {
            // Wake for the next refresh (unless one runs), for the message to go, for a screen's
            // check, and while a clone runs, often enough to see a signal soon.
            let refresh = (!loading && !app.alone).then_some(next_refresh);
            let watch = (cloning > 0 || finishing > 0).then(|| app.now.instant + SIGNAL_POLL);
            let deadline =
                [refresh, app.message.as_ref().map(|m| m.until), app.due(), watch].into_iter().flatten().min();
            let deadline = deadline.unwrap_or(app.now.instant + Duration::from_secs(3600));
            events.next(&rx, deadline, loading || running > 0)
        };
        app.now = events.now();
        let action = match input {
            None | Some(Input::Resized) => Action::None,
            Some(Input::Key(key)) => app.on_key(key),
            Some(Input::Paste(text)) => app.on_paste(&text),
            Some(Input::Failed(e)) => {
                leaving = Some(Err(e));
                Action::None
            }
            Some(Input::Loaded(snapshot)) => {
                // Whatever the outcome, this refresh is over and the next one is scheduled.
                (loading, next_refresh) = (false, app.now.instant + REFRESH);
                app.loaded(snapshot);
                Action::None
            }
            Some(Input::Listed(parent, listing)) => {
                running -= 1;
                app.listed(&parent, &listing);
                Action::None
            }
            Some(Input::Cloning(line)) => {
                app.cloning(line);
                Action::None
            }
            Some(Input::Done(job, result)) => {
                running -= 1;
                finishing -= usize::from(job.must_finish());
                cloning -= usize::from(matches!(job, Job::Clone { .. }));
                // They end itm again; one that came just before still counts.
                if cloning == 0
                    && finishing == 0
                    && let Some(signal) = signals.take().and_then(|mut caught| caught.arrived())
                {
                    leaving = Some(Err(Error::Exit(128 + signal as u8)));
                }
                reload |= job.changes_tasks();
                let failed = result.is_err() && job.must_finish(); // what the loop waits for
                let action = app.done(job, result);
                // The switcher stays to show what went wrong (a screen alone has nothing left to show it on).
                if failed && !app.alone && matches!(leaving, Some(Ok(_))) {
                    leaving = None;
                }
                action
            }
        };
        if leaving.is_none() {
            // A screen alone is done once its last screen closed: its caller acts on what it said.
            if app.alone && app.screens.is_empty() {
                leaving = Some(Ok(Some(action).filter(|a| *a != Action::None)));
            } else {
                match action {
                    Action::None
                    | Action::Answer(_)
                    | Action::ProjectAdded(..)
                    | Action::Picked(..)
                    | Action::Filter(_) => {}
                    Action::Quit => leaving = Some(Ok(None)),
                    Action::Switch(session) => leaving = Some(Ok(Some(Action::Switch(session)))),
                    Action::StopClone => clones.stop(),
                    Action::Notes(session) => {
                        // The one blocking call on the loop: nothing is drawn while the editor has the terminal.
                        if let Err(e) = events.suspended(&mut || work.open_notes(&session)) {
                            app.flash(e.to_string(), true);
                        }
                        terminal.clear().context(|| "cannot draw".into())?;
                        reload = true; // the preview shows the notes as they are now
                    }
                    Action::Run(job) => {
                        cloning += usize::from(matches!(job, Job::Clone { .. }));
                        finishing += usize::from(job.must_finish());
                        if cloning > 0 || finishing > 0 {
                            signals = signals.or_else(|| ExitSignals::catch().ok());
                        }
                        running += 1;
                        start(job, &work, &tx, &clones);
                    }
                }
            }
        }
        if let Some(ended) = leaving.take_if(|_| finishing == 0) {
            return ended;
        }
    }
}

/// Do `job` in a worker; its result comes back as [`Input::Done`].
fn start(job: Job, work: &Arc<dyn Work>, tx: &Sender<Input>, clones: &Clones) {
    let (work, failed) = (Arc::clone(work), Input::Done(job.clone(), Err(unexpected())));
    let run = Run { clones: clones.clone(), said: tx.clone() };
    in_worker(tx, move || Input::Done(job.clone(), work.perform(&job, &run)), failed);
}
#[cfg(test)]
mod tests {
    use super::app::tests::{NOW, ctrl, key, snapshot};
    use super::*;
    use crate::paths::Paths;
    use crate::store::{Project, ProjectKind};
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyCode;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::process::Command;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    enum Step {
        Key(KeyEvent),
        /// Let this much time pass with no key.
        Wait(Duration),
        /// A key pressed while a worker runs (Esc during a clone), once no result came for a while.
        Meanwhile(KeyEvent),
        /// Text pasted into the terminal.
        Paste(&'static str),
        /// The terminal is gone (a popup closed), while a worker runs.
        Gone,
    }

    fn secs(n: u64) -> Step {
        Step::Wait(Duration::from_secs(n))
    }

    fn press(code: KeyCode) -> Step {
        Step::Key(key(code))
    }

    fn typing(text: &str) -> Vec<Step> {
        text.chars().map(|c| press(KeyCode::Char(c))).collect()
    }

    /// Time that moves only when the script waits, and keys from the
    /// script. Workers answer before the script goes on, so a test reads
    /// like a person who waits for each result.
    struct Script {
        start: Instant,
        elapsed: Cell<Duration>,
        steps: RefCell<VecDeque<Step>>,
    }

    impl Script {
        fn new(steps: impl IntoIterator<Item = Step>) -> Script {
            Script { start: Instant::now(), elapsed: Cell::default(), steps: RefCell::new(steps.into_iter().collect()) }
        }
    }

    impl Events for Script {
        fn now(&self) -> Moment {
            let elapsed = self.elapsed.get();
            Moment { instant: self.start + elapsed, epoch: NOW + elapsed.as_secs() as i64 }
        }

        fn next(&self, results: &Receiver<Input>, deadline: Instant, busy: bool) -> Option<Input> {
            if busy {
                for _ in 0..100 {
                    if let Ok(input) = results.recv_timeout(Duration::from_millis(100)) {
                        return Some(input);
                    }
                    let mut steps = self.steps.borrow_mut();
                    match steps.front() {
                        Some(Step::Meanwhile(key)) => {
                            let key = *key;
                            steps.pop_front();
                            return Some(Input::Key(key));
                        }
                        Some(Step::Gone) => {
                            steps.pop_front();
                            return Some(Input::Failed(Error::Other("the terminal is gone".into())));
                        }
                        _ => {}
                    }
                }
                panic!("a worker never answered");
            }
            if let Ok(input) = results.try_recv() {
                return Some(input);
            }
            let step = self.steps.borrow_mut().pop_front().expect("the switcher wanted more input than the script has");
            match step {
                Step::Key(key) | Step::Meanwhile(key) => Some(Input::Key(key)),
                Step::Paste(text) => Some(Input::Paste(text.into())),
                Step::Gone => Some(Input::Failed(Error::Other("the terminal is gone".into()))),
                Step::Wait(time) => {
                    let left = deadline.saturating_duration_since(self.now().instant);
                    if time > left {
                        self.steps.borrow_mut().push_front(Step::Wait(time - left));
                    }
                    self.elapsed.set(self.elapsed.get() + time.min(left));
                    None
                }
            }
        }

        fn suspended(&self, program: &mut dyn FnMut() -> Result<()>) -> Result<()> {
            program()
        }
    }

    /// Loads [`snapshot`] (failing or panicking on the loads `fails` names)
    /// and records the jobs it is given. A project saved is `p-new`; a
    /// clone says what git would and is a real child that Esc stops.
    #[derive(Default)]
    struct FakeWork {
        loads: AtomicUsize,
        fails: Vec<(usize, &'static str)>,
        jobs: Mutex<Vec<Job>>,
        /// The tasks whose notes were opened.
        notes: Mutex<Vec<String>>,
        /// How the last clone child ended.
        clone_ended: Mutex<Option<std::process::ExitStatus>>,
        /// The tasks whose delete was done (a delete takes 300 ms, or
        /// `delete_takes`, so keys can come meanwhile).
        deleted: Mutex<Vec<String>>,
        delete_fails: bool,
        delete_takes: Option<Duration>,
        /// Load the snapshot with projects; a removal plan fails after this long.
        plan_fails_after: Option<Duration>,
    }

    fn new_project(name: &str, path: Option<&str>) -> Project {
        Project {
            name: name.into(),
            path: path.map(Into::into),
            url: None,
            added_at: 1,
            last_used_at: 1,
            extra: Default::default(),
        }
    }

    impl Work for FakeWork {
        fn load(&self) -> Result<Snapshot> {
            let n = self.loads.fetch_add(1, Ordering::SeqCst) + 1;
            match self.fails.iter().find(|(at, _)| *at == n) {
                Some((_, "panic")) => panic!("load {n} panicked"),
                Some((_, error)) => Err(Error::Other(error.to_string())),
                None if self.plan_fails_after.is_some() => Ok(super::app::tests::with_projects()),
                None => Ok(snapshot()),
            }
        }

        fn perform(&self, job: &Job, run: &Run) -> Result<Reply> {
            if !matches!(job, Job::CheckProject { .. }) {
                self.jobs.lock().unwrap().push(job.clone());
            }
            match job {
                Job::Create { .. } => Ok(Reply::Created("task-new".into())),
                Job::RemovalPlan { .. } if self.plan_fails_after.is_some() => {
                    thread::sleep(self.plan_fails_after.unwrap_or_default());
                    Err(Error::Other("cannot read the clone".into()))
                }
                Job::Delete { session, .. } => {
                    thread::sleep(self.delete_takes.unwrap_or(Duration::from_millis(300)));
                    self.deleted.lock().unwrap().push(session.clone());
                    match self.delete_fails {
                        true => Err(Error::Other(format!("deleted, but {session}'s session did not stop"))),
                        false => Ok(Reply::Done),
                    }
                }
                Job::SnoozeTime { text } if text == "next week" => Ok(Reply::Until(Until::Time(NOW + 7 * 86400))),
                Job::SnoozeTime { text } => Err(Error::Other(format!("cannot understand snooze time: {text}"))),
                Job::AddProject { kind: ProjectKind::Dir, source, .. } => {
                    Ok(Reply::Added("p-new".into(), new_project("alpha", Some(source))))
                }
                Job::AddProject { source, .. } => Ok(Reply::Added("p-new".into(), new_project(source, None))),
                Job::Clone { url, .. } => {
                    run.say("Receiving objects:  50% (1/2)".into());
                    let pid =
                        run.clones.spawn(Command::new("sleep").arg(if url.ends_with("slow") { "30" } else { "0" }));
                    let status = run.clones.wait(pid.unwrap()).unwrap();
                    *self.clone_ended.lock().unwrap() = Some(status);
                    match status.success() {
                        true => Ok(Reply::Added("p-new".into(), new_project("backend", Some("/r/clones/backend")))),
                        false => Err(Error::Other("itm: could not clone".into())),
                    }
                }
                _ => Ok(Reply::Done),
            }
        }

        fn open_notes(&self, session: &str) -> Result<()> {
            self.notes.lock().unwrap().push(session.into());
            match session {
                "task-later" => Err(Error::Other("no editor found; set $EDITOR".into())),
                _ => Ok(()),
            }
        }
    }

    /// Runs the loop on a test terminal; its result and the last screen drawn.
    fn switcher(steps: Vec<Step>, work: &Arc<FakeWork>, exit_after_draw: bool) -> (Result<Option<String>>, String) {
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        let clock = Script::new(steps);
        let app = App::new(View::Inbox, Paths::under("/r".as_ref()), TimeZone::UTC, clock.now());
        let work = Arc::clone(work) as Arc<dyn Work>;
        let result = event_loop(&mut terminal, app, work, &clock, exit_after_draw).map(|chosen| match chosen {
            Some(Action::Switch(session)) => Some(session),
            _ => None,
        });
        (result, format!("{}", terminal.backend()))
    }

    /// Runs `screen` alone; what it closed with and the last screen drawn.
    fn alone_screen(screen: Screen, steps: Vec<Step>, work: Arc<dyn Work>, paths: Paths) -> (Option<Action>, String) {
        let mut terminal = Terminal::new(TestBackend::new(90, 26)).unwrap();
        let clock = Script::new(steps);
        let app = App::alone(screen, paths, TimeZone::UTC, clock.now());
        let chosen = event_loop(&mut terminal, app, work, &clock, false).unwrap();
        (chosen, format!("{}", terminal.backend()))
    }

    fn loads(work: &FakeWork) -> usize {
        work.loads.load(Ordering::SeqCst)
    }

    fn jobs(work: &FakeWork) -> Vec<Job> {
        work.jobs.lock().unwrap().clone()
    }

    /// R5.3, DESIGN.md section 2: Esc right after a delete closes the
    /// switcher only once the delete is done. Stopped halfway, the task
    /// would be gone and its session would run on, unseen.
    #[test]
    fn closing_the_switcher_waits_for_a_delete_to_finish() {
        let work = Arc::new(FakeWork::default());
        let steps = vec![Step::Key(ctrl('d')), press(KeyCode::Char('y')), Step::Meanwhile(key(KeyCode::Esc))];
        let (ended, shown) = switcher(steps, &work, false);
        assert_eq!(ended, Ok(None));
        assert_eq!(*work.deleted.lock().unwrap(), ["task-fix-login"], "done before the switcher closed");
        assert!(shown.contains("Finishing first…"), "{shown}");
    }

    /// The same when the terminal cannot be read any more during the delete.
    #[test]
    fn a_terminal_gone_during_a_delete_waits_for_it() {
        let work = Arc::new(FakeWork::default());
        let (ended, _) = switcher(vec![Step::Key(ctrl('d')), press(KeyCode::Char('y')), Step::Gone], &work, false);
        assert_eq!(ended, Err(Error::Other("the terminal is gone".into())));
        assert_eq!(*work.deleted.lock().unwrap(), ["task-fix-login"]);
    }

    /// A signal while the switcher waits to switch (a delete runs) ends itm
    /// once the delete is done, instead of switching; the next signal is
    /// caught again (an earlier job's end left nothing behind).
    #[test]
    fn a_signal_while_waiting_to_switch_ends_itm_instead() {
        use crate::system::tests::{ignores, in_child};
        use signal_hook::consts::SIGTERM;
        if ignores(&[SIGTERM]) {
            return;
        }
        let Some(child) = in_child("tui::tests::a_signal_while_waiting_to_switch_ends_itm_instead") else {
            for _ in 0..2 {
                let work = Arc::new(FakeWork::default());
                let killer = thread::spawn(|| {
                    thread::sleep(Duration::from_millis(150)); // after Enter, while the delete runs
                    signal_hook::low_level::raise(SIGTERM).unwrap();
                });
                let steps = vec![Step::Key(ctrl('d')), press(KeyCode::Char('y')), Step::Meanwhile(key(KeyCode::Enter))];
                let (ended, _) = switcher(steps, &work, false);
                killer.join().unwrap();
                assert_eq!(ended, Err(Error::Exit(128 + SIGTERM as u8)));
                assert_eq!(*work.deleted.lock().unwrap(), ["task-fix-login"]);
            }
            return;
        };
        let said = String::from_utf8_lossy(&child.stdout);
        assert!(child.status.success(), "{said}");
    }

    /// Only a job the closing waits for keeps the switcher open when it
    /// fails: another one failing meanwhile (a project's removal plan,
    /// which only reads) leaves the Esc as it was.
    #[test]
    fn a_failure_the_closing_does_not_wait_for_keeps_the_esc() {
        let work = Arc::new(FakeWork {
            delete_takes: Some(Duration::from_millis(1500)),
            plan_fails_after: Some(Duration::from_millis(800)),
            ..FakeWork::default()
        });
        let mut steps = vec![Step::Key(ctrl('d')), press(KeyCode::Char('y'))];
        // Meanwhile: the Projects screen, a removal (its plan runs), then Esc twice.
        steps.extend([ctrl('p'), ctrl('d'), key(KeyCode::Esc), key(KeyCode::Esc)].map(Step::Meanwhile));
        let (ended, _) = switcher(steps, &work, false);
        assert_eq!(ended, Ok(None), "closed once the delete was done, without another key");
        assert_eq!(*work.deleted.lock().unwrap(), ["task-fix-login"]);
        assert!(jobs(&work).iter().any(|job| matches!(job, Job::RemovalPlan { .. })), "the plan ran meanwhile");
    }

    /// DESIGN.md section 3, errors stay visible: a delete that fails while
    /// the switcher closes keeps it open, saying why.
    #[test]
    fn a_failure_while_closing_keeps_the_switcher_open() {
        let work = Arc::new(FakeWork { delete_fails: true, ..FakeWork::default() });
        let mut steps = vec![Step::Key(ctrl('d')), press(KeyCode::Char('y')), Step::Meanwhile(key(KeyCode::Esc))];
        steps.push(press(KeyCode::Esc));
        let (ended, shown) = switcher(steps, &work, false);
        assert_eq!(ended, Ok(None), "the second Esc closes it");
        assert!(shown.contains("deleted, but task-fix-login's session did not stop"), "{shown}");
    }

    #[test]
    fn enter_returns_the_task_to_switch_to_and_esc_returns_none() {
        let work = Arc::new(FakeWork::default());
        assert_eq!(switcher(vec![press(KeyCode::Enter)], &work, false).0, Ok(Some("task-fix-login".into())));
        assert_eq!(switcher(vec![press(KeyCode::Esc)], &work, false).0, Ok(None));
        assert_eq!(loads(&work), 2, "one load each");
    }

    /// N3: the switcher refreshes every 2 s, one load at a time.
    #[test]
    fn the_list_refreshes_every_two_seconds() {
        let work = Arc::new(FakeWork::default());
        let steps = vec![secs(1), secs(1), secs(2), secs(2), press(KeyCode::Esc)];
        assert_eq!(switcher(steps, &work, false).0, Ok(None));
        assert_eq!(loads(&work), 4, "at 0, 2, 4 and 6 s");
    }

    /// DESIGN.md section 2: a refresh that fails for any reason, a panic
    /// included, still ends, and the next one comes.
    #[test]
    fn a_failed_or_panicking_refresh_ends_and_the_next_one_comes() {
        let work = Arc::new(FakeWork { fails: vec![(1, "panic"), (2, "Herdr not answering")], ..FakeWork::default() });
        let (result, screen) = switcher(vec![secs(2), secs(2), press(KeyCode::Esc)], &work, false);
        assert_eq!(result, Ok(None));
        assert_eq!(loads(&work), 3);
        assert!(screen.contains("Fix login"), "the third load is shown:\n{screen}");
        assert!(screen.contains("Herdr not answering"), "the error stays visible:\n{screen}");
    }

    /// A job runs in a worker; its result is said, a refresh follows, and the message goes after 4 s.
    #[test]
    fn a_job_runs_in_a_worker_then_the_list_refreshes() {
        let work = Arc::new(FakeWork::default());
        let wake = vec![press(KeyCode::Tab), Step::Key(ctrl('u'))];
        let (_, screen) = switcher(wake.into_iter().chain([secs(3), press(KeyCode::Esc)]).collect(), &work, false);
        assert_eq!(jobs(&work), [Job::Wake { session: "task-later".into(), title: "Later".into() }]);
        assert_eq!(loads(&work), 3, "the first load, one right after the job, and the one 2 s later");
        assert!(screen.contains("“Later” is active."), "{screen}");

        let wake = vec![press(KeyCode::Tab), Step::Key(ctrl('u'))];
        let (_, screen) = switcher(wake.into_iter().chain([secs(5), press(KeyCode::Esc)]).collect(), &work, false);
        assert!(!screen.contains("is active"), "the message went:\n{screen}");
    }

    /// R9.4: Ctrl+O hands the terminal to the editor, says what went wrong,
    /// and the preview reloads with the notes as they are now.
    #[test]
    fn ctrl_o_runs_the_editor_and_says_what_failed() {
        let work = Arc::new(FakeWork::default());
        let steps = vec![Step::Key(ctrl('o')), press(KeyCode::Tab), Step::Key(ctrl('o')), press(KeyCode::Esc)];
        let (result, screen) = switcher(steps, &work, false);
        assert_eq!(result, Ok(None));
        assert_eq!(*work.notes.lock().unwrap(), ["task-fix-login", "task-later"]);
        assert_eq!(loads(&work), 3, "a load after each editor");
        assert!(screen.contains("no editor found; set $EDITOR"), "{screen}");
    }

    #[test]
    fn the_measurement_switch_quits_on_the_first_frame_with_tasks() {
        let work = Arc::new(FakeWork::default());
        let (result, screen) = switcher(vec![], &work, true);
        assert_eq!(result, Ok(None));
        assert!(screen.contains("Fix login"), "{screen}");
    }

    /// DESIGN.md section 2: without a session list (Herdr not answering),
    /// nothing is adopted and no tombstone expires; sessions are unknown, not absent.
    #[test]
    fn a_refresh_without_a_session_list_adopts_nothing_and_expires_no_tombstone() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, itm) = crate::itm::tests::itm();
        let sessions = itm.paths.herdr_config.join("sessions");
        std::fs::create_dir_all(sessions.join("task-made-elsewhere")).unwrap();
        itm.store().edit(|data| Ok(data.deleted.insert("task-being-deleted".into(), 1))).unwrap();
        std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = std::fs::read_dir(&sessions).is_err(); // root reads it anyway
        let snapshot = itm.load();
        std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !unreadable {
            return;
        }
        let snapshot = snapshot.unwrap();
        assert!(!snapshot.listed);
        let data = itm.store().load().unwrap();
        assert!(data.tasks.is_empty() && data.deleted.contains_key("task-being-deleted"));
        // Once Herdr answers, the session made elsewhere is adopted and the tombstone of a gone session goes.
        let snapshot = itm.load().unwrap();
        assert!(snapshot.listed && snapshot.data.tasks.contains_key("task-made-elsewhere"));
        assert!(snapshot.data.deleted.is_empty());
    }

    /// R10.5: the folder question alone: y gives it, n keeps it without one, Esc cancels.
    #[test]
    fn r10_5_the_folder_question_alone() {
        let answer = |code: KeyCode| {
            let screen = Screen::ConfirmFolder { project: "X".into(), folder: "~/x".into() };
            let steps = vec![press(KeyCode::Enter), press(code)];
            match alone_screen(screen, steps, Arc::new(NoWork), Paths::under("/r".as_ref())).0 {
                Some(Action::Answer(give)) => Some(give),
                None => None,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(answer(KeyCode::Char('y')), Some(true));
        assert_eq!(answer(KeyCode::Char('n')), Some(false));
        assert_eq!(answer(KeyCode::Esc), None);
    }

    /// R10.5, R8.5: the add dialog alone saves the folder under the name
    /// given, in a worker; a name taken keeps the cursor on the name, with why.
    #[test]
    fn r10_5_the_add_dialog_alone_saves_the_folder() {
        let (_tmp, itm) = crate::itm::tests::itm();
        let code = crate::projects::tests::folder(&itm, "code/new-thing");
        itm.add_project(ProjectKind::Name, "My Thing", None).unwrap();
        let itm = Arc::new(itm);
        let run = |steps: Vec<Step>| {
            let screen = Screen::AddProject(Box::new(add::AddDialog::folder("~/code/new-thing", "My Thing")));
            alone_screen(screen, steps, Arc::clone(&itm) as Arc<dyn Work>, itm.paths.clone())
        };
        let (chosen, shown) = run(vec![press(KeyCode::Enter), press(KeyCode::Esc)]);
        assert_eq!(chosen, None);
        assert!(shown.contains("project My Thing has no folder yet"), "{shown}");
        let mut steps = vec![press(KeyCode::Enter)];
        steps.extend(typing("My Tool")); // the name is selected, as in the Python itm: typing replaces it
        steps.push(press(KeyCode::Enter));
        let (chosen, _) = run(steps);
        let Some(Action::ProjectAdded(id, _)) = chosen else { panic!("{chosen:?}") };
        let project = itm.store().load().unwrap().projects[&id].clone();
        assert_eq!((project.name.as_str(), project.path), ("My Tool", Some(code.display().to_string())));
    }

    /// R8.5: the add dialog checks what is typed once typing pauses, and
    /// says so before Enter: a name taken, where a clone goes.
    #[test]
    fn r8_5_the_add_dialog_checks_while_typing() {
        let (_tmp, mut itm) = crate::itm::tests::itm();
        crate::projects::tests::fake_git(&mut itm);
        crate::projects::tests::folder(&itm, "code/api");
        itm.add_project(ProjectKind::Dir, "~/code/api", None).unwrap();
        let itm = Arc::new(itm);
        let paths = itm.paths.clone();
        let mut steps = typing("https://github.com/acme/api.git");
        steps.extend([Step::Wait(Duration::from_millis(300)), press(KeyCode::Esc)]);
        let screen = Screen::AddProject(Box::new(add::AddDialog::new(ProjectKind::Git, "", "", screen::Field::Source)));
        let (chosen, shown) = alone_screen(screen, steps, Arc::clone(&itm) as Arc<dyn Work>, paths.clone());
        assert_eq!(chosen, None);
        assert!(shown.contains("project already exists: api (~/code/api); choose another name"), "{shown}");
        let mut steps = typing("https://github.com/acme/web.git");
        steps.extend([Step::Wait(Duration::from_millis(300)), press(KeyCode::Esc)]);
        let screen = Screen::AddProject(Box::new(add::AddDialog::new(ProjectKind::Git, "", "", screen::Field::Source)));
        let (_, shown) = alone_screen(screen, steps, itm as Arc<dyn Work>, paths);
        assert!(shown.contains("Clones to") && shown.contains("projects/github.com/acme/web"), "{shown}");
        assert!(shown.contains("Esc stops the clone and removes what it copied."), "{shown}");
        assert!(shown.contains("Name    web"), "the default name: {shown}");
    }

    /// R10.4: in the switcher, Ctrl+S then Enter saves the first setting in a worker.
    #[test]
    fn r10_4_settings_are_saved_by_a_worker() {
        let work = Arc::new(FakeWork::default());
        let steps = vec![
            secs(1),
            Step::Key(ctrl('s')),
            press(KeyCode::Enter),
            secs(1),
            press(KeyCode::Esc),
            press(KeyCode::Esc),
        ];
        let (result, shown) = switcher(steps, &work, false);
        assert_eq!(result.unwrap(), None);
        let setting = Job::Setting { setting: crate::config::Setting::ProjectsFromWorkspaces, on: false };
        assert_eq!(jobs(&work), [setting]);
        assert!(!shown.contains("Herdr integration"), "closed");
    }

    /// R4.1: `itm snooze` without a time opens the dialog alone; it returns the choice, or None on Esc.
    #[test]
    fn the_snooze_dialog_alone_returns_the_choice() {
        let presets = vec![("1 hour", Until::Time(NOW + 3600)), ("Until an agent needs me", Until::Agent)];
        let dialog = || Screen::Snooze(SnoozeDialog::new("task-a".into(), "A".into(), presets.clone()));
        let chosen =
            |steps: Vec<Step>| match alone_screen(dialog(), steps, Arc::new(NoWork), Paths::under("/r".as_ref())).0 {
                Some(Action::Run(Job::Snooze { session, until, .. })) => Some((session, until)),
                None => None,
                other => panic!("{other:?}"),
            };
        assert_eq!(chosen(vec![press(KeyCode::Down), press(KeyCode::Enter)]), Some(("task-a".into(), Until::Agent)));
        assert_eq!(chosen(vec![press(KeyCode::Esc)]), None);
        let typed = typing("2h").into_iter().chain([press(KeyCode::Enter)]).collect();
        assert_eq!(chosen(typed), Some(("task-a".into(), Until::Time(NOW + 7200))));
        // R4.1: a phrase only GNU date reads is read by its worker; the dialog closes with it.
        let work = Arc::new(FakeWork::default());
        let steps = typing("next week").into_iter().chain([press(KeyCode::Enter)]).collect();
        let paths = Paths::under("/r".as_ref());
        let (chosen, _) = alone_screen(dialog(), steps, Arc::clone(&work) as Arc<dyn Work>, paths.clone());
        let Some(Action::Run(Job::Snooze { until, .. })) = chosen else { panic!("{chosen:?}") };
        assert_eq!(until, Until::Time(NOW + 7 * 86400));
        assert_eq!(jobs(&work), [Job::SnoozeTime { text: "next week".into() }]);
        let steps = typing("monkey").into_iter().chain([press(KeyCode::Enter), press(KeyCode::Esc)]).collect();
        let (chosen, shown) = alone_screen(dialog(), steps, work, paths);
        assert_eq!(chosen, None);
        assert!(shown.contains("cannot understand snooze time: monkey"), "{shown}");
    }

    /// R1.1: Ctrl+N, a title, Tab to the description, Ctrl+S: the task is
    /// created with both, and the switcher switches to it.
    #[test]
    fn r1_1_the_new_task_form_creates_and_switches() {
        let work = Arc::new(FakeWork::default());
        let mut steps = vec![Step::Key(ctrl('n'))];
        steps.extend(typing("Plan launch"));
        steps.extend([Step::Key(ctrl('a')), press(KeyCode::Tab)]); // ctrl+a stays in the form
        steps.extend(typing("one"));
        steps.push(press(KeyCode::Enter));
        steps.extend(typing("two"));
        steps.push(Step::Key(ctrl('s')));
        let (result, _) = switcher(steps, &work, false);
        assert_eq!(result, Ok(Some("task-new".into())));
        let create = Job::Create {
            title: "Plan launch".into(),
            description: Some("one\ntwo".into()),
            projects: vec![],
            until: None,
        };
        assert_eq!(jobs(&work), [create]);
    }

    /// R1.6: a typed path lists folders (read in a thread); Tab goes into
    /// one, Enter opens the add dialog on it, which saves it in a worker,
    /// and the form has it. Enter in the title then creates the task with it.
    #[test]
    fn r1_6_a_folder_typed_in_the_finder_is_added_through_the_add_dialog() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::under(&crate::paths::resolved(tmp.path()));
        for name in ["work/alpha/inner", "work/beta"] {
            std::fs::create_dir_all(paths.home.join(name)).unwrap();
        }
        let saved = Saved::default();
        let form = TaskForm::new_task("Plan", vec![], saved, vec![], &paths);
        let mut steps = vec![press(KeyCode::Tab), press(KeyCode::Tab)]; // to Projects
        steps.extend(typing("~/work/al")); // the first key opens the finder; the rest goes into it
        steps.extend([press(KeyCode::Tab), press(KeyCode::Enter)]); // into ~/work/alpha/, then "Use" it
        steps.push(press(KeyCode::Enter)); // the add dialog, ready on the name: saves the folder
        steps.extend([press(KeyCode::BackTab), press(KeyCode::BackTab), press(KeyCode::Enter)]); // the title: create
        let work = Arc::new(FakeWork::default());
        let (chosen, _) = alone_screen(Screen::Form(Box::new(form)), steps, Arc::clone(&work) as Arc<dyn Work>, paths);
        let add = Job::AddProject { kind: ProjectKind::Dir, source: "~/work/alpha/".into(), name: None };
        assert_eq!(jobs(&work), [add]);
        let create =
            Job::Create { title: "Plan".into(), description: None, projects: vec!["p-new".into()], until: None };
        assert_eq!(chosen, Some(Action::Run(create)), "the form alone returns what to create");
    }

    /// R8.8: a clone is a real child (here a stand-in that waits); Esc
    /// stops it with SIGTERM and the dialog says nothing was kept.
    #[test]
    fn r8_8_esc_stops_a_clone_and_the_dialog_says_so() {
        let work = Arc::new(FakeWork::default());
        let dialog = add::AddDialog::new(ProjectKind::Git, "", "", screen::Field::Source);
        let mut steps = typing("https://github.com/acme/slow");
        steps.extend([press(KeyCode::Enter), Step::Meanwhile(key(KeyCode::Esc)), press(KeyCode::Esc)]);
        let started = Instant::now();
        let paths = Paths::under("/r".as_ref());
        let (chosen, shown) =
            alone_screen(Screen::AddProject(Box::new(dialog)), steps, Arc::clone(&work) as Arc<dyn Work>, paths);
        assert_eq!(chosen, None);
        assert!(shown.contains("Stopped. Nothing of the clone was kept."), "{shown}");
        assert!(started.elapsed() < Duration::from_secs(10), "the child did not wait its 30 s");
        assert_eq!(jobs(&work), [Job::Clone { url: "https://github.com/acme/slow".into(), name: None }]);
    }

    /// R8.3, R8.8: Ctrl+N on the Projects screen adds a repository; once its
    /// clone is done, the screen says so and lists it.
    #[test]
    fn r8_3_the_projects_screen_adds_a_repository() {
        let work = Arc::new(FakeWork::default());
        let screen = Screen::Projects(projects::ProjectsScreen::new(&snapshot()));
        let mut steps = vec![Step::Key(ctrl('n'))];
        steps.extend(typing("git@github.com:acme/backend.git"));
        steps.extend([press(KeyCode::Enter), press(KeyCode::Esc)]);
        let (chosen, shown) =
            alone_screen(screen, steps, Arc::clone(&work) as Arc<dyn Work>, Paths::under("/r".as_ref()));
        assert_eq!(chosen, None);
        assert!(shown.contains("Project added.") && shown.contains("◆ backend"), "{shown}");
    }

    /// R1.6, R8.8, after Python's test_clone_runs_as_a_child_and_selects_the_project:
    /// a git URL pasted on the form's project field opens the finder, whose
    /// first row clones it; the add dialog opens ready on the name, and once
    /// the clone is done the form has the project.
    #[test]
    fn r1_6_a_pasted_git_url_is_cloned_and_given_to_the_task() {
        let paths = Paths::under("/r".as_ref());
        let form = TaskForm::new_task("Plan", vec![], Saved::default(), vec![], &paths);
        let mut steps =
            vec![press(KeyCode::Tab), press(KeyCode::Tab), Step::Paste(" git@github.com:acme/backend.git\n")];
        steps.extend([press(KeyCode::Enter), press(KeyCode::Enter)]); // "+ Clone …", then the dialog's add
        steps.push(Step::Key(ctrl('s')));
        let work = Arc::new(FakeWork::default());
        let (chosen, _) = alone_screen(Screen::Form(Box::new(form)), steps, Arc::clone(&work) as Arc<dyn Work>, paths);
        let clone = Job::Clone { url: "git@github.com:acme/backend.git".into(), name: None };
        assert_eq!(jobs(&work), [clone]);
        let create =
            Job::Create { title: "Plan".into(), description: None, projects: vec!["p-new".into()], until: None };
        assert_eq!(chosen, Some(Action::Run(create)));
    }

    /// R8.8: whenever the UI ends during a clone (here its terminal went),
    /// the clone child is stopped: in a session of its own, nothing else would.
    #[test]
    fn r8_8_a_clone_stops_when_the_ui_ends() {
        let work = Arc::new(FakeWork::default());
        let dialog = add::AddDialog::new(ProjectKind::Git, "https://github.com/acme/slow", "", screen::Field::Name);
        let mut terminal = Terminal::new(TestBackend::new(90, 26)).unwrap();
        let clock = Script::new([press(KeyCode::Enter), Step::Gone]);
        let app =
            App::alone(Screen::AddProject(Box::new(dialog)), Paths::under("/r".as_ref()), TimeZone::UTC, clock.now());
        let ended = event_loop(&mut terminal, app, Arc::clone(&work) as Arc<dyn Work>, &clock, false);
        assert_eq!(ended, Err(Error::Other("the terminal is gone".into())));
        let stopped = (0..100).find_map(|_| {
            thread::sleep(Duration::from_millis(20));
            *work.clone_ended.lock().unwrap()
        });
        let signal = stopped.and_then(|s| std::os::unix::process::ExitStatusExt::signal(&s));
        assert_eq!(signal, Some(15), "stopped with SIGTERM");
    }
}
