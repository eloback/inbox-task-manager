//! What differs between operating systems, and what itm needs from them:
//! the process table (to validate attach-loop markers and find their `herdr
//! session attach` children), signals, programs on `PATH`, sessions of
//! their own, the desktop notification command, and a `date` that reads
//! snooze phrases. Linux reads the
//! process table from `/proc`; macOS and the BSDs ask `ps` (DESIGN.md
//! section 4, portability). Which one is a run-time choice, so both are
//! built and tested everywhere.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process, test_kill_process};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::error::{Error, Result, bail};

/// The process table, behind a trait so the switching protocol's tests can
/// describe processes (an `itm` loop, its attach child, a reused pid)
/// without starting them.
pub trait Processes: Send + Sync {
    /// The command line of `pid`; None when it is gone or cannot be read.
    fn argv(&self, pid: u32) -> Option<Vec<String>>;
    fn alive(&self, pid: u32) -> bool;
    /// (pid, command line) of every child of `parent`.
    fn children(&self, parent: u32) -> Vec<(u32, Vec<String>)>;
    /// Send SIGTERM; a process already gone is not an error.
    fn terminate(&self, pid: u32);
}

/// This system's process table: `/proc` where there is one, else `ps`.
pub fn process_table() -> Box<dyn Processes> {
    if fs::metadata("/proc/self/cmdline").is_ok() { Box::new(Proc) } else { Box::new(Ps) }
}

/// The process table from `/proc` (Linux).
pub struct Proc;

impl Processes for Proc {
    fn argv(&self, pid: u32) -> Option<Vec<String>> {
        let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        Some(
            raw.split(|&b| b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect(),
        )
    }

    fn alive(&self, pid: u32) -> bool {
        alive(pid)
    }

    fn children(&self, parent: u32) -> Vec<(u32, Vec<String>)> {
        let Ok(entries) = fs::read_dir("/proc") else { return vec![] };
        entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
            .filter(|&pid| parent_of(pid) == Some(parent))
            .filter_map(|pid| Some((pid, self.argv(pid)?)))
            .collect()
    }

    fn terminate(&self, pid: u32) {
        terminate(pid)
    }
}

/// The process table from `ps -ww` (macOS, the BSDs). `ps` joins a command
/// line's arguments with spaces, so an argument containing a space comes
/// back as several: a `herdr` whose path has a space is not recognized as
/// an attach child there.
pub struct Ps;

/// How long `ps` may take.
const PS_TIMEOUT: Duration = Duration::from_secs(5);

impl Ps {
    /// What `ps -ww <args>` printed, when it succeeded.
    fn run(args: &[&str]) -> Option<String> {
        match output_within(Command::new("ps").arg("-ww").args(args), PS_TIMEOUT) {
            Ok(Some((status, text))) if status.success() => Some(text),
            _ => None,
        }
    }
}

impl Processes for Ps {
    fn argv(&self, pid: u32) -> Option<Vec<String>> {
        ps_command(&Ps::run(&["-o", "command=", "-p", &pid.to_string()])?)
    }

    fn alive(&self, pid: u32) -> bool {
        alive(pid)
    }

    fn children(&self, parent: u32) -> Vec<(u32, Vec<String>)> {
        // One -o per column: FreeBSD reads everything after the first "=" of a list as its header.
        Ps::run(&["-A", "-o", "pid=", "-o", "ppid=", "-o", "command="])
            .map_or(vec![], |table| ps_children(&table, parent))
    }

    fn terminate(&self, pid: u32) {
        terminate(pid)
    }
}

/// The command line `ps -o command= -p <pid>` printed; None for nothing.
fn ps_command(output: &str) -> Option<Vec<String>> {
    let argv: Vec<String> = output.split_whitespace().map(str::to_string).collect();
    (!argv.is_empty()).then_some(argv)
}

/// (pid, command line) of each child of `parent` in `ps -A -o pid= -o ppid= -o command=` output.
fn ps_children(table: &str, parent: u32) -> Vec<(u32, Vec<String>)> {
    table
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid: u32 = fields.next()?.parse().ok()?;
            let argv: Vec<String> = fields.map(str::to_string).collect();
            (ppid == parent && !argv.is_empty()).then_some((pid, argv))
        })
        .collect()
}

