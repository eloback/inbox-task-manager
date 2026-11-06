//! The switching protocol (DESIGN.md section 2) with real processes and
//! real signals: the real `itm` binary, and this test binary playing
//! `herdr`. Put on `PATH` under that name, it is
//!
//! - `herdr session attach <name>`: an attached client that logs
//!   "<pid> <name> <folder>" to `$ITM_TEST_LOG` and stays until a signal
//!   detaches it, as a real client does on SIGTERM;
//! - `herdr server` (with `HERDR_SESSION`): a headless session that logs
//!   "server <pid> <session id> <name> <HERDR_* variables>" and each
//!   workspace it is asked to open, and answers on its socket until
//!   `server.stop`; for a session named `*-broken` it exits with 1 instead;
//! - `herdr session delete <name> --json`: refused while the session
//!   answers, else its folder goes and "deleted <name>" is logged.
//!
//! Put on `PATH` as `git`, it plays the git a clone runs: `git clone` logs
//! "clone <pid> <session id> <url> <GIT_TERMINAL_PROMPT>", writes part of
//! the checkout and progress, and then, for a URL with `slow`, waits for
//! SIGTERM or SIGINT; for one with `password` it fails as git does when it
//! may not ask; else it finishes the checkout. `ls-remote --get-url` and
//! `remote get-url origin` answer as git would for such a checkout.
//!
//! Every process gets a temporary home and XDG folders, and no `HERDR_*`
//! variable but the ones a test sets. The unit tests describe the process
//! table instead (`FakeProcs`); these check that the real one, the signals
//! and the binary's command line work together.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs, panic, process, thread};

use rustix::process::{Pid, Signal, getsid, kill_process, test_kill_process};
use serde_json::{Value, json};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

const ITM: &str = env!("CARGO_BIN_EXE_itm");

fn main() {
    let me = env::args().next().unwrap_or_default();
    match Path::new(&me).file_name().and_then(|n| n.to_str()) {
        Some("herdr") => fake_herdr(env::args().skip(1).collect()),
        Some("git") => fake_git(env::args().skip(1).collect()),
        _ => run_tests(),
    }
}

// ---------------------------------------------------------------- tests

/// Outside Herdr, `itm attach` runs a client in a loop: a hand-off (here
/// from snoozing the task, R4.7) moves it to the next task, and detaching
/// without one ends the loop and removes its markers (R2.1).
fn the_attach_loop_follows_hand_offs_until_a_detach() {
    let world = World::new();
    world.itm(&["new", "Other", "--no-switch"]);
    let mut loop_a = world.attach("task-a");
    let client = world.attached(&loop_a, "task-a");
    assert_eq!(world.marker(&loop_a), "task-a");

    world.itm(&["snooze", "task-a", "1h"]);
    let moved = world.attached(&loop_a, "task-other");
    assert!(!alive(client), "the first client was detached");
    assert_eq!(world.marker(&loop_a), "task-other");

    signal(moved, Signal::TERM); // a detach: no hand-off waits
    assert!(world.ended(&mut loop_a).success());
    assert_eq!(world.markers(), Vec::<String>::new());
}

/// Inside Herdr, a switch moves only the clients on the current session;
/// when that session cannot be told, it refuses, or every terminal would move.
fn a_switch_inside_herdr_moves_only_this_sessions_clients() {
    let world = World::new();
    let mut here = world.attach("task-a");
    let mut other = world.attach("task-b");
    world.attached(&here, "task-a");
    let other_client = world.attached(&other, "task-b");
    let _session = world.listening("task-a");

    let socket = world.socket("task-a");
    world.itm_with(&["attach", "task-c"], &[("HERDR_SOCKET_PATH", socket.to_str().unwrap())]);
    world.attached(&here, "task-c");
    assert!(alive(other_client), "the client on task-b stayed");

    let (ok, said) = world.try_itm(&["attach", "task-a"], &[("HERDR_SOCKET_PATH", "/nowhere/herdr.sock")]);
    assert!(!ok && said.contains("cannot tell which Herdr session this is"), "{said}");
    world.stop(&mut here);
    world.stop(&mut other);
}

