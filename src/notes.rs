//! Task notes (R9): `tasks/<session>/notes.md` in itm's data folder, a
//! markdown file for whatever context the task needs. Keyed by session name,
//! which a task keeps for good, so notes survive snooze, archive and reopen;
//! deleting the task deletes them (a later task may get its session name).
//! itm starts them, appends what changed by hand and notes given with
//! `--append`; it never rewrites them, and never writes them under the
//! store lock.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use jiff::Timestamp;

use crate::error::{Error, Result, bail};
use crate::itm::Itm;
use crate::store::{Project, Task};
use crate::system::which;

/// A heading for the notes and the new value, if it has one: `Title changed`, `Note`, …
pub type Entry = (&'static str, Option<String>);

impl Itm {
    /// Where a task's notes are. Adopted `task-*` sessions are named by
    /// whoever made them, so a name must be one safe path segment.
    pub fn notes_path(&self, session: &str) -> Result<PathBuf> {
        if session.is_empty() || session.contains(['/', '\0']) || session == "." || session == ".." {
            bail!("cannot keep notes for a session named {session:?}");
        }
        Ok(self.paths.data.join("tasks").join(session).join("notes.md"))
    }

    /// Session names that still have a notes folder: a new task must not inherit them.
    pub fn notes_taken(&self) -> BTreeSet<String> {
        let Ok(entries) = fs::read_dir(self.paths.data.join("tasks")) else { return BTreeSet::new() };
        entries.flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().into_owned()).collect()
    }

    /// A task's notes, started from `task` (its title, projects and
    /// description, R9.2) when missing. Written in full, then hard-linked
    /// into place: notes that exist are never overwritten, and an append
    /// racing this never lands before the header.
    pub fn ensure_notes(&self, session: &str, task: &Task, projects: &BTreeMap<String, Project>) -> Result<PathBuf> {
        let path = self.notes_path(session)?;
        if path.exists() {
            return Ok(path);
        }
        let folder = path.parent().expect("notes are in a folder");
        let header = self.notes_header(session, task, projects);
        let start = || -> io::Result<()> {
            fs::create_dir_all(folder)?;
            let mut tmp = tempfile::Builder::new().prefix(".notes.").tempfile_in(folder)?;
            tmp.write_all(header.as_bytes())?;
            match fs::hard_link(tmp.path(), &path) {
                Err(e) if e.kind() != ErrorKind::AlreadyExists => {
                    // A file system without hard links: an exclusive create still never overwrites.
                    match OpenOptions::new().write(true).create_new(true).open(&path) {
                        Ok(mut file) => file.write_all(header.as_bytes()),
                        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(()),
                        Err(e) => Err(e),
                    }
                }
                _ => Ok(()), // the temp file is removed when dropped
            }
        };
        start().map_err(|e| Error::Other(e.to_string()))?;
        Ok(path)
    }

    fn notes_header(&self, session: &str, task: &Task, projects: &BTreeMap<String, Project>) -> String {
        let names: Vec<&str> =
            task.projects.iter().filter_map(|id| projects.get(id)).map(|p| p.name.as_str()).collect();
        let projects = match names.len() {
            0 => "no project".to_string(),
            1 => format!("project {}", names[0]),
            _ => format!("projects {}", names.join(", ")),
        };
        let title = task.title.split_whitespace().collect::<Vec<_>>().join(" ");
        let created = self.stamp(task.created_at);
        let mut text = format!("# {title}\n\n*Created {created} · {projects} · {session}*\n");
        if let Some(description) = &task.description {
            text.push_str(&format!("\n{description}\n"));
        }
        text
    }

    /// "2026-11-05 14:30" in local time.
    fn stamp(&self, ts: i64) -> String {
        Timestamp::from_second(ts)
            .map_or("?".into(), |t| t.to_zoned(self.tz.clone()).strftime("%Y-%m-%d %H:%M").to_string())
    }

    /// Append dated entries (`## <date time> · <heading>`, then the value)
    /// after one blank line from what is there, in one write (R9.3).
    pub fn append_notes(&self, session: &str, entries: &[Entry], now: i64) -> Result<()> {
        let stamp = self.stamp(now);
        let text: String = entries
            .iter()
            .map(|(heading, body)| match body {
                Some(body) => format!("## {stamp} · {heading}\n\n{body}\n\n"),
                None => format!("## {stamp} · {heading}\n\n"),
            })
            .collect();
        let path = self.notes_path(session)?;
        append_entry(&path, &text).map_err(|e| Error::Other(e.to_string()))
    }

    /// Append a dated `Note` (`itm notes --append`, e.g. from an agent working in the task).
    pub fn add_note(&self, session: &str, text: &str, now: i64) -> Result<()> {
        let text = text.trim();
        if text.is_empty() {
            bail!("the note is empty");
        }
        self.append_notes(session, &[("Note", Some(text.into()))], now)
    }

    /// Delete a task's notes, and its folder when nothing else is in it.
    pub fn remove_notes(&self, session: &str) -> Result<()> {
        let path = self.notes_path(session)?;
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(path.parent().expect("notes are in a folder"));
        Ok(())
    }

    /// When a task's notes were last changed and their last non-empty lines
    /// (R9.5, the switcher's preview); None without notes.
    pub fn notes_tail(&self, session: &str, lines: usize) -> Option<(SystemTime, Vec<String>)> {
        let mut file = fs::File::open(self.notes_path(session).ok()?).ok()?;
        let size = file.seek(SeekFrom::End(0)).ok()?;
        file.seek(SeekFrom::Start(size.saturating_sub(4096))).ok()?;
        let mut end = vec![];
        file.read_to_end(&mut end).ok()?;
        let modified = file.metadata().and_then(|m| m.modified()).ok()?;
        let text = String::from_utf8_lossy(&end);
        let mut last: Vec<String> =
            text.lines().rev().filter(|l| !l.trim().is_empty()).take(lines).map(Into::into).collect();
        last.reverse();
        Some((modified, last))
    }

    /// Open `path` in the user's editor, which gets the terminal with no timeout (R9.4).
    pub fn edit_notes(&self, path: &Path) -> Result<()> {
        let have = |program: &str| which(program).is_some();
        let mut command = editor_command(&|var| std::env::var(var).ok(), &have, path)?;
        let program = command.remove(0);
        std::process::Command::new(&program)
            .args(command)
            .env_remove("ITM_POPUP") // itm run from the editor would delay every error
            .status()
            .map_err(|e| Error::Other(format!("could not run {}: {e}", program.to_string_lossy())))?;
        Ok(())
    }
}

