//! `tasks.json`: task and project metadata, guarded by an exclusive `flock`
//! and written atomically (DESIGN.md section 2, "Data safety"). Every field
//! itm does not know (a newer itm's, or one added by hand) is kept as it
//! is, and keys are written sorted with a two-space indent.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::error::{Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Active,
    Snoozed,
    Archived,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Active => "active",
            State::Snoozed => "snoozed",
            State::Archived => "archived",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub title: String,
    pub state: State,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub last_used_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Project ids; the first is where the session starts.
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "ids_or_none")]
    pub projects: Vec<String>,
    /// The projects workspace sync added (R10.1); it removes only these. A
    /// hand edit naming one makes it the user's.
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "ids_or_none")]
    pub found_projects: Vec<String>,
    /// The projects whose workspace was open on the daemon's last pass over
    /// the running session (R10.3); only true while the daemon reads it.
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "ids_or_none")]
    pub workspace_projects: Vec<String>,
    /// When a snoozed task wakes; a snoozed task without it waits for an agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snoozed_until: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<i64>,
    /// Fields itm does not use, kept as they are.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// What a project is, read off its fields (never stored, R8.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ProjectKind {
    /// A git repository; its folder is itm's clone.
    Git,
    /// A local folder.
    Dir,
    /// A name only, without a folder: no workspace opens for it.
    Name,
}

impl ProjectKind {
    pub fn name(self) -> &'static str {
        match self {
            ProjectKind::Git => "git",
            ProjectKind::Dir => "dir",
            ProjectKind::Name => "name",
        }
    }
}

/// A saved project: a git repository (its URL, and itm's clone as its
/// folder), a folder, or a name only. Its kind is read off these fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub added_at: i64,
    #[serde(default)]
    pub last_used_at: i64,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Project {
    pub fn kind(&self) -> ProjectKind {
        match (&self.url, &self.path) {
            (Some(_), _) => ProjectKind::Git,
            (None, Some(_)) => ProjectKind::Dir,
            (None, None) => ProjectKind::Name,
        }
    }

    /// Its folder; None for a name only.
    pub fn folder(&self) -> Option<&Path> {
        self.path.as_deref().map(Path::new)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Data {
    /// Session name → task.
    #[serde(default)]
    pub tasks: BTreeMap<String, Task>,
    /// Tombstones: a deleted task's session → when, until the session is gone.
    #[serde(default)]
    pub deleted: BTreeMap<String, i64>,
    /// Project id → project.
    #[serde(default)]
    pub projects: BTreeMap<String, Project>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Anything but a list reads as no projects, and a list keeps only its
/// strings: a file edited by hand stays readable.
fn ids_or_none<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Array(items) => items.into_iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
        _ => vec![],
    })
}

pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(path: PathBuf) -> Store {
        Store { path }
    }

    /// A short read under the lock (the daemon holds it for milliseconds).
    pub fn load(&self) -> Result<Data> {
        let _lock = self.lock()?;
        self.read()
    }

    /// Read, change and write under the lock. An error from `change` cancels
    /// the write; the daemon relies on this. Callers re-resolve their task
    /// inside `change`: it may have been deleted meanwhile.
    pub fn edit<T>(&self, change: impl FnOnce(&mut Data) -> Result<T>) -> Result<T> {
        let _lock = self.lock()?;
        let mut data = self.read()?;
        let result = change(&mut data)?;
        // Through a Value, whose maps sort every key, unknown ones included.
        let value = serde_json::to_value(&data).context(|| "cannot save tasks".into())?;
        write_atomic(&self.path, &to_json(&value, true)).context(|| format!("cannot write {}", self.path.display()))?;
        Ok(result)
    }

    fn lock(&self) -> Result<File> {
        let path = self.path.with_file_name(".tasks.lock");
        flocked(&path).context(|| format!("cannot lock {}", path.display()))
    }

    fn read(&self) -> Result<Data> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(Data { extra: [("version".into(), 1.into())].into(), ..Data::default() });
            }
            Err(e) => return Err(e).context(|| format!("cannot read {}", self.path.display())),
        };
        let raw: Value =
            serde_json::from_str(&text).context(|| format!("{} is not valid JSON", self.path.display()))?;
        serde_json::from_value(raw).context(|| format!("{} is not a task list itm can read", self.path.display()))
    }
}

/// An exclusive `flock` on `path`, held until the file is dropped.
pub fn flocked(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let file = File::options().write(true).create(true).truncate(false).open(path)?;
    file.lock()?;
    Ok(file)
}

/// Write a temp file next to `path`, fsync it and rename it over `path`, so
/// readers see the old file or the new one, never half of one.
pub fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    let mut tmp = tempfile::Builder::new().prefix(&format!(".{name}.")).tempfile_in(dir)?;
    tmp.write_all(text.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?; // the temp file is removed on failure
    Ok(())
}

