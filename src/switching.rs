//! In-place switching (DESIGN.md section 2, "Switching protocol").
//!
//! Herdr cannot nest clients and has no detach API, so `itm attach` runs
//! `herdr session attach` in a loop and registers itself as
//! `<runtime>/clients/<pid>`, a marker naming the session it is attached
//! to. A switch writes `<pid>.next` with the target, then sends SIGTERM to
//! the loop's attach child (which detaches cleanly); the loop claims `.next`
//! and attaches to the target.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::error::{Context, Error, Result, bail};
use crate::herdr::Sessions;
use crate::itm::Itm;
use crate::store::write_atomic;
use crate::system::which;

/// The loop may be between two attaches: look for its attach child this
/// many times, this far apart, before giving up on it. Unit tests describe
/// the process table, so they need not wait as long.
const FIND_CHILD: (u32, Duration) =
    if cfg!(test) { (3, Duration::from_millis(10)) } else { (20, Duration::from_millis(100)) };

/// Whether a command line is an itm process. Part of the protocol: a
/// marker whose process fails this is stale and deleted, so every entry
/// point must keep one of these names.
pub fn is_itm_process(argv: &[String]) -> bool {
    argv.iter().any(|arg| {
        arg.contains("inbox-task-manager")
            || arg.contains("inbox_task_manager")
            || arg.rsplit('/').next() == Some("itm")
    })
}

fn is_attach_child(argv: &[String]) -> bool {
    argv.len() >= 3 && argv[0].rsplit('/').next() == Some("herdr") && argv[1..3] == ["session", "attach"]
}

