//! Saved projects (R8): named folders tasks work in. A task's session starts
//! with one Herdr workspace per project, named after it (R1.2). This version
//! saves folders only; git projects the Python itm saved are listed and used
//! as they are.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Context, Result, bail};
use crate::itm::Itm;
use crate::store::{Data, Project, flocked};
use crate::times;

impl Itm {
    /// Save a folder as a project; returns its name. The name defaults to the folder's.
    pub fn add_folder_project(&self, source: &str, name: Option<&str>) -> Result<String> {
        let source = source.trim();
        if source.is_empty() {
            bail!("empty folder path");
        }
        let folder = expand_home(source, &self.paths.home);
        if !folder.is_dir() {
            bail!("not a folder: {source}");
        }
        let folder = fs::canonicalize(&folder).context(|| format!("cannot resolve {source}"))?;
        let clones = fs::canonicalize(&self.paths.projects).unwrap_or_else(|_| self.paths.projects.clone());
        if folder.starts_with(&clones) {
            bail!("{} is inside itm's projects folder; add its repository instead", self.paths.display(&folder));
        }
        let default = folder.file_name().map_or("project".into(), |n| n.to_string_lossy().into_owned());
        let name = check_project_name(name.map(str::trim).filter(|n| !n.is_empty()).unwrap_or(&default))?;
        // Every writer of projects holds this lock (the Python itm clones under it).
        let lock = self.paths.state.join(".projects.lock");
        let _lock = flocked(&lock).context(|| format!("cannot lock {}", lock.display()))?;
        let now = times::now();
        self.store().edit(|data| {
            let same_folder =
                |p: &&Project| p.path.as_ref().is_some_and(|path| fs::canonicalize(path).is_ok_and(|p| p == folder));
            if let Some(same) = data.projects.values().find(same_folder) {
                bail!("project already exists: {}", same.name);
            }
            if let Some(taken) = data.projects.values().find(|p| p.name.to_lowercase() == name.to_lowercase()) {
                bail!(
                    "project already exists: {} ({}); choose another name with --name",
                    taken.name,
                    self.paths.place(taken)
                );
            }
            let project = Project {
                name: name.clone(),
                path: Some(folder.display().to_string()),
                url: None,
                added_at: now,
                last_used_at: now,
                extra: Default::default(),
            };
            data.projects.insert(new_project_id(data)?, project);
            Ok(name.clone())
        })
    }

    /// (name, folder) of a task's projects whose folders exist, the first
    /// project first. `warn` says which are missing (on a first start, where they matter).
    pub fn task_folders(&self, data: &Data, session: &str, warn: bool) -> Vec<(String, PathBuf)> {
        let Some(task) = data.tasks.get(session) else { return vec![] };
        let mut folders = vec![];
        // A project without a folder gets no workspace.
        for project in task.projects.iter().filter_map(|id| data.projects.get(id)) {
            let Some(folder) = project.path.as_ref().map(PathBuf::from) else { continue };
            if folder.is_dir() {
                folders.push((project.name.clone(), folder));
            } else if warn {
                eprintln!("itm: {} ({}) is missing; it gets no workspace", self.paths.display(&folder), project.name);
            }
        }
        folders
    }

    /// Where a task's session starts: its first project's folder, else the task folder.
    pub fn task_cwd(&self, data: &Data, session: &str) -> PathBuf {
        self.task_folders(data, session, false)
            .into_iter()
            .next()
            .map_or_else(|| self.paths.task_dir.clone(), |(_, f)| f)
    }

    /// Before a task's first attach, start its session headless with one
    /// named workspace per project (`herdr session attach` takes one folder
    /// and no name). Under a lock per session: two clients handed to a new
    /// task must not both build it. A failure is said and the attach goes
    /// on; Herdr then starts the session as usual.
    pub fn prepare_first_start(&self, data: &Data, session: &str) -> Result<()> {
        if data.tasks.get(session).is_none_or(|t| t.projects.is_empty()) || !self.first_start(session) {
            return Ok(());
        }
        let lock = self.paths.runtime()?.join("starting").join(format!("{session}.lock"));
        let _lock = flocked(&lock).context(|| format!("cannot lock {}", lock.display()))?;
        let folders = self.task_folders(data, session, true);
        if !folders.is_empty() && self.first_start(session) {
            let built =
                self.herdr.start_server(session, &folders[0].1).and_then(|s| self.open_workspaces(&s, &folders));
            if let Err(e) = built {
                eprintln!("itm: {e}; the project workspaces may be missing");
            }
        }
        Ok(())
    }

