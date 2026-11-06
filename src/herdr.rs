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

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError};
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
/// How long a session that refuses connections but was found running
/// (macOS, the BSDs: [`refused_by_a_live_server`]) counts as running
/// before it is checked again.
const RECHECK_REFUSER: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub name: String,
    pub running: bool,
    pub socket: PathBuf,
}

pub type Sessions = BTreeMap<String, Session>;

/// An agent in a session (`session.snapshot`, as `agent.list` lists them). Decoded tolerantly: Herdr adds
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

/// A workspace of a running session, in the session's order.
#[derive(Debug, Clone, PartialEq)]
pub struct Workspace {
    pub id: String,
    /// What Herdr shows: its name, or for an unnamed one a label Herdr made
    /// up from its folder (DESIGN.md section 1).
    pub label: String,
    pub focused: bool,
    /// The folder Herdr names an unnamed workspace after: the working folder
    /// of the first pane (in layout order) of its first tab (in tab order).
    /// None when Herdr does not report it.
    pub folder: Option<PathBuf>,
}

/// What one `session.snapshot` shows of a session.
#[derive(Debug, Clone)]
pub struct SessionView {
    pub agents: Vec<Agent>,
    /// None when the snapshot's workspaces, tabs, layouts or panes are not
    /// what itm reads (the agents are still read).
    pub workspaces: Option<Vec<Workspace>>,
}

/// The parts of a snapshot a workspace's folder is read from. No call
/// reports the folder Herdr names a workspace after, so it is found here.
#[derive(Deserialize)]
struct Layout {
    workspaces: Vec<LayoutWorkspace>,
    tabs: Vec<LayoutTab>,
    layouts: Vec<TabLayout>,
    panes: Vec<LayoutPane>,
}

#[derive(Deserialize)]
struct LayoutWorkspace {
    workspace_id: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    focused: Option<bool>,
}

#[derive(Deserialize)]
struct LayoutTab {
    tab_id: String,
    workspace_id: String,
}

#[derive(Deserialize)]
struct TabLayout {
    tab_id: String,
    panes: Vec<PaneRef>,
}

#[derive(Deserialize)]
struct PaneRef {
    pane_id: String,
}

#[derive(Deserialize)]
struct LayoutPane {
    pane_id: String,
    #[serde(default)]
    cwd: Option<String>,
}

impl Layout {
    fn workspaces(self) -> Vec<Workspace> {
        let mut first_tab: BTreeMap<&str, &str> = BTreeMap::new();
        for tab in &self.tabs {
            first_tab.entry(&tab.workspace_id).or_insert(&tab.tab_id);
        }
        let first_pane: BTreeMap<&str, &str> = (self.layouts.iter())
            .filter_map(|layout| Some((layout.tab_id.as_str(), layout.panes.first()?.pane_id.as_str())))
            .collect();
        let cwd: BTreeMap<&str, &str> =
            self.panes.iter().filter_map(|pane| Some((pane.pane_id.as_str(), pane.cwd.as_deref()?))).collect();
        (self.workspaces.iter())
            .map(|w| {
                let tab = first_tab.get(w.workspace_id.as_str());
                let folder = tab.and_then(|tab| first_pane.get(tab)).and_then(|pane| cwd.get(pane));
                Workspace {
                    id: w.workspace_id.clone(),
                    label: w.label.clone().unwrap_or_default(),
                    focused: w.focused.unwrap_or(false),
                    folder: folder.filter(|f| !f.is_empty()).map(PathBuf::from),
                }
            })
            .collect()
    }
}

pub struct Herdr {
    /// `$XDG_CONFIG_HOME/herdr`.
    config: PathBuf,
    connect_timeout: Duration,
    answer_timeout: Duration,
    refusers: LiveRefusers,
}

impl Herdr {
    pub fn new(config: PathBuf) -> Herdr {
        Herdr {
            config,
            connect_timeout: CONNECT_TIMEOUT,
            answer_timeout: ANSWER_TIMEOUT,
            refusers: LiveRefusers::default(),
        }
    }