/// R1.4: in a Herdr client not started by `itm attach`, `itm new` fails
/// before saving; and a missing herdr saves nothing either (DESIGN.md
/// section 2: a failed attach leaves no task).
fn a_task_is_not_saved_when_switching_to_it_cannot_work() {
    let world = World::new();
    let _plain = world.listening("task-plain");
    let socket = world.socket("task-plain");
    let (ok, said) = world.try_itm(&["new", "Fresh"], &[("HERDR_SOCKET_PATH", socket.to_str().unwrap())]);
    assert!(!ok && said.contains("not started by `itm attach`"), "{said}");
    let (ok, said) = world.try_itm(&["new", "Fresh"], &[("PATH", "/nowhere")]);
    assert!(!ok && said.contains("herdr not found; install it or add it to PATH"), "{said}");
    let (ok, said) = world.try_itm(&["attach", "task-fresh"], &[("PATH", "/nowhere")]);
    assert!(!ok && said.contains("herdr not found"), "{said}");
    let tasks = fs::read_to_string(world.root.join("state/inbox-task-manager/tasks.json")).unwrap_or_default();
    assert!(!tasks.contains("task-fresh"), "a task was saved: {tasks}");
}

/// SIGHUP (the terminal closed), SIGTERM and SIGINT end the loop like a
/// detach: the client is detached and the markers are removed.
fn signals_end_the_loop_and_remove_its_markers() {
    for (name, sig) in [("SIGHUP", Signal::HUP), ("SIGTERM", Signal::TERM), ("SIGINT", Signal::INT)] {
        let world = World::new();
        let mut attach = world.attach("task-a");
        let client = world.attached(&attach, "task-a");
        signal(attach.id(), sig);
        let status = world.ended(&mut attach);
        assert!(status.success(), "{name}: {status}");
        assert_eq!(world.markers(), Vec::<String>::new(), "{name}");
        assert!(wait_until(|| !alive(client)), "{name}: the client was not detached");
    }
}

/// Markers are validated by their process's command line, since pids are
/// reused: a marker whose process is not itm is stale and goes, with its
/// hand-off, and that process is never signalled.
fn stale_markers_are_removed_and_their_processes_left_alone() {
    let world = World::new();
    let mut sleeper = Command::new("sleep").arg("30").spawn().unwrap();
    let mut done = Command::new("true").spawn().unwrap();
    done.wait().unwrap();
    let clients = world.clients();
    fs::create_dir_all(&clients).unwrap();
    for (pid, session) in [(sleeper.id(), "task-a"), (done.id(), "task-a")] {
        fs::write(clients.join(pid.to_string()), format!("{session}\n")).unwrap();
        fs::write(clients.join(format!("{pid}.next")), "task-x\n").unwrap();
    }
    world.itm(&["new", "A", "--no-switch"]);
    world.itm(&["snooze", "task-a", "1h"]); // moves the clients on task-a: there are none
    assert_eq!(world.markers(), Vec::<String>::new());
    assert!(alive(sleeper.id()), "the sleep was signalled");
    sleeper.kill().unwrap();
    sleeper.wait().unwrap();
}

/// A task's first start runs `herdr server` in its own session (so closing
/// the terminal does not hang it up) with a named workspace per project,
/// then attaches in the first project's folder (R1.2).
fn a_first_start_runs_its_server_in_its_own_session() {
    let world = World::new();
    let api = world.home.join("api");
    fs::create_dir_all(&api).unwrap();
    world.itm(&["project", "add", "dir", api.to_str().unwrap()]);
    world.itm(&["new", "Fix", "-p", "api", "--no-switch"]);
    // Herdr variables from wherever itm runs must not point the new server elsewhere.
    let mut attach = world.attach_with("task-fix", &[("HERDR_WORKSPACE_ID", "w9")]);
    world.attached(&attach, "task-fix");

    let log = world.log();
    let server = log.iter().find_map(|line| line.strip_prefix("server ")).expect("no server started");
    let [pid, sid, name, herdr_vars] = server.split(' ').collect::<Vec<_>>()[..] else { panic!("{server}") };
    assert_eq!(name, "task-fix");
    assert_eq!(herdr_vars, "HERDR_SESSION", "no other HERDR_* variable");
    assert_eq!(pid, sid, "the server leads its own session");
    let itm_sid = getsid(Pid::from_raw(attach.id() as i32)).unwrap().as_raw_nonzero().get();
    assert_ne!(sid, itm_sid.to_string(), "the server is not in the attach loop's session");
    let api = api.canonicalize().unwrap();
    assert!(log.contains(&format!("workspace api {} true", api.display())), "{log:?}");
    assert!(log.iter().any(|l| l.ends_with(&format!(" task-fix {}", api.display()))), "{log:?}");

    world.stop(&mut attach);
    signal(pid.parse().unwrap(), Signal::TERM);
}