    /// Whether a session never ran (Herdr keeps nothing for it). Unknown counts as no.
    fn first_start(&self, session: &str) -> bool {
        self.herdr.sessions().is_ok_and(|sessions| !sessions.contains_key(session))
    }

    /// One workspace per project, the first focused; none when the session has some already.
    fn open_workspaces(&self, socket: &Path, folders: &[(String, PathBuf)]) -> Result<()> {
        if self.herdr.workspace_count(socket)? > 0 {
            return Ok(());
        }
        for (i, (label, folder)) in folders.iter().enumerate() {
            self.herdr.create_workspace(socket, folder, label, i == 0)?;
        }
        Ok(())
    }
}

/// The id of the project named `query` (ignoring case), or with that id.
pub fn resolve_project(data: &Data, query: &str) -> Result<String> {
    let named: Vec<&String> =
        data.projects.iter().filter(|(_, p)| p.name.to_lowercase() == query.to_lowercase()).map(|(id, _)| id).collect();
    match named.as_slice() {
        [id] => Ok((*id).clone()),
        _ if data.projects.contains_key(query) => Ok(query.into()),
        _ => bail!("no project named '{query}'; add it with `itm project add`"),
    }
}

/// 1 to 60 characters, without tabs or other control characters.
fn check_project_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 60 || name.chars().any(char::is_control) {
        bail!("a project name has 1 to 60 characters, without tabs or other control characters");
    }
    Ok(name.into())
}

/// Eight random hex digits, like the Python itm's `secrets.token_hex(4)`.
fn new_project_id(data: &Data) -> Result<String> {
    let mut urandom = File::open("/dev/urandom").context(|| "cannot read /dev/urandom".into())?;
    loop {
        let mut bytes = [0u8; 4];
        urandom.read_exact(&mut bytes).context(|| "cannot read /dev/urandom".into())?;
        let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        if !data.projects.contains_key(&id) {
            return Ok(id);
        }
    }
}

/// `~` and `~/…` as the shell reads them.
fn expand_home(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix('~') {
        Some("") => home.to_path_buf(),
        Some(rest) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(path),
    }
}

/// One project as `itm project list --json` prints it, in the Python itm's key order.
#[derive(Serialize)]
pub struct ProjectRow<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub kind: &'a str,
    /// None for a name only.
    pub path: Option<&'a str>,
    pub url: Option<&'a str>,
    pub tasks: usize,
}

