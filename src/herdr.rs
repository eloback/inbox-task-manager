//! Herdr, the terminal multiplexer whose sessions are itm's tasks.
//!
//! Queries and notifications go over each session's socket API: one JSON
//! line each way per connection (DESIGN.md, section 1). That is 10–20
//! times faster than the CLI, starts no process, and is not refused after a
//! Herdr upgrade the way the CLI's protocol check is. Sessions are found the
//! way `herdr session list` finds them, by reading Herdr's sessions folder.
//! Only `herdr session attach` (an interactive client) and `herdr server`
//! (a server start) run the `herdr` program.
//!
//! Every connection and every answer has a deadline (DESIGN.md section 2):
//! a session that stops accepting connections (a deadlock, a SIGSTOP) must
//! not hang `itm list`, an archive or the daemon.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use std::{fs, io, thread};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use rustix::net::{AddressFamily, SocketAddrUnix, SocketType};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{Context, Error, HerdrFailure, Result};
use crate::status::AgentState;
use crate::system::{in_new_session, which};

/// How long a connection may take. A healthy session accepts at once.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// How long Herdr may take to answer a request.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a headless server may take to answer after it was started.
const SERVER_START: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub name: String,
    pub running: bool,
    pub socket: PathBuf,
}

pub type Sessions = BTreeMap<String, Session>;

/// An agent in a session (`agent.list`). Decoded tolerantly: Herdr adds
/// fields often, and only these matter to itm.
#[derive(Debug, Clone, Deserialize)]
pub struct Agent {
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default = "unknown")]
    pub agent_status: AgentState,
    #[serde(default)]
    pub pane_id: Option<String>,
    /// Changes on every state transition, so one wait is reported once.
    #[serde(default)]
    pub state_change_seq: Option<Value>,
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
}

fn unknown() -> AgentState {
    AgentState::Unknown
}

pub struct Herdr {
    /// `$XDG_CONFIG_HOME/herdr`.
    config: PathBuf,
    connect_timeout: Duration,
    answer_timeout: Duration,
}

impl Herdr {
    pub fn new(config: PathBuf) -> Herdr {
        Herdr { config, connect_timeout: CONNECT_TIMEOUT, answer_timeout: ANSWER_TIMEOUT }
    }

    /// Every session Herdr knows (running or stopped), like `herdr session
    /// list`: the default session, plus each folder under `sessions/` with a
    /// valid name. A session runs when its socket takes a connection; one
    /// that exists but does not take it in time runs too, stuck: its
    /// requests then time out instead of the session looking stopped (which
    /// would wake tasks snoozed until an agent asks).
    pub fn sessions(&self) -> Result<Sessions> {
        let mut found = vec![("default".to_string(), self.config.join("herdr.sock"))];
        let dir = self.config.join("sessions");
        match fs::read_dir(&dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if valid_session_name(&name) && entry.path().is_dir() {
                        found.push((name, entry.path().join("herdr.sock")));
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).context(|| format!("cannot list Herdr sessions in {}", dir.display())),
        }
        Ok(found
            .into_iter()
            .map(|(name, socket)| {
                let running = match connect(&socket, self.connect_timeout) {
                    Ok(_) => true,
                    Err(e) => e.kind() == ErrorKind::TimedOut,
                };
                (name.clone(), Session { name, running, socket })
            })
            .collect())
    }

    /// Where a session's socket is, whether or not it runs.
    pub fn socket(&self, session: &str) -> PathBuf {
        match session {
            "default" => self.config.join("herdr.sock"),
            name => self.config.join("sessions").join(name).join("herdr.sock"),
        }
    }

    pub fn agents(&self, socket: &Path) -> Result<Vec<Agent>> {
        #[derive(Deserialize)]
        struct AgentList {
            agents: Vec<Agent>,
        }
        let list: AgentList = decode("agent.list", self.call(socket, "agent.list", json!({}))?)?;
        Ok(list.agents)
    }

    /// A toast in the session's attached client; false when Herdr did not
    /// show it (toasts off, no client, rate limited).
    pub fn toast(&self, socket: &Path, title: &str, body: &str) -> Result<bool> {
        let result =
            self.call(socket, "notification.show", json!({"title": title, "body": body, "sound": "request"}))?;
        Ok(result.get("shown").and_then(Value::as_bool).unwrap_or(false))
    }

    /// Ask a running session to stop. Herdr answers before it shuts down,
    /// so the caller may be a process inside that very session.
    pub fn stop(&self, socket: &Path) -> Result<()> {
        self.call(socket, "server.stop", json!({})).map(drop)
    }

    /// How many workspaces a running session has.
    pub fn workspace_count(&self, socket: &Path) -> Result<usize> {
        let result = self.call(socket, "workspace.list", json!({}))?;
        Ok(result.get("workspaces").and_then(Value::as_array).map_or(0, Vec::len))
    }