/// The editor for `path`: `$VISUAL`, else `$EDITOR` (split like a shell
/// would), else the first of nvim, vim, vi and nano there is.
fn editor_command(
    var: &dyn Fn(&str) -> Option<String>,
    have: &dyn Fn(&str) -> bool,
    path: &Path,
) -> Result<Vec<OsString>> {
    for name in ["VISUAL", "EDITOR"] {
        let value = var(name).unwrap_or_default();
        let Some(words) = shlex::split(&value) else {
            bail!("cannot read ${name}: no closing quotation, or nothing after an escape");
        };
        if !words.is_empty() {
            return Ok(words.into_iter().map(OsString::from).chain([path.into()]).collect());
        }
    }
    match ["nvim", "vim", "vi", "nano"].into_iter().find(|e| have(e)) {
        Some(editor) => Ok(vec![editor.into(), path.into()]),
        None => bail!("no editor found; set $EDITOR"),
    }
}

/// One write in append mode, after one blank line from what is there.
fn append_entry(path: &Path, entry: &str) -> io::Result<()> {
    let mut file = OpenOptions::new().read(true).append(true).open(path)?;
    let size = file.seek(SeekFrom::End(0))?;
    let mut tail = Vec::new();
    file.seek(SeekFrom::Start(size.saturating_sub(2)))?;
    file.read_to_end(&mut tail)?;
    let gap = match () {
        _ if size == 0 || tail.ends_with(b"\n\n") => "",
        _ if tail.ends_with(b"\n") => "\n",
        _ => "\n\n",
    };
    file.write_all(format!("{gap}{entry}").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::itm::tests::itm;

    fn notes(itm: &Itm, session: &str) -> String {
        fs::read_to_string(itm.notes_path(session).unwrap()).unwrap()
    }

    /// R9.2: notes start with the task; existing notes are never overwritten.
    #[test]
    fn notes_start_with_the_task_and_are_never_overwritten() {
        let (_tmp, itm) = itm();
        let session = itm.create_task("Fix  login", Some("Users get\nlogged out"), &[], None).unwrap();
        let data = itm.store().load().unwrap();
        let path = itm.ensure_notes(&session, &data.tasks[&session], &data.projects).unwrap();
        assert_eq!(path, itm.paths.data.join("tasks/task-fix-login/notes.md"));
        let created = itm.stamp(data.tasks[&session].created_at);
        let header =
            format!("# Fix login\n\n*Created {created} · no project · task-fix-login*\n\nUsers get\nlogged out\n");
        assert_eq!(fs::read_to_string(&path).unwrap(), header, "created with the task");
        fs::write(&path, "mine\n").unwrap();
        itm.ensure_notes(&session, &data.tasks[&session], &data.projects).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "mine\n");
        let leftovers = fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "no temp file is left");
    }

    #[test]
    fn notes_are_appended_after_one_blank_line() {
        let (_tmp, itm) = itm();
        let session = itm.create_task("A", None, &[], None).unwrap();
        let path = itm.notes_path(&session).unwrap();
        for (before, gap) in [("x", "\n\n"), ("x\n", "\n"), ("x\n\n", ""), ("", "")] {
            fs::write(&path, before).unwrap();
            itm.add_note(&session, "  done  ", 0).unwrap();
            let stamp = itm.stamp(0);
            assert_eq!(fs::read_to_string(&path).unwrap(), format!("{before}{gap}## {stamp} · Note\n\ndone\n\n"));
        }
        assert_eq!(itm.add_note(&session, " ", 0).unwrap_err().to_string(), "the note is empty");
        let entries = [("Description cleared", None), ("Project removed", Some("api".into()))];
        fs::write(&path, "x\n").unwrap();
        itm.append_notes(&session, &entries, 0).unwrap();
        let stamp = itm.stamp(0);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("x\n\n## {stamp} · Description cleared\n\n## {stamp} · Project removed\n\napi\n\n")
        );
    }

    #[test]
    fn names_that_are_not_one_path_segment_have_no_notes() {
        let (_tmp, itm) = itm();
        for name in ["", ".", "..", "task-a/b", "task-a/../../x", "task-\0"] {
            assert!(itm.notes_path(name).is_err(), "{name:?}");
        }
    }

    /// R9.1: deleting the notes keeps what else the user put in their folder.
    #[test]
    fn removing_notes_keeps_other_files() {
        let (_tmp, itm) = itm();
        let gone = itm.create_task("Gone", None, &[], None).unwrap();
        let kept = itm.create_task("Kept", None, &[], None).unwrap();
        let screenshot = itm.notes_path(&kept).unwrap().with_file_name("screenshot.png");
        fs::write(&screenshot, "png").unwrap();
        itm.remove_notes(&gone).unwrap();
        itm.remove_notes(&kept).unwrap();
        assert!(!itm.notes_path(&gone).unwrap().parent().unwrap().exists());
        assert!(!itm.notes_path(&kept).unwrap().exists() && screenshot.exists());
        itm.remove_notes("task-never-had-any").unwrap();
    }

    /// R9.5: the preview's last lines of the notes, and when they changed.
    #[test]
    fn the_tail_is_the_last_lines_that_say_something() {
        let (_tmp, itm) = itm();
        assert_eq!(itm.notes_tail("task-none", 3), None);
        let session = itm.create_task("Fix", None, &[], None).unwrap();
        fs::write(itm.notes_path(&session).unwrap(), "# Fix\n\none\n\ntwo\nthree\n\n").unwrap();
        let (modified, lines) = itm.notes_tail(&session, 3).unwrap();
        assert_eq!(lines, ["one", "two", "three"]);
        assert!(modified.elapsed().unwrap().as_secs() < 5);
        assert_eq!(notes(&itm, &session), "# Fix\n\none\n\ntwo\nthree\n\n");
    }

    /// R9.4: `$VISUAL`, else `$EDITOR`, split like a shell would, else a common editor.
    #[test]
    fn the_editor_is_chosen_like_a_shell_would() {
        let path = Path::new("/n.md");
        let vars = |visual: &'static str, editor: &'static str| {
            move |name: &str| Some(if name == "VISUAL" { visual } else { editor }.to_string())
        };
        let only = |which: &'static str| move |name: &str| name == which;
        let words = |command: Vec<OsString>| command.into_iter().map(|w| w.into_string().unwrap()).collect::<Vec<_>>();
        let chosen = editor_command(&vars("nvim -u NONE", "nano"), &only("vi"), path).unwrap();
        assert_eq!(words(chosen), ["nvim", "-u", "NONE", "/n.md"]);
        let chosen = editor_command(&vars("  ", "code -w"), &only("vi"), path).unwrap();
        assert_eq!(words(chosen), ["code", "-w", "/n.md"]);
        assert_eq!(words(editor_command(&vars("", ""), &only("vi"), path).unwrap()), ["vi", "/n.md"]);
        let err = editor_command(&vars("", ""), &only("ed"), path).unwrap_err();
        assert_eq!(err.to_string(), "no editor found; set $EDITOR");
        let err = editor_command(&vars("'unclosed", "nano"), &only("vi"), path).unwrap_err();
        assert!(err.to_string().starts_with("cannot read $VISUAL"), "{err}");
    }
}
