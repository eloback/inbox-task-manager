//! Saved projects (R8): what a task is about, saved once and shared by any
//! number of tasks. A project is a git repository (itm's clone is its
//! folder), a local folder, or a name only; a task's session starts with one
//! Herdr workspace per project with a folder, named after it (R1.2).
//!
//! Adding and removing hold `.projects.lock`; a clone runs under it but
//! outside the store lock, because it can take minutes and the daemon
//! writes the store every 2 s (DESIGN.md section 2, Projects).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;

use regex_lite::Regex;
use serde::Serialize;

use crate::error::{Context, Error, Result, bail, shell_quote};
use crate::git::normalize_git_url;
use crate::itm::Itm;
use crate::paths::{Paths, expand_home, resolved};
use crate::store::{Data, Project, ProjectKind, State, flocked};
use crate::system::{ExitSignals, in_new_session};
use crate::times;

/// What to do with a removed git project's clone (R8.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneChoice {
    Keep,
    /// Even with local work (`--delete-clone`).
    Delete,
    /// Only without local work, checked again under the lock (the dialog's choice).
    IfClean,
}

/// What became of a git project's clone, as a dialog asks about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloneState {
    /// Gone already.
    Missing,
    /// Not a clone itm may delete (not a checkout of the repository, outside
    /// the projects folder, or holding another project).
    Foreign,
    /// itm's own clone; `local_work` says what only it has.
    Deletable { local_work: Option<String> },
}

/// What removing a project would do, for a question before it (nothing is changed).
#[derive(Debug, Clone, PartialEq)]
pub struct RemovalPlan {
    pub id: String,
    pub project: Project,
    /// Titles of the tasks that have it, archived ones included: removing is refused then.
    pub users: Vec<String>,
    /// For a git project only.
    pub clone: Option<CloneState>,
}

/// Where a new project's folder would be, and the name it defaults to.
struct Spec {
    folder: Option<PathBuf>,
    default_name: String,
}

/// What adding a project would do ([`Itm::check_project`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    /// The folder it gets: a repository's clone, a folder resolved; None for a name.
    pub folder: Option<PathBuf>,
    /// The name it gets when none is given (empty for a name only).
    pub default_name: String,
    /// Why it cannot be added; None when it can.
    pub problem: Option<Error>,
}

/// A line of what removing a project did. A clone kept is a warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemovalNote {
    Done(String),
    Kept(String),
}

impl RemovalNote {
    pub fn line(self) -> String {
        match self {
            RemovalNote::Done(line) | RemovalNote::Kept(line) => line,
        }
    }
}

impl Itm {
    /// Save a project; returns its id (R8.2). A git repository is cloned
    /// first (R8.6); a folder is saved resolved; a name only is named by its
    /// source. The name defaults to the repository's or folder's.
    pub fn add_project(&self, kind: ProjectKind, source: &str, name: Option<&str>) -> Result<String> {
        let source = source.trim();
        if kind == ProjectKind::Name && name.is_some_and(|n| !n.is_empty()) {
            bail!("a name-only project is named by its source: `itm project add name <name>`")
        }
        let spec = self.project_spec(kind, source)?;
        let name = check_project_name(match kind {
            ProjectKind::Name => source, // nothing else to name it after
            _ => name.map(str::trim).filter(|n| !n.is_empty()).unwrap_or(&spec.default_name),
        })?;
        // Every writer of projects holds this lock, so checking once before the clone is enough.
        let _lock = self.projects_lock()?;
        self.check_project_free(&self.store().load()?, spec.folder.as_deref(), Some(&name), kind, None)?;
        let url = (kind == ProjectKind::Git).then_some(source);
        if let (Some(url), Some(clone)) = (url, &spec.folder) {
            self.clone_repo(url, clone)?;
        }
        let now = times::now();
        self.store().edit(|data| {
            let id = new_project_id(data)?;
            let path = spec.folder.as_ref().map(|p| p.display().to_string());
            let project = Project {
                name,
                path,
                url: url.map(Into::into),
                added_at: now,
                last_used_at: now,
                extra: Default::default(),
            };
            data.projects.insert(id.clone(), project);
            Ok(id)
        })
    }

    /// Where a project of `kind` from `source` goes: a repository's clone
    /// under the projects folder (its normalized URL, `insteadOf` followed,
    /// R8.4), a folder resolved, nothing for a name; and the name it
    /// defaults to.
    fn project_spec(&self, kind: ProjectKind, source: &str) -> Result<Spec> {
        let source = source.trim();
        let folder = match kind {
            ProjectKind::Git => Some(self.paths.projects.join(self.git.repo_key(source)?)),
            ProjectKind::Dir => Some(self.folder_spec(source)?),
            ProjectKind::Name => None,
        };
        let default = folder.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
        Ok(Spec { default_name: default.unwrap_or_else(|| "project".into()), folder })
    }

    /// What adding a project would do, as the add dialog shows it while
    /// typing (R8.5); nothing is saved. Err: the source is no folder or
    /// git URL. Otherwise the folder it would get, the name it defaults
    /// to, and why it could not be added (saved already, a name taken),
    /// as adding it would say.
    pub fn check_project(&self, kind: ProjectKind, source: &str, name: Option<&str>) -> Result<Check> {
        let spec = match kind {
            ProjectKind::Name => Spec { folder: None, default_name: String::new() },
            _ => self.project_spec(kind, source)?,
        };
        // As adding it checks: the name first, then whether it is free.
        let name = match kind {
            ProjectKind::Name => source.trim(),
            _ => name.map(str::trim).filter(|n| !n.is_empty()).unwrap_or(&spec.default_name),
        };
        let free = check_project_name(name).and_then(|name| {
            self.check_project_free(&self.store().load()?, spec.folder.as_deref(), Some(&name), kind, None)
        });
        Ok(Check { folder: spec.folder, default_name: spec.default_name, problem: free.err() })
    }

    pub(crate) fn projects_lock(&self) -> Result<File> {
        let lock = self.paths.state.join(".projects.lock");
        flocked(&lock).context(|| format!("cannot lock {}", lock.display()))
    }

    /// A folder as a project's folder is saved: resolved, and never inside
    /// itm's projects folder (R8.5: add its repository instead).
    pub(crate) fn folder_spec(&self, source: &str) -> Result<PathBuf> {
        if source.is_empty() {
            bail!("empty folder path");
        }
        let folder = expand_home(Path::new(source), &self.paths.home);
        if !folder.is_dir() {
            bail!("not a folder: {source}");
        }
        let folder = fs::canonicalize(&folder).context(|| format!("cannot resolve {source}"))?;
        if folder.starts_with(resolved(&self.paths.projects)) {
            bail!(
                "{} is inside itm's projects folder; add its repository with `itm project add git`",
                self.paths.display(&folder)
            );
        }
        Ok(folder)
    }

    /// Refuse a repository or folder saved already (R8.4: by resolved path;
    /// a repository's clone path comes from its normalized URL, so this also
    /// finds it given another way), or a name another project has (R8.5).
    /// `exclude` is a project being changed.
    fn check_project_free(
        &self,
        data: &Data,
        folder: Option<&Path>,
        name: Option<&str>,
        kind: ProjectKind,
        exclude: Option<&str>,
    ) -> Result<()> {
        if let Some(folder) = folder {
            let target = resolved(folder);
            let same = |p: &Project| p.folder().is_some_and(|path| resolved(path) == target);
            if let Some((_, project)) = data.projects.iter().find(|(id, p)| Some(id.as_str()) != exclude && same(p)) {
                return Err(Error::ProjectExists { name: project.name.clone() });
            }
        }
        let Some(taken) = name.and_then(|name| named(data, name, None)) else { return Ok(()) };
        if taken.kind() == ProjectKind::Name && kind == ProjectKind::Dir {
            // A name waiting for its folder: give it this one rather than a second project.
            let folder = folder.expect("a folder project has one").display().to_string();
            return Err(Error::ProjectWithoutFolder { name: taken.name.clone(), folder });
        }
        Err(Error::ProjectNameTaken { name: taken.name.clone(), place: self.paths.place(taken) })
    }