/// A first start whose server fails is not waited for: it is said, and the
/// attach goes on (Herdr then starts the session as usual).
fn a_failed_first_start_is_said_and_the_attach_goes_on() {
    let world = World::new();
    fs::create_dir_all(world.home.join("api")).unwrap();
    world.itm(&["project", "add", "dir", "~/api"]);
    world.itm(&["new", "Broken", "-p", "api", "--no-switch"]);
    let started = Instant::now();
    let mut attach =
        world.command(&["attach", "task-broken"], &[]).stdin(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
    world.started.borrow_mut().push(attach.id());
    world.attached(&attach, "task-broken");
    assert!(started.elapsed() < Duration::from_secs(4), "waited {:?} for a server that exited", started.elapsed());
    world.stop(&mut attach);
    let mut said = String::new();
    std::io::Read::read_to_string(&mut attach.stderr.take().unwrap(), &mut said).unwrap();
    assert!(
        said.contains("herdr server for task-broken exited with") && said.contains("workspaces may be missing"),
        "{said}"
    );
}

/// R8.6, DESIGN.md section 2: a signal during a clone (SIGTERM: a cancel,
/// SIGHUP: a popup closed) stops git and keeps nothing, not the hidden
/// folder nor the host and owner folders; itm ends with 128 + the signal.
fn a_clone_stopped_by_a_signal_stops_git_and_keeps_nothing() {
    for (name, sig, status) in
        [("SIGTERM", Signal::TERM, 143), ("SIGINT", Signal::INT, 130), ("SIGHUP", Signal::HUP, 129)]
    {
        let world = World::new();
        let mut add =
            world.spawn(Command::new(ITM).args(["project", "add", "git", "https://example.com/acme/slow.git"]));
        let git = world.cloning();
        signal(add.id(), sig);
        assert_eq!(world.ended(&mut add).code(), Some(status), "{name}");
        assert!(!alive(git), "{name}: git was left running");
        assert!(
            world.log().contains(&"git stopped".to_string()),
            "{name}: git was not asked to stop: {:?}",
            world.log()
        );
        assert_eq!(world.projects_left(), Vec::<String>::new(), "{name}");
        assert!(!world.tasks().contains("example.com"), "{name}: a project was saved");
    }
}

/// DESIGN.md section 2: signals ignored before a clone (`nohup`) stay
/// ignored: closing the terminal does not stop it, SIGTERM still does.
fn signals_ignored_before_a_clone_stay_ignored() {
    let world = World::new();
    let mut add =
        world.spawn(Command::new("nohup").args([ITM, "project", "add", "git", "https://example.com/acme/slow.git"]));
    let git = world.cloning();
    signal(add.id(), Signal::HUP);
    thread::sleep(Duration::from_millis(300));
    assert!(alive(add.id()) && alive(git), "SIGHUP stopped a clone started with nohup");
    signal(add.id(), Signal::TERM);
    assert_eq!(world.ended(&mut add).code(), Some(143));
    assert_eq!(world.projects_left(), Vec::<String>::new());
}

/// R8.8: a TUI's clone child runs in its own session through `itm setsid`,
/// which makes sure SIGTERM can cancel it even when the TUI was started
/// with SIGTERM ignored; a repository that needs a password fails with
/// git's words for the TUI to read, and keeps nothing.
fn a_clone_child_has_its_own_session_and_can_always_be_cancelled() {
    let world = World::new();
    // As a TUI started with SIGTERM ignored would start it.
    let child = r#"trap '' TERM; exec "$0" setsid "$0" project add git https://example.com/acme/slow.git"#;
    let mut add = world.spawn(Command::new("sh").args(["-c", child, ITM]).env("GIT_TERMINAL_PROMPT", "0"));
    let git = world.cloning();
    let clone = world.log().into_iter().find(|l| l.starts_with("clone ")).unwrap();
    let [_, _, sid, _, prompt] = clone.split(' ').collect::<Vec<_>>()[..] else { panic!("{clone}") };
    assert_eq!(sid, add.id().to_string(), "the clone leads its own session");
    assert_eq!(prompt, "0");
    signal(add.id(), Signal::TERM);
    assert_eq!(world.ended(&mut add).code(), Some(143), "SIGTERM was ignored");
    assert!(!alive(git));

    let (ok, said) = world.try_itm(&["project", "add", "git", "https://example.com/acme/password.git"], &[]);
    assert!(!ok, "{said}");
    assert!(said.contains("terminal prompts disabled"), "git's words reach the output: {said}");
    assert!(said.ends_with("itm: could not clone https://example.com/acme/password.git\n"), "{said}");
    assert_eq!(world.projects_left(), Vec::<String>::new());
}

/// R8.2, R8.6: a clone finishes into place and is saved under the
/// repository's name; the same repository by another URL is refused.
fn a_clone_is_saved_once() {
    let world = World::new();
    let (ok, said) = world.try_itm(&["project", "add", "git", "git@example.com:acme/api.git"], &[]);
    assert!(ok, "{said}");
    // The name on stdout (try_itm puts it first), git's progress on stderr.
    assert!(said.starts_with("api\n") && said.contains("Receiving objects: 100%"), "{said}");
    let left = world.projects_left();
    assert_eq!(left[..3], ["example.com", "example.com/acme", "example.com/acme/api"], "{left:?}");
    let (ok, said) = world.try_itm(&["project", "add", "git", "https://example.com/acme/api"], &[]);
    assert!(!ok && said == "itm: project already exists: api\n", "{said}");
}

/// R5.3: `itm delete` asks first (so without a terminal it needs --yes),
/// stops the session, and Herdr deletes it once it is gone; the tombstone
/// keeps it from coming back until then.
fn deleting_a_task_stops_its_session_and_has_herdr_delete_it() {
    let world = World::new();
    world.itm(&["new", "Doomed", "--no-switch"]);
    let (ok, said) = world.try_itm(&["delete", "doomed"], &[]);
    assert!(!ok && said.contains("this needs a terminal; pass the missing values as options"), "{said}");
    let mut server =
        world.spawn(Command::new(world.root.join("bin/herdr")).arg("server").env("HERDR_SESSION", "task-doomed"));
    assert!(wait_until(|| std::os::unix::net::UnixStream::connect(world.socket("task-doomed")).is_ok()));
    let (ok, said) = world.try_itm(&["delete", "doomed", "--yes"], &[]);
    assert!(ok && said == "task-doomed deleted\n", "{said}");
    assert!(world.ended(&mut server).success(), "the session was stopped");
    assert!(world.tasks().contains("\"deleted\": {\n    \"task-doomed\""), "a tombstone: {}", world.tasks());
    assert!(wait_until(|| world.log().contains(&"deleted task-doomed".to_string())), "{:?}", world.log());
    assert!(!world.socket("task-doomed").parent().unwrap().exists());
    world.itm(&["list"]); // the session is gone: the tombstone goes too
    assert!(!world.tasks().contains("task-doomed"), "{}", world.tasks());
}

/// The tests, each with its name.
macro_rules! named {
    ($($test:ident),* $(,)?) => { [$((stringify!($test), $test as fn())),*] };
}

fn run_tests() {
    let tests = named![
        the_attach_loop_follows_hand_offs_until_a_detach,
        a_switch_inside_herdr_moves_only_this_sessions_clients,
        a_task_is_not_saved_when_switching_to_it_cannot_work,
        signals_end_the_loop_and_remove_its_markers,
        stale_markers_are_removed_and_their_processes_left_alone,
        a_first_start_runs_its_server_in_its_own_session,
        a_failed_first_start_is_said_and_the_attach_goes_on,
        a_clone_stopped_by_a_signal_stops_git_and_keeps_nothing,
        signals_ignored_before_a_clone_stay_ignored,
        a_clone_child_has_its_own_session_and_can_always_be_cancelled,
        a_clone_is_saved_once,
        deleting_a_task_stops_its_session_and_has_herdr_delete_it,
    ];
    // `cargo test <filter>` passes the filter on; options are for libtest, which this is not.
    let filters: Vec<String> = env::args().skip(1).filter(|a| !a.starts_with('-')).collect();
    let chosen: Vec<_> = tests.iter().filter(|(name, _)| filters.iter().all(|f| name.contains(f.as_str()))).collect();
    println!("\nrunning {} tests", chosen.len());
    let mut failed = vec![];
    for (name, test) in chosen.iter().copied() {
        match panic::catch_unwind(test) {
            Ok(()) => println!("test {name} ... ok"),
            Err(_) => {
                println!("test {name} ... FAILED");
                failed.push(*name);
            }
        }
    }
    let passed = chosen.len() - failed.len();
    let result = if failed.is_empty() { "ok" } else { "FAILED" };
    println!("\ntest result: {result}. {passed} passed; {} failed\n", failed.len());
    if !failed.is_empty() {
        println!("failures: {failed:?}");
        process::exit(101);
    }
}

// ---------------------------------------------------------------- the world

/// A temporary home and XDG folders, with this binary on `PATH` as `herdr`.
struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    path: String,
    /// Processes started by the test that must not outlive it.
    started: std::cell::RefCell<Vec<u32>>,
}