/// JSON with a final newline: pretty with a two-space indent, or on one line.
pub fn to_json(value: &(impl Serialize + ?Sized), pretty: bool) -> String {
    let text = if pretty { serde_json::to_string_pretty(value) } else { serde_json::to_string(value) };
    text.expect("itm's values always serialize") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn store() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path().join("tasks.json"));
        (tmp, store)
    }

    fn task(title: &str) -> Task {
        Task {
            title: title.into(),
            state: State::Active,
            created_at: 1,
            last_used_at: 1,
            description: None,
            projects: vec![],
            found_projects: vec![],
            workspace_projects: vec![],
            snoozed_until: None,
            archived_at: None,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn a_missing_file_is_an_empty_store_and_edits_round_trip() {
        let (_tmp, store) = store();
        assert!(store.load().unwrap().tasks.is_empty());
        store.edit(|data| Ok(data.tasks.insert("task-a".into(), task("A")))).unwrap();
        assert_eq!(store.load().unwrap().tasks["task-a"].title, "A");
        assert!(fs::read_to_string(&store.path).unwrap().contains("\"version\": 1"));
    }

    #[test]
    fn an_error_inside_an_edit_cancels_the_write() {
        let (_tmp, store) = store();
        store.edit(|data| Ok(data.tasks.insert("task-a".into(), task("A")))).unwrap();
        let before = fs::read(&store.path).unwrap();
        let failed: Result<()> = store.edit(|data| {
            data.tasks.clear();
            crate::error::bail!("no")
        });
        assert!(failed.is_err());
        assert_eq!(fs::read(&store.path).unwrap(), before);
    }

    #[test]
    fn a_corrupt_file_is_an_error() {
        let (_tmp, store) = store();
        fs::write(&store.path, "{nope").unwrap();
        assert!(store.load().unwrap_err().to_string().contains("is not valid JSON"));
    }

    #[test]
    fn concurrent_edits_lose_no_update() {
        let (_tmp, store) = store();
        let store = Arc::new(store);
        let workers: Vec<_> = (0..8)
            .map(|w| {
                let store = Arc::clone(&store);
                thread::spawn(move || {
                    for i in 0..10 {
                        store.edit(|data| Ok(data.tasks.insert(format!("task-{w}-{i}"), task("x")))).unwrap();
                    }
                })
            })
            .collect();
        workers.into_iter().for_each(|w| w.join().unwrap());
        assert_eq!(store.load().unwrap().tasks.len(), 80);
        let leftovers: Vec<_> = fs::read_dir(store.path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with(".tasks.json."))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    /// Non-ASCII text, an emoji, escapes and unknown fields at every level.
    const WRITTEN: &str = r#"{
  "deleted": {
    "task-gone": 1700000000
  },
  "future": {
    "nested": [
      1.5,
      null,
      true
    ]
  },
  "projects": {
    "a1b2c3d4": {
      "added_at": 1700000000,
      "color": "blue",
      "last_used_at": 1700000100,
      "name": "api",
      "path": "/home/me/src/api"
    }
  },
  "tasks": {
    "task-caf": {
      "created_at": 1700000000,
      "description": "line \"one\"\n\ttab \\ slash",
      "found_projects": [
        "a1b2c3d4"
      ],
      "last_used_at": 1700000200,
      "pinned": true,
      "projects": [
        "a1b2c3d4"
      ],
      "state": "active",
      "title": "Café crème 🚀"
    }
  },
  "version": 1
}
"#;

    #[test]
    fn a_rewrite_keeps_every_field_and_byte() {
        let (_tmp, store) = store();
        fs::write(&store.path, WRITTEN).unwrap();
        let data = store.load().unwrap();
        let task = &data.tasks["task-caf"];
        assert_eq!(task.title, "Café crème 🚀");
        assert_eq!(task.projects, ["a1b2c3d4"]);
        assert_eq!(task.found_projects, ["a1b2c3d4"]);
        assert_eq!(
            (data.projects["a1b2c3d4"].name.as_str(), data.projects["a1b2c3d4"].kind()),
            ("api", ProjectKind::Dir)
        );

        store.edit(|_| Ok(())).unwrap();
        assert_eq!(fs::read_to_string(&store.path).unwrap(), WRITTEN);
    }

    #[test]
    fn odd_project_lists_read_as_none() {
        let (_tmp, store) = store();
        fs::write(&store.path, r#"{"tasks": {"task-a": {"title": "A", "state": "active", "projects": "oops"}}}"#)
            .unwrap();
        assert!(store.load().unwrap().tasks["task-a"].projects.is_empty());
    }

    #[test]
    fn a_second_open_of_the_lock_waits_for_the_first() {
        // flock is per open file: a second open of the lock file waits for the first.
        let (tmp, _store) = store();
        let lock = tmp.path().join(".tasks.lock");
        let held = flocked(&lock).unwrap();
        let other = File::options().write(true).open(&lock).unwrap();
        assert!(matches!(other.try_lock(), Err(fs::TryLockError::WouldBlock)));
        drop(held);
        assert!(other.try_lock().is_ok());
    }
}