    /// Clone `url` into `target`, or keep a checkout of it already there
    /// (R8.6). The clone goes to a hidden folder next to the target and is
    /// renamed into place; a failed or interrupted one leaves nothing, not
    /// even the host and owner folders it made. SIGHUP (a popup closed),
    /// SIGTERM (a cancel) and SIGINT stop git and end itm after the cleanup,
    /// with 128 + the signal as its exit status.
    pub fn clone_repo(&self, url: &str, target: &Path) -> Result<()> {
        if fs::symlink_metadata(target).is_ok() {
            if !self.git.is_checkout_of(target, url)? {
                bail!("{} already exists and is not a checkout of {url}", self.paths.display(target));
            }
            return Ok(());
        }
        // Held until the cleanup is done: further signals change nothing meanwhile.
        let mut stop = ExitSignals::catch().context(|| "cannot watch for signals".into())?;
        let parent = target.parent().expect("a clone is inside the projects folder");
        let cloned = (|| {
            fs::create_dir_all(parent).context(|| format!("cannot create {}", parent.display()))?;
            let name = target.file_name().map_or("repo".into(), |n| n.to_string_lossy().into_owned());
            let hidden = tempfile::Builder::new()
                .prefix(&format!(".{name}."))
                .tempdir_in(parent)
                .context(|| format!("cannot create a folder in {}", parent.display()))?;
            let repo = hidden.path().join("repo");
            self.git.clone(url, &repo, &mut stop)?;
            fs::rename(&repo, target).context(|| format!("cannot move the clone to {}", target.display()))
            // `hidden` goes here, with whatever a failed clone left in it.
        })();
        if cloned.is_err() {
            self.prune_empty_parents(target);
        }
        drop(stop);
        cloned
    }

    /// Remove the empty folders above `path`, up to the projects folder.
    fn prune_empty_parents(&self, path: &Path) {
        let root = resolved(&self.paths.projects);
        for parent in resolved(path).ancestors().skip(1) {
            if parent == root || !parent.starts_with(&root) || fs::remove_dir(parent).is_err() {
                break;
            }
        }
    }

    /// A removed git project's clone, when itm may delete it: a real checkout
    /// of its repository inside the projects folder, holding no other project
    /// of `data`.
    fn deletable_clone(&self, data: &Data, project: &Project) -> Result<Option<PathBuf>> {
        let (Some(url), Some(path)) = (&project.url, project.folder()) else { return Ok(None) };
        if !self.git.is_checkout_of(path, url)? {
            return Ok(None);
        }
        let clone = resolved(path);
        let root = resolved(&self.paths.projects);
        if clone == root || !clone.starts_with(&root) {
            return Ok(None);
        }
        let holds = |other: &Project| other.folder().is_some_and(|f| resolved(f).starts_with(&clone));
        Ok((!data.projects.values().any(holds)).then(|| path.to_path_buf()))
    }

    /// What removing the project `query` names would do (R8.7), for a question first.
    pub fn removal_plan(&self, query: &str) -> Result<RemovalPlan> {
        let data = self.store().load()?;
        let id = resolve_project(&data, query)?;
        let project = data.projects[&id].clone();
        let users = project_users(&data, &id);
        let clone = match (&project.url, project.folder()) {
            (Some(_), Some(path)) => Some(if fs::symlink_metadata(path).is_err() {
                CloneState::Missing
            } else {
                // As removing sees it: once this project is gone, others may still hold the clone.
                let mut others = data.clone();
                others.projects.remove(&id);
                match self.deletable_clone(&others, &project)? {
                    None => CloneState::Foreign,
                    Some(clone) => CloneState::Deletable { local_work: self.git.local_work(&clone)? },
                }
            }),
            _ => None,
        };
        Ok(RemovalPlan { id, project, users, clone })
    }

    /// Forget a project no task has, archived ones included (R8.7); returns
    /// what happened. Only a git project's clone is ever deleted, as `clone`
    /// says, and only when it is one itm may delete.
    pub fn remove_project(&self, query: &str, clone: CloneChoice) -> Result<Vec<RemovalNote>> {
        let _lock = self.projects_lock()?;
        let (project, data) = self.store().edit(|data| {
            let id = resolve_project(data, query)?;
            let name = data.projects[&id].name.clone();
            let users = project_users(data, &id);
            if !users.is_empty() {
                bail!(
                    "project '{name}' is used by {} task(s); remove it from them (`itm edit <task> --remove-project {}`) or delete them first: {}",
                    users.len(),
                    shell_quote(&name),
                    users.join(", ")
                );
            }
            let project = data.projects.remove(&id).expect("resolved");
            Ok((project, data.clone()))
        })?;
        let mut notes = vec![RemovalNote::Done(format!("project {} removed", project.name))];
        let Some(folder) = project.folder().filter(|f| project.url.is_some() && fs::symlink_metadata(f).is_ok()) else {
            return Ok(notes);
        };
        if clone == CloneChoice::Keep {
            return Ok(notes);
        }
        let Some(path) = self.deletable_clone(&data, &project)? else {
            notes.push(RemovalNote::Kept(format!("kept {}: not a clone itm can delete", self.paths.display(folder))));
            return Ok(notes);
        };
        let shown = self.paths.display(&path);
        if clone == CloneChoice::IfClean
            && let Some(work) = self.git.local_work(&path)?
        {
            notes.push(RemovalNote::Kept(format!("kept {shown}: it has {work}; --delete-clone deletes it anyway")));
            return Ok(notes);
        }
        fs::remove_dir_all(&path).context(|| format!("cannot delete {shown}"))?;
        self.prune_empty_parents(&path);
        notes.push(RemovalNote::Done(format!("deleted {shown}")));
        Ok(notes)
    }

    /// Rename a project; its tasks keep it, since they store its id. Returns
    /// (old name, new name, a note). A project without a folder is found in
    /// workspaces only by name (R10.1), so the note names the tasks it may
    /// leave while their workspaces are still named the old way.
    pub fn rename_project(&self, query: &str, new_name: &str) -> Result<(String, String, Option<String>)> {
        let _lock = self.projects_lock()?;
        let (old, name, kind, found_in) = self.store().edit(|data| {
            let id = resolve_project(data, query)?;
            let name = check_project_name(new_name)?;
            if let Some(taken) = named(data, &name, Some(&id)) {
                return Err(Error::ProjectNameTaken { name: taken.name.clone(), place: self.paths.place(taken) });
            }
            let project = data.projects.get_mut(&id).expect("resolved");
            let old = std::mem::replace(&mut project.name, name.clone());
            let kind = project.kind();
            // Only where workspace sync can drop it: found there, with its workspace open in a session the daemon reads.
            let found_in: Vec<String> = (data.tasks.values())
                .filter(|t| t.state != State::Archived)
                .filter(|t| t.found_projects.contains(&id) && t.workspace_projects.contains(&id))
                .map(|t| t.title.clone())
                .collect();
            Ok((old, name, kind, found_in))
        })?;
        // Only while "remove a project when its workspace closes" is on (a broken settings file says nothing).
        let removes = self.settings().map_or(true, |s| s.remove_projects_without_workspace);
        let renamed = old.to_lowercase() != name.to_lowercase();
        let note = (kind == ProjectKind::Name && !found_in.is_empty() && renamed && removes).then(|| {
                format!(
                    "workspaces named {old} no longer count as it (it has no folder), so it may leave {} unless they are renamed {name}",
                    found_in.join(", ")
                )
            });
        Ok((old, name, note))
    }