impl Itm {
    /// Attach-loop pid → the session it is attached to. Markers are checked
    /// against the process table, since pids are reused: a stale one is
    /// removed; one whose process is alive but unreadable is left alone.
    pub fn live_clients(&self) -> Result<BTreeMap<u32, String>> {
        let dir = self.paths.clients()?;
        let mut clients = BTreeMap::new();
        let Ok(entries) = fs::read_dir(&dir) else { return Ok(clients) };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(pid) = name.bytes().all(|b| b.is_ascii_digit()).then(|| name.parse::<u32>().ok()).flatten() else {
                continue;
            };
            match self.procs.argv(pid) {
                Some(argv) if is_itm_process(&argv) => {
                    if let Ok(session) = fs::read_to_string(entry.path()) {
                        clients.insert(pid, session.trim().to_string());
                    }
                }
                None if self.procs.alive(pid) => {} // neither a client nor stale
                _ => {
                    let _ = fs::remove_file(entry.path());
                    let _ = fs::remove_file(dir.join(format!("{pid}.next")));
                }
            }
        }
        Ok(clients)
    }

    fn attach_children(&self, pid: u32) -> Vec<u32> {
        self.procs.children(pid).into_iter().filter(|(_, argv)| is_attach_child(argv)).map(|(child, _)| child).collect()
    }

    /// Ask attach loops to move to `target`: those on `from` (all when None),
    /// or only the most recently attached one. Returns whether any moved.
    pub fn handoff(&self, target: &str, from: Option<&str>, newest_only: bool) -> Result<bool> {
        let dir = self.paths.clients()?;
        let mut clients: Vec<u32> =
            self.live_clients()?.into_iter().filter(|(_, s)| from.is_none_or(|f| f == s)).map(|(pid, _)| pid).collect();
        if newest_only {
            let attached_at = |pid: &u32| fs::metadata(dir.join(pid.to_string())).and_then(|m| m.modified()).ok();
            clients = clients.into_iter().max_by_key(attached_at).into_iter().collect();
        }
        let mut switched = false;
        for pid in clients {
            let next = dir.join(format!("{pid}.next"));
            write_atomic(&next, &format!("{target}\n")).context(|| format!("cannot write {}", next.display()))?;
            let mut children = self.attach_children(pid);
            for _ in 1..FIND_CHILD.0 {
                if !children.is_empty() {
                    break;
                }
                thread::sleep(FIND_CHILD.1);
                children = self.attach_children(pid);
            }
            children.iter().for_each(|&child| self.procs.terminate(child));
            if children.is_empty() {
                let _ = fs::remove_file(&next);
            }
            switched |= !children.is_empty();
        }
        Ok(switched)
    }

    /// The current session, if this client can be switched in place.
    pub fn switchable_session(&self, sessions: &Sessions) -> Result<String> {
        let Some(current) = self.current_session(sessions) else { bail!("cannot tell which Herdr session this is") };
        if !self.live_clients()?.values().any(|s| *s == current) {
            bail!("this Herdr client was not started by `itm attach`; detach and use `itm attach`");
        }
        Ok(current)
    }

    /// Attach to `target` from outside Herdr, or switch this client to it from inside.
    pub fn switch_to(&self, target: &str) -> Result<()> {
        if !self.inside_herdr() {
            if which("herdr").is_none() {
                return Err(Error::Missing("herdr")); // before a task is saved for nothing
            }
            return self.attach_loop(target, |name, cwd| self.herdr.attach(name, cwd));
        }
        // Refuse when the current session is unknown: otherwise every terminal would switch.
        let current = self.switchable_session(&self.herdr.sessions()?)?;
        if target == current {
            self.touch_task(target)?;
            return Ok(());
        }
        if !self.handoff(target, Some(&current), false)? {
            bail!("could not switch this client to {target}");
        }
        Ok(())
    }

    /// Run `attach` (a `herdr session attach`) in a loop, following hand-offs.
    ///
    /// A closed terminal (SIGHUP), `kill` (SIGTERM) or Ctrl+C between two
    /// attaches (SIGINT) ends the loop like a detach: its attach child is
    /// detached the way a hand-off detaches it, and its markers are removed
    /// (left behind, they would only go once a later check found the pid stale).
    pub fn attach_loop(&self, target: &str, attach: impl FnMut(&str, Option<&Path>) -> Result<()>) -> Result<()> {
        let mut signals = Signals::new([SIGHUP, SIGTERM, SIGINT]).context(|| "cannot handle signals".into())?;
        let closing = signals.handle();
        let stop = AtomicBool::new(false);
        thread::scope(|scope| {
            scope.spawn(|| {
                if signals.forever().next().is_some() {
                    stop.store(true, Ordering::SeqCst);
                    self.detach_own_children();
                }
            });
            let result = self.follow_handoffs(target, attach, &stop);
            closing.close(); // ends the signal thread
            result
        })
    }

    fn follow_handoffs(
        &self,
        target: &str,
        mut attach: impl FnMut(&str, Option<&Path>) -> Result<()>,
        stop: &AtomicBool,
    ) -> Result<()> {
        let stopped = || stop.load(Ordering::SeqCst);
        let marker = Marker::new(&self.paths.clients()?);
        // A hand-off left by an earlier loop that had this pid and crashed.
        let _ = fs::remove_file(&marker.next);
        let mut name = target.to_string();
        while !name.is_empty() && !stopped() {
            write_atomic(&marker.path, &format!("{name}\n")).context(|| "cannot register this client".into())?;
            // A task starts in its first project's folder; Herdr restores it after its first start.
            let cwd = match self.touch_task(&name)? {
                Some(data) => {
                    self.prepare_first_start(&data, &name)?;
                    Some(self.task_cwd(&data, &name))
                }
                None => None,
            };
            if stopped() {
                break;
            }
            attach(&name, cwd.as_deref())?;
            if stopped() {
                break; // a signal detached it: whatever a hand-off says, this loop ends
            }
            // Claim the hand-off with a rename before reading it: one written
            // while the next attach starts must survive until that attach ends.
            name = match fs::rename(&marker.next, &marker.claimed) {
                Ok(()) => {
                    let next = fs::read_to_string(&marker.claimed).unwrap_or_default();
                    let _ = fs::remove_file(&marker.claimed);
                    next.trim().to_string()
                }
                Err(e) if e.kind() == ErrorKind::NotFound => String::new(), // a normal detach
                Err(e) => return Err(e).context(|| "cannot read the hand-off".into()),
            };
        }
        Ok(())
    }

    /// Detach this process's attach child, as a hand-off would; it may be
    /// starting, so it is looked for as briefly as a hand-off looks.
    fn detach_own_children(&self) {
        let me = std::process::id();
        for _ in 0..FIND_CHILD.0 {
            let children = self.attach_children(me);
            if !children.is_empty() {
                return children.into_iter().for_each(|child| self.procs.terminate(child));
            }
            thread::sleep(FIND_CHILD.1);
        }
    }
}

/// This loop's marker files, removed however the loop ends.
struct Marker {
    path: PathBuf,
    next: PathBuf,
    claimed: PathBuf,
}

impl Marker {
    fn new(dir: &Path) -> Marker {
        let pid = std::process::id();
        Marker {
            path: dir.join(pid.to_string()),
            next: dir.join(format!("{pid}.next")),
            claimed: dir.join(format!("{pid}.next.claimed")),
        }
    }
}