impl World {
    fn new() -> World {
        // Unix socket paths must stay short (about 108 bytes).
        let tmp = tempfile::Builder::new().prefix("itm").tempdir_in("/tmp").unwrap();
        let root = tmp.path().to_path_buf();
        let bin = root.join("bin");
        for dir in ["bin", "home", "config", "state", "data", "runtime"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        symlink(env::current_exe().unwrap(), bin.join("herdr")).unwrap();
        symlink(env::current_exe().unwrap(), bin.join("git")).unwrap();
        let path = format!("{}:{}", bin.display(), env::var("PATH").unwrap_or_default());
        World { home: root.join("home"), root, _tmp: tmp, path, started: Default::default() }
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut command = Command::new(ITM);
        command.args(args);
        self.environment(&mut command).envs(env.iter().copied());
        command
    }

    /// Only this world's variables, in its home folder.
    fn environment<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        command
            .env_clear()
            .current_dir(&self.home)
            .env("PATH", &self.path)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("ITM_TEST_LOG", self.root.join("log"))
    }

    /// `command` in the background in this world, its own variables kept; output dropped.
    fn spawn(&self, command: &mut Command) -> Child {
        let own: Vec<_> = command.get_envs().filter_map(|(k, v)| Some((k.to_owned(), v?.to_owned()))).collect();
        self.environment(command).envs(own).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let child = command.spawn().unwrap();
        self.started.borrow_mut().push(child.id());
        child
    }