    /// Give a project without a repository a folder, move its folder, or
    /// (None) make it a name only; returns what that did (R8.2). Nothing on
    /// disk changes. A repository's folder is its clone and stays.
    pub fn set_project_folder(&self, query: &str, folder: Option<&str>) -> Result<Vec<String>> {
        let path = folder.map(|f| self.folder_spec(f.trim())).transpose()?;
        let _lock = self.projects_lock()?;
        let (name, old, users) = self.store().edit(|data| {
            let id = resolve_project(data, query)?;
            let project = &data.projects[&id];
            if let (Some(url), ProjectKind::Git) = (&project.url, project.kind()) {
                bail!("{}'s folder is itm's clone of {url}; it cannot be changed", project.name);
            }
            self.check_project_free(data, path.as_deref(), None, ProjectKind::Dir, Some(&id))?;
            let project = data.projects.get_mut(&id).expect("resolved");
            let old = std::mem::replace(&mut project.path, path.as_ref().map(|p| p.display().to_string()));
            let name = project.name.clone();
            let users: Vec<String> = (data.tasks.values())
                .filter(|t| t.projects.contains(&id) && t.state != State::Archived)
                .map(|t| t.title.clone())
                .collect();
            Ok((name, old, users))
        })?;
        let new = path.as_ref().map(|p| p.display().to_string());
        if new == old {
            return Ok(vec![match new {
                Some(_) => format!("{name} already has that folder"),
                None => format!("{name} has no folder"),
            }]);
        }
        let mut said = vec![match &path {
            Some(path) => format!("{name} is now in {}", self.paths.display(path)),
            None => format!("{name} has no folder now"),
        }];
        if !users.is_empty() {
            // Which of their workspaces count as it follows the new folder (R10.3).
            let how = if path.is_some() { "by the new folder" } else { "only when named after it" };
            said.push(format!("workspaces of {} count as it {how} from now on", users.join(", ")));
        }
        Ok(said)
    }

    /// `itm project add git <url>` as a terminal UI starts it (R8.8): a child
    /// in its own session (closing a popup hangs up nothing; the UI stops it
    /// with SIGTERM, which `itm setsid` makes sure is not ignored), stdin
    /// closed, and git and ssh told never to prompt, so a password or
    /// host-key question can neither draw over the UI nor hang. Its output,
    /// git's progress included, is for the UI to read; its last line is the
    /// project's name.
    pub fn clone_child(&self, url: &str, name: Option<&str>) -> io::Result<Command> {
        let mut command = in_new_session(&self.program)?;
        command.args(["project", "add", "git"]);
        // `--name=…` and `--`: a name or URL starting with "-" is not read as an option.
        command.args(name.map(|name| format!("--name={name}"))).args(["--", url]);
        let ssh = std::env::var_os("GIT_SSH_COMMAND");
        command
            .env_remove("ITM_POPUP") // it would delay every error by 2.5 s
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_SSH_COMMAND", batch_ssh(ssh.as_deref()))
            .stdin(Stdio::null());
        Ok(command)
    }

    /// (name, folder) of a task's projects whose folders exist, the first
    /// project first. `warn` says which are missing (on a first start, where they matter).
    pub fn task_folders(&self, data: &Data, session: &str, warn: bool) -> Vec<(String, PathBuf)> {
        let Some(task) = data.tasks.get(session) else { return vec![] };
        let mut folders = vec![];
        // A project without a folder gets no workspace.
        for project in task.projects.iter().filter_map(|id| data.projects.get(id)) {
            let Some(folder) = project.folder() else { continue };
            if folder.is_dir() {
                folders.push((project.name.clone(), folder.to_path_buf()));
            } else if warn {
                eprintln!("itm: {} ({}) is missing; it gets no workspace", self.paths.display(folder), project.name);
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

    /// What projects added to a task mean for its session (R5.4): one that
    /// never started will start with a workspace in each; a running one gets
    /// a workspace per project now, named after it (the first focused when
    /// it is the session this runs in); a stopped one keeps its workspaces.
    /// A project without a folder, or whose folder is missing, gets none.
    pub fn project_change_messages(&self, session: &str, added: &[Project]) -> Vec<String> {
        let display = |folder: &Path| self.paths.display(folder);
        let mut said = vec![];
        let mut present = vec![];
        for project in added {
            match project.folder() {
                None => said.push(format!("{} has no folder, so it gets no workspace", project.name)),
                Some(folder) if !folder.is_dir() => {
                    said.push(format!("{} is missing, so it gets no workspace", display(folder)))
                }
                Some(folder) => present.push((project, folder)),
            }
        }
        if present.is_empty() {
            return said;
        }
        let places = present.iter().map(|(_, folder)| display(folder)).collect::<Vec<_>>().join(", ");
        let sessions = match self.herdr.sessions() {
            Ok(sessions) => sessions,
            Err(e) => {
                said.push(format!(
                    "could not tell whether its session runs ({e}); open a workspace in {places} if it does"
                ));
                return said;
            }
        };
        match sessions.get(session) {
            None => said.push(format!("its session will start with a workspace in {places}")),
            Some(stopped) if !stopped.running => {
                said.push(format!("its stopped session keeps its workspaces; open one in {places} when you need it"))
            }
            Some(running) => {
                let focus = self.current_session(&sessions).as_deref() == Some(session);
                for (i, (project, folder)) in present.iter().enumerate() {
                    said.push(
                        match self.herdr.create_workspace(&running.socket, folder, &project.name, focus && i == 0) {
                            Ok(()) => format!("opened a workspace in {}", display(folder)),
                            Err(e) => format!("could not open a workspace in {}: {e}", display(folder)),
                        },
                    );
                }
            }
        }
        said
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

/// `$GIT_SSH_COMMAND` (else ssh) in batch mode: it fails instead of asking.
fn batch_ssh(current: Option<&std::ffi::OsStr>) -> OsString {
    let mut ssh = current.filter(|c| !c.is_empty()).map_or_else(|| OsString::from("ssh"), Into::into);
    ssh.push(" -o BatchMode=yes");
    ssh
}

/// What a clone that failed because git or ssh wanted to ask something
/// should say next (R8.8): add it from a terminal, where they can ask.
pub fn clone_hint(output: &str) -> Option<&'static str> {
    const PROMPTS: [&str; 6] = [
        "terminal prompts disabled",
        "could not read username",
        "could not read password",
        "host key verification failed",
        "permission denied",
        "batch mode",
    ];
    let lowered = output.to_lowercase();
    PROMPTS.iter().any(|p| lowered.contains(p)).then_some(
        "this repository needs a password or a host key confirmation; run `itm project add git <url>` in a terminal",
    )
}

/// Each saved folder with its links resolved, and its project: a folder
/// reached through a link is the project's (the finder, R1.6).
pub fn saved_folders(data: &Data) -> BTreeMap<PathBuf, String> {
    (data.projects.iter()).filter_map(|(id, p)| Some((resolved(p.folder()?), id.clone()))).collect()
}

/// Ids of the projects whose folder is set and not there; a project
/// without a folder has none to miss.
pub fn missing_folders(data: &Data) -> BTreeSet<String> {
    let missing = |p: &Project| p.folder().is_some_and(|path| !path.is_dir());
    data.projects.iter().filter(|(_, p)| missing(p)).map(|(id, _)| id.clone()).collect()
}

/// The id of the project named `query` (ignoring case), or with that id.
pub fn resolve_project(data: &Data, query: &str) -> Result<String> {
    let named: Vec<&String> =
        data.projects.iter().filter(|(_, p)| p.name.to_lowercase() == query.to_lowercase()).map(|(id, _)| id).collect();
    match named.as_slice() {
        [id] => Ok((*id).clone()),
        _ if data.projects.contains_key(query) => Ok(query.into()),
        _ => Err(Error::NoSuchProject(query.into())),
    }
}

/// Projects with their ids, the most recently used first (R1.6, R8.3).
pub fn projects_by_recent(projects: &BTreeMap<String, Project>) -> Vec<(&String, &Project)> {
    let mut projects: Vec<_> = projects.iter().collect();
    projects.sort_by_key(|(_, p)| (std::cmp::Reverse(p.last_used_at), p.name.to_lowercase()));
    projects
}

/// Whether typed text is a git URL being typed (`scheme://…` or
/// `user@host:path`) rather than a project's name.
pub fn looks_like_git_url(text: &str) -> bool {
    static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?i)[a-z][a-z0-9+.-]*://\S").unwrap());
    static SCP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[^@\s/:]+@[^@\s/:]+:\S+$").unwrap());
    URL.is_match(text) || SCP.is_match(text)
}

/// The saved git project of `url`'s repository, compared as R8.4 normalizes
/// URLs (no git call, so no `insteadOf`).
pub fn saved_repository<'a>(projects: &'a BTreeMap<String, Project>, url: &str) -> Option<&'a String> {
    let key = normalize_git_url(url).ok()?;
    let same = |p: &Project| p.url.as_deref().and_then(|u| normalize_git_url(u).ok()).is_some_and(|k| k == key);
    projects.iter().find(|(_, p)| same(p)).map(|(id, _)| id)
}

/// Why `name` cannot be the name of the project `id` (a rename, R8.5):
/// not a name a project can have, or another project's, ignoring case.
pub fn name_problem(data: &Data, paths: &Paths, name: &str, id: &str) -> Option<Error> {
    match check_project_name(name) {
        Err(e) => Some(e),
        Ok(name) => named(data, &name, Some(id))
            .map(|taken| Error::ProjectNameTaken { name: taken.name.clone(), place: paths.place(taken) }),
    }
}

/// What a TUI's clone child (R8.8) said when it failed: its last `itm:`
/// error, else its last line, and the hint when git or ssh wanted to ask.
pub fn clone_failure(output: &str) -> String {
    let error = output.lines().rev().find_map(|l| l.strip_prefix("itm: "));
    let message = error.or_else(|| output.trim().lines().next_back()).unwrap_or("the clone failed").trim().to_string();
    match clone_hint(output) {
        Some(hint) => format!("{message}\n{hint}"),
        None => message,
    }
}

/// The project other than `exclude` that has `name`, ignoring case (R8.5).
fn named<'a>(data: &'a Data, name: &str, exclude: Option<&str>) -> Option<&'a Project> {
    let name = name.to_lowercase();
    data.projects.iter().find(|(id, p)| Some(id.as_str()) != exclude && p.name.to_lowercase() == name).map(|(_, p)| p)
}

