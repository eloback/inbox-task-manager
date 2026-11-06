//! Task notes (R9): `tasks/<session>/notes.md` in itm's data folder, a
//! markdown file for whatever context the task needs. Keyed by session name,
//! which a task keeps for good, so notes survive snooze, archive and reopen.
//! itm starts them and appends notes; it never rewrites them, and never
//! writes them under the store lock.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use jiff::Timestamp;

use crate::error::{Context, Result, bail};
use crate::itm::Itm;
use crate::store::{Data, Task};
use crate::system::which;

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

    /// A task's notes, started from its title, projects and description
    /// when missing (R9.2). Written in full, then hard-linked into place:
    /// notes that exist are never overwritten, and an append racing this
    /// never lands before the header.
    pub fn ensure_notes(&self, session: &str, data: &Data) -> Result<PathBuf> {
        let path = self.notes_path(session)?;
        if path.exists() {
            return Ok(path);
        }
        let Some(task) = data.tasks.get(session) else { bail!("task {session} no longer exists") };
        let folder = path.parent().expect("notes are in a folder");
        fs::create_dir_all(folder).context(|| format!("cannot create {}", folder.display()))?;
        let header = self.notes_header(session, task, data);
        let start = || -> io::Result<()> {
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
        start().context(|| format!("cannot start {}", path.display()))?;
        Ok(path)
    }

    fn notes_header(&self, session: &str, task: &Task, data: &Data) -> String {
        let names: Vec<&str> =
            task.projects.iter().filter_map(|id| data.projects.get(id)).map(|p| p.name.as_str()).collect();
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

    /// Append a dated `Note` (`itm notes --append`, e.g. from an agent working in the task).
    pub fn add_note(&self, session: &str, text: &str, now: i64) -> Result<()> {
        let text = text.trim();
        if text.is_empty() {
            bail!("the note is empty");
        }
        let path = self.notes_path(session)?;
        let entry = format!("## {} · Note\n\n{text}\n\n", self.stamp(now));
        append_entry(&path, &entry).context(|| format!("cannot write {}", path.display()))
    }

    /// The editor for notes: `$VISUAL`, else `$EDITOR` (split like a shell
    /// would), else a common one. It gets the terminal, with no timeout.
    pub fn edit_notes(&self, path: &std::path::Path) -> Result<()> {
        let configured =
            ["VISUAL", "EDITOR"].iter().find_map(|var| std::env::var(var).ok().filter(|v| !v.trim().is_empty()));
        let mut command = match configured {
            Some(editor) => shlex::split(&editor).filter(|words| !words.is_empty()),
            None => ["nvim", "vim", "vi", "nano"].into_iter().find(|e| which(e).is_some()).map(|e| vec![e.to_string()]),
        }
        .ok_or_else(|| crate::error::Error::Other("no editor found; set $EDITOR".into()))?;
        let program = command.remove(0);
        std::process::Command::new(&program)
            .args(command)
            .arg(path)
            .env_remove("ITM_POPUP") // itm run from the editor would delay every error
            .status()
            .context(|| format!("could not run {program}"))?;
        Ok(())
    }
}

/// One write in append mode, after one blank line from what is there.
fn append_entry(path: &std::path::Path, entry: &str) -> io::Result<()> {
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

    #[test]
    fn notes_start_with_the_task_and_are_never_overwritten() {
        let (_tmp, itm) = itm();
        let session = itm.create_task("Fix  login", Some("Users get\nlogged out"), &[], None).unwrap();
        let data = itm.store().load().unwrap();
        let path = itm.ensure_notes(&session, &data).unwrap();
        let created = itm.stamp(data.tasks[&session].created_at);
        let header =
            format!("# Fix login\n\n*Created {created} · no project · task-fix-login*\n\nUsers get\nlogged out\n");
        assert_eq!(fs::read_to_string(&path).unwrap(), header, "created with the task");
        fs::write(&path, "mine\n").unwrap();
        itm.ensure_notes(&session, &data).unwrap();
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
    }

    #[test]
    fn names_that_are_not_one_path_segment_have_no_notes() {
        let (_tmp, itm) = itm();
        for name in ["", ".", "..", "task-a/b", "task-\0"] {
            assert!(itm.notes_path(name).is_err(), "{name:?}");
        }
    }
}