    /// Waits for the fake git to start a clone; its pid.
    fn cloning(&self) -> u32 {
        let mut git = None;
        let started = wait_until(|| {
            git = self.log().iter().find_map(|l| l.strip_prefix("clone ")?.split(' ').next()?.parse().ok());
            git.is_some()
        });
        assert!(started, "no clone started: {:?}", self.log());
        self.started.borrow_mut().push(git.unwrap());
        git.unwrap()
    }

    /// What is under itm's projects folder, as relative paths, sorted.
    fn projects_left(&self) -> Vec<String> {
        let root = self.root.join("data/inbox-task-manager/projects");
        let mut found = vec![];
        let mut folders = vec![root.clone()];
        while let Some(folder) = folders.pop() {
            for entry in fs::read_dir(&folder).into_iter().flatten().flatten() {
                found.push(entry.path().strip_prefix(&root).unwrap().display().to_string());
                if entry.file_type().unwrap().is_dir() {
                    folders.push(entry.path());
                }
            }
        }
        found.sort();
        found
    }

    fn tasks(&self) -> String {
        fs::read_to_string(self.root.join("state/inbox-task-manager/tasks.json")).unwrap_or_default()
    }

    fn try_itm(&self, args: &[&str], env: &[(&str, &str)]) -> (bool, String) {
        let out = self.command(args, env).stdin(Stdio::null()).output().unwrap();
        let said = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        (out.status.success(), said)
    }

