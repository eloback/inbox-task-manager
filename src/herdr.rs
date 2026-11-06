//! Herdr, the terminal multiplexer whose sessions are itm's tasks.
//!
//! Queries and notifications go over each session's socket API: one JSON
//! line each way per connection (DESIGN.md, section 1). That is 10–20
//! times faster than the CLI, starts no process, and is not refused after a
//! Herdr upgrade the way the CLI's protocol check is. Sessions are found the
//! way `herdr session list` finds them, by reading Herdr's sessions folder.
//! Only `herdr session attach` (an interactive client) and `herdr server`
//! (a server start) run the `herdr` program.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use std::{fs, io, thread};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{Context, Error, Result, bail};
use crate::status::AgentState;

/// Every query has a deadline (DESIGN.md section 2).
const TIMEOUT: Duration = Duration::from_secs(10);
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
}

impl Herdr {
    pub fn new(config: PathBuf) -> Herdr {
        Herdr { config }
    }

    /// Every session Herdr knows (running or stopped), like `herdr session
    /// list`: the default session, plus each folder under `sessions/` with a
    /// valid name. A session runs when its socket accepts a connection.
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
                let running = UnixStream::connect(&socket).is_ok();
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
        let list: AgentList = decode("agent.list", call(socket, "agent.list", json!({}))?)?;
        Ok(list.agents)
    }

    /// A toast in the session's attached client; false when Herdr did not
    /// show it (toasts off, no client, rate limited).
    pub fn toast(&self, socket: &Path, title: &str, body: &str) -> Result<bool> {
        let result = call(socket, "notification.show", json!({"title": title, "body": body, "sound": "request"}))?;
        Ok(result.get("shown").and_then(Value::as_bool).unwrap_or(false))
    }

    /// Ask a running session to stop. Herdr answers before it shuts down,
    /// so the caller may be a process inside that very session.
    pub fn stop(&self, socket: &Path) -> Result<()> {
        call(socket, "server.stop", json!({})).map(drop)
    }

    /// How many workspaces a running session has.
    pub fn workspace_count(&self, socket: &Path) -> Result<usize> {
        let result = call(socket, "workspace.list", json!({}))?;
        Ok(result.get("workspaces").and_then(Value::as_array).map_or(0, Vec::len))
    }

    /// Open a workspace in `folder`, named `label`. Herdr renames an unnamed
    /// workspace whenever its first pane changes folder; a named one keeps
    /// its name. A folder that does not exist would silently become `$HOME`,
    /// so callers check it first.
    pub fn create_workspace(&self, socket: &Path, folder: &Path, label: &str, focus: bool) -> Result<()> {
        call(socket, "workspace.create", json!({"cwd": folder, "label": label, "focus": focus})).map(drop)
    }

    /// Start a session headless (`herdr server`) in `cwd`, and wait until it
    /// answers. The server gets no `HERDR_*` variables from wherever itm runs
    /// but `HERDR_SESSION`, and its own process group, so it outlives this
    /// terminal. One that exits with an error is not waited for.
    pub fn start_server(&self, name: &str, cwd: &Path) -> Result<PathBuf> {
        let mut command = Command::new("herdr");
        for (key, _) in std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("HERDR_")) {
            command.env_remove(key);
        }
        command.arg("server").env("HERDR_SESSION", name).current_dir(cwd).process_group(0);
        let mut server =
            command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map_err(missing_herdr)?;
        let socket = self.socket(name);
        let deadline = Instant::now() + SERVER_START;
        while UnixStream::connect(&socket).is_err() {
            if let Ok(Some(status)) = server.try_wait()
                && !status.success()
            {
                bail!("herdr server for {name} exited with {status}");
            }
            if Instant::now() > deadline {
                bail!("session {name} did not start");
            }
            thread::sleep(Duration::from_millis(25));
        }
        Ok(socket)
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
        Error("herdr not found; install it or add it to PATH".into())
    } else {
        Error(format!("cannot run herdr: {e}"))
    }
}

/// One request on its own connection: Herdr answers one line and closes.
fn call(socket: &Path, method: &str, params: Value) -> Result<Value> {
    let failed = |e: io::Error| match e.kind() {
        ErrorKind::NotFound | ErrorKind::ConnectionRefused => Error(format!("herdr {method}: session not running")),
        ErrorKind::WouldBlock | ErrorKind::TimedOut => Error(format!("herdr {method} timed out")),
        _ => Error(format!("herdr {method}: {e}")),
    };
    let mut stream = UnixStream::connect(socket).map_err(failed)?;
    stream.set_read_timeout(Some(TIMEOUT)).map_err(failed)?;
    stream.set_write_timeout(Some(TIMEOUT)).map_err(failed)?;
    let request = json!({"id": "itm", "method": method, "params": params});
    writeln!(stream, "{request}").map_err(failed)?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).map_err(failed)?;

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
    match serde_json::from_str::<Response>(&line).context(|| format!("herdr {method} returned invalid JSON"))? {
        Response { result: Some(result), .. } => Ok(result),
        Response { error: Some(e), .. } => bail!("herdr {method} failed: {} ({})", e.message, e.code),
        _ => bail!("herdr {method} returned no result"),
    }
}

fn decode<T: for<'de> Deserialize<'de>>(method: &str, result: Value) -> Result<T> {
    serde_json::from_value(result).context(|| format!("herdr {method} returned an unexpected answer"))
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
        assert_eq!(err.0, "herdr agent.list: session not running");

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
        assert_eq!(err.0, "herdr notification.show failed: notification title is empty (invalid_params)");
    }
}