/// Whether `pid` runs; someone else's process counts (EPERM).
fn alive(pid: u32) -> bool {
    match pid_of(pid) {
        Some(pid) => !matches!(test_kill_process(pid), Err(rustix::io::Errno::SRCH)),
        None => false,
    }
}

pub fn terminate(pid: u32) {
    if let Some(pid) = pid_of(pid) {
        let _ = kill_process(pid, Signal::TERM);
    }
}

/// Stop `child` with SIGTERM, which lets it stop its own helpers (git's),
/// and SIGKILL when it is still there after `patience`; how it ended.
pub fn stop_child(child: &mut Child, patience: Duration) -> io::Result<ExitStatus> {
    terminate(child.id());
    let deadline = Instant::now() + patience;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(20));
    }
    child.kill()?;
    child.wait()
}

/// The signals this process ignores, a bit each (SIGHUP is bit 0), as
/// `nohup` or a parent left them: from `/proc` on Linux, else from `ps`.
/// None when neither says.
pub fn ignored_signals() -> Option<u64> {
    match fs::read_to_string("/proc/self/status") {
        Ok(status) => status.lines().find_map(|l| l.strip_prefix("SigIgn:")).and_then(signal_mask),
        Err(_) => {
            Ps::run(&["-o", "sigignore=", "-p", &std::process::id().to_string()]).as_deref().and_then(signal_mask)
        }
    }
}

/// A mask of signals as `/proc` and `ps -o sigignore=` print it, in hex.
fn signal_mask(text: &str) -> Option<u64> {
    u64::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok()
}

/// SIGHUP (a popup closed), SIGTERM (a cancel) and SIGINT, caught from
/// [`ExitSignals::catch`] until dropped instead of ending the process at
/// once, so it can clean up first; more of them meanwhile change nothing.
/// Those ignored already (`nohup`) stay ignored.
pub struct ExitSignals {
    // Dropped in this order: the signals go, then they end the process again.
    signals: Signals,
    _catching: Catching,
}

impl ExitSignals {
    pub fn catch() -> io::Result<ExitSignals> {
        let ignored = ignored_signals().unwrap_or(0);
        let caught: Vec<i32> = [SIGHUP, SIGTERM, SIGINT].into_iter().filter(|&sig| !is_set(ignored, sig)).collect();
        let catching = Catching::start(&caught)?;
        Ok(ExitSignals { signals: Signals::new(&caught)?, _catching: catching })
    }

    /// The first one that arrived since the last look, if any.
    pub fn arrived(&mut self) -> Option<i32> {
        self.signals.pending().next()
    }
}

fn is_set(mask: u64, signal: i32) -> bool {
    mask & (1 << (signal - 1)) != 0
}

/// While one lives, something catches SIGHUP, SIGTERM and SIGINT with
/// signal-hook ([`ExitSignals`], the attach loop); once none does, they end
/// the process again, as by default. signal-hook keeps its handler once it
/// installed one, which would leave them ignored, so a conditional default
/// action is registered once per signal and process, on while no catcher
/// lives. (Actions registered at each catcher's end could never be taken
/// back, and would end the process under the next catcher.) A signal the
/// process ignored when first caught (`nohup`) gets none: it stays ignored.
pub struct Catching(());

struct Catchers {
    live: usize,
    defaults: Vec<i32>,
}

static CATCHERS: Mutex<Catchers> = Mutex::new(Catchers { live: 0, defaults: Vec::new() });
/// Whether the conditional defaults act: no [`Catching`] lives.
static BY_DEFAULT: LazyLock<Arc<AtomicBool>> = LazyLock::new(|| Arc::new(AtomicBool::new(true)));

impl Catching {
    pub fn start(signals: &[i32]) -> io::Result<Catching> {
        let ignored = ignored_signals().unwrap_or(0);
        let mut catchers = CATCHERS.lock().unwrap_or_else(PoisonError::into_inner);
        for &signal in signals {
            if !catchers.defaults.contains(&signal) && !is_set(ignored, signal) {
                signal_hook::flag::register_conditional_default(signal, Arc::clone(&BY_DEFAULT))?;
                catchers.defaults.push(signal);
            }
        }
        catchers.live += 1;
        BY_DEFAULT.store(false, Ordering::SeqCst);
        Ok(Catching(()))
    }
}

