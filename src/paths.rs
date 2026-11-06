//! Where itm keeps its files (DESIGN.md section 2, "Ownership"). The same
//! paths as the Python itm, so either can run on the other's state.

use std::env;
use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

const APP: &str = "inbox-task-manager";

#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
    /// `$XDG_STATE_HOME/inbox-task-manager`: tasks.json, notified.json, locks.
    pub state: PathBuf,
    /// `$XDG_DATA_HOME/inbox-task-manager`: task notes.
    pub data: PathBuf,
    /// `$XDG_CONFIG_HOME/herdr`: where Herdr keeps each session's sockets.
    pub herdr_config: PathBuf,
    /// Where a task's session starts (`ITM_TASK_DIR`, else the home folder).
    pub task_dir: PathBuf,
    /// Where git projects are cloned (`ITM_PROJECTS_DIR`, else `<data>/projects`).
    pub projects: PathBuf,
    /// `<runtime>`: status cache and attach-loop markers. Computed once; a
    /// failure only matters to the commands that need it.
    runtime: Result<PathBuf>,
}

impl Paths {
    pub fn from_env() -> Paths {
        Paths::from_vars(env::home_dir().unwrap_or_else(|| PathBuf::from("/")), |name| env::var_os(name))
    }

    /// The folders, given the home folder and how to read a variable (set
    /// and empty counts as unset, as in the Python itm).
    fn from_vars(home: PathBuf, var: impl Fn(&str) -> Option<OsString>) -> Paths {
        let var = |name: &str| var(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        let xdg = |name: &str, fallback: &str| var(name).unwrap_or_else(|| home.join(fallback));
        let data = xdg("XDG_DATA_HOME", ".local/share").join(APP);
        let projects = var("ITM_PROJECTS_DIR").map_or_else(|| data.join("projects"), |dir| expand_home(&dir, &home));
        Paths {
            state: xdg("XDG_STATE_HOME", ".local/state").join(APP),
            projects: std::path::absolute(&projects).unwrap_or(projects),
            task_dir: var("ITM_TASK_DIR").unwrap_or_else(|| home.clone()),
            data,
            herdr_config: xdg("XDG_CONFIG_HOME", ".config").join("herdr"),
            runtime: runtime_dir(var("XDG_RUNTIME_DIR")),
            home,
        }
    }

    /// Everything under one folder, as the sandbox lays it out.
    #[cfg(test)]
    pub fn under(root: &Path) -> Paths {
        Paths {
            home: root.join("home"),
            state: root.join("state").join(APP),
            data: root.join("data").join(APP),
            herdr_config: root.join("config/herdr"),
            task_dir: root.join("home"),
            projects: root.join("data").join(APP).join("projects"),
            runtime: Ok(root.join("runtime").join(APP)),
        }
    }

    pub fn tasks_file(&self) -> PathBuf {
        self.state.join("tasks.json")
    }

    pub fn notified_file(&self) -> PathBuf {
        self.state.join("notified.json")
    }

    pub fn runtime(&self) -> Result<&Path> {
        self.runtime.as_deref().map_err(Error::clone)
    }

    /// `<runtime>/clients`: one marker per `itm attach` loop.
    pub fn clients(&self) -> Result<PathBuf> {
        Ok(self.runtime()?.join("clients"))
    }

    /// `<runtime>/status.json`: the daemon's agent status cache.
    pub fn status_file(&self) -> Result<PathBuf> {
        Ok(self.runtime()?.join("status.json"))
    }

    /// Where a project is: a repository's URL, else its folder, else "no folder".
    pub fn place(&self, project: &crate::store::Project) -> String {
        match (&project.url, &project.path) {
            (Some(url), _) => url.clone(),
            (None, Some(path)) => self.display(Path::new(path)),
            (None, None) => "no folder".into(),
        }
    }

    /// A path with the home folder shown as `~`.
    pub fn display(&self, path: &Path) -> String {
        match path.strip_prefix(&self.home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        }
    }
}

/// `~` and `~/…` as the shell reads them (`~user` is not read).
pub fn expand_home(path: &Path, home: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// `$XDG_RUNTIME_DIR/inbox-task-manager`, else `/run/user/<uid>/…` when that
/// folder is ours, else a private `/tmp/inbox-task-manager-<uid>`. The daemon
/// (started by a service manager) and every client (started from a shell)
/// must find the same folder, so nothing here depends on `$TMPDIR`.
fn runtime_dir(xdg_runtime: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = xdg_runtime {
        return Ok(dir.join(APP));
    }
    let uid = rustix::process::getuid().as_raw();
    let run_user = PathBuf::from(format!("/run/user/{uid}"));
    if let Ok(meta) = fs::metadata(&run_user)
        && meta.is_dir()
        && meta.uid() == uid
        && meta.mode() & 0o200 != 0
    {
        return Ok(run_user.join(APP));
    }
    private_dir(&PathBuf::from(format!("/tmp/{APP}-{uid}")))
        .map_err(|e| Error::Other(format!("no private runtime folder: {e}; set $XDG_RUNTIME_DIR")))
}

/// `path` created `0700`; refused when someone else owns it or it is a link
/// (anyone can create a folder in `/tmp` first).
fn private_dir(path: &Path) -> std::io::Result<PathBuf> {
    match DirBuilder::new().mode(0o700).create(path) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.uid() != rustix::process::getuid().as_raw() {
        return Err(std::io::Error::other(format!("{} is not a folder owned by you", path.display())));
    }
    if meta.mode() & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_runtime_dir_wins() {
        assert_eq!(runtime_dir(Some("/x/run".into())).unwrap(), PathBuf::from("/x/run/inbox-task-manager"));
    }

    #[test]
    fn private_folder_is_made_private_and_links_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let open = tmp.path().join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
        private_dir(&open).unwrap();
        assert_eq!(fs::metadata(&open).unwrap().mode() & 0o777, 0o700);

        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&open, &link).unwrap();
        assert!(private_dir(&link).unwrap_err().to_string().contains("not a folder owned by you"));
    }

    #[test]
    fn the_projects_folder_reads_a_tilde_like_python() {
        let home = PathBuf::from("/h");
        let with = |value: &'static str| {
            let var = move |name: &str| match name {
                "ITM_PROJECTS_DIR" => Some(value.into()),
                "XDG_RUNTIME_DIR" => Some("/run/test".into()), // no fallback folder made
                _ => None,
            };
            Paths::from_vars(home.clone(), var).projects
        };
        assert_eq!(with("~/clones"), PathBuf::from("/h/clones"));
        assert_eq!(with("~"), PathBuf::from("/h"));
        assert_eq!(with("/srv/clones"), PathBuf::from("/srv/clones"));
        assert_eq!(with("~other/x"), std::path::absolute("~other/x").unwrap(), "only the shell's own ~ is read");
        assert_eq!(with(""), PathBuf::from("/h/.local/share/inbox-task-manager/projects"), "empty is unset");
    }

    #[test]
    fn home_is_shown_as_a_tilde() {
        let paths = Paths::under(Path::new("/r"));
        assert_eq!(paths.display(Path::new("/r/home")), "~");
        assert_eq!(paths.display(Path::new("/r/home/src/api")), "~/src/api");
        assert_eq!(paths.display(Path::new("/r/homework")), "/r/homework");
    }
}