    /// Open a workspace in `folder`, named `label`. Herdr renames an unnamed
    /// workspace whenever its first pane changes folder; a named one keeps
    /// its name. A folder that does not exist would silently become `$HOME`,
    /// so callers check it first.
    pub fn create_workspace(&self, socket: &Path, folder: &Path, label: &str, focus: bool) -> Result<()> {
        self.call(socket, "workspace.create", json!({"cwd": folder, "label": label, "focus": focus})).map(drop)
    }

    /// Start a session headless (`herdr server`) in `cwd`, and wait until it
    /// answers. The server gets no `HERDR_*` variables from wherever itm runs
    /// but `HERDR_SESSION`, and its own session (setsid), so it outlives this
    /// terminal. One that exits with an error is not waited for.
    pub fn start_server(&self, name: &str, cwd: &Path) -> Result<PathBuf> {
        if which("herdr").is_none() {
            return Err(Error::Missing("herdr"));
        }
        let mut command = in_new_session("herdr").context(|| "cannot start herdr in its own session".into())?;
        for (key, _) in std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("HERDR_")) {
            command.env_remove(key);
        }
        command.arg("server").env("HERDR_SESSION", name).current_dir(cwd);
        let mut server = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context(|| "cannot start herdr in its own session".into())?;
        let socket = self.socket(name);
        let deadline = Instant::now() + SERVER_START;
        let started = loop {
            if connect(&socket, self.connect_timeout).is_ok() {
                break Ok(socket);
            }
            if let Ok(Some(status)) = server.try_wait()
                && !status.success()
            {
                break Err(Error::Other(format!("herdr server for {name} exited with {status}")));
            }
            if Instant::now() > deadline {
                break Err(Error::Other(format!("session {name} did not start")));
            }
            thread::sleep(Duration::from_millis(25));
        };
        // Reaped whenever it ends, so it leaves no zombie behind a long-running attach loop.
        thread::spawn(move || server.wait());
        started
    }

    /// Have `herdr session delete` remove a stopped session's saved layout
    /// and agents, in a process of its own session that tries again until
    /// the server is gone (Herdr refuses to delete a running session; a
    /// stop is answered before the server goes) and outlives this one,
    /// which may be inside the session. Its outcome is not waited for.
    pub fn delete_detached(&self, name: &str) -> io::Result<()> {
        const RETRY: &str = r#"for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30; do
  herdr session delete "$1" --json >/dev/null 2>&1 && exit 0; sleep 0.5
done; exit 1"#;
        let mut sh = in_new_session("/bin/sh")?;
        sh.args(["-c", RETRY, "sh", name]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        for (key, _) in std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("HERDR_")) {
            sh.env_remove(key); // the session to delete is named; nothing else should point herdr elsewhere
        }
        let mut child = sh.spawn()?;
        thread::spawn(move || child.wait()); // reaped whenever it ends
        Ok(())
    }

    /// Run `herdr session attach <name>` in this terminal until it detaches.
    pub fn attach(&self, name: &str, cwd: Option<&Path>) -> Result<()> {
        let mut command = Command::new("herdr");
        command.args(["session", "attach", name]);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        command.status().map_err(missing_herdr)?;
        Ok(())
    }
}

/// Herdr's rule for session folder names (`session.rs`, `validate_name`).
fn valid_session_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name != "."
        && name != ".."
        && name != "default"
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// "herdr not found" for a missing program, the error itself otherwise.
pub fn missing_herdr(e: io::Error) -> Error {
    if e.kind() == ErrorKind::NotFound {
        Error::Missing("herdr")
    } else {
        Error::Other(format!("cannot run herdr: {e}"))
    }
}

impl Herdr {
    /// One request on its own connection: Herdr answers one line and closes.
    fn call(&self, socket: &Path, method: &'static str, params: Value) -> Result<Value> {
        let failed = |failure| Error::Herdr { method, failure };
        let io_failed = |e: io::Error| {
            failed(match e.kind() {
                ErrorKind::NotFound | ErrorKind::ConnectionRefused => HerdrFailure::NotRunning,
                ErrorKind::WouldBlock | ErrorKind::TimedOut => HerdrFailure::TimedOut,
                _ => HerdrFailure::Io(e.to_string()),
            })
        };
        let mut stream = connect(socket, self.connect_timeout).map_err(io_failed)?;
        stream.set_read_timeout(Some(self.answer_timeout)).map_err(io_failed)?;
        stream.set_write_timeout(Some(self.answer_timeout)).map_err(io_failed)?;
        let request = json!({"id": "itm", "method": method, "params": params});
        writeln!(stream, "{request}").map_err(io_failed)?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).map_err(io_failed)?;