/// Titles of the tasks that have a project, archived ones included, sorted.
fn project_users(data: &Data, id: &str) -> Vec<String> {
    let mut titles: Vec<String> =
        data.tasks.values().filter(|t| t.projects.iter().any(|p| p == id)).map(|t| t.title.clone()).collect();
    titles.sort();
    titles
}

/// 1 to 60 characters, without tabs or other control characters.
fn check_project_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 60 || name.chars().any(char::is_control) {
        return Err(Error::ProjectName);
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

/// One project as `itm project list --json` prints it, in the Python itm's key order.
#[derive(Serialize)]
pub struct ProjectRow<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub kind: ProjectKind,
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
            tasks: project_users(data, id).len(),
        })
        .collect();
    rows.sort_by_key(|row| row.name.to_lowercase());
    rows
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::git::Git;
    use crate::herdr::fake;
    use crate::itm::tests::itm;
    use crate::store::Task;
    use serde_json::json;

    pub fn folder(itm: &Itm, rel: &str) -> PathBuf {
        let path = itm.paths.home.join(rel);
        fs::create_dir_all(&path).unwrap();
        path
    }

    /// A git stand-in: a clone makes a checkout whose `origin` is the URL
    /// (or fails as one that needs a password, when `<its folder>/clone-fails`
    /// exists); `ls-remote --get-url` prints the URL as given; a clone's
    /// stashes are `.git/stashes`; it has no other local work.
    pub fn fake_git(itm: &mut Itm) -> PathBuf {
        let script = itm.paths.home.parent().unwrap().join("fake-git");
        fs::write(
            &script,
            r#"here=$(dirname "$0")
case "$1" in
clone)
  echo "Cloning into '$5'..." >&2
  if [ -e "$here/clone-fails" ]; then
    echo "fatal: could not read Username for 'https://example.com': terminal prompts disabled" >&2; exit 128
  fi
  mkdir -p "$5/.git" && echo "$4" >"$5/.git/origin" ;;
ls-remote) echo "$3" ;;
-C)
  dir=$2; shift 2
  case "$1 $2" in
  "remote get-url") cat "$dir/.git/origin" ;;
  "stash list") cat "$dir/.git/stashes" 2>/dev/null || true ;;
  esac ;;