impl Drop for Catching {
    fn drop(&mut self) {
        let mut catchers = CATCHERS.lock().unwrap_or_else(PoisonError::into_inner);
        catchers.live -= 1;
        if catchers.live == 0 {
            BY_DEFAULT.store(true, Ordering::SeqCst);
        }
    }
}

fn pid_of(pid: u32) -> Option<Pid> {
    Pid::from_raw(i32::try_from(pid).ok()?)
}

/// The parent pid from `/proc/<pid>/stat`. The process name in parentheses
/// may contain spaces and parentheses, so fields are counted after the last `)`.
fn parent_of(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat[stat.rfind(')')? + 1..].split_whitespace().nth(1)?.parse().ok()
}

/// Run `command` to its end and return what it printed, or kill it after
/// `timeout` (None). Its output is read while it runs, so a long one cannot
/// fill the pipe and stall it.
pub fn output_within(
    command: &mut Command,
    timeout: Duration,
) -> io::Result<Option<(std::process::ExitStatus, String)>> {
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    let mut stdout = child.stdout.take().expect("piped");
    let reader = thread::spawn(move || {
        let mut bytes = vec![];
        stdout.read_to_end(&mut bytes).map(|_| String::from_utf8_lossy(&bytes).into_owned())
    });
    let deadline = Instant::now() + timeout;
    let mut nap = Duration::from_millis(5);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        thread::sleep(nap.min(left));
        nap = (nap * 2).min(Duration::from_secs(1));
    };
    let text = reader.join().unwrap_or_else(|_| Ok(String::new()))?;
    Ok(status.map(|status| (status, text)))
}

/// A command that runs `program` in a new session (setsid), away from this
/// terminal: closing it hangs up nothing `program` runs. std calls setsid
/// only in an `unsafe` `pre_exec`, so itm runs itself as `itm setsid
/// <program>` (hidden), which calls setsid and then becomes `program`.
pub fn in_new_session(program: impl AsRef<OsStr>) -> io::Result<Command> {
    let mut command = Command::new(env::current_exe()?);
    command.arg("setsid").arg(program);
    Ok(command)
}

/// `itm setsid <program> [args…]`: leave this session, then become
/// `program`, with SIGTERM not ignored: an ignored signal stays ignored
/// through exec, and SIGTERM is how a clone child is cancelled (and a
/// headless server stopped). Returns only when that failed.
pub fn setsid_exec(program: &str, args: &[String]) -> Error {
    if let Err(e) = rustix::process::setsid() {
        return Error::Other(format!("cannot start a session: {e}"));
    }
    // A caught signal is reset to its default by exec; an ignored one is not.
    if let Err(e) = signal_hook::flag::register(SIGTERM, Arc::new(AtomicBool::new(false))) {
        return Error::Other(format!("cannot reset SIGTERM: {e}"));
    }
    let e = Command::new(program).args(args).exec();
    match e.kind() {
        io::ErrorKind::NotFound => Error::Other(format!("{program} not found; install it or add it to PATH")),
        _ => Error::Other(format!("cannot run {program}: {e}")),
    }
}

/// The first executable called `program` on `PATH`.
pub fn which(program: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|path| fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// The epoch second `date -d` reads `phrase` as (`next week`, `+3 days`;
/// R4.1), with a `date` that reads relative phrases: GNU's (`gdate` on
/// macOS, where `date` is BSD's) or uutils'. None when there is no such
/// `date`, or it does not read `phrase`.
pub fn date_reads(phrase: &str) -> Option<i64> {
    let date = ["gdate", "date"].into_iter().filter_map(which).find(|date| reads_relative(date))?;
    date_epoch(&date, &["-d", phrase, "+%s"])
}

/// Whether `date` reads a relative phrase. Asked, not told by its version
/// text: BusyBox `date` reads `-d` too, but only absolute times.
fn reads_relative(date: &Path) -> bool {
    date_epoch(date, &["-u", "-d", "1970-01-01 +1 day", "+%s"]) == Some(86400)
}

/// The epoch second `date <args>` printed.
fn date_epoch(date: &Path, args: &[&str]) -> Option<i64> {
    match output_within(Command::new(date).args(args), PS_TIMEOUT) {
        Ok(Some((status, text))) if status.success() => text.trim().parse().ok(),
        _ => None,
    }
}

/// How to show a desktop notification: `$ITM_NOTIFY_COMMAND` (split like a
/// shell would) with the title and body as arguments, else `osascript` on
/// macOS (the text goes in as arguments, never into the script), else
/// `notify-send`. `open` asks `notify-send` for an Open action; it then
/// waits until the notification closes and prints `open` when it was
/// chosen (R3.3). None when this system has no way to notify.
#[derive(Debug, PartialEq)]
pub struct NotifyCommand {
    pub argv: Vec<String>,
    pub offers_open: bool,
}

pub fn notify_command(custom: Option<&str>, title: &str, body: &str, open: bool) -> Result<Option<NotifyCommand>> {
    let here = Notifiers { macos: env::consts::OS == "macos", have: &|program| which(program).is_some() };
    here.command(custom, title, body, open)
}

/// What this system offers to notify with.
struct Notifiers<'a> {
    macos: bool,
    have: &'a dyn Fn(&str) -> bool,
}

