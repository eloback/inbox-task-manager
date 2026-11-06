//! The switcher (`itm`, `itm ui`): a terminal UI over ratatui, and the
//! dialogs the CLI opens alone: snooze without a time, the delete question,
//! what to do with a removed project's clone.
//!
//! The event loop waits for a key, a worker's result or a deadline (the
//! 2 s refresh, a message expiring). Anything that can block (Herdr calls,
//! the store lock, a hand-off) runs in a worker thread that sends its
//! result over a channel. What the loop needs from the world is injected
//! (where time and keys come from, [`Events`]; what workers do, [`Work`];
//! the terminal backend), so its tests run it on a test backend with a
//! scripted clock. Choosing a task returns it to the caller, which switches
//! once the terminal is restored.

mod app;
mod draw;
mod screen;

use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use jiff::tz::TimeZone;
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::crossterm::event::{self, Event, KeyEvent, KeyEventKind};

pub use app::View;
use app::{Action, App, Job, Moment, NotesTail, Snapshot};
use screen::{Context as ScreenContext, Outcome, RemoveDialog, Screen, SnoozeDialog};

use crate::error::{Context, Error, Result};
use crate::itm::Itm;
use crate::projects::{CloneChoice, RemovalNote, RemovalPlan};
use crate::snooze::{self, Until};

const REFRESH: Duration = Duration::from_secs(2);
/// While a worker runs, how long to wait for a key before looking for its result.
const BUSY_POLL: Duration = Duration::from_millis(10);

/// What wakes the loop.
#[derive(Debug)]
pub enum Input {
    Key(KeyEvent),
    /// The terminal changed size: draw again.
    Resized,
    /// The terminal could not be read.
    Failed(Error),
    Loaded(Result<Snapshot>),
    Done(Job, Result<String>),
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
                Ok(Some(Event::Resize(..))) => return Some(Input::Resized),
                Ok(_) => {}
                Err(e) => return Some(Input::Failed(Error::Other(format!("cannot read the terminal: {e}")))),
            }
        }
    }

    fn suspended(&self, program: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        use ratatui::crossterm::{execute, terminal};
        ratatui::restore();
        let result = program();
        terminal::enable_raw_mode()
            .and_then(|()| execute!(std::io::stdout(), terminal::EnterAlternateScreen))
            .context(|| "cannot take the terminal back".into())?;
        result
    }
}

/// The blocking work the switcher hands to workers.
pub trait Work: Send + Sync {
    /// Everything the switcher shows.
    fn load(&self) -> Result<Snapshot>;
    /// Do a job; the session it concerns (removing a project: what happened).
    fn perform(&self, job: &Job) -> Result<String>;
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
        let notes = (data.tasks.keys())
            .filter_map(|name| {
                let (modified, lines) = self.notes_tail(name, 12)?;
                let modified = modified.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
                Some((name.clone(), NotesTail { modified, lines }))
            })
            .collect();
        Ok(Snapshot { data, sessions, status: self.status(), current, switch_problem, listed, missing, notes })
    }

    fn perform(&self, job: &Job) -> Result<String> {
        match job {
            Job::Wake { session, .. } => self.wake_task(session).map(|()| session.clone()),
            Job::Archive { session, .. } => self.archive_task(session).map(|()| session.clone()),
            Job::Delete { session, .. } => self.delete_task(session).map(|()| session.clone()),
            Job::Create { title } => self.create_task(title, None, &[], None),
            Job::Snooze { session, until, .. } => self.snooze_task(session, *until).map(|()| session.clone()),
            Job::RemoveProject { id, choice, .. } => {
                let notes = self.remove_project(id, *choice)?;
                let lines = notes.into_iter().map(|note| match note {
                    RemovalNote::Done(line) | RemovalNote::Kept(line) => line,
                });
                Ok(lines.collect::<Vec<_>>().join("; "))
            }
        }
    }

    fn open_notes(&self, session: &str) -> Result<()> {
        let data = self.store().load()?; // a task deleted meanwhile must not get its notes back
        let task = data.tasks.get(session).ok_or_else(|| Error::Other("this task no longer exists".into()))?;
        let path = self.ensure_notes(session, task, &data.projects)?;
        self.edit_notes(&path)
    }
}

/// Run the switcher; returns the session to switch to, if one was chosen.
pub fn run(itm: &Arc<Itm>, view: View) -> Result<Option<String>> {
    // A measurement switch (scripts/measure.sh): quit right after the first frame with tasks.
    let exit_after_draw = std::env::var_os("ITM_EXIT_AFTER_DRAW").is_some_and(|v| v == "1");
    let app = App::new(view, itm.paths.clone(), itm.tz.clone(), Moment::now());
    let work: Arc<dyn Work> = Arc::clone(itm) as Arc<dyn Work>;
    in_terminal(|terminal| event_loop(terminal, app, work, &TerminalEvents, exit_after_draw))
}