pub fn project_rows(data: &Data) -> Vec<ProjectRow<'_>> {
    let mut rows: Vec<ProjectRow> = (data.projects.iter())
        .map(|(id, p)| ProjectRow {
            id,
            name: &p.name,
            kind: p.kind(),
            path: p.path.as_deref(),
            url: p.url.as_deref(),
            tasks: data.tasks.values().filter(|t| t.projects.contains(id)).count(),
        })
        .collect();
    rows.sort_by_key(|row| row.name.to_lowercase());
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake;
    use crate::itm::tests::itm;
    use crate::store::Task;
    use serde_json::json;

    fn folder(itm: &Itm, rel: &str) -> PathBuf {
        let path = itm.paths.home.join(rel);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn a_folder_is_saved_once_under_a_free_name() {
        let (_tmp, itm) = itm();
        let api = folder(&itm, "code/api");
        assert_eq!(itm.add_folder_project("~/code/api", None).unwrap(), "api");
        let err = itm.add_folder_project(api.to_str().unwrap(), Some("other")).unwrap_err();
        assert_eq!(err.0, "project already exists: api");
        folder(&itm, "work/api");
        let err = itm.add_folder_project("~/work/api", Some("API")).unwrap_err();
        assert_eq!(err.0, "project already exists: api (~/code/api); choose another name with --name");
        assert_eq!(itm.add_folder_project("~/work/api", Some(" work api ")).unwrap(), "work api");

        let data = itm.store().load().unwrap();
        let id = resolve_project(&data, "API").unwrap();
        assert_eq!(data.projects[&id].path, Some(api.canonicalize().unwrap().display().to_string()));
        assert_eq!(resolve_project(&data, &id).unwrap(), id);
        assert!(resolve_project(&data, "web").unwrap_err().0.starts_with("no project named 'web'"));
    }

    #[test]
    fn what_cannot_be_a_project() {
        let (_tmp, itm) = itm();
        assert_eq!(itm.add_folder_project("~/nowhere", None).unwrap_err().0, "not a folder: ~/nowhere");
        assert_eq!(itm.add_folder_project("  ", None).unwrap_err().0, "empty folder path");
        folder(&itm, "x");
        assert!(itm.add_folder_project("~/x", Some("a\tb")).is_err());
        let clone = itm.paths.projects.join("github.com/acme/api");
        fs::create_dir_all(&clone).unwrap();
        let err = itm.add_folder_project(clone.to_str().unwrap(), None).unwrap_err();
        assert!(err.0.contains("inside itm's projects folder"), "{err}");
    }

    #[test]
    fn a_new_task_takes_projects_by_name_and_marks_them_used() {
        let (_tmp, itm) = itm();
        folder(&itm, "api");
        folder(&itm, "web");
        itm.add_folder_project("~/api", None).unwrap();
        itm.add_folder_project("~/web", None).unwrap();
        let projects = ["WEB".to_string(), "api".to_string(), "web".to_string()];
        let name = itm.create_task("Fix", None, &projects, None).unwrap();
        let data = itm.store().load().unwrap();
        let ids = [resolve_project(&data, "web").unwrap(), resolve_project(&data, "api").unwrap()];
        assert_eq!(data.tasks[&name].projects, ids);
        assert!(ids.iter().all(|id| data.projects[id].last_used_at > 0));
        let rows = project_rows(&data);
        assert_eq!(rows.iter().map(|r| (r.name, r.tasks)).collect::<Vec<_>>(), [("api", 1), ("web", 1)]);

        let unknown = itm.create_task("Nope", None, &["missing".to_string()], None).unwrap_err();
        assert!(unknown.0.starts_with("no project named 'missing'"));
        assert!(!itm.store().load().unwrap().tasks.contains_key("task-nope"));
    }

    #[test]
    fn a_session_starts_in_its_first_project_with_a_folder() {
        let (_tmp, itm) = itm();
        let web = folder(&itm, "web");
        folder(&itm, "gone");
        itm.add_folder_project("~/gone", None).unwrap();
        itm.add_folder_project("~/web", None).unwrap();
        let name = itm.create_task("Fix", None, &["gone".into(), "web".into()], None).unwrap();
        fs::remove_dir(itm.paths.home.join("gone")).unwrap();
        let data = itm.store().load().unwrap();
        assert_eq!(itm.task_cwd(&data, &name), web.canonicalize().unwrap());
        assert_eq!(itm.task_cwd(&data, "task-other"), itm.paths.task_dir);
    }

    #[test]
    fn a_first_start_opens_a_named_workspace_per_project() {
        let (_tmp, itm) = itm();
        let (api, web) = (folder(&itm, "api"), folder(&itm, "web"));
        let folders = vec![("api".to_string(), api.clone()), ("web".to_string(), web.clone())];
        let calls = fake::serve(&itm.paths.herdr_config, "task-a", |method, _| match method {
            "workspace.list" => json!({"workspaces": []}),
            _ => json!({}),
        });
        itm.open_workspaces(&itm.herdr.socket("task-a"), &folders).unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls[0].0, "workspace.list");
        assert_eq!(calls[1].1, json!({"cwd": api, "label": "api", "focus": true}));
        assert_eq!(calls[2].1, json!({"cwd": web, "label": "web", "focus": false}));

        // A session another client built already gets none.
        let calls = fake::serve(&itm.paths.herdr_config, "task-b", |_, _| json!({"workspaces": [{"id": "w1"}]}));
        itm.open_workspaces(&itm.herdr.socket("task-b"), &folders).unwrap();
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_session_that_ran_before_is_not_built_again() {
        let (_tmp, itm) = itm();
        folder(&itm, "api");
        itm.add_folder_project("~/api", None).unwrap();
        let name = itm.create_task("Fix", None, &["api".into()], None).unwrap();
        fs::create_dir_all(itm.paths.herdr_config.join("sessions").join(&name)).unwrap(); // Herdr knows it
        let mut data = itm.store().load().unwrap();
        itm.prepare_first_start(&data, &name).unwrap(); // would fail to start a server: none is started
        data.tasks.insert("task-plain".into(), Task::new("Plain", 1));
        itm.prepare_first_start(&data, "task-plain").unwrap();
    }
}
