//! The switcher (`itm`, `itm ui`): a terminal UI over ratatui.
//!
//! Input is read on this thread only, between frames. Anything that can
//! block (Herdr calls, the store lock, a hand-off) runs in a worker thread
//! that sends its result back over a channel. A refresh runs every 2 s,
//! one at a time. Choosing a task returns it to the caller, which switches
//! once the terminal is restored.

mod app;
mod draw;

use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEvent, KeyEventKind};

pub use app::View;
use app::{Action, App, Job, Snapshot};

use crate::error::{Context, Result};
use crate::itm::Itm;
use crate::times;

const REFRESH: Duration = Duration::from_secs(2);
/// While a worker runs, how long to wait for its result before looking at the keyboard again.
const BUSY_POLL: Duration = Duration::from_millis(10);

enum Msg {
    Loaded(Result<Snapshot>),
    Done(Job, Result<String>),
}

/// What woke the loop.
enum Input {
    Key(KeyEvent),
    Msg(Msg),
}

/// Run the switcher; returns the session to switch to, if one was chosen.
pub fn run(itm: &Arc<Itm>, view: View) -> Result<Option<String>> {
    // A measurement switch (scripts/measure.sh): quit right after the first frame with tasks.
    let exit_after_draw = std::env::var_os("ITM_EXIT_AFTER_DRAW").is_some_and(|v| v == "1");
    let mut terminal = ratatui::try_init().context(|| "cannot open the terminal".into())?;
    let result = event_loop(&mut terminal, itm, view, exit_after_draw);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    itm: &Arc<Itm>,
    view: View,
    exit_after_draw: bool,
) -> Result<Option<String>> {
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(view, itm.paths.clone(), itm.tz.clone());
    // One refresh at a time; a job's result asks for another one right after.
    let (mut loading, mut reload, mut next_refresh, mut jobs) = (false, true, Instant::now(), 0);
    loop {
        if !loading && (reload || Instant::now() >= next_refresh) {
            (loading, reload) = (true, false);
            let (itm, tx) = (Arc::clone(itm), tx.clone());
            thread::spawn(move || {
                let _ = tx.send(Msg::Loaded(load(&itm))); // nothing to tell once the switcher closed
            });
        }
        app.expire_message();
        terminal.draw(|frame| draw::draw(frame, &mut app, times::now())).context(|| "cannot draw".into())?;
        if exit_after_draw && app.snapshot.is_some() {
            return Ok(None);
        }
        let action = match next_input(&rx, loading || jobs > 0, next_refresh)? {
            None => Action::None,
            Some(Input::Key(key)) => app.on_key(key),
            Some(Input::Msg(Msg::Loaded(snapshot))) => {
                // Whatever the outcome, this refresh is over and the next one is scheduled.
                (loading, next_refresh) = (false, Instant::now() + REFRESH);
                app.loaded(snapshot);
                Action::None
            }
            Some(Input::Msg(Msg::Done(job, result))) => {
                (jobs, reload) = (jobs - 1, true);
                app.done(job, result)
            }
        };
        match action {
            Action::None => {}
            Action::Quit => return Ok(None),
            Action::Switch(session) => return Ok(Some(session)),
            Action::Run(job) => {
                jobs += 1;
                let (itm, tx) = (Arc::clone(itm), tx.clone());
                thread::spawn(move || {
                    let result = perform(&itm, &job);
                    let _ = tx.send(Msg::Done(job, result));
                });
            }
        }
    }
}

/// The next key or worker result. While a worker runs, its result is
/// waited for and keys are only peeked at; otherwise keys are waited for
/// until the next refresh (at most a second, so messages expire).
fn next_input(rx: &mpsc::Receiver<Msg>, busy: bool, next_refresh: Instant) -> Result<Option<Input>> {
    let read_failed = || "cannot read the terminal".to_string();
    if busy && let Ok(msg) = rx.recv_timeout(BUSY_POLL) {
        return Ok(Some(Input::Msg(msg)));
    }
    let wait = if busy { Duration::ZERO } else { next_refresh.saturating_duration_since(Instant::now()) };
    if event::poll(wait.min(Duration::from_secs(1))).context(read_failed)?
        && let Event::Key(key) = event::read().context(read_failed)?
        && key.kind == KeyEventKind::Press
    {
        return Ok(Some(Input::Key(key)));
    }
    Ok(None)
}

/// Everything the switcher shows. Adopts `task-*` sessions made elsewhere,
/// writing the store only when there is something to adopt or expire, and
/// only with a real session list.
fn load(itm: &Itm) -> Result<Snapshot> {
    let (sessions, listed) = match itm.herdr.sessions() {
        Ok(sessions) => (sessions, true),
        Err(_) => (Default::default(), false),
    };
    let names = sessions.keys().map(String::as_str);
    let mut data = itm.store().load()?;
    if listed && crate::tasks::needs_adoption(&data, names.clone()) {
        data = itm.store().edit(|data| {
            crate::tasks::adopt_sessions(data, names, times::now());
            Ok(data.clone())
        })?;
    }
    let (current, switch_problem) = match itm.inside_herdr() {
        true => (itm.current_session(&sessions), itm.switchable_session(&sessions).err().map(|e| e.to_string())),
        false => (None, None),
    };
    Ok(Snapshot { data, sessions, status: itm.status(), current, switch_problem, listed })
}

fn perform(itm: &Itm, job: &Job) -> Result<String> {
    match job {
        Job::Wake { session, .. } => itm.wake_task(session).map(|()| session.clone()),
        Job::Archive { session, .. } => itm.archive_task(session).map(|()| session.clone()),
        Job::Create { title } => itm.create_task(title, None, &[], None),
        Job::Snooze { session, until, .. } => itm.snooze_task(session, *until).map(|()| session.clone()),
    }
}