        #[derive(Deserialize)]
        struct Response {
            result: Option<Value>,
            error: Option<ApiError>,
        }
        #[derive(Deserialize)]
        struct ApiError {
            code: String,
            #[serde(default)]
            message: String,
        }
        match serde_json::from_str::<Response>(&line) {
            Ok(Response { result: Some(result), .. }) => Ok(result),
            Ok(Response { error: Some(ApiError { code, message }), .. }) => {
                Err(failed(HerdrFailure::Refused { code, message }))
            }
            Ok(_) => Err(failed(HerdrFailure::BadAnswer("no result"))),
            Err(_) => Err(failed(HerdrFailure::BadAnswer("invalid JSON"))),
        }
    }
}

/// Connect to a session's socket within `timeout`. A blocking connect to a
/// server that stopped accepting waits forever once its backlog is full,
/// so the socket is non-blocking: Linux refuses a full backlog at once
/// (EAGAIN; nothing is queued), which is tried again until the deadline,
/// and other systems may answer EINPROGRESS, which is waited for.
fn connect(socket: &Path, timeout: Duration) -> io::Result<UnixStream> {
    let deadline = Instant::now() + timeout;
    let address = SocketAddrUnix::new(socket)?;
    loop {
        let fd = nonblocking_socket()?;
        let left = deadline.saturating_duration_since(Instant::now());
        match rustix::net::connect(&fd, &address) {
            Ok(()) => return blocking(fd),
            Err(Errno::INPROGRESS) => {
                let mut fds = [PollFd::new(&fd, PollFlags::OUT)];
                let left = Timespec::try_from(left).map_err(|_| io::Error::from(ErrorKind::TimedOut))?;
                if poll(&mut fds, Some(&left))? == 0 {
                    return Err(ErrorKind::TimedOut.into());
                }
                rustix::net::sockopt::socket_error(&fd)??;
                return blocking(fd);
            }
            Err(Errno::AGAIN) if !left.is_zero() => thread::sleep(left.min(Duration::from_millis(10))),
            Err(Errno::AGAIN) => return Err(ErrorKind::TimedOut.into()),
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(not(target_vendor = "apple"))]
fn nonblocking_socket() -> io::Result<OwnedFd> {
    use rustix::net::SocketFlags;
    Ok(rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        None,
    )?)
}

/// macOS has no SOCK_CLOEXEC or SOCK_NONBLOCK: the flags are set right after.
#[cfg(target_vendor = "apple")]
fn nonblocking_socket() -> io::Result<OwnedFd> {
    let fd = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None)?;
    rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC)?;
    rustix::io::ioctl_fionbio(&fd, true)?;
    Ok(fd)
}

/// The connected socket as a stream whose reads and writes block (with timeouts).
fn blocking(fd: OwnedFd) -> io::Result<UnixStream> {
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(false)?;
    Ok(stream)
}

fn decode<T: for<'de> Deserialize<'de>>(method: &'static str, result: Value) -> Result<T> {
    serde_json::from_value(result)
        .map_err(|_| Error::Herdr { method, failure: HerdrFailure::BadAnswer("an unexpected answer") })
}

