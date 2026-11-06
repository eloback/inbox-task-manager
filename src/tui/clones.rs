//! Clones started from a terminal UI (R8.8, DESIGN.md section 2,
//! Projects): a child `itm project add git` in a session of its own that
//! never prompts. Its output (git's progress) is read as it comes. It is
//! stopped with SIGTERM on Esc and whenever the UI ends: in its own
//! session it gets no SIGHUP from a closing popup, and would finish on its
//! own.

use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use super::app::Reply;
use crate::error::{Context, Error, Result};
use crate::itm::Itm;
use crate::projects::{clone_failure, resolve_project};
use crate::store::ProjectKind;
use crate::system::terminate;

/// The children this UI started. They are reaped only under the lock, so a
/// pid it holds is never another process's when it is signalled.
#[derive(Debug, Clone, Default)]
pub struct Clones(Arc<Mutex<Children>>);

#[derive(Debug, Default)]
struct Children {
    running: Vec<Child>,
    /// The UI ended: children started from now on are stopped at once.
    closed: bool,
}

impl Clones {
    fn lock(&self) -> MutexGuard<'_, Children> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Start `command`, as a child that is stopped with the others; its pid.
    pub fn spawn(&self, command: &mut Command) -> io::Result<u32> {
        let child = command.spawn()?;
        let pid = child.id();
        let mut children = self.lock();
        if children.closed {
            terminate(pid);
        }
        children.running.push(child);
        Ok(pid)
    }

    /// Wait for the child `pid` to end; until then it can be stopped.
    pub fn wait(&self, pid: u32) -> io::Result<ExitStatus> {
        loop {
            {
                let mut children = self.lock();
                let Some(at) = children.running.iter().position(|c| c.id() == pid) else {
                    return Err(io::Error::other("not a child of this UI"));
                };
                if children.running[at].try_wait()?.is_some() {
                    return children.running.remove(at).wait(); // reaped already: its status at once
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// SIGTERM to every child that runs (Esc during a clone).
    pub fn stop(&self) {
        for child in &mut self.lock().running {
            if child.try_wait().is_ok_and(|ended| ended.is_none()) {
                terminate(child.id());
            }
        }
    }

    /// The UI ends: stop every child, and any started from now on.
    pub fn close(&self) {
        self.lock().closed = true;
        self.stop();
    }
}

/// Closes [`Clones`] when dropped: on every way out of the event loop.
pub struct CloseOnDrop(pub Clones);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// Clone `url` as a child `itm project add git` (R8.8), telling `said` the
/// last thing it printed as it goes; the project saved. It is checked
/// first as adding checks it, so a name taken is said as such (R8.5).
pub fn clone_project(
    itm: &Itm,
    url: &str,
    name: Option<&str>,
    clones: &Clones,
    said: &dyn Fn(String),
) -> Result<Reply> {
    if let Some(problem) = itm.check_project(ProjectKind::Git, url, name)?.problem {
        return Err(problem);
    }
    let cannot = |e: io::Error| Error::Other(format!("Could not start the clone: {e}"));
    let (mut output, writer) = io::pipe().map_err(cannot)?;
    let mut command = itm.clone_child(url, name).map_err(cannot)?;
    command.stdout(writer.try_clone().map_err(cannot)?).stderr(writer);
    let pid = clones.spawn(&mut command).map_err(cannot)?;
    drop(command); // its copies of the pipe: the output ends when the child's does
    let text = read_progress(&mut output, said);
    let status = clones.wait(pid).context(|| "the clone".into())?;
    if !status.success() {
        return Err(Error::Other(clone_failure(&text)));
    }
    // Its last line is the project's name.
    let named = text.lines().map(str::trim).rfind(|l| !l.is_empty()).or(name).unwrap_or_default();
    let finished = |e: Error| Error::Other(format!("The clone finished, but {e}"));
    let data = itm.store().load().map_err(finished)?;
    let id = resolve_project(&data, named).map_err(finished)?;
    Ok(Reply::Added(id.clone(), data.projects[&id].clone()))
}

/// Everything `output` says until it ends; `said` gets the last line of each
/// chunk (git rewrites its progress line with "\r").
fn read_progress(output: &mut impl Read, said: &dyn Fn(String)) -> String {
    // Read off whole at the end: a chunk can end inside a character (of the name on the last line).
    let mut bytes = vec![];
    let mut chunk = [0u8; 512];
    while let Ok(n @ 1..) = output.read(&mut chunk) {
        bytes.extend_from_slice(&chunk[..n]);
        let part = String::from_utf8_lossy(&chunk[..n]);
        if let Some(last) = part.split(['\r', '\n']).map(str::trim).rfind(|p| !p.is_empty()) {
            let shown: String = last.chars().rev().take(70).collect::<Vec<_>>().into_iter().rev().collect();
            said(shown);
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// R8.8: a child can be stopped until it is reaped, and once the UI
    /// ended, one started afterwards is stopped at once.
    #[test]
    fn r8_8_children_are_stopped_on_cancel_and_when_the_ui_ends() {
        let clones = Clones::default();
        let pid = clones.spawn(Command::new("sleep").arg("30")).unwrap();
        clones.stop();
        let status = clones.wait(pid).unwrap();
        assert_eq!(std::os::unix::process::ExitStatusExt::signal(&status), Some(15));
        assert!(clones.wait(pid).is_err(), "reaped: no longer this UI's");
        let quick = clones.spawn(&mut Command::new("true")).unwrap();
        assert!(clones.wait(quick).unwrap().success());
        clones.close();
        let late = clones.spawn(Command::new("sleep").arg("30")).unwrap();
        let status = clones.wait(late).unwrap();
        assert_eq!(std::os::unix::process::ExitStatusExt::signal(&status), Some(15), "started after the end");
    }

    /// R8.8: git's progress, rewritten with "\r", is said a line at a time, its end kept.
    #[test]
    fn r8_8_progress_is_the_last_line_of_each_chunk() {
        let (tx, rx) = mpsc::channel();
        let say = move |line: String| tx.send(line).unwrap();
        let output = "Cloning into 'x'...\nReceiving objects:  50% (1/2)\rReceiving objects: 100% (2/2), done.\n";
        let text = read_progress(&mut output.as_bytes(), &say);
        assert_eq!(text, output);
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), ["Receiving objects: 100% (2/2), done."]);
        let long = "x".repeat(100);
        read_progress(&mut long.as_bytes(), &|line: String| assert_eq!(line.len(), 70));
        let name = format!("{}Café crème\n", "x".repeat(509)); // "é" across the first chunk's end
        assert!(read_progress(&mut name.as_bytes(), &|_| ()).ends_with("Café crème\n"), "the name read whole");
    }
}