/// Shows its second argument as a notification titled with its first.
const OSASCRIPT: &str = "on run argv\ndisplay notification (item 2 of argv) with title (item 1 of argv)\nend run";

impl Notifiers<'_> {
    fn command(&self, custom: Option<&str>, title: &str, body: &str, open: bool) -> Result<Option<NotifyCommand>> {
        if let Some(custom) = custom.map(str::trim).filter(|c| !c.is_empty()) {
            let Some(mut argv) = shlex::split(custom) else { bail!("cannot read $ITM_NOTIFY_COMMAND: {custom}") };
            argv.extend([title.into(), body.into()]);
            return Ok(Some(NotifyCommand { argv, offers_open: false }));
        }
        if self.macos {
            let argv = ["osascript", "-e", OSASCRIPT, title, body].map(String::from).into();
            return Ok((self.have)("osascript").then_some(NotifyCommand { argv, offers_open: false }));
        }
        if !(self.have)("notify-send") {
            return Ok(None);
        }
        let mut argv = vec!["notify-send".into(), "--app-name=inbox-task-manager".into(), title.into(), body.into()];
        if open {
            argv.splice(1..1, ["--action=open=Open".into(), "--wait".into()]);
        }
        Ok(Some(NotifyCommand { argv, offers_open: open }))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::process::Command;

    /// Runs the test named `test` (its full path) alone in a child process,
    /// for what acts on the whole process (signals): None in that child, so
    /// the test does its work there; in the test itself, how the child
    /// ended and what it printed.
    pub fn in_child(test: &str) -> Option<std::process::Output> {
        if env::var("ITM_TEST_CHILD").is_ok_and(|t| t == test) {
            return None;
        }
        let exe = env::current_exe().unwrap();
        let args = ["--exact", test, "--nocapture", "--test-threads=1"];
        Some(Command::new(exe).args(args).env("ITM_TEST_CHILD", test).output().unwrap())
    }

    /// Whether this process ignores one of `signals` (`nohup`), so a test of them cannot run.
    pub fn ignores(signals: &[i32]) -> bool {
        let ignored = ignored_signals().unwrap_or(0);
        signals.iter().any(|&signal| is_set(ignored, signal))
    }

    /// A catcher catches even after an earlier one ended (that one's end
    /// leaves nothing behind that would end the process first), and once
    /// the last one ends the signals end the process again.
    #[test]
    fn signals_are_caught_by_each_catcher_and_end_the_process_after_the_last() {
        if ignores(&[SIGHUP, SIGTERM]) {
            return;
        }
        let Some(child) =
            in_child("system::tests::signals_are_caught_by_each_catcher_and_end_the_process_after_the_last")
        else {
            drop(ExitSignals::catch().unwrap());
            let mut second = ExitSignals::catch().unwrap();
            let attach_loop = Catching::start(&[SIGHUP, SIGTERM, SIGINT]).unwrap();
            signal_hook::low_level::raise(SIGHUP).unwrap();
            let caught = (0..100).find_map(|_| second.arrived().or_else(|| sleep_none(10)));
            assert_eq!(caught, Some(SIGHUP));
            drop(second);
            signal_hook::low_level::raise(SIGHUP).unwrap(); // still caught: the attach loop's catcher lives
            drop(attach_loop);
            println!("caught twice");
            signal_hook::low_level::raise(SIGTERM).unwrap();
            thread::sleep(Duration::from_secs(5));
            println!("SIGTERM did not end the process");
            return;
        };
        let said = String::from_utf8_lossy(&child.stdout);
        assert!(said.contains("caught twice") && !said.contains("did not end"), "{said}");
        assert_eq!(std::os::unix::process::ExitStatusExt::signal(&child.status), Some(SIGTERM), "{said}");
    }

    fn sleep_none(ms: u64) -> Option<i32> {
        thread::sleep(Duration::from_millis(ms));
        None
    }

    #[test]
    fn a_command_runs_to_its_end_or_is_killed_at_the_timeout() {
        let (status, text) =
            output_within(Command::new("sh").args(["-c", "echo open"]), Duration::from_secs(5)).unwrap().unwrap();
        assert!(status.success());
        assert_eq!(text, "open\n");
        let started = Instant::now();
        assert_eq!(output_within(Command::new("sleep").arg("5"), Duration::from_millis(100)).unwrap(), None);
        assert!(started.elapsed() < Duration::from_secs(2));
        // More than a pipe holds: read while it runs, or it would wait for the timeout.
        let (_, text) = output_within(Command::new("head").args(["-c", "300000", "/dev/zero"]), Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(text.len(), 300_000);
    }

    /// DESIGN.md section 2: signals ignored before a clone (`nohup`) stay
    /// ignored. Rust ignores SIGPIPE in every program, which shows each reader works.
    #[test]
    fn ignored_signals_are_read_from_the_process_table() {
        let sigpipe = 1 << (13 - 1);
        assert!(ignored_signals().is_some_and(|mask| mask & sigpipe != 0));
        if Ps::run(&["-o", "sigignore=", "-p", &std::process::id().to_string()]).is_some() {
            let ps = Ps::run(&["-o", "sigignore=", "-p", &std::process::id().to_string()]).unwrap();
            assert!(signal_mask(&ps).is_some_and(|mask| mask & sigpipe != 0), "{ps}");
        }
        assert_eq!(signal_mask("\t0000000000001000\n"), Some(sigpipe));
        assert_eq!(signal_mask(" 1000"), Some(sigpipe), "ps pads with spaces");
        assert_eq!(signal_mask("-"), None);
    }

    #[test]
    fn a_child_is_stopped_with_sigterm_then_killed() {
        let mut polite = Command::new("sleep").arg("30").spawn().unwrap();
        let status = stop_child(&mut polite, Duration::from_secs(5)).unwrap();
        assert_eq!(std::os::unix::process::ExitStatusExt::signal(&status), Some(15));
        let mut stubborn = Command::new("sh")
            .args(["-c", "trap '' TERM; echo ready; exec sleep 30"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = [0; 6];
        stubborn.stdout.take().unwrap().read_exact(&mut ready).unwrap(); // the trap is set
        let started = Instant::now();
        let status = stop_child(&mut stubborn, Duration::from_millis(200)).unwrap();
        assert_eq!(std::os::unix::process::ExitStatusExt::signal(&status), Some(9));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// R4.1: other phrases go to a `date` that reads relative phrases, when there is one.
    #[test]
    fn r4_1_a_date_that_reads_relative_phrases() {
        if let Some(fails) = which("false") {
            assert!(!reads_relative(&fails));
        }
        assert!(!reads_relative(Path::new("/nonexistent/date")));
        let Some(date) = ["gdate", "date"].into_iter().filter_map(which).find(|d| reads_relative(d)) else {
            return; // no GNU date here (macOS without coreutils): itm's own phrases only
        };
        let now = crate::times::now();
        let week = date_reads("next week").expect("GNU date reads next week");
        assert!((now + 7 * 86400 - 5..=now + 7 * 86400 + 5).contains(&week), "{week} is a week after {now}");
        assert_eq!(date_reads("monkey"), None);
        assert!(date_epoch(&date, &["-d", "@86400", "+%s"]) == Some(86400));
    }

    #[test]
    fn a_custom_notify_command_gets_title_and_body() {
        let command = notify_command(Some("notify --urgent 'a b'"), "T", "B", true).unwrap().unwrap();
        assert_eq!(command.argv, ["notify", "--urgent", "a b", "T", "B"]);
        assert!(!command.offers_open);
        assert!(notify_command(Some("notify 'open"), "T", "B", false).is_err());
    }

    /// R3.2, R3.3: osascript on macOS, with the text as arguments and no Open action;
    /// notify-send elsewhere, offering Open; nothing when neither is there.
    #[test]
    fn each_system_notifies_with_its_own_program() {
        let everything = |_: &str| true;
        let mac = Notifiers { macos: true, have: &everything };
        let command = mac.command(None, r#"say "hi""#, "B", true).unwrap().unwrap();
        assert_eq!(command.argv[..2], ["osascript", "-e"]);
        assert_eq!(command.argv[3..], [r#"say "hi""#, "B"], "arguments, never script text");
        assert!(!command.offers_open);

        let linux = Notifiers { macos: false, have: &everything };
        let command = linux.command(Some("  \t"), "T", "B", true).unwrap().unwrap();
        assert_eq!(
            command.argv,
            ["notify-send", "--action=open=Open", "--wait", "--app-name=inbox-task-manager", "T", "B"]
        );
        assert!(command.offers_open);
        assert!(!linux.command(None, "T", "B", false).unwrap().unwrap().offers_open);

        let nothing = |_: &str| false;
        for macos in [true, false] {
            assert_eq!(Notifiers { macos, have: &nothing }.command(None, "T", "B", true).unwrap(), None);
        }
    }

    /// `ps -ww -A -o pid= -o ppid= -o command=` as macOS and FreeBSD print it.
    const PS_TABLE: &str = "    1     0 /sbin/launchd
  512     1 /usr/libexec/logd
 4242  4100 /Users/me/.nix-profile/bin/itm attach task-a
 4243  4242 herdr session attach task-a
 4250  4242 herdr session list
 4300  4242 /Applications/My Tools/herdr session attach task-b
94512 94500 -zsh
";

    #[test]
    fn ps_output_is_read_into_command_lines() {
        let children = ps_children(PS_TABLE, 4242);
        let argv = |words: &str| words.split(' ').map(str::to_string).collect::<Vec<_>>();
        assert_eq!(
            children,
            [
                (4243, argv("herdr session attach task-a")),
                (4250, argv("herdr session list")),
                // ps joins arguments with spaces: a path with one comes back split.
                (4300, argv("/Applications/My Tools/herdr session attach task-b")),
            ]
        );
        assert_eq!(ps_children(PS_TABLE, 7), []);
        assert_eq!(ps_children("  PID  PPID COMMAND\n", 0), [], "a header is no process");
        assert_eq!(
            ps_command("/usr/bin/python3 /home/me/.local/bin/itm attach\n"),
            Some(argv("/usr/bin/python3 /home/me/.local/bin/itm attach"))
        );
        assert_eq!(ps_command(""), None, "ps prints nothing for a process that is gone");
    }

    /// Both readers where they work: /proc, and ps when it takes BSD-style options.
    fn tables() -> Vec<(&'static str, Box<dyn Processes>)> {
        let mut tables: Vec<(&str, Box<dyn Processes>)> = vec![];
        if fs::metadata("/proc/self/cmdline").is_ok() {
            tables.push(("proc", Box::new(Proc)));
        }
        if Ps::run(&["-A", "-o", "pid="]).is_some() {
            tables.push(("ps", Box::new(Ps)));
        }
        assert!(!tables.is_empty(), "no way to read the process table");
        tables
    }

    #[test]
    fn every_process_table_reads_command_lines_children_and_endings() {
        // Not a faked argv[0]: coreutils built as one program picks the tool by it.
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let me = std::process::id();
        let expected = ["sleep".to_string(), "30".to_string()];
        for (name, table) in tables() {
            // spawn returns while the kernel still sets up the new program's arguments.
            let argv = (0..100).find_map(|_| {
                std::thread::sleep(Duration::from_millis(10));
                table.argv(child.id()).filter(|argv| argv[..] == expected)
            });
            assert_eq!(argv.as_deref(), Some(&expected[..]), "{name}");
            assert!(table.children(me).iter().any(|(pid, argv)| *pid == child.id() && argv[..] == expected), "{name}");
            assert!(table.alive(child.id()), "{name}");
        }
        terminate(child.id());
        child.wait().unwrap();
        for (name, table) in tables() {
            assert_eq!(table.argv(child.id()), None, "{name}");
            assert!(!table.alive(child.id()), "{name}");
        }
    }
}