    fn itm_with(&self, args: &[&str], env: &[(&str, &str)]) {
        let (ok, said) = self.try_itm(args, env);
        assert!(ok, "itm {args:?}: {said}");
    }

    fn itm(&self, args: &[&str]) {
        self.itm_with(args, &[]);
    }

    /// `itm attach <task>` in the background.
    fn attach(&self, task: &str) -> Child {
        self.attach_with(task, &[])
    }

    fn attach_with(&self, task: &str, env: &[(&str, &str)]) -> Child {
        let child = self.command(&["attach", task], env).stdin(Stdio::null()).spawn().unwrap();
        self.started.borrow_mut().push(child.id());
        child
    }

    /// Waits until `loop_` has a client attached to `session`; its pid.
    fn attached(&self, loop_: &Child, session: &str) -> u32 {
        let mut found = None;
        let attached = wait_until(|| {
            found = self.clients_of(loop_.id()).last().filter(|(_, s)| s == session).map(|(pid, _)| *pid);
            found.is_some_and(alive)
        });
        assert!(attached, "no client on {session}; log: {:?}", self.log());
        let pid = found.unwrap();
        self.started.borrow_mut().push(pid);
        pid
    }

    /// (pid, session) of each attach the fake herdr logged for children of `parent`.
    fn clients_of(&self, parent: u32) -> Vec<(u32, String)> {
        self.log()
            .iter()
            .filter_map(|line| {
                let mut words = line.split(' ');
                let (parent_pid, pid, session) = (words.next()?, words.next()?, words.next()?);
                (parent_pid == parent.to_string()).then(|| Some((pid.parse().ok()?, session.to_string())))?
            })
            .collect()
    }

    fn log(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("log")).unwrap_or_default().lines().map(str::to_string).collect()
    }

    fn clients(&self) -> PathBuf {
        self.root.join("runtime/inbox-task-manager/clients")
    }

    fn markers(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.clients())
            .map(|entries| entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    fn marker(&self, loop_: &Child) -> String {
        fs::read_to_string(self.clients().join(loop_.id().to_string())).unwrap_or_default().trim().to_string()
    }

    fn socket(&self, session: &str) -> PathBuf {
        self.root.join("config/herdr/sessions").join(session).join("herdr.sock")
    }

    /// A running session that takes connections and says nothing.
    fn listening(&self, session: &str) -> UnixListener {
        let socket = self.socket(session);
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let accepting = listener.try_clone().unwrap();
        thread::spawn(move || accepting.incoming().for_each(drop));
        listener
    }

    /// Waits for `loop_` to end; its exit status.
    fn ended(&self, loop_: &mut Child) -> ExitStatus {
        let mut status = None;
        assert!(
            wait_until(|| {
                status = loop_.try_wait().unwrap();
                status.is_some()
            }),
            "the loop did not end"
        );
        status.unwrap()
    }

    fn stop(&self, loop_: &mut Child) {
        signal(loop_.id(), Signal::TERM);
        self.ended(loop_);
    }
}

impl Drop for World {
    fn drop(&mut self) {
        for &pid in self.started.borrow().iter() {
            signal(pid, Signal::KILL);
        }
    }
}

fn alive(pid: u32) -> bool {
    Pid::from_raw(pid as i32).is_some_and(|pid| test_kill_process(pid).is_ok())
}

fn signal(pid: u32, sig: Signal) {
    if let Some(pid) = Pid::from_raw(pid as i32) {
        let _ = kill_process(pid, sig);
    }
}

fn wait_until(mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    done()
}

// ---------------------------------------------------------------- the fakes

fn log(line: String) {
    let path = env::var("ITM_TEST_LOG").expect("ITM_TEST_LOG");
    let mut file = fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
    writeln!(file, "{line}").unwrap();
}