impl Drop for Marker {
    fn drop(&mut self) {
        for path in [&self.path, &self.next, &self.claimed] {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::herdr;
    use crate::itm::tests::itm;
    use crate::system::Processes;
    use serde_json::json;
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{Arc, Mutex};

    /// A process table described by the test.
    #[derive(Default)]
    pub struct FakeProcs {
        pub argv: Mutex<BTreeMap<u32, Vec<String>>>,
        /// Alive, but the command line cannot be read.
        pub unreadable: BTreeSet<u32>,
        pub parent: Mutex<BTreeMap<u32, u32>>,
        pub terminated: Mutex<Vec<u32>>,
    }

    impl FakeProcs {
        pub fn add(&self, pid: u32, parent: u32, argv: &[&str]) {
            self.argv.lock().unwrap().insert(pid, argv.iter().map(|a| a.to_string()).collect());
            self.parent.lock().unwrap().insert(pid, parent);
        }
    }

    impl Processes for FakeProcs {
        fn argv(&self, pid: u32) -> Option<Vec<String>> {
            self.argv.lock().unwrap().get(&pid).cloned()
        }
        fn alive(&self, pid: u32) -> bool {
            self.unreadable.contains(&pid) || self.argv.lock().unwrap().contains_key(&pid)
        }
        fn children(&self, parent: u32) -> Vec<(u32, Vec<String>)> {
            let parents = self.parent.lock().unwrap();
            let argv = self.argv.lock().unwrap();
            parents.iter().filter(|(_, p)| **p == parent).map(|(pid, _)| (*pid, argv[pid].clone())).collect()
        }
        fn terminate(&self, pid: u32) {
            self.terminated.lock().unwrap().push(pid);
            self.argv.lock().unwrap().remove(&pid);
            self.parent.lock().unwrap().remove(&pid);
        }
    }

    /// The fake is shared: the test keeps one handle to describe processes and check signals.
    impl Processes for Arc<FakeProcs> {
        fn argv(&self, pid: u32) -> Option<Vec<String>> {
            self.as_ref().argv(pid)
        }
        fn alive(&self, pid: u32) -> bool {
            self.as_ref().alive(pid)
        }
        fn children(&self, parent: u32) -> Vec<(u32, Vec<String>)> {
            self.as_ref().children(parent)
        }
        fn terminate(&self, pid: u32) {
            self.as_ref().terminate(pid)
        }
    }

    fn itm_with(fake: FakeProcs) -> (tempfile::TempDir, Itm, Arc<FakeProcs>) {
        let (tmp, mut itm) = itm();
        let fake = Arc::new(fake);
        itm.procs = Box::new(Arc::clone(&fake));
        (tmp, itm, fake)
    }

    fn marker(itm: &Itm, pid: u32, session: &str) -> PathBuf {
        let path = itm.paths.clients().unwrap().join(pid.to_string());
        write_atomic(&path, &format!("{session}\n")).unwrap();
        path
    }

    #[test]
    fn process_names_that_count_as_itm() {
        let cases: [(&[&str], bool); 6] = [
            (&["/nix/store/abc-inbox-task-manager-0.2.0/bin/.inbox-task-manager-wrapped", "attach"], true),
            (&["/usr/bin/python3", "-m", "inbox_task_manager", "attach"], true),
            (&["/usr/bin/python3", "/home/me/.local/bin/itm", "attach"], true),
            (&["/home/me/src/itm/target/release/itm", "attach"], true),
            (&["/usr/bin/python3", "/home/me/bin/item", "attach"], false),
            (&["vim", "notes.md"], false),
        ];
        for (argv, expected) in cases {
            let argv: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
            assert_eq!(is_itm_process(&argv), expected, "{argv:?}");
        }
    }

    #[test]
    fn markers_are_checked_against_the_process_table() {
        let (_tmp, itm, fake) = itm_with(FakeProcs { unreadable: [30].into(), ..FakeProcs::default() });
        fake.add(10, 1, &["/bin/itm", "attach"]);
        fake.add(20, 1, &["vim"]); // a reused pid
        let live = marker(&itm, 10, "task-a");
        let reused = marker(&itm, 20, "task-b");
        let unreadable = marker(&itm, 30, "task-c");
        let gone = marker(&itm, 40, "task-d");
        write_atomic(&gone.with_file_name("40.next"), "task-x\n").unwrap();

        assert_eq!(itm.live_clients().unwrap(), [(10, "task-a".to_string())].into());
        assert!(live.exists() && unreadable.exists());
        assert!(!reused.exists() && !gone.exists() && !gone.with_file_name("40.next").exists());
    }

    #[test]
    fn a_hand_off_moves_only_the_clients_on_the_session_and_signals_their_attach_child() {
        let (_tmp, itm, fake) = itm_with(FakeProcs::default());
        fake.add(10, 1, &["itm", "attach"]);
        fake.add(11, 10, &["/usr/bin/herdr", "session", "attach", "task-a"]);
        fake.add(12, 10, &["herdr", "session", "list"]);
        fake.add(20, 1, &["itm", "attach"]);
        fake.add(21, 20, &["herdr", "session", "attach", "task-b"]);
        marker(&itm, 10, "task-a");
        marker(&itm, 20, "task-b");

        assert!(itm.handoff("task-c", Some("task-a"), false).unwrap());
        assert_eq!(*fake.terminated.lock().unwrap(), [11]);
        let clients = itm.paths.clients().unwrap();
        assert_eq!(fs::read_to_string(clients.join("10.next")).unwrap(), "task-c\n");
        assert!(!clients.join("20.next").exists());
    }

    #[test]
    fn a_loop_without_an_attach_child_keeps_no_hand_off() {
        let (_tmp, itm, fake) = itm_with(FakeProcs::default());
        fake.add(10, 1, &["itm", "attach"]);
        marker(&itm, 10, "task-a");
        assert!(!itm.handoff("task-c", None, false).unwrap()); // after looking for the child FIND_CHILD times
        assert!(!itm.paths.clients().unwrap().join("10.next").exists());
    }

    #[test]
    fn the_open_action_moves_only_the_most_recently_attached_client() {
        let (_tmp, itm, fake) = itm_with(FakeProcs::default());
        for pid in [10, 20] {
            fake.add(pid, 1, &["itm", "attach"]);
            fake.add(pid + 1, pid, &["herdr", "session", "attach", "x"]);
        }
        marker(&itm, 20, "task-b");
        thread::sleep(Duration::from_millis(20));
        marker(&itm, 10, "task-a");
        assert!(itm.handoff("task-c", None, true).unwrap());
        assert_eq!(*fake.terminated.lock().unwrap(), [11]);
    }

    #[test]
    fn a_hand_off_written_during_an_attach_is_followed() {
        let (_tmp, itm) = itm();
        let clients = itm.paths.clients().unwrap();
        let next = clients.join(format!("{}.next", std::process::id()));
        write_atomic(&next, "task-stale\n").unwrap(); // left by a crashed loop with this pid
        let mut attached = vec![];
        itm.attach_loop("task-a", |name, cwd| {
            attached.push((name.to_string(), cwd.map(Path::to_path_buf)));
            match attached.len() {
                1 => write_atomic(&next, "task-b\n").unwrap(), // a switch while task-a is attached
                2 => write_atomic(&next, "default\n").unwrap(), // one more, right as task-b starts
                _ => {}
            }
            Ok(())
        })
        .unwrap();
        let task_dir = Some(itm.paths.task_dir.clone());
        assert_eq!(
            attached,
            [("task-a".into(), task_dir.clone()), ("task-b".into(), task_dir), ("default".into(), None)]
        );
        assert_eq!(fs::read_dir(&clients).unwrap().count(), 0, "markers are removed when the loop ends");
        let data = itm.store().load().unwrap();
        assert_eq!(data.tasks.keys().collect::<Vec<_>>(), ["task-a", "task-b"], "attached tasks are adopted");
    }

    #[test]
    fn the_marker_names_the_session_while_attached() {
        let (_tmp, itm) = itm();
        let marker = itm.paths.clients().unwrap().join(std::process::id().to_string());
        itm.attach_loop("default", |_, _| {
            assert_eq!(fs::read_to_string(&marker).unwrap(), "default\n");
            Ok(())
        })
        .unwrap();
        assert!(!marker.exists());
    }

    #[test]
    fn switching_inside_herdr_needs_an_itm_client_on_a_known_session() {
        let (_tmp, mut itm, fake) = itm_with(FakeProcs::default());
        herdr::fake::serve(&itm.paths.herdr_config, "task-a", |_, _| json!({}));
        itm.client_socket = Some("/elsewhere.sock".into());
        assert_eq!(itm.switch_to("task-b").unwrap_err().to_string(), "cannot tell which Herdr session this is");
        itm.client_socket = Some(itm.herdr.socket("task-a"));
        assert!(itm.switch_to("task-b").unwrap_err().to_string().contains("not started by `itm attach`"));

        fake.add(10, 1, &["itm", "attach"]);
        fake.add(11, 10, &["herdr", "session", "attach", "task-a"]);
        marker(&itm, 10, "task-a");
        itm.switch_to("task-b").unwrap();
        assert_eq!(*fake.terminated.lock().unwrap(), [11]);
        itm.switch_to("task-a").unwrap(); // already there: only marked used
        assert_eq!(itm.store().load().unwrap().tasks["task-a"].state, crate::store::State::Active);
    }
}