    fn connect(&self, socket: &Path) -> io::Result<UnixStream> {
        connect(socket, self.connect_timeout, FULL_IS_REFUSED.then_some(&self.refusers))
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
                let running = match self.connect(&socket) {
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

    /// A session's agents and workspaces, in one request.
    pub fn snapshot(&self, socket: &Path) -> Result<SessionView> {
        const METHOD: &str = "session.snapshot";
        #[derive(Deserialize)]
        struct Answer {
            snapshot: Value,
        }
        #[derive(Deserialize)]
        struct Agents {
            agents: Vec<Agent>,
        }
        let Answer { snapshot } = decode(METHOD, self.call(socket, METHOD, json!({}))?)?;
        let Agents { agents } = decode(METHOD, &snapshot)?;
        let workspaces = Layout::deserialize(&snapshot).ok().map(Layout::workspaces);
        Ok(SessionView { agents, workspaces })
    }

    /// Name a workspace. A named workspace keeps its name when its folder
    /// changes; Herdr takes a label starting with "-" as it is.
    pub fn rename_workspace(&self, socket: &Path, id: &str, label: &str) -> Result<()> {
        self.call(socket, "workspace.rename", json!({"workspace_id": id, "label": label})).map(drop)
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
            if self.connect(&socket).is_ok() {
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
        let mut stream = self.connect(socket).map_err(io_failed)?;
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

/// Whether a full backlog is refused like a socket nobody listens on
/// (ECONNREFUSED): so on macOS and the BSDs; Linux says EAGAIN instead.
const FULL_IS_REFUSED: bool = !cfg!(any(target_os = "linux", target_os = "android"));

/// Connect to a session's socket within `timeout`. A blocking connect to a
/// server that stopped accepting waits forever once its backlog is full,
/// so the socket is non-blocking. A full backlog is refused at once
/// (nothing is queued) and tried again until the deadline: Linux says
/// EAGAIN, macOS and the BSDs ECONNREFUSED, as for a server that is gone,
/// so there (`refusers`) a refusal from a server still running counts as a
/// full backlog. Other systems may answer EINPROGRESS, which is waited for.
fn connect(socket: &Path, timeout: Duration, refusers: Option<&LiveRefusers>) -> io::Result<UnixStream> {
    let deadline = Instant::now() + timeout;
    let address = SocketAddrUnix::new(socket)?;
    let mut alive = None; // asked once: whether a refusing server still runs
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
            Err(Errno::AGAIN) => {}
            Err(Errno::CONNREFUSED)
                if refusers.is_some_and(|refusers| *alive.get_or_insert_with(|| refusers.alive(socket))) => {}
            Err(e) => return Err(e.into()),
        }
        // The backlog is full.
        if left.is_zero() {
            return Err(ErrorKind::TimedOut.into());
        }
        thread::sleep(left.min(Duration::from_millis(10)));
    }
}

/// The sessions that refused a connection but were found running, and
/// when: the check queues a connection on the session's client socket that
/// a stuck server never takes, so it runs at most once a minute per
/// session and its client socket's backlog (128 on macOS) lasts an hour or
/// more of a hang, not minutes. A process (the daemon, the switcher) keeps one.
#[derive(Default)]
struct LiveRefusers(Mutex<HashMap<PathBuf, Instant>>);

impl LiveRefusers {
    fn alive(&self, socket: &Path) -> bool {
        let mut found = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if found.get(socket).is_some_and(|at| at.elapsed() < RECHECK_REFUSER) {
            return true;
        }
        let alive = refused_by_a_live_server(socket);
        match alive {
            true => found.insert(socket.into(), Instant::now()),
            false => found.remove(socket),
        };
        alive
    }
}

/// Whether the server behind a session socket that refused a connection
/// still runs, stuck with a full backlog, rather than gone. The session's
/// other socket tells: a running server listens on `herdr-client.sock`
/// too, and its backlog fills only with clients trying to attach. A
/// session that stopped removes both sockets; one whose server crashed
/// keeps both, and nothing listens on either. The connection this queues
/// is closed at once; Herdr reads it as a client that left.
fn refused_by_a_live_server(socket: &Path) -> bool {
    let Ok(address) = SocketAddrUnix::new(socket.with_file_name("herdr-client.sock")) else {
        return false;
    };
    let Ok(fd) = nonblocking_socket() else {
        return false;
    };
    matches!(rustix::net::connect(&fd, &address), Ok(()) | Err(Errno::INPROGRESS | Errno::AGAIN))
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

fn decode<'de, T: Deserialize<'de>>(method: &'static str, result: impl serde::Deserializer<'de>) -> Result<T> {
    T::deserialize(result)
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
            json!({"type": "session_snapshot", "snapshot": {"agents": [
                {"agent": "claude", "agent_status": "blocked", "pane_id": "w1:p1", "state_change_seq": 5,
                 "terminal_title_stripped": "Fix it", "new_field": [1]},
                {"agent_status": "pondering"}]}})
        });
        let view = herdr.snapshot(&herdr.socket("task-a")).unwrap();
        let agents = view.agents;
        assert_eq!(agents[0].agent_status, AgentState::Blocked);
        assert_eq!(agents[0].state_change_seq, Some(json!(5)));
        assert_eq!((agents[1].agent_status, agents[1].pane_id.as_deref()), (AgentState::Unknown, None));
        assert_eq!(view.workspaces, None, "no workspaces in it: they are unknown, the agents are still read");
    }

    /// R10.2: a workspace's folder is the working folder of the first pane
    /// (in layout order) of its first tab (in tab order).
    #[test]
    fn a_workspaces_folder_is_its_first_tabs_first_pane() {
        let tmp = tempfile::tempdir().unwrap();
        let herdr = Herdr::new(tmp.path().to_path_buf());
        let calls = fake::serve(tmp.path(), "task-a", |_, _| {
            json!({"type": "session_snapshot", "snapshot": {
                "workspaces": [{"workspace_id": "w1", "label": "api", "focused": false},
                               {"workspace_id": "w2", "label": "web", "focused": true},
                               {"workspace_id": "w3", "label": "empty"}],
                // Tab order, not numbers: w1's second tab was moved to the front.
                "tabs": [{"tab_id": "w1:t3", "workspace_id": "w1"}, {"tab_id": "w1:t1", "workspace_id": "w1"},
                         {"tab_id": "w2:t1", "workspace_id": "w2"}],
                "layouts": [{"tab_id": "w1:t1", "panes": [{"pane_id": "w1:p1"}]},
                            {"tab_id": "w1:t3", "panes": [{"pane_id": "w1:p5"}, {"pane_id": "w1:p2"}]},
                            {"tab_id": "w2:t1", "panes": [{"pane_id": "w2:p1"}]}],
                "panes": [{"pane_id": "w1:p1", "cwd": "/a"}, {"pane_id": "w1:p5", "cwd": "/b"},
                          {"pane_id": "w1:p2", "cwd": "/c"}, {"pane_id": "w2:p1", "cwd": "/d"}],
                "agents": []}})
        });
        let view = herdr.snapshot(&herdr.socket("task-a")).unwrap();
        let workspace = |id: &str, label: &str, focused, folder: Option<&str>| Workspace {
            id: id.into(),
            label: label.into(),
            focused,
            folder: folder.map(PathBuf::from),
        };
        assert_eq!(
            view.workspaces.unwrap(),
            [
                workspace("w1", "api", false, Some("/b")),
                workspace("w2", "web", true, Some("/d")),
                workspace("w3", "empty", false, None)
            ]
        );
        assert_eq!(calls.lock().unwrap()[0].0, "session.snapshot");
        let err = herdr.snapshot(&herdr.socket("task-gone")).unwrap_err();
        assert_eq!(err.to_string(), "herdr session.snapshot: session not running");
    }

    #[test]
    fn errors_name_the_method() {
        let tmp = tempfile::tempdir().unwrap();
        let herdr = Herdr::new(tmp.path().to_path_buf());
        let err = herdr.snapshot(&herdr.socket("task-gone")).unwrap_err();
        assert_eq!(err, Error::Herdr { method: "session.snapshot", failure: HerdrFailure::NotRunning });
        assert_eq!(err.to_string(), "herdr session.snapshot: session not running");

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

    /// A session whose server never accepts: listening, with its backlog
    /// full, and listening on its client socket too, as Herdr does.
    /// Returns what must stay open for it to stay that way.
    fn stuck_session(socket: &Path) -> (OwnedFd, Vec<OwnedFd>, std::os::unix::net::UnixListener) {
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let client = std::os::unix::net::UnixListener::bind(socket.with_file_name("herdr-client.sock")).unwrap();
        let listener = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
        let address = SocketAddrUnix::new(socket).unwrap();
        rustix::net::bind(&listener, &address).unwrap();
        rustix::net::listen(&listener, 0).unwrap();
        let mut queued = vec![];
        loop {
            let fd = nonblocking_socket().unwrap();
            match rustix::net::connect(&fd, &address) {
                Ok(()) | Err(Errno::INPROGRESS) => queued.push(fd),
                Err(Errno::AGAIN) => break,                          // Linux
                Err(Errno::CONNREFUSED) if FULL_IS_REFUSED => break, // macOS, the BSDs
                Err(e) => panic!("{e}"),
            }
            assert!(queued.len() < 1000, "the backlog never filled");
        }
        (listener, queued, client)
    }

    /// The sockets a crashed server leaves: both still there, nothing listening.
    fn crashed_session(socket: &Path) {
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        for path in [socket.to_path_buf(), socket.with_file_name("herdr-client.sock")] {
            drop(std::os::unix::net::UnixListener::bind(path).unwrap());
        }
    }

    /// A session whose server crashed is not running, and says so at once.
    #[test]
    fn a_crashed_session_is_not_running() {
        let tmp = tempfile::tempdir().unwrap();
        let herdr = impatient(tmp.path());
        let socket = herdr.socket("task-crashed");
        crashed_session(&socket);
        let started = Instant::now();
        assert!(!herdr.sessions().unwrap()["task-crashed"].running);
        let err = herdr.snapshot(&socket).unwrap_err();
        assert_eq!(err, Error::Herdr { method: "session.snapshot", failure: HerdrFailure::NotRunning });
        assert!(started.elapsed() < Duration::from_millis(100), "took {:?}", started.elapsed());
    }

    /// On macOS and the BSDs (`refusers`) a refusal from a server whose
    /// client socket listens is a full backlog, waited for until the
    /// timeout, and the check is remembered for a minute: a stuck server
    /// takes none of the connections it queues, so checking on every connect
    /// would fill its client socket and make it look stopped after all.
    /// Without (Linux) a refusal is final.
    #[test]
    fn a_stuck_server_is_checked_once_a_minute_not_on_every_connect() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("task-x").join("herdr.sock");
        crashed_session(&socket);
        let refusers = LiveRefusers::default();
        let short = Duration::from_millis(20);
        let refused = |refusers| connect(&socket, short, refusers).unwrap_err().kind();
        assert_eq!(refused(Some(&refusers)), ErrorKind::ConnectionRefused, "crashed");
        fs::remove_file(socket.with_file_name("herdr-client.sock")).unwrap();
        let client = std::os::unix::net::UnixListener::bind(socket.with_file_name("herdr-client.sock")).unwrap();
        client.set_nonblocking(true).unwrap();
        for _ in 0..5 {
            assert_eq!(refused(Some(&refusers)), ErrorKind::TimedOut, "stuck: waited for");
        }
        assert!(client.accept().is_ok(), "checked once");
        assert_eq!(client.accept().unwrap_err().kind(), ErrorKind::WouldBlock, "only once");
        assert_eq!(refused(None), ErrorKind::ConnectionRefused, "Linux: nobody listens, so it is final");
    }

    /// What macOS and the BSDs decide on a refused connection (they refuse a
    /// full backlog too), checked here on any system: a server that still
    /// listens on its client socket runs, stuck; a crashed one does not.
    #[test]
    fn a_refused_session_runs_while_its_client_socket_listens() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("task-x").join("herdr.sock");
        crashed_session(&socket);
        assert!(!refused_by_a_live_server(&socket), "crashed");
        fs::remove_file(socket.with_file_name("herdr-client.sock")).unwrap();
        let _client = std::os::unix::net::UnixListener::bind(socket.with_file_name("herdr-client.sock")).unwrap();
        assert!(refused_by_a_live_server(&socket), "stuck");
        fs::remove_dir_all(socket.parent().unwrap()).unwrap();
        assert!(!refused_by_a_live_server(&socket), "stopped: no sockets at all");
    }

    /// DESIGN.md section 2: every Herdr query has a timeout, the connection included.
    #[test]
    fn a_session_that_never_accepts_times_out_instead_of_hanging() {
        let tmp = tempfile::tempdir().unwrap();
        let herdr = impatient(tmp.path());
        let socket = herdr.socket("task-stuck");
        let _stuck = stuck_session(&socket);

        let started = Instant::now();
        let err = herdr.snapshot(&socket).unwrap_err();
        assert_eq!(err, Error::Herdr { method: "session.snapshot", failure: HerdrFailure::TimedOut });
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
        let err = herdr.snapshot(&socket).unwrap_err();
        assert_eq!(err, Error::Herdr { method: "session.snapshot", failure: HerdrFailure::TimedOut });
        drop(held.join());
    }
}