fn fake_git(args: Vec<String>) {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args[..] {
        ["clone", "--progress", "--", url, dest] => {
            let sid = getsid(None).unwrap().as_raw_nonzero().get();
            let prompt = env::var("GIT_TERMINAL_PROMPT").unwrap_or_else(|_| "-".into());
            // Only what a cancel sends: an ignored SIGHUP (nohup) stays ignored.
            let mut signals = Signals::new([SIGTERM, SIGINT]).unwrap();
            log(format!("clone {} {sid} {url} {prompt}", process::id()));
            fs::create_dir_all(Path::new(dest).join(".git")).unwrap();
            fs::write(Path::new(dest).join("half-written"), "").unwrap();
            eprint!("Cloning into '{dest}'...\nReceiving objects:  42% (21/50)\r");
            if url.contains("password") {
                eprintln!("fatal: could not read Username for 'https://example.com': terminal prompts disabled");
                process::exit(128);
            }
            if url.contains("slow") {
                signals.forever().next();
                log("git stopped".into());
                process::exit(128 + 15);
            }
            fs::write(Path::new(dest).join(".git/origin"), url).unwrap();
            eprintln!("Receiving objects: 100% (50/50), done.");
        }
        ["ls-remote", "--get-url", url] => println!("{url}"),
        ["-C", dir, "remote", "get-url", "origin"] => {
            print!("{}", fs::read_to_string(Path::new(dir).join(".git/origin")).unwrap_or_default());
        }
        _ => {
            eprintln!("fake git: unexpected {args:?}");
            process::exit(2);
        }
    }
}

fn fake_herdr(args: Vec<String>) {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args[..] {
        ["session", "attach", name] => {
            // Like a real client: SIGTERM detaches it (exit 0); SIGHUP and SIGINT end it.
            let mut signals = Signals::new([SIGTERM, SIGHUP, SIGINT]).unwrap();
            let cwd = env::current_dir().unwrap();
            log(format!(
                "{} {} {name} {}",
                rustix::process::getppid().map_or(0, |p| p.as_raw_nonzero().get()),
                process::id(),
                cwd.display()
            ));
            signals.forever().next();
            process::exit(0);
        }
        ["server"] => {
            let name = env::var("HERDR_SESSION").expect("HERDR_SESSION");
            let sid = getsid(None).unwrap().as_raw_nonzero().get();
            let mut herdr_vars: Vec<String> =
                env::vars().map(|(key, _)| key).filter(|key| key.starts_with("HERDR_")).collect();
            herdr_vars.sort();
            log(format!("server {} {sid} {name} {}", process::id(), herdr_vars.join(",")));
            if name.ends_with("-broken") {
                process::exit(1);
            }
            let config = PathBuf::from(env::var("XDG_CONFIG_HOME").unwrap()).join("herdr");
            let socket = config.join("sessions").join(&name).join("herdr.sock");
            fs::create_dir_all(socket.parent().unwrap()).unwrap();
            let listener = UnixListener::bind(&socket).unwrap();
            for stream in listener.incoming().flatten() {
                let mut line = String::new();
                if BufReader::new(&stream).read_line(&mut line).unwrap_or(0) == 0 {
                    continue; // a liveness check
                }
                let request: Value = serde_json::from_str(&line).unwrap();
                let params = &request["params"];
                let result = match request["method"].as_str().unwrap() {
                    "workspace.list" => json!({"workspaces": []}),
                    "workspace.create" => {
                        log(format!(
                            "workspace {} {} {}",
                            params["label"].as_str().unwrap(),
                            params["cwd"].as_str().unwrap(),
                            params["focus"]
                        ));
                        json!({})
                    }
                    "server.stop" => json!({"type": "ok"}),
                    _ => json!({}),
                };
                let _ = writeln!(&stream, "{}", json!({"id": request["id"], "result": result}));
                if request["method"] == "server.stop" {
                    process::exit(0); // like Herdr: it answers, then stops
                }
            }
        }
        ["session", "delete", name, "--json"] => {
            let config = PathBuf::from(env::var("XDG_CONFIG_HOME").unwrap()).join("herdr");
            let folder = config.join("sessions").join(name);
            if std::os::unix::net::UnixStream::connect(folder.join("herdr.sock")).is_ok() {
                process::exit(1); // Herdr refuses to delete a running session
            }
            fs::remove_dir_all(folder).unwrap();
            log(format!("deleted {name}"));
        }
        _ => {
            eprintln!("fake herdr: unexpected {args:?}");
            process::exit(2);
        }
    }
}