/// The snooze dialog alone (`itm snooze` without a time, R4.1): the chosen
/// snooze, or None when it was closed. "Until an agent needs me" is offered
/// only when `agent` (an agent could ever wake the task).
pub fn snooze_dialog(session: &str, title: &str, agent: bool, tz: &TimeZone) -> Result<Option<Until>> {
    let presets = snooze::presets(crate::times::now(), tz, agent);
    let screen = Screen::Snooze(SnoozeDialog::new(session.into(), title.into(), presets));
    let chosen = in_terminal(|terminal| alone(terminal, screen, tz, &TerminalEvents))?;
    Ok(match chosen {
        Some(Action::Run(Job::Snooze { until, .. })) => Some(until),
        _ => None,
    })
}

/// The delete question alone (`itm delete`, R5.3): whether the user said yes.
pub fn confirm_delete(session: &str, title: &str) -> Result<bool> {
    let screen = Screen::ConfirmDelete { session: session.into(), title: title.into() };
    let chosen = in_terminal(|terminal| alone(terminal, screen, &TimeZone::UTC, &TerminalEvents))?;
    Ok(matches!(chosen, Some(Action::Run(Job::Delete { .. }))))
}

/// The remove dialog alone (`itm project remove`, R8.7): what to do with
/// the clone, or None when it was closed (nothing changes then).
pub fn remove_project_dialog(plan: &RemovalPlan, paths: &crate::paths::Paths) -> Result<Option<CloneChoice>> {
    let screen = Screen::RemoveProject(RemoveDialog::new(plan, paths));
    let chosen = in_terminal(|terminal| alone(terminal, screen, &TimeZone::UTC, &TerminalEvents))?;
    Ok(match chosen {
        Some(Action::Run(Job::RemoveProject { choice, .. })) => Some(choice),
        _ => None,
    })
}

/// Take the terminal while `ui` runs, and give it back.
fn in_terminal<T>(ui: impl FnOnce(&mut ratatui::DefaultTerminal) -> Result<T>) -> Result<T> {
    let mut terminal = ratatui::try_init().context(|| "cannot open the terminal".into())?;
    let result = ui(&mut terminal);
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

/// The switcher's loop: refresh every 2 s, one load at a time; keys to the
/// app; jobs to workers, each followed by a refresh. Returns the session
/// chosen, if any.
fn event_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    mut app: App,
    work: Arc<dyn Work>,
    events: &impl Events,
    exit_after_draw: bool,
) -> Result<Option<String>> {
    let (tx, rx) = mpsc::channel();
    // One load at a time; a job's result asks for another one right after.
    let (mut loading, mut reload, mut next_refresh, mut jobs) = (false, true, events.now().instant, 0);
    loop {
        app.now = events.now();
        if !loading && (reload || app.now.instant >= next_refresh) {
            (loading, reload) = (true, false);
            let work = Arc::clone(&work);
            in_worker(&tx, move || Input::Loaded(work.load()), Input::Loaded(Err(unexpected())));
        }
        app.expire_message();
        let shown = app.now.epoch;
        terminal.draw(|frame| draw::draw(frame, &mut app, shown)).context(|| "cannot draw".into())?;
        if exit_after_draw && app.snapshot.is_some() {
            return Ok(None);
        }
        // Wake for the next refresh (unless one runs) and for the message to go.
        let refresh = (!loading).then_some(next_refresh);
        let deadline = refresh.into_iter().chain(app.message.as_ref().map(|m| m.until)).min();
        let deadline = deadline.unwrap_or(app.now.instant + Duration::from_secs(3600));
        let input = events.next(&rx, deadline, loading || jobs > 0);
        app.now = events.now();
        let action = match input {
            None | Some(Input::Resized) => Action::None,
            Some(Input::Key(key)) => app.on_key(key),
            Some(Input::Failed(e)) => return Err(e),
            Some(Input::Loaded(snapshot)) => {
                // Whatever the outcome, this refresh is over and the next one is scheduled.
                (loading, next_refresh) = (false, app.now.instant + REFRESH);
                app.loaded(snapshot);
                Action::None
            }
            Some(Input::Done(job, result)) => {
                (jobs, reload) = (jobs - 1, true);
                app.done(job, result)
            }
        };
        match action {
            Action::None => {}
            Action::Quit => return Ok(None),
            Action::Switch(session) => return Ok(Some(session)),
            Action::Notes(session) => {
                // The one blocking call on the loop: nothing is drawn while the editor has the terminal.
                if let Err(e) = events.suspended(&mut || work.open_notes(&session)) {
                    app.flash(e.to_string(), true);
                }
                terminal.clear().context(|| "cannot draw".into())?;
                reload = true; // the preview shows the notes as they are now
            }
            Action::Run(job) => {
                jobs += 1;
                let (work, failed) = (Arc::clone(&work), Input::Done(job.clone(), Err(unexpected())));
                in_worker(&tx, move || Input::Done(job.clone(), work.perform(&job)), failed);
            }
        }
    }
}