#[cfg(test)]
pub mod fake {
    //! A Herdr session socket that answers canned JSON lines, for tests.
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// Serves `answer(method, params)` on `<config>/sessions/<name>/herdr.sock`
    /// (or the default session's socket) and records each request.
    pub fn serve(
        config: &Path,
        name: &str,
        answer: impl Fn(&str, &Value) -> Value + Send + 'static,
    ) -> Arc<Mutex<Vec<(String, Value)>>> {
        let socket = Herdr::new(config.to_path_buf()).socket(name);
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let seen = Arc::new(Mutex::new(vec![]));
        let log = Arc::clone(&seen);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut line = String::new();
                if BufReader::new(&stream).read_line(&mut line).unwrap_or(0) == 0 {
                    continue; // a liveness check: connect and close
                }
                let request: Value = serde_json::from_str(&line).unwrap();
                let method = request["method"].as_str().unwrap().to_string();
                let reply = answer(&method, &request["params"]);
                log.lock().unwrap().push((method, request["params"].clone()));
                let _ = writeln!(&stream, "{}", json!({"id": request["id"], "result": reply}));
            }
        });
        seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_are_the_folders_with_running_ones_answering() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("herdr");
        fs::create_dir_all(config.join("sessions/task-stopped")).unwrap();
        fs::create_dir_all(config.join("sessions/bad name")).unwrap();
        fs::write(config.join("sessions/a-file"), "").unwrap();
        fake::serve(&config, "task-a", |_, _| json!({}));
        let sessions = Herdr::new(config.clone()).sessions().unwrap();
        assert_eq!(sessions.keys().collect::<Vec<_>>(), ["default", "task-a", "task-stopped"]);
        assert!(sessions["task-a"].running);
        assert!(!sessions["task-stopped"].running && !sessions["default"].running);
        assert_eq!(sessions["task-a"].socket, config.join("sessions/task-a/herdr.sock"));
    }

    #[test]
    fn no_herdr_folder_means_only_the_default_session() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = Herdr::new(tmp.path().join("herdr")).sessions().unwrap();
        assert_eq!(sessions.keys().collect::<Vec<_>>(), ["default"]);
    }

    #[test]
    fn agents_are_decoded_tolerantly() {
        let tmp = tempfile::tempdir().unwrap();
        let herdr = Herdr::new(tmp.path().to_path_buf());
        fake::serve(tmp.path(), "task-a", |_, _| {
            json!({"type": "agent_list", "agents": [
                {"agent": "claude", "agent_status": "blocked", "pane_id": "w1:p1", "state_change_seq": 5,
                 "terminal_title_stripped": "Fix it", "new_field": [1]},
                {"agent_status": "pondering"}]})
        });
        let agents = herdr.agents(&herdr.socket("task-a")).unwrap();
        assert_eq!(agents[0].agent_status, AgentState::Blocked);
        assert_eq!(agents[0].state_change_seq, Some(json!(5)));
        assert_eq!((agents[1].agent_status, agents[1].pane_id.as_deref()), (AgentState::Unknown, None));
    }

    #[test]
    fn errors_name_the_method() {
        let tmp = tempfile::tempdir().unwrap();
        let herdr = Herdr::new(tmp.path().to_path_buf());
        let err = herdr.agents(&herdr.socket("task-gone")).unwrap_err();
        assert_eq!(err, Error::Herdr { method: "agent.list", failure: HerdrFailure::NotRunning });
        assert_eq!(err.to_string(), "herdr agent.list: session not running");

        let socket = herdr.socket("task-x");
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                let reply = r#"{"id":"itm","error":{"code":"invalid_params","message":"notification title is empty"}}"#;
                writeln!(&stream, "{reply}").unwrap();
            }
        });
        let err = herdr.toast(&socket, "", "").unwrap_err();
        assert_eq!(err.to_string(), "herdr notification.show failed: notification title is empty (invalid_params)");
    }

    /// A Herdr with short timeouts, so the tests of a stuck session are quick.
    fn impatient(config: &Path) -> Herdr {
        Herdr {
            connect_timeout: Duration::from_millis(100),
            answer_timeout: Duration::from_millis(200),
            ..Herdr::new(config.into())
        }
    }

    /// A session socket that never accepts: listening, with its backlog full.
    /// Returns what must stay open for it to stay that way.
    fn stuck_session(socket: &Path) -> (OwnedFd, Vec<OwnedFd>) {
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
        let address = SocketAddrUnix::new(socket).unwrap();
        rustix::net::bind(&listener, &address).unwrap();
        rustix::net::listen(&listener, 0).unwrap();
        let mut queued = vec![];
        loop {
            let fd = nonblocking_socket().unwrap();
            match rustix::net::connect(&fd, &address) {
                Ok(()) | Err(Errno::INPROGRESS) => queued.push(fd),
                Err(Errno::AGAIN) => break,
                Err(e) => panic!("{e}"),
            }
            assert!(queued.len() < 1000, "the backlog never filled");
        }
        (listener, queued)
    }

    /// DESIGN.md section 2: every Herdr query has a timeout, the connection included.
    #[test]
    fn a_session_that_never_accepts_times_out_instead_of_hanging() {
        let tmp = tempfile::tempdir().unwrap();
        let herdr = impatient(tmp.path());
        let socket = herdr.socket("task-stuck");
        let _stuck = stuck_session(&socket);

        let started = Instant::now();
        let err = herdr.agents(&socket).unwrap_err();
        assert_eq!(err, Error::Herdr { method: "agent.list", failure: HerdrFailure::TimedOut });
        assert_eq!(herdr.stop(&socket).unwrap_err().to_string(), "herdr server.stop timed out");
        let sessions = herdr.sessions().unwrap();
        assert!(sessions["task-stuck"].running, "stuck, not stopped: tasks snoozed until an agent stay snoozed");
        assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
    }

    #[test]
    fn a_session_that_never_answers_times_out() {
        let tmp = tempfile::tempdir().unwrap();
        let herdr = impatient(tmp.path());
        let socket = herdr.socket("task-silent");
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let held = std::thread::spawn(move || listener.accept().map(|(stream, _)| stream));
        let err = herdr.agents(&socket).unwrap_err();
        assert_eq!(err, Error::Herdr { method: "agent.list", failure: HerdrFailure::TimedOut });
        drop(held.join());
    }
}