esac
"#,
        )
        .unwrap();
        itm.git = Git::with(&["sh", script.to_str().unwrap()]);
        script
    }

    fn add(itm: &Itm, kind: ProjectKind, source: &str, name: Option<&str>) -> Result<String> {
        itm.add_project(kind, source, name).map(|id| itm.store().load().unwrap().projects[&id].name.clone())
    }

    fn project(itm: &Itm, name: &str) -> (String, Project) {
        let data = itm.store().load().unwrap();
        let id = resolve_project(&data, name).unwrap();
        (id.clone(), data.projects[&id].clone())
    }

    fn api_clone(itm: &Itm) -> PathBuf {
        itm.paths.projects.join("github.com/acme/api")
    }

    /// Every entry under the projects folder, as paths relative to it.
    fn leftovers(itm: &Itm) -> Vec<String> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
            for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
                out.push(entry.path().strip_prefix(root).unwrap().display().to_string());
                if entry.file_type().unwrap().is_dir() {
                    walk(&entry.path(), root, out);
                }
            }
        }
        let mut out = vec![];
        walk(&itm.paths.projects, &itm.paths.projects, &mut out);
        out.sort();
        out
    }

    #[test]
    fn a_folder_is_saved_once_under_a_free_name() {
        let (_tmp, itm) = itm();
        let api = folder(&itm, "code/api");
        std::os::unix::fs::symlink(&api, itm.paths.home.join("link")).unwrap();
        assert_eq!(add(&itm, ProjectKind::Dir, "~/code/api", None).unwrap(), "api");
        let (_, saved) = project(&itm, "api");
        assert_eq!((saved.kind(), saved.added_at), (ProjectKind::Dir, saved.last_used_at));
        // R8.4: the same folder (by resolved path) is the same project; R8.5: a name is one project's, ignoring case.
        for source in [format!("{}/", api.display()), "~/link".into()] {
            let err = add(&itm, ProjectKind::Dir, &source, Some("other")).unwrap_err();
            assert_eq!(err, Error::ProjectExists { name: "api".into() });
            assert_eq!(err.to_string(), "project already exists: api");
        }
        folder(&itm, "work/api");
        let err = add(&itm, ProjectKind::Dir, "~/work/api", Some("API")).unwrap_err();
        assert_eq!(err, Error::ProjectNameTaken { name: "api".into(), place: "~/code/api".into() });
        assert_eq!(add(&itm, ProjectKind::Dir, "~/work/api", Some(" work api ")).unwrap(), "work api");

        let data = itm.store().load().unwrap();
        let id = resolve_project(&data, "API").unwrap();
        assert_eq!(data.projects[&id].path, Some(api.canonicalize().unwrap().display().to_string()));
        assert_eq!(resolve_project(&data, &id).unwrap(), id);
        assert_eq!(resolve_project(&data, "web").unwrap_err(), Error::NoSuchProject("web".into()));
    }

    /// R8.2: adding a folder under the name of a project without a folder says to `set` it instead.
    #[test]
    fn a_folder_named_like_a_name_only_project_is_offered_to_it() {
        let (_tmp, itm) = itm();
        add(&itm, ProjectKind::Name, "Client X", None).unwrap();
        let x = folder(&itm, "client-x").canonicalize().unwrap();
        let err = add(&itm, ProjectKind::Dir, "~/client-x", Some("client x")).unwrap_err();
        assert_eq!(err, Error::ProjectWithoutFolder { name: "Client X".into(), folder: x.display().to_string() });
        let hint = format!("give it this one with `itm project set 'Client X' --dir {}`", x.display());
        assert!(err.to_string().ends_with(&hint), "{err}");
    }

    #[test]
    fn what_cannot_be_a_project() {
        let (_tmp, mut itm) = itm();
        fake_git(&mut itm);
        assert_eq!(add(&itm, ProjectKind::Dir, "~/nowhere", None).unwrap_err().to_string(), "not a folder: ~/nowhere");
        assert_eq!(add(&itm, ProjectKind::Dir, "  ", None).unwrap_err().to_string(), "empty folder path");
        folder(&itm, "x");
        assert!(
            add(&itm, ProjectKind::Dir, "~/x", Some("a\tb")).unwrap_err().to_string().contains("1 to 60 characters")
        );
        // R8.5: a folder inside itm's projects folder is its repository's, not a folder project.
        add(&itm, ProjectKind::Git, "git@github.com:acme/api.git", None).unwrap();
        let inside = itm.paths.projects.join("github.com/acme");
        for path in [api_clone(&itm), itm.paths.projects.clone(), inside] {
            let err = add(&itm, ProjectKind::Dir, path.to_str().unwrap(), Some("x")).unwrap_err().to_string();
            assert!(
                err.ends_with("is inside itm's projects folder; add its repository with `itm project add git`"),
                "{err}"
            );
        }
        assert_eq!(
            add(&itm, ProjectKind::Git, "/home/me/repo", None).unwrap_err().to_string(),
            "not a git URL: /home/me/repo"
        );
    }

    /// R8.1, R8.2: a name only, named by its source; never "project" for a blank one.
    /// DESIGN.md section 2: adding (setting, renaming, removing) holds
    /// `.projects.lock`, so a clone, which runs outside the store lock, is
    /// never raced by another add that checked for duplicates first.
    #[test]
    fn adding_waits_for_the_projects_lock() {
        let (_tmp, itm) = itm();
        let held = itm.projects_lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(|| tx.send(add(&itm, ProjectKind::Name, "Client X", None)).unwrap());
            let waited = rx.recv_timeout(std::time::Duration::from_millis(300));
            assert!(waited.is_err(), "it waits while another holds the lock: {waited:?}");
            drop(held);
            let added = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("it goes on once the lock is free");
            assert_eq!(added.unwrap(), "Client X");
        });
    }

    #[test]
    fn a_name_only_project() {
        let (_tmp, itm) = itm();
        assert_eq!(add(&itm, ProjectKind::Name, " Client X ", None).unwrap(), "Client X");
        let (_, saved) = project(&itm, "client x");
        assert_eq!((saved.kind(), saved.folder()), (ProjectKind::Name, None));
        assert_eq!(itm.paths.place(&saved), "no folder");
        let err = add(&itm, ProjectKind::Name, "Other", Some("Another name")).unwrap_err();
        assert!(err.to_string().contains("named by its source"), "{err}");
        for blank in ["", "   "] {
            assert!(
                add(&itm, ProjectKind::Name, blank, None)
                    .unwrap_err()
                    .to_string()
                    .contains("a project name has 1 to 60")
            );
        }
        let err = add(&itm, ProjectKind::Name, "client x", None).unwrap_err();
        assert_eq!(err.to_string().split(" (").next().unwrap(), "project already exists: Client X");
        let data = itm.store().load().unwrap();
        let rows = project_rows(&data);
        assert_eq!((rows[0].kind, rows[0].path, rows[0].url), (ProjectKind::Name, None, None));
    }

    /// R8.6: a repository is cloned under projects/<host>/<path> and saved with its URL as given.
    #[test]
    fn a_repository_is_cloned_into_the_projects_folder() {
        let (_tmp, mut itm) = itm();
        fake_git(&mut itm);
        assert_eq!(add(&itm, ProjectKind::Git, " git@github.com:acme/api.git ", None).unwrap(), "api");
        let (_, api) = project(&itm, "api");
        assert_eq!(api.url.as_deref(), Some("git@github.com:acme/api.git"));
        assert_eq!(api.folder(), Some(api_clone(&itm).as_path()));
        assert_eq!(api.kind(), ProjectKind::Git);
        assert!(api_clone(&itm).join(".git").is_dir());
        assert_eq!(
            leftovers(&itm),
            [
                "github.com",
                "github.com/acme",
                "github.com/acme/api",
                "github.com/acme/api/.git",
                "github.com/acme/api/.git/origin"
            ],
            "no hidden folder is left"
        );
        let mode = fs::metadata(api_clone(&itm)).unwrap().permissions();
        assert_ne!(std::os::unix::fs::PermissionsExt::mode(&mode) & 0o077, 0, "not the hidden folder's 0700");

        // R8.4: the same repository by another URL is refused before any clone.
        let err = add(&itm, ProjectKind::Git, "https://github.com/acme/api", Some("api2")).unwrap_err();
        assert_eq!(err.to_string(), "project already exists: api");
        // R8.5: two remotes named api need another name; the message names the other one.
        let err = add(&itm, ProjectKind::Git, "git@gitlab.com:other/api.git", None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "project already exists: api (git@github.com:acme/api.git); choose another name, or rename it with `itm project rename api <name>`"
        );
        assert!(!itm.paths.projects.join("gitlab.com").exists(), "nothing was cloned");
        assert_eq!(
            add(&itm, ProjectKind::Git, "git@gitlab.com:other/api.git", Some("other-api")).unwrap(),
            "other-api"
        );
        assert_eq!(project(&itm, "other-api").1.url.as_deref(), Some("git@gitlab.com:other/api.git"));
    }

    /// R8.6: a checkout of the repository already there is reused; any other folder or a link is an error.
    #[test]
    fn a_checkout_already_there_is_reused_and_anything_else_refused() {
        let (_tmp, mut itm) = itm();
        let script = fake_git(&mut itm);
        fs::create_dir_all(api_clone(&itm).join(".git")).unwrap();
        fs::write(api_clone(&itm).join(".git/origin"), "git@github.com:acme/api\n").unwrap();
        fs::write(script.parent().unwrap().join("clone-fails"), "").unwrap(); // a clone would fail
        itm.clone_repo("https://github.com/acme/api", &api_clone(&itm)).unwrap();
        let err = itm.clone_repo("https://github.com/other/api", &api_clone(&itm)).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "{} already exists and is not a checkout of https://github.com/other/api",
                api_clone(&itm).display()
            )
        );
        let web = itm.paths.projects.join("github.com/acme/web");
        std::os::unix::fs::symlink(api_clone(&itm), &web).unwrap();
        assert!(
            itm.clone_repo("git@github.com:acme/web.git", &web).unwrap_err().to_string().contains("not a checkout")
        );
        let plain = itm.paths.projects.join("github.com/acme/plain");
        fs::create_dir(&plain).unwrap();
        assert!(
            itm.clone_repo("git@github.com:acme/plain.git", &plain).unwrap_err().to_string().contains("not a checkout")
        );
    }

    /// DESIGN.md section 2: a failed clone leaves nothing, not even the folders it made.
    #[test]
    fn a_failed_clone_leaves_nothing() {
        let (_tmp, mut itm) = itm();
        let script = fake_git(&mut itm);
        fs::write(script.parent().unwrap().join("clone-fails"), "").unwrap();
        let err = add(&itm, ProjectKind::Git, "https://github.com/acme/api", None).unwrap_err();
        assert_eq!(err.to_string(), "could not clone https://github.com/acme/api");
        assert_eq!(leftovers(&itm), Vec::<String>::new());
        assert!(itm.store().load().unwrap().projects.is_empty());
        // A missing git is said as such.
        itm.git = Git::with(&["/nowhere/git"]);
        let err = add(&itm, ProjectKind::Git, "https://github.com/acme/api", None).unwrap_err();
        assert_eq!(err.to_string(), "git not found; install it or add it to PATH");
    }

    /// R8.2: rename frees the name and keeps the tasks (they store its id); the new name is checked.
    #[test]
    fn rename_frees_the_name_and_keeps_tasks() {
        let (_tmp, mut itm) = itm();
        fake_git(&mut itm);
        add(&itm, ProjectKind::Git, "git@github.com:acme/api.git", None).unwrap();
        let api = itm.create_task("A", None, &["api".into()], None).unwrap();
        assert_eq!(itm.rename_project("api", "acme-api").unwrap(), ("api".into(), "acme-api".into(), None));
        add(&itm, ProjectKind::Git, "git@gitlab.com:other/api.git", None).unwrap();
        let data = itm.store().load().unwrap();
        assert_eq!(data.projects[&data.tasks[&api].projects[0]].name, "acme-api");

        add(&itm, ProjectKind::Name, "notes", None).unwrap();
        assert_eq!(itm.rename_project("acme-api", "ACME-api").unwrap().1, "ACME-api", "only the case changes");
        let err = itm.rename_project("acme-api", "Notes").unwrap_err().to_string();
        assert!(err.starts_with("project already exists: notes (no folder)"), "{err}");
        assert!(itm.rename_project("acme-api", " ").unwrap_err().to_string().contains("1 to 60 characters"));
        assert_eq!(itm.rename_project("missing", "x").unwrap_err(), Error::NoSuchProject("missing".into()));
        assert_eq!(project(&itm, "ACME-api").1.kind(), ProjectKind::Git);
    }

    /// R8.2, R10.1: renaming a project without a folder names the tasks that
    /// may lose it, where workspace sync found it in a workspace open now.
    #[test]
    fn renaming_says_named_workspaces_no_longer_count() {
        let (_tmp, itm) = itm();
        add(&itm, ProjectKind::Name, "Client X", None).unwrap();
        let (id, _) = project(&itm, "Client X");
        itm.create_task("Run", None, &[], None).unwrap();
        itm.create_task("Stopped", None, &[], None).unwrap();
        itm.store()
            .edit(|data| {
                let run = data.tasks.get_mut("task-run").unwrap();
                (run.projects, run.found_projects, run.workspace_projects) =
                    (vec![id.clone()], vec![id.clone()], vec![id.clone()]);
                let stopped = data.tasks.get_mut("task-stopped").unwrap();
                (stopped.projects, stopped.found_projects) = (vec![id.clone()], vec![id.clone()]); // no workspace open now
                Ok(())
            })
            .unwrap();
        let (old, new, note) = itm.rename_project("Client X", "Client Y").unwrap();
        assert_eq!((old.as_str(), new.as_str()), ("Client X", "Client Y"));
        let note = note.unwrap();
        assert!(
            note.contains("workspaces named Client X no longer count as it (it has no folder), so it may leave Run"),
            "{note}"
        );
        assert!(!note.contains("Stopped"), "{note}");
        assert_eq!(itm.rename_project("Client Y", "client y").unwrap().2, None, "the same name, another case");
        // R10.4: with "remove a project when its workspace closes" off, nothing is removed.
        itm.change_setting(crate::config::Setting::RemoveProjectsWithoutWorkspace, false).unwrap();
        assert_eq!(itm.rename_project("client y", "Client Z").unwrap().2, None);
        // A broken settings file says nothing about it: the note is given.
        fs::create_dir_all(&itm.paths.config).unwrap();
        fs::write(itm.paths.config_file(), "[herdr\n").unwrap();
        assert!(itm.rename_project("Client Z", "Client W").unwrap().2.is_some());
    }

    /// R8.2: `set` gives a project without a repository a folder, moves it or takes it; nothing on disk changes.
    #[test]
    fn set_gives_moves_and_takes_the_folder() {
        let (_tmp, mut itm) = itm();
        fake_git(&mut itm);
        add(&itm, ProjectKind::Name, "Client X", None).unwrap();
        let (first, second) = (folder(&itm, "code/x").canonicalize().unwrap(), folder(&itm, "code/x2"));
        itm.create_task("Work", None, &["Client X".into()], None).unwrap();
        assert_eq!(
            itm.set_project_folder("client x", Some("~/code/x")).unwrap(),
            ["Client X is now in ~/code/x", "workspaces of Work count as it by the new folder from now on"]
        );
        assert_eq!(project(&itm, "Client X").1.folder(), Some(first.as_path()));
        assert_eq!(
            itm.set_project_folder("Client X", Some(" ~/code/x ")).unwrap(),
            ["Client X already has that folder"]
        );
        itm.set_project_folder("Client X", Some("~/code/x2")).unwrap();
        assert_eq!(
            itm.set_project_folder("Client X", None).unwrap(),
            ["Client X has no folder now", "workspaces of Work count as it only when named after it from now on"]
        );
        assert_eq!(itm.set_project_folder("Client X", None).unwrap(), ["Client X has no folder"]);
        assert_eq!(project(&itm, "Client X").1.folder(), None);
        assert!(second.is_dir(), "nothing on disk changes");

        add(&itm, ProjectKind::Dir, "~/code/other", None).unwrap_err();
        folder(&itm, "code/other");
        add(&itm, ProjectKind::Dir, "~/code/other", None).unwrap();
        let err = itm.set_project_folder("Client X", Some("~/code/other")).unwrap_err();
        assert_eq!(err.to_string(), "project already exists: other");
        let clones = itm.paths.projects.join("github.com/a/b");
        fs::create_dir_all(&clones).unwrap();
        let err = itm.set_project_folder("Client X", clones.to_str()).unwrap_err().to_string();
        assert!(err.contains("inside itm's projects folder"), "{err}");
        add(&itm, ProjectKind::Git, "git@github.com:acme/api.git", None).unwrap();
        let err = itm.set_project_folder("api", None).unwrap_err().to_string();
        assert_eq!(err, "api's folder is itm's clone of git@github.com:acme/api.git; it cannot be changed");
        // A folder project can move too, and the old folder is free again.
        assert_eq!(itm.set_project_folder("other", Some("~/code/x")).unwrap(), ["other is now in ~/code/x"]);
    }

    /// R8.7: removing is refused while any task has the project, archived ones included.
    #[test]
    fn remove_is_refused_while_used() {
        let (_tmp, itm) = itm();
        folder(&itm, "a");
        add(&itm, ProjectKind::Dir, "~/a", None).unwrap();
        itm.create_task("X", None, &["a".into()], None).unwrap();
        itm.store().edit(|data| Ok(data.tasks.get_mut("task-x").map(|t| t.archive(1)))).unwrap();
        let err = itm.remove_project("a", CloneChoice::Keep).unwrap_err();
        assert_eq!(
            err.to_string(),
            "project 'a' is used by 1 task(s); remove it from them (`itm edit <task> --remove-project a`) or delete them first: X"
        );
        assert_eq!(itm.store().load().unwrap().projects.len(), 1);
    }

    /// R8.7: removing a folder or a name only forgets it.
    #[test]
    fn removing_a_folder_or_a_name_forgets_it() {
        let (_tmp, itm) = itm();
        let a = folder(&itm, "a");
        add(&itm, ProjectKind::Dir, "~/a", None).unwrap();
        add(&itm, ProjectKind::Name, "Client X", None).unwrap();
        assert_eq!(itm.removal_plan("Client X").unwrap().clone, None);
        assert_eq!(
            itm.remove_project("A", CloneChoice::Delete).unwrap(),
            [RemovalNote::Done("project a removed".into())]
        );
        assert_eq!(
            itm.remove_project("client x", CloneChoice::Keep).unwrap(),
            [RemovalNote::Done("project Client X removed".into())]
        );
        assert!(itm.store().load().unwrap().projects.is_empty());
        assert!(a.is_dir());
    }

    /// R8.7: a clone is deleted with its empty host and owner folders, kept
    /// with local work unless asked, and checked again when deleting.
    #[test]
    fn a_clone_is_deleted_only_when_clean_or_asked() {
        let (_tmp, mut itm) = itm();
        fake_git(&mut itm);
        let api = || add(&itm, ProjectKind::Git, "git@github.com:acme/api.git", None).unwrap();
        api();
        let plan = itm.removal_plan("API").unwrap();
        assert_eq!((plan.users.len(), plan.clone), (0, Some(CloneState::Deletable { local_work: None })));
        assert_eq!(itm.remove_project("api", CloneChoice::Keep).unwrap().len(), 1);
        assert!(api_clone(&itm).is_dir(), "kept");
        fs::remove_dir_all(&itm.paths.projects).unwrap();

        api();
        let notes = itm.remove_project("api", CloneChoice::IfClean).unwrap();
        assert_eq!(notes[1], RemovalNote::Done(format!("deleted {}", api_clone(&itm).display())));
        assert_eq!(leftovers(&itm), Vec::<String>::new(), "the empty host and owner folders went too");

        api();
        fs::write(api_clone(&itm).join(".git/stashes"), "stash@{0}: WIP\n").unwrap();
        let plan = itm.removal_plan("api").unwrap();
        assert_eq!(plan.clone, Some(CloneState::Deletable { local_work: Some("stashes".into()) }));
        let notes = itm.remove_project("api", CloneChoice::IfClean).unwrap();
        let kept = format!("kept {}: it has stashes; --delete-clone deletes it anyway", api_clone(&itm).display());
        assert_eq!(notes[1], RemovalNote::Kept(kept));
        assert!(api_clone(&itm).is_dir());
        api();
        itm.remove_project("api", CloneChoice::Delete).unwrap();
        assert!(!api_clone(&itm).exists(), "--delete-clone deletes it anyway");

        api();
        fs::remove_dir_all(api_clone(&itm)).unwrap();
        assert_eq!(itm.removal_plan("api").unwrap().clone, Some(CloneState::Missing));
        assert_eq!(
            itm.remove_project("api", CloneChoice::Delete).unwrap().len(),
            1,
            "nothing to say about a clone gone"
        );
    }

    /// R8.7: only a real checkout of the repository inside the projects folder, holding no other project, is deleted.
    #[test]
    fn what_is_not_its_clone_is_kept() {
        let (_tmp, mut itm) = itm();
        fake_git(&mut itm);
        // Another project inside the clone.
        add(&itm, ProjectKind::Git, "git@github.com:acme/api.git", None).unwrap();
        itm.store()
            .edit(|data| {
                let docs = api_clone(&itm).join("docs");
                fs::create_dir(&docs).unwrap();
                let path = Some(docs.display().to_string());
                let project = Project {
                    name: "docs".into(),
                    path,
                    url: None,
                    added_at: 1,
                    last_used_at: 1,
                    extra: Default::default(),
                };
                Ok(data.projects.insert("d0c5d0c5".into(), project))
            })
            .unwrap();
        assert_eq!(itm.removal_plan("api").unwrap().clone, Some(CloneState::Foreign));
        let notes = itm.remove_project("api", CloneChoice::Delete).unwrap();
        assert_eq!(
            notes[1],
            RemovalNote::Kept(format!("kept {}: not a clone itm can delete", api_clone(&itm).display()))
        );
        assert!(api_clone(&itm).is_dir());
        itm.remove_project("docs", CloneChoice::Keep).unwrap();
        fs::remove_dir_all(&itm.paths.projects).unwrap();

        // Not a checkout of its repository.
        add(&itm, ProjectKind::Git, "git@github.com:acme/api.git", None).unwrap();
        fs::write(api_clone(&itm).join(".git/origin"), "git@github.com:someone/else\n").unwrap();
        itm.remove_project("api", CloneChoice::Delete).unwrap();
        assert!(api_clone(&itm).is_dir());
        fs::remove_dir_all(&itm.paths.projects).unwrap();

        // A clone behind a symlinked folder, and a symlinked clone: what they point at stays.
        let elsewhere = folder(&itm, "code/acme/api");
        fs::create_dir(elsewhere.join(".git")).unwrap();
        fs::write(elsewhere.join(".git/origin"), "git@github.com:acme/api.git\n").unwrap();
        let save = |path: &Path| {
            itm.store()
                .edit(|data| {
                    let url = Some("git@github.com:acme/api.git".into());
                    let path = Some(path.display().to_string());
                    Ok(data.projects.insert(
                        "a91a91a9".into(),
                        Project {
                            name: "api".into(),
                            path,
                            url,
                            added_at: 1,
                            last_used_at: 1,
                            extra: Default::default(),
                        },
                    ))
                })
                .unwrap()
        };
        fs::create_dir_all(itm.paths.projects.join("github.com")).unwrap();
        std::os::unix::fs::symlink(elsewhere.parent().unwrap(), itm.paths.projects.join("github.com/acme")).unwrap();
        save(&api_clone(&itm));
        itm.remove_project("api", CloneChoice::Delete).unwrap();
        assert!(elsewhere.join(".git").is_dir(), "behind a symlinked folder");
        fs::remove_file(itm.paths.projects.join("github.com/acme")).unwrap();
        fs::create_dir(itm.paths.projects.join("github.com/acme")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, api_clone(&itm)).unwrap();
        save(&api_clone(&itm));
        itm.remove_project("api", CloneChoice::Delete).unwrap();
        assert!(elsewhere.join(".git").is_dir(), "a symlinked clone");
    }

    /// R8.8: the child a terminal UI clones with never prompts, and a clone
    /// that wanted to ask says to add it from a terminal.
    #[test]
    fn the_clone_child_never_prompts() {
        let (_tmp, mut itm) = itm();
        itm.program = "/opt/itm/bin/itm".into();
        let command = itm.clone_child("git@github.com:acme/api.git", Some("api2")).unwrap();
        let args: Vec<_> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(
            args,
            ["setsid", "/opt/itm/bin/itm", "project", "add", "git", "--name=api2", "--", "git@github.com:acme/api.git"]
        );
        let envs: Vec<_> = command
            .get_envs()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned())))
            .collect();
        assert!(envs.contains(&("ITM_POPUP".into(), None)), "{envs:?}");
        assert!(envs.contains(&("GIT_TERMINAL_PROMPT".into(), Some("0".into()))), "{envs:?}");
        assert!(
            envs.iter()
                .any(|(k, v)| k == "GIT_SSH_COMMAND" && v.as_deref().is_some_and(|v| v.ends_with(" -o BatchMode=yes")))
        );
        assert_eq!(batch_ssh(Some("ssh -i key".as_ref())), "ssh -i key -o BatchMode=yes");
        assert_eq!(batch_ssh(Some("".as_ref())), "ssh -o BatchMode=yes");
        assert_eq!(batch_ssh(None), "ssh -o BatchMode=yes");
        assert!(clone_hint("fatal: could not read Username for 'https://x'").unwrap().contains("in a terminal"));
        assert!(clone_hint("Host key verification failed.").is_some());
        assert_eq!(clone_hint("fatal: repository not found"), None);
    }

    #[test]
    fn a_new_task_takes_projects_by_name_and_marks_them_used() {
        let (_tmp, itm) = itm();
        folder(&itm, "api");
        folder(&itm, "web");
        add(&itm, ProjectKind::Dir, "~/api", None).unwrap();
        add(&itm, ProjectKind::Dir, "~/web", None).unwrap();
        let projects = ["WEB".to_string(), "api".to_string(), "web".to_string()];
        let name = itm.create_task("Fix", None, &projects, None).unwrap();
        let data = itm.store().load().unwrap();
        let ids = [resolve_project(&data, "web").unwrap(), resolve_project(&data, "api").unwrap()];
        assert_eq!(data.tasks[&name].projects, ids);
        assert!(ids.iter().all(|id| data.projects[id].last_used_at > 0));
        let rows = project_rows(&data);
        assert_eq!(rows.iter().map(|r| (r.name, r.tasks)).collect::<Vec<_>>(), [("api", 1), ("web", 1)]);

        let unknown = itm.create_task("Nope", None, &["missing".to_string()], None).unwrap_err();
        assert_eq!(unknown, Error::NoSuchProject("missing".into()));
        assert!(!itm.store().load().unwrap().tasks.contains_key("task-nope"));
    }

    /// R1.2: a session starts in its first project with a folder; a name
    /// only or a missing folder gets no workspace.
    #[test]
    fn a_session_starts_in_its_first_project_with_a_folder() {
        let (_tmp, itm) = itm();
        let web = folder(&itm, "web");
        folder(&itm, "gone");
        add(&itm, ProjectKind::Dir, "~/gone", None).unwrap();
        add(&itm, ProjectKind::Dir, "~/web", None).unwrap();
        add(&itm, ProjectKind::Name, "Client X", None).unwrap();
        let name = itm.create_task("Fix", None, &["Client X".into(), "gone".into(), "web".into()], None).unwrap();
        fs::remove_dir(itm.paths.home.join("gone")).unwrap();
        let data = itm.store().load().unwrap();
        assert_eq!(itm.task_cwd(&data, &name), web.canonicalize().unwrap());
        assert_eq!(itm.task_folders(&data, &name, false), [("web".to_string(), web.canonicalize().unwrap())]);
        assert_eq!(itm.task_cwd(&data, "task-other"), itm.paths.task_dir);
        let only = itm.create_task("Only", None, &["Client X".into()], None).unwrap();
        assert_eq!(itm.task_cwd(&itm.store().load().unwrap(), &only), itm.paths.task_dir);
        assert_eq!(missing_folders(&data).len(), 1, "a name only is never missing");
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
        add(&itm, ProjectKind::Dir, "~/api", None).unwrap();
        let name = itm.create_task("Fix", None, &["api".into()], None).unwrap();
        fs::create_dir_all(itm.paths.herdr_config.join("sessions").join(&name)).unwrap(); // Herdr knows it
        let mut data = itm.store().load().unwrap();
        itm.prepare_first_start(&data, &name).unwrap(); // would fail to start a server: none is started
        data.tasks.insert("task-plain".into(), Task::new("Plain", 1));
        itm.prepare_first_start(&data, "task-plain").unwrap();
    }

    /// R8.5: the add dialog's check while typing says what adding would do,
    /// as adding says it, and saves nothing.
    #[test]
    fn r8_5_the_check_while_typing_says_what_adding_would_do() {
        let (_tmp, mut itm) = itm();
        fake_git(&mut itm);
        let api = folder(&itm, "code/api");
        folder(&itm, "other/api");
        let check = itm.check_project(ProjectKind::Dir, "~/code/api", None).unwrap();
        assert_eq!(
            (check.folder.as_deref(), check.default_name.as_str(), check.problem),
            (Some(api.as_path()), "api", None)
        );
        itm.add_project(ProjectKind::Dir, "~/code/api", None).unwrap();
        let problem =
            |kind, source, name| itm.check_project(kind, source, name).unwrap().problem.map(|e| e.to_string());
        assert_eq!(problem(ProjectKind::Dir, "~/code/api/", None).as_deref(), Some("project already exists: api"));
        let taken = problem(ProjectKind::Dir, "~/other/api", None).unwrap();
        assert!(taken.starts_with("project already exists: api (~/code/api); choose another name"), "{taken}");
        assert_eq!(problem(ProjectKind::Dir, "~/other/api", Some("api2")), None);
        assert_eq!(problem(ProjectKind::Name, "API", None).as_deref(), Some(taken.as_str()));
        assert_eq!(problem(ProjectKind::Name, "Client X", None), None);
        let check = itm.check_project(ProjectKind::Git, "https://github.com/acme/web.git", None).unwrap();
        assert_eq!(check.folder, Some(itm.paths.projects.join("github.com/acme/web")));
        assert_eq!((check.default_name.as_str(), check.problem), ("web", None));
        let err = itm.check_project(ProjectKind::Dir, "~no-such-user-x", None).unwrap_err();
        assert_eq!(err.to_string(), "not a folder: ~no-such-user-x");
        assert_eq!(itm.store().load().unwrap().projects.len(), 1, "nothing saved");
        let blank = itm.add_project(ProjectKind::Name, "  ", None).unwrap_err();
        assert_eq!(blank, Error::ProjectName, "a blank name is refused, not saved as \"project\"");
    }

    /// R1.6, R8.3, R8.4: recency order, git URLs being typed, and the saved repository of a URL.
    #[test]
    fn r1_6_projects_by_recent_and_saved_repositories() {
        let mut data = Data::default();
        let project = |name: &str, url: Option<&str>, used| Project {
            name: name.into(),
            path: Some(format!("/p/{name}")),
            url: url.map(Into::into),
            added_at: 0,
            last_used_at: used,
            extra: Default::default(),
        };
        data.projects.insert("p1".into(), project("web", None, 5));
        data.projects.insert("p2".into(), project("api", Some("git@github.com:acme/api.git"), 9));
        data.projects.insert("p3".into(), project("Docs", None, 5));
        let order: Vec<&str> = projects_by_recent(&data.projects).iter().map(|(_, p)| p.name.as_str()).collect();
        assert_eq!(order, ["api", "Docs", "web"]);
        for url in ["https://github.com/acme/api", "git@github.com:acme/api.git", "ssh://git@github.com/acme/api"] {
            assert!(looks_like_git_url(url), "{url}");
            assert_eq!(saved_repository(&data.projects, url).map(String::as_str), Some("p2"), "{url}");
        }
        assert_eq!(saved_repository(&data.projects, "https://github.com/acme/web"), None);
        for text in ["api", "my api", "~/code", "git@github.com", "a@b:"] {
            assert!(!looks_like_git_url(text), "{text}");
        }
        let paths = Paths::under(Path::new("/r"));
        assert_eq!(name_problem(&data, &paths, "API", "p2"), None, "its own name, in any case");
        let taken = name_problem(&data, &paths, "docs", "p2").unwrap();
        assert!(taken.is_about_the_name() && taken.to_string().starts_with("project already exists: Docs (/p/Docs)"));
        assert_eq!(name_problem(&data, &paths, " ", "p2"), Some(Error::ProjectName));
    }

    /// R8.8: what a failed clone child said, and the hint when it wanted to ask.
    #[test]
    fn r8_8_a_failed_clone_says_its_last_error() {
        let said =
            "Cloning into 'x'...\nfatal: could not read Username for 'https://github.com'\nitm: could not clone x\n";
        assert_eq!(
            clone_failure(said),
            "could not clone x\nthis repository needs a password or a host key confirmation; run `itm project add git <url>` in a terminal"
        );
        assert_eq!(clone_failure("fatal: repository not found\n"), "fatal: repository not found");
        assert_eq!(clone_failure(""), "the clone failed");
    }
}
