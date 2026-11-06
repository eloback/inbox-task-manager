//! What itm needs from the operating system: the process table (to validate
//! attach-loop markers and find their `herdr session attach` children),
//! signals, programs on `PATH` and the desktop notification command.
//! Linux only for now: the process table is read from `/proc`.

use std::env;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process, test_kill_process};

use crate::error::{Result, bail};

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

/// The real process table, from `/proc`.
pub struct Proc;

impl Processes for Proc {
    fn argv(&self, pid: u32) -> Option<Vec<String>> {
        let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        Some(
            raw.split(|&b| b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect(),
        )
    }

    fn alive(&self, pid: u32) -> bool {
        match pid_of(pid) {
            Some(pid) => !matches!(test_kill_process(pid), Err(rustix::io::Errno::SRCH)), // EPERM: someone else's
            None => false,
        }
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
        if let Some(pid) = pid_of(pid) {
            let _ = kill_process(pid, Signal::TERM);
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
        let mut text = String::new();
        stdout.read_to_string(&mut text).map(|_| text)
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

/// The first executable called `program` on `PATH`.
pub fn which(program: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|path| fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// How to show a desktop notification: `$ITM_NOTIFY_COMMAND` (split like a
/// shell would) with the title and body as arguments, else `notify-send`.
/// `open` asks `notify-send` for an Open action; it then waits until the
/// notification closes and prints `open` when it was chosen (R3.3).
#[derive(Debug, PartialEq)]
pub struct NotifyCommand {
    pub argv: Vec<String>,
    pub offers_open: bool,
}

pub fn notify_command(custom: Option<&str>, title: &str, body: &str, open: bool) -> Result<Option<NotifyCommand>> {
    if let Some(custom) = custom.map(str::trim).filter(|c| !c.is_empty()) {
        let Some(mut argv) = shlex::split(custom) else { bail!("cannot read $ITM_NOTIFY_COMMAND: {custom}") };
        argv.extend([title.into(), body.into()]);
        return Ok(Some(NotifyCommand { argv, offers_open: false }));
    }
    if which("notify-send").is_none() {
        return Ok(None);
    }
    let mut argv = vec!["notify-send".into(), "--app-name=inbox-task-manager".into(), title.into(), body.into()];
    if open {
        argv.splice(1..1, ["--action=open=Open".into(), "--wait".into()]);
    }
    Ok(Some(NotifyCommand { argv, offers_open: open }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn reads_command_lines_and_children_from_proc() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let me = std::process::id();
        // spawn returns while the kernel still sets up the new program's arguments.
        let argv = (0..100).find_map(|_| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            Proc.argv(child.id()).filter(|argv| !argv.is_empty())
        });
        assert_eq!(argv, Some(vec!["sleep".to_string(), "30".to_string()]));
        assert!(Proc.children(me).iter().any(|(pid, argv)| *pid == child.id() && argv == &["sleep", "30"]));
        assert!(Proc.alive(child.id()));
        Proc.terminate(child.id());
        child.wait().unwrap();
        assert_eq!(Proc.argv(child.id()), None);
        assert!(!Proc.alive(child.id()));
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

    #[test]
    fn a_custom_notify_command_gets_title_and_body() {
        let command = notify_command(Some("notify --urgent 'a b'"), "T", "B", true).unwrap().unwrap();
        assert_eq!(command.argv, ["notify", "--urgent", "a b", "T", "B"]);
        assert!(!command.offers_open);
        assert!(notify_command(Some("notify 'open"), "T", "B", false).is_err());
    }
}