/// A screen with nothing behind it, until it closes; what it decided.
fn alone<B: Backend>(
    terminal: &mut Terminal<B>,
    mut screen: Screen,
    tz: &TimeZone,
    events: &impl Events,
) -> Result<Option<Action>> {
    let (_none, results) = mpsc::channel(); // no workers here
    loop {
        let now = events.now();
        terminal.draw(|frame| draw::draw_alone(frame, &screen, tz, now.epoch)).context(|| "cannot draw".into())?;
        let key = match events.next(&results, now.instant + Duration::from_secs(3600), false) {
            Some(Input::Key(key)) => key,
            Some(Input::Failed(e)) => return Err(e),
            _ => continue,
        };
        match screen.on_key(key, &ScreenContext { tz, now: events.now().epoch }) {
            Outcome::Stay => {}
            Outcome::Close => return Ok(None),
            Outcome::CloseAnd(action) => return Ok(Some(action)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::app::tests::{NOW, ctrl, key, snapshot};
    use super::*;
    use crate::paths::Paths;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyCode;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    enum Step {
        Key(KeyEvent),
        /// Let this much time pass with no key.
        Wait(Duration),
    }

    fn secs(n: u64) -> Step {
        Step::Wait(Duration::from_secs(n))
    }

    fn press(code: KeyCode) -> Step {
        Step::Key(key(code))
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
                return Some(results.recv_timeout(Duration::from_secs(10)).expect("a worker never answered"));
            }
            if let Ok(input) = results.try_recv() {
                return Some(input);
            }
            let step = self.steps.borrow_mut().pop_front().expect("the switcher wanted more input than the script has");
            match step {
                Step::Key(key) => Some(Input::Key(key)),
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
    /// and records the jobs it is given.
    #[derive(Default)]
    struct FakeWork {
        loads: AtomicUsize,
        fails: Vec<(usize, &'static str)>,
        jobs: Mutex<Vec<Job>>,
        /// The tasks whose notes were opened.
        notes: Mutex<Vec<String>>,
    }

    impl Work for FakeWork {
        fn load(&self) -> Result<Snapshot> {
            let n = self.loads.fetch_add(1, Ordering::SeqCst) + 1;
            match self.fails.iter().find(|(at, _)| *at == n) {
                Some((_, "panic")) => panic!("load {n} panicked"),
                Some((_, error)) => Err(Error::Other(error.to_string())),
                None => Ok(snapshot()),
            }
        }

        fn perform(&self, job: &Job) -> Result<String> {
            self.jobs.lock().unwrap().push(job.clone());
            Ok(String::new())
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
        let mut terminal = Terminal::new(TestBackend::new(110, 22)).unwrap();
        let clock = Script::new(steps);
        let app = App::new(View::Inbox, Paths::under("/r".as_ref()), TimeZone::UTC, clock.now());
        let work = Arc::clone(work) as Arc<dyn Work>;
        let result = event_loop(&mut terminal, app, work, &clock, exit_after_draw);
        (result, format!("{}", terminal.backend()))
    }

    fn loads(work: &FakeWork) -> usize {
        work.loads.load(Ordering::SeqCst)
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
        assert_eq!(*work.jobs.lock().unwrap(), [Job::Wake { session: "task-later".into(), title: "Later".into() }]);
        assert_eq!(loads(&work), 3, "the first load, one right after the job, and the one 2 s later");
        assert!(screen.contains("“Later” is active."), "{screen}");

        let wake = vec![press(KeyCode::Tab), Step::Key(ctrl('u'))];
        let (_, screen) = switcher(wake.into_iter().chain([secs(5), press(KeyCode::Esc)]).collect(), &work, false);
        assert!(!screen.contains("is active"), "the message went:\n{screen}");
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

    /// R4.1: `itm snooze` without a time opens the dialog alone; it returns the choice, or None on Esc.
    #[test]
    fn the_snooze_dialog_alone_returns_the_choice() {
        let presets = vec![("1 hour", Until::Time(NOW + 3600)), ("Until an agent needs me", Until::Agent)];
        let dialog = || Screen::Snooze(SnoozeDialog::new("task-a".into(), "A".into(), presets.clone()));
        let chosen = |steps: Vec<Step>| {
            let mut terminal = Terminal::new(TestBackend::new(70, 18)).unwrap();
            match alone(&mut terminal, dialog(), &TimeZone::UTC, &Script::new(steps)).unwrap() {
                Some(Action::Run(Job::Snooze { session, until, .. })) => Some((session, until)),
                None => None,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(chosen(vec![press(KeyCode::Down), press(KeyCode::Enter)]), Some(("task-a".into(), Until::Agent)));
        assert_eq!(chosen(vec![press(KeyCode::Esc)]), None);
        let typed = "2h".chars().map(|c| press(KeyCode::Char(c))).chain([press(KeyCode::Enter)]).collect();
        assert_eq!(chosen(typed), Some(("task-a".into(), Until::Time(NOW + 7200))));
    }
}
