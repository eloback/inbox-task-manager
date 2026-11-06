//! Herdr workspaces as a source of a task's projects (R10): which saved
//! project a workspace is in (R10.2), and `itm project add-workspace`
//! (R10.5), which saves the folder of the workspace it runs in.
//!
//! A workspace named after a saved project is in it wherever its first pane
//! went; a label Herdr made up from the folder is not a name. Any other
//! workspace is in the innermost saved project holding its folder, else in
//! the project of its git repository. Only saved projects count: the home
//! folder, where tasks without projects start, is in none unless it is
//! saved itself (DESIGN.md section 2, Projects).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::error::{Error, Result, bail, shell_quote};
use crate::git::Repository;
use crate::herdr::Workspace;
use crate::itm::Itm;
use crate::paths::resolved;
use crate::store::{Project, ProjectKind};
use crate::tasks::Edit;

/// How long a folder's repository is remembered: the daemon asks on every pass.
const REPOSITORY_CACHE: Duration = Duration::from_secs(300);

/// Saved projects by id.
pub type Projects = BTreeMap<String, Project>;

/// (id, resolved folder) of the projects with a folder, resolved once per
/// pass rather than once per workspace.
pub type Roots = Vec<(String, PathBuf)>;

pub fn project_roots(projects: &Projects) -> Roots {
    projects.iter().filter_map(|(id, p)| Some((id.clone(), resolved(p.folder()?)))).collect()
}

/// The saved project `folder` is in, the innermost one when projects nest.
/// A folder above a project (`~` holding `~/code/api`) is in none, and a
/// project without a folder holds nothing.
pub fn folder_project(roots: &Roots, folder: &Path) -> Option<String> {
    let folder = resolved(folder);
    let holders = roots.iter().filter(|(_, root)| folder.starts_with(root));
    holders.max_by_key(|(_, root)| root.as_os_str().len()).map(|(id, _)| id.clone())
}

/// Whether `label` is what Herdr shows for an unnamed workspace in `folder`:
/// the name of the folder or of one above it (a repository's top folder).
pub fn automatic_label(label: &str, folder: &Path) -> bool {
    folder.ancestors().any(|part| part.file_name().is_some_and(|name| name == label))
}

/// The folders' git repositories, remembered for 5 minutes.
#[derive(Default)]
pub struct Repositories(Mutex<HashMap<PathBuf, (Instant, Option<Repository>)>>);

impl Itm {
    /// The repository `folder` is in, asked once per [`REPOSITORY_CACHE`].
    fn repository(&self, folder: &Path) -> Option<Repository> {
        let cache = || self.repositories.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((when, found)) = cache().get(folder)
            && when.elapsed() < REPOSITORY_CACHE
        {
            return found.clone();
        }
        let found = self.git.repository(folder); // not under the lock: git may take seconds
        cache().insert(folder.to_path_buf(), (Instant::now(), found.clone()));
        found
    }

    /// The saved project of the git repository `folder` is in, for a folder
    /// in none of them: a linked worktree (Herdr's live outside the project)
    /// is in the project its main work tree is in; another checkout is in
    /// the one saved project whose checkout has the same `origin` (R8.4
    /// forms are one); with two such projects, in neither.
    fn repository_project(&self, projects: &Projects, roots: &Roots, folder: &Path) -> Option<String> {
        let repository = self.repository(folder)?;
        if let Some(id) = repository.root.as_deref().and_then(|root| folder_project(roots, root)) {
            return Some(id);
        }
        let origin = repository.origin?;
        let same_origin = |p: &Project| {
            p.folder().and_then(|f| self.repository(f)).and_then(|r| r.origin).as_deref() == Some(origin.as_str())
        };
        let mut same = projects.iter().filter(|(_, p)| same_origin(p)).map(|(id, _)| id);
        match (same.next(), same.next()) {
            (Some(id), None) => Some(id.clone()),
            _ => None,
        }
    }

    /// The project a workspace is in (R10.2). A workspace named after a
    /// project (itm names the ones it opens; add-workspace names its own) is
    /// in it wherever its first pane went. Any other is in the project its
    /// folder is in, else in the project of its git repository.
    pub fn workspace_project(&self, projects: &Projects, roots: &Roots, workspace: &Workspace) -> Option<String> {
        let label = workspace.label.trim();
        if !label.is_empty() && !workspace.folder.as_deref().is_some_and(|f| automatic_label(label, f)) {
            let lowered = label.to_lowercase();
            let mut named = projects.iter().filter(|(_, p)| p.name.to_lowercase() == lowered);
            if let (Some((id, _)), None) = (named.next(), named.next()) {
                return Some(id.clone());
            }
        }
        let folder = workspace.folder.as_deref()?;
        folder_project(roots, folder).or_else(|| self.repository_project(projects, roots, folder))
    }

    /// The saved projects a session's workspaces are in, in workspace order,
    /// each once; and whether every workspace's folder was known.
    pub fn workspace_projects(
        &self,
        projects: &Projects,
        roots: &Roots,
        workspaces: &[Workspace],
    ) -> (Vec<String>, bool) {
        let mut found: Vec<String> = vec![];
        for id in workspaces.iter().filter_map(|w| self.workspace_project(projects, roots, w)) {
            if !found.contains(&id) {
                found.push(id);
            }
        }
        (found, workspaces.iter().all(|w| w.folder.is_some()))
    }
}

// ------------------------------------------------------------------ add-workspace (R10.5)

/// How `itm project add-workspace` asks, in a terminal.
pub trait Ask {
    /// The add dialog, filled with the workspace's folder and name: the id
    /// of the project saved, or None when it was closed.
    fn add_folder(&self, folder: &Path, name: &str) -> Result<Option<String>>;
    /// Whether `project`, which has no folder, gets `folder`: Some(true)
    /// give it, Some(false) keep it without one, None cancel everything.
    fn give_folder(&self, project: &str, folder: &str) -> Result<Option<bool>>;
}

/// What `add-workspace` did: the project, and what happened besides.
#[derive(Debug)]
pub struct Added {
    pub project: Project,
    pub said: Vec<String>,
}

impl Added {
    /// What a popup keeps on screen long enough to read, as it does errors.
    pub fn warnings(&self) -> impl Iterator<Item = &String> {
        let warning = |line: &&String| {
            line.contains("kept without a folder:")
                || line.contains("kept without a folder;")
                || line.starts_with("could not")
        };
        self.said.iter().filter(warning)
    }
}

impl Itm {
    /// The session and workspace this command runs in: in a popup, the one
    /// it was opened over (`active`: `HERDR_ACTIVE_WORKSPACE_ID`), else the
    /// focused one. Never a pane's `HERDR_WORKSPACE_ID`, which stays what it
    /// was when the pane moved to another workspace.
    fn current_workspace(&self, active: Option<&str>) -> Result<(String, Workspace)> {
        let session = match self.inside_herdr() {
            true => self.current_session(&self.herdr.sessions()?),
            false => None,
        };
        let Some(session) = session else { bail!("run this inside Herdr, from the workspace to add (prefix+shift+a)") };
        let view = self.herdr.snapshot(&self.herdr.socket(&session)).ok();
        let workspaces = view.and_then(|v| v.workspaces).ok_or_else(|| Error::Other("Herdr did not answer".into()))?;
        let opened_over = workspaces.iter().find(|w| active.is_some_and(|id| !id.is_empty() && w.id == id));
        match opened_over.or_else(|| workspaces.iter().find(|w| w.focused)) {
            Some(workspace) if workspace.folder.is_some() => Ok((session, workspace.clone())),
            _ => bail!("cannot tell which workspace this is"),
        }
    }

    /// Refuse the home folder, a folder above it and the folder tasks start
    /// in: saved, such a folder would hold every workspace under it, and
    /// workspace sync would add it to every task. `itm project add dir`
    /// still saves them.
    fn check_workspace_folder(&self, folder: &Path) -> Result<()> {
        let (folder, home) = (resolved(folder), resolved(&self.paths.home));
        if folder == home || folder == resolved(&self.paths.task_dir) || (home.starts_with(&folder)) {
            let shown = self.paths.display(&folder);
            bail!(
                "{shown} holds every workspace under it, so it is not added from a workspace; open the workspace in a project's folder, or use `itm project add dir {shown}`"
            );
        }
        Ok(())
    }

    /// Why a workspace's folder cannot go to the project `id` (which has
    /// none), if it cannot: the home folder or above, inside itm's projects
    /// folder, or in another project.
    fn folder_problem(&self, projects: &Projects, id: &str, folder: &Path) -> Option<String> {
        let checked = self.check_workspace_folder(folder).and_then(|()| self.folder_spec(&folder.to_string_lossy()));
        let path = match checked {
            Ok(path) => path,
            Err(e) => return Some(e.to_string()),
        };
        match folder_project(&project_roots(projects), &path) {
            Some(holder) if holder != id => {
                Some(format!("{} is in project {}", self.paths.display(&path), projects[&holder].name))
            }
            _ => None,
        }
    }

    /// Give the project `id`, which has no folder, the workspace's folder.
    /// Checked again under the projects lock: another command may have
    /// given it one, or saved a project there, meanwhile.
    fn give_folder(&self, id: &str, folder: &Path) -> Result<()> {
        let _lock = self.projects_lock()?;
        self.store().edit(|data| {
            let project = data.projects.get(id).ok_or_else(|| Error::Other("the project no longer exists".into()))?;
            if project.kind() != ProjectKind::Name {
                bail!("{} has a folder already", project.name);
            }
            if let Some(problem) = self.folder_problem(&data.projects, id, folder) {
                bail!("{problem}");
            }
            let path = self.folder_spec(&folder.to_string_lossy())?;
            data.projects.get_mut(id).expect("checked").path = Some(path.display().to_string());
            Ok(())
        })
    }

    /// Offer a project without a folder the workspace's folder. It changes
    /// which workspaces count as the project in every task, so it never
    /// happens without a yes (`ask`: a terminal, and no name given); a
    /// folder it could not get is not asked about. Returns what happened.
    fn offer_folder(&self, id: &str, folder: &Path, ask: Option<&dyn Ask>) -> Result<Vec<String>> {
        let projects = self.store().load()?.projects;
        let name = &projects.get(id).ok_or_else(|| Error::Other("the project no longer exists".into()))?.name;
        if let Some(problem) = self.folder_problem(&projects, id, folder) {
            return Ok(vec![format!("{name} kept without a folder: {problem}")]);
        }
        let Some(ask) = ask else {
            let (quoted, place) = (shell_quote(name), shell_quote(&folder.to_string_lossy()));
            return Ok(vec![format!(
                "{name} kept without a folder; `itm project set {quoted} --dir {place}` gives it this folder"
            )]);
        };
        Ok(vec![match ask.give_folder(name, &self.paths.display(folder))? {
            None => return Err(Error::Cancelled),
            Some(false) => format!("{name} kept without a folder"),
            Some(true) => match self.give_folder(id, folder) {
                Ok(()) => format!("{name} is now in {}", self.paths.display(folder)),
                Err(e) => format!("{name} kept without a folder: {e}"),
            },
        }])
    }

    /// Name the workspace after its project and give the project to the
    /// session's task. The workspace is there, so none is opened. While
    /// projects follow workspaces, the project is one found in it (it leaves
    /// when the workspace closes); otherwise it is added by hand.
    fn add_workspace_project(&self, session: &str, workspace: &Workspace, id: &str) -> Result<Vec<String>> {
        let data = self.store().load()?;
        let project = data.projects.get(id).ok_or_else(|| Error::Other("the project no longer exists".into()))?;
        let mut said = vec![];
        if let Err(e) = self.herdr.rename_workspace(&self.herdr.socket(session), &workspace.id, &project.name) {
            said.push(format!("could not rename the workspace: {e}"));
        }
        let Some(task) = data.tasks.get(session) else {
            said.push(format!("{session} is not a task, so no task got the project"));
            return Ok(said);
        };
        let had = task.projects.iter().any(|p| p == id);
        let edit =
            Edit { add: vec![id.into()], found: self.follows_workspaces(), has_workspace: true, ..Edit::default() };
        said.extend(self.edit_task(session, &edit)?);
        said.push(match had {
            true => format!("'{}' has it already", task.title),
            false => format!("added to '{}'", task.title),
        });
        Ok(said)
    }

    /// `itm project add-workspace [--name N]` (R10.5): save the folder of
    /// the workspace this runs in as a folder project, name the workspace
    /// after it and give it to the current task. A folder inside a saved
    /// project, or a workspace named after one, is that project. A project
    /// without a folder that the workspace is named after, or named like,
    /// is used instead of a new one and offered the folder. `ask` is None
    /// without a terminal: then nothing asks, and the add dialog is skipped.
    pub fn add_workspace(&self, name: Option<&str>, active: Option<&str>, ask: Option<&dyn Ask>) -> Result<Added> {
        let (session, workspace) = self.current_workspace(active)?;
        let folder = workspace.folder.clone().expect("current_workspace has a folder");
        let projects = self.store().load()?.projects;
        let mut id = self.workspace_project(&projects, &project_roots(&projects), &workspace);
        if id.is_none() && name.is_none() {
            id = named_like(&projects, &workspace, &folder);
        }
        let mut said = vec![];
        let id = match id {
            Some(id) if projects[&id].kind() == ProjectKind::Name => {
                // Only a terminal and no name given asks.
                said = self.offer_folder(&id, &folder, ask.filter(|_| name.is_none()))?;
                id
            }
            Some(id) => id,
            None => {
                self.check_workspace_folder(&folder)?;
                // A workspace the user named carries the name they want; an unnamed one is named after its folder.
                let label = Some(workspace.label.trim()).filter(|l| !l.is_empty());
                let default = name.or(label);
                match ask.filter(|_| name.is_none()) {
                    None => self.add_project(ProjectKind::Dir, &folder.to_string_lossy(), default)?,
                    Some(ask) => ask.add_folder(&folder, default.unwrap_or_default())?.ok_or(Error::Cancelled)?,
                }
            }
        };
        said.extend(self.add_workspace_project(&session, &workspace, &id)?);
        let data = self.store().load()?;
        let project = data.projects.get(&id).ok_or_else(|| Error::Other("the project no longer exists".into()))?;
        Ok(Added { project: project.clone(), said })
    }
}

/// A project without a folder named like a workspace's label or folder,
/// ignoring case: an unnamed workspace in `~/code/client-x` is labeled
/// "client-x", which is not a name ([`automatic_label`]), so the project
/// "Client-X" would not be found by name alone.
fn named_like(projects: &Projects, workspace: &Workspace, folder: &Path) -> Option<String> {
    let label = workspace.label.trim().to_lowercase();
    let base = folder.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let named = |p: &Project| {
        let name = p.name.to_lowercase();
        p.kind() == ProjectKind::Name && ((!label.is_empty() && name == label) || (!base.is_empty() && name == base))
    };
    let mut found = projects.iter().filter(|(_, p)| named(p)).map(|(id, _)| id);
    match (found.next(), found.next()) {
        (Some(id), None) => Some(id.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake;
    use crate::itm::tests::itm;
    use crate::projects::tests::folder;
    use serde_json::{Value, json};
    use std::cell::RefCell;
    use std::fs;
    use std::process::Command;
    use std::sync::Arc;

    fn workspace(id: &str, label: &str, folder: Option<&Path>) -> Workspace {
        Workspace { id: id.into(), label: label.into(), focused: false, folder: folder.map(Path::to_path_buf) }
    }

    fn unnamed(folder: &Path) -> Workspace {
        workspace("w1", "", Some(folder))
    }

    fn add_dir(itm: &Itm, rel: &str, name: &str) -> String {
        itm.add_project(ProjectKind::Dir, &folder(itm, rel).to_string_lossy(), Some(name)).unwrap()
    }

    fn projects(itm: &Itm) -> Projects {
        itm.store().load().unwrap().projects
    }

    fn id_of(itm: &Itm, name: &str) -> String {
        crate::projects::resolve_project(&itm.store().load().unwrap(), name).unwrap()
    }

    fn project_of(itm: &Itm, workspace: &Workspace) -> Option<String> {
        let projects = projects(itm);
        itm.workspace_project(&projects, &project_roots(&projects), workspace)
    }

    #[test]
    fn r10_2_folders_map_to_the_innermost_saved_project() {
        let (_tmp, itm) = itm();
        let (api, web) = (add_dir(&itm, "code/api", "api"), add_dir(&itm, "code/web", "web"));
        let lib = add_dir(&itm, "code/api/lib", "lib");
        let roots = project_roots(&projects(&itm));
        assert_eq!(folder_project(&roots, &folder(&itm, "code/api/src")), Some(api.clone()));
        assert_eq!(folder_project(&roots, &folder(&itm, "code/api/lib/x")), Some(lib));
        assert_eq!(folder_project(&roots, &itm.paths.home.join("code/apix")), None, "a prefix is not a folder");
        let workspaces = [
            unnamed(&folder(&itm, "code/web")),
            unnamed(&folder(&itm, "code/api")),
            unnamed(&folder(&itm, "code/api/src")),
            workspace("w4", "", None),
        ];
        let projects = projects(&itm);
        let found = itm.workspace_projects(&projects, &project_roots(&projects), &workspaces);
        assert_eq!(found, (vec![web, api], false), "one workspace's folder is unknown");
    }

    /// R10.2: tasks without a project start in the home folder; projects
    /// inside it must not claim it, and a project without a folder never does.
    #[test]
    fn r10_2_home_is_in_no_project_unless_saved() {
        let (_tmp, itm) = itm();
        let app = add_dir(&itm, "code/app", "app");
        itm.add_project(ProjectKind::Name, "named", None).unwrap();
        let home = itm.paths.home.clone();
        let roots = project_roots(&projects(&itm));
        assert_eq!(folder_project(&roots, &home), None);
        assert_eq!(itm.workspace_projects(&projects(&itm), &roots, &[unnamed(&home)]), (vec![], true));
        assert_eq!(folder_project(&roots, &home.join("code/app")), Some(app));
    }

    /// R10.2: a workspace named after a project is in it wherever its first
    /// pane went; an unnamed workspace's label (its folder's name) says nothing.
    #[test]
    fn r10_2_a_named_workspace_stays_with_its_project() {
        let (_tmp, itm) = itm();
        let (api, web) = (add_dir(&itm, "code/api", "api"), add_dir(&itm, "code/web", "web"));
        let web_src = folder(&itm, "code/web/src");
        assert_eq!(
            project_of(&itm, &workspace("w1", "api", Some(&web_src))),
            Some(api.clone()),
            "its pane went to web"
        );
        assert_eq!(project_of(&itm, &workspace("w1", " API ", None)), Some(api.clone()));
        let other = folder(&itm, "other/api");
        assert_eq!(project_of(&itm, &workspace("w1", "api", Some(&other))), None, "Herdr's label for other/api");
        assert_eq!(project_of(&itm, &workspace("w1", "web", Some(&folder(&itm, "code/web")))), Some(web));
        assert_eq!(project_of(&itm, &workspace("w1", "my stuff", Some(&folder(&itm, "code/api")))), Some(api));
    }

    /// R10.2: Herdr names an unnamed workspace after its folder, or after the
    /// repository it was opened in: the name of the folder or one above it.
    #[test]
    fn r10_2_herdr_labels_are_not_names() {
        assert!(automatic_label("sub", Path::new("/code/api/sub")));
        assert!(automatic_label("api", Path::new("/code/api/sub")));
        assert!(!automatic_label("my api", Path::new("/code/api/sub")));
        let (_tmp, itm) = itm();
        let api = add_dir(&itm, "code/api", "api");
        let other = folder(&itm, "other/api/src"); // another repository with that name
        assert_eq!(project_of(&itm, &workspace("w1", "api", Some(&other))), None);
        assert_eq!(project_of(&itm, &workspace("w1", "API", Some(&folder(&itm, "elsewhere")))), Some(api));
    }

    /// R10.1: a project without a folder is found only by a workspace named after it.
    #[test]
    fn r10_1_a_name_only_project_is_found_by_workspace_name_only() {
        let (_tmp, itm) = itm();
        let id = itm.add_project(ProjectKind::Name, "Client X", None).unwrap();
        let home = itm.paths.home.clone();
        assert_eq!(project_of(&itm, &workspace("w1", "Client X", Some(&home))), Some(id));
        assert_eq!(project_of(&itm, &workspace("w1", "home", Some(&home))), None);
    }

    /// git in `repo`, without the user's settings.
    fn git(repo: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main", "-C"])
            .arg(repo)
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    fn checkout(itm: &Itm, rel: &str, origin: Option<&str>) -> PathBuf {
        let path = folder(itm, rel);
        git(&path, &["init", "-q"]);
        git(&path, &["commit", "-q", "--allow-empty", "-m", "init"]);
        if let Some(origin) = origin {
            git(&path, &["remote", "add", "origin", origin]);
        }
        path
    }

    /// R10.2: a linked worktree (Herdr's live outside the project) is in the
    /// project of the checkout it was made from.
    #[test]
    fn r10_2_a_linked_worktree_is_in_the_project_of_its_checkout() {
        let (_tmp, itm) = itm();
        let api = checkout(&itm, "code/api", None);
        let id = itm.add_project(ProjectKind::Dir, &api.to_string_lossy(), Some("api")).unwrap();
        let tree = itm.paths.home.join("worktrees/api-feature");
        git(&api, &["worktree", "add", "-q", &tree.to_string_lossy(), "-b", "feature"]);
        assert_eq!(folder_project(&project_roots(&projects(&itm)), &tree), None, "by folder: in none");
        assert_eq!(project_of(&itm, &unnamed(&tree)), Some(id));
        assert_eq!(itm.repository(&tree), Some(Repository { root: Some(api), origin: None }));
    }

    /// R10.2, R8.4: another checkout is in the one project whose checkout has
    /// the same origin, in any of its forms; with two such projects, in neither.
    #[test]
    fn r10_2_another_checkout_is_in_the_project_with_its_remote() {
        let (_tmp, itm) = itm();
        let clone = checkout(&itm, "clones/api", Some("git@github.com:acme/api.git"));
        itm.store()
            .edit(|data| {
                let path = Some(clone.display().to_string());
                let url = Some("git@github.com:acme/api.git".to_string());
                let project =
                    Project { name: "api".into(), path, url, added_at: 1, last_used_at: 1, extra: <_>::default() };
                Ok(data.projects.insert("git1".into(), project))
            })
            .unwrap();
        let mine = checkout(&itm, "mine/api", Some("https://github.com/acme/api"));
        fs::create_dir(mine.join("src")).unwrap();
        assert_eq!(project_of(&itm, &unnamed(&mine.join("src"))), Some("git1".into()));
        let other = checkout(&itm, "mine/web", Some("https://github.com/acme/web"));
        assert_eq!(project_of(&itm, &unnamed(&other)), None);
        assert_eq!(project_of(&itm, &unnamed(&folder(&itm, "plain"))), None, "not a repository");
        let work = checkout(&itm, "work/api", Some("git@github.com:acme/api"));
        itm.add_project(ProjectKind::Dir, &work.to_string_lossy(), Some("work-api")).unwrap();
        assert_eq!(project_of(&itm, &unnamed(&mine)), None, "two projects with that remote: in neither");
    }

    /// R10.2: a folder's repository is asked once per 5 minutes; without git there is none.
    #[test]
    fn r10_2_repository_lookups_are_remembered() {
        let (_tmp, mut itm) = itm();
        let api = checkout(&itm, "code/api", Some("git@github.com:acme/api.git"));
        let found = Repository { root: Some(api.clone()), origin: Some("github.com/acme/api".into()) };
        assert_eq!(itm.repository(&api), Some(found.clone()));
        git(&api, &["remote", "set-url", "origin", "https://github.com/acme/other"]);
        assert_eq!(itm.repository(&api), Some(found), "remembered, not asked again");
        assert_eq!(itm.repository(&folder(&itm, "plain")), None);
        itm.git = crate::git::Git::with(&["/nowhere/git"]);
        assert_eq!(itm.repository(&folder(&itm, "no-git-here")), None, "git missing: no repository");
    }

    // -------------------------------------------------------------- add-workspace (R10.5)

    /// The task "Here" with its session running and two workspaces, the
    /// second focused: w1 ("other", above the home folder) and w2 ("My
    /// Thing", in ~/code/new-thing). Herdr's answers follow `found`; the
    /// workspace renames are recorded.
    struct Here {
        _tmp: tempfile::TempDir,
        itm: Itm,
        code: PathBuf,
        found: Arc<Mutex<Vec<Workspace>>>,
        calls: Arc<Mutex<Vec<(String, Value)>>>,
    }

    fn here() -> Here {
        let (tmp, mut itm) = itm();
        let code = folder(&itm, "code/new-thing");
        itm.create_task("Here", None, &[], None).unwrap();
        let root = tmp.path().to_path_buf();
        let found = Arc::new(Mutex::new(vec![
            workspace("w1", "other", Some(&root)),
            Workspace { focused: true, ..workspace("w2", "My Thing", Some(&code)) },
        ]));
        let shown = Arc::clone(&found);
        let calls = fake::serve(&itm.paths.herdr_config, "task-here", move |method, _| match method {
            "session.snapshot" => json!({"snapshot": snapshot_of(&shown.lock().unwrap())}),
            _ => json!({}),
        });
        itm.client_socket = Some(itm.herdr.socket("task-here"));
        Here { _tmp: tmp, itm, code, found, calls }
    }

    /// A snapshot with one tab and one pane per workspace.
    fn snapshot_of(workspaces: &[Workspace]) -> Value {
        let ws = workspaces.iter().map(|w| json!({"workspace_id": w.id, "label": w.label, "focused": w.focused}));
        let tabs = workspaces.iter().map(|w| json!({"tab_id": format!("{}:t1", w.id), "workspace_id": w.id}));
        let layouts = (workspaces.iter())
            .map(|w| json!({"tab_id": format!("{}:t1", w.id), "panes": [{"pane_id": format!("{}:p1", w.id)}]}));
        let panes = workspaces.iter().map(|w| json!({"pane_id": format!("{}:p1", w.id), "cwd": w.folder}));
        json!({"workspaces": ws.collect::<Vec<_>>(), "tabs": tabs.collect::<Vec<_>>(),
               "layouts": layouts.collect::<Vec<_>>(), "panes": panes.collect::<Vec<_>>(), "agents": []})
    }

    impl Here {
        fn renames(&self) -> Vec<Value> {
            let calls = self.calls.lock().unwrap();
            calls.iter().filter(|(m, _)| m == "workspace.rename").map(|(_, p)| p.clone()).collect()
        }

        fn task(&self) -> crate::store::Task {
            self.itm.store().load().unwrap().tasks["task-here"].clone()
        }

        fn project(&self, name: &str) -> (String, Project) {
            let id = id_of(&self.itm, name);
            (id.clone(), projects(&self.itm)[&id].clone())
        }
    }

    fn renamed(id: &str, label: &str) -> Value {
        json!({"workspace_id": id, "label": label})
    }

    /// Canned answers to the questions, recording them.
    #[derive(Default)]
    struct Answers {
        dialog: Option<String>,
        give: Option<bool>,
        asked: RefCell<Vec<String>>,
        meanwhile: Option<Box<dyn Fn()>>,
    }

    impl Ask for Answers {
        fn add_folder(&self, folder: &Path, name: &str) -> Result<Option<String>> {
            self.asked.borrow_mut().push(format!("add {} {name}", folder.display()));
            Ok(self.dialog.clone())
        }

        fn give_folder(&self, project: &str, folder: &str) -> Result<Option<bool>> {
            self.asked.borrow_mut().push(format!("give {project} {folder}"));
            if let Some(meanwhile) = &self.meanwhile {
                meanwhile();
            }
            Ok(self.give)
        }
    }

    /// R10.5: without a terminal (no dialog), the focused workspace is saved
    /// under its name, renamed after it and given to the task as found in it.
    #[test]
    fn r10_5_saves_the_focused_workspace_named_like_it_and_gives_it_to_the_task() {
        let here = here();
        let added = here.itm.add_workspace(None, None, None).unwrap();
        let (id, project) = here.project("My Thing");
        assert_eq!((added.project.name.as_str(), project.path), ("My Thing", Some(here.code.display().to_string())));
        assert_eq!(added.said, ["added to 'Here'"]);
        let task = here.task();
        assert_eq!(
            (task.projects, task.found_projects),
            (vec![id.clone()], vec![id]),
            "it goes when its workspace closes"
        );
        assert_eq!(here.renames(), [renamed("w2", "My Thing")], "no new workspace");
    }

    /// R10.5: with "projects follow workspaces" off, it is added by hand.
    #[test]
    fn r10_5_without_following_workspaces_it_is_added_by_hand() {
        let here = here();
        here.itm.change_setting(crate::config::Setting::ProjectsFromWorkspaces, false).unwrap();
        here.itm.add_workspace(Some("mine"), None, None).unwrap();
        let task = here.task();
        assert_eq!((task.projects, task.found_projects), (vec![here.project("mine").0], vec![]));
    }

    /// R10.5: a popup's workspace is the one it was opened over; otherwise
    /// the focused one, whatever a pane's own variable says.
    #[test]
    fn r10_5_a_popup_names_the_workspace_it_was_opened_over() {
        let here = here();
        let elsewhere = folder(&here.itm, "elsewhere");
        here.found.lock().unwrap()[0].folder = Some(elsewhere.clone());
        here.itm.add_workspace(Some("popup"), Some("w1"), None).unwrap();
        assert_eq!(here.project("popup").1.path, Some(elsewhere.display().to_string()));
        here.itm.add_workspace(Some("chosen"), Some(""), None).unwrap();
        assert_eq!(here.project("chosen").1.path, Some(here.code.display().to_string()));
        assert_eq!(here.renames(), [renamed("w1", "popup"), renamed("w2", "chosen")]);
    }

    /// R10.5: the home folder, a folder above it and the task folder are refused.
    #[test]
    fn r10_5_home_and_folders_above_it_are_refused() {
        let here = here();
        let home = here.itm.paths.home.clone();
        for refused in [home.clone(), home.parent().unwrap().to_path_buf(), PathBuf::from("/")] {
            here.found.lock().unwrap()[1].folder = Some(refused.clone());
            let err = here.itm.add_workspace(Some("everything"), None, None).unwrap_err().to_string();
            assert!(err.contains("holds every workspace under it, so it is not added from a workspace"), "{err}");
        }
        here.found.lock().unwrap()[1].folder = Some(home);
        let err = here.itm.add_workspace(Some("everything"), None, None).unwrap_err().to_string();
        assert_eq!(
            err,
            "~ holds every workspace under it, so it is not added from a workspace; open the workspace in a project's folder, or use `itm project add dir ~`"
        );
        assert!(projects(&here.itm).is_empty());
    }

    /// R10.5: a folder inside a saved project is that project; none is added.
    #[test]
    fn r10_5_a_saved_folder_is_not_added_twice() {
        let here = here();
        let id = add_dir(&here.itm, "code", "code");
        let added = here.itm.add_workspace(None, None, None).unwrap();
        assert_eq!(added.project.name, "code");
        assert_eq!(projects(&here.itm).len(), 1);
        assert_eq!(here.task().projects, [id]);
        assert_eq!(here.renames(), [renamed("w2", "code")]);
    }

    /// R10.5: in a terminal, the add dialog gets the folder and the workspace's name; Esc saves nothing.
    #[test]
    fn r10_5_the_dialog_gets_the_folder_and_name() {
        let here = here();
        let answers = Answers::default();
        assert_eq!(here.itm.add_workspace(None, None, Some(&answers)).unwrap_err(), Error::Cancelled);
        assert_eq!(*answers.asked.borrow(), [format!("add {} My Thing", here.code.display())]);
        assert!(projects(&here.itm).is_empty() && here.renames().is_empty());
        // What the dialog saved is the project.
        let id = add_dir(&here.itm, "elsewhere", "saved");
        let answers = Answers { dialog: Some(id.clone()), ..Answers::default() };
        assert_eq!(here.itm.add_workspace(None, None, Some(&answers)).unwrap().project.name, "saved");
        assert_eq!(here.task().projects, [id]);
        // A name given means no dialog.
        let answers = Answers::default();
        here.itm.add_workspace(Some("named"), None, Some(&answers)).unwrap();
        assert!(answers.asked.borrow().is_empty());
    }

    /// R10.5: outside Herdr it says where to run it; a session that is not
    /// a task still gets its project saved.
    #[test]
    fn r10_5_outside_herdr_or_a_task() {
        let mut here = here();
        here.itm.client_socket = None;
        let err = here.itm.add_workspace(Some("x"), None, None).unwrap_err().to_string();
        assert_eq!(err, "run this inside Herdr, from the workspace to add (prefix+shift+a)");
        here.itm.client_socket = Some(here.itm.herdr.socket("task-gone"));
        assert!(here.itm.add_workspace(Some("x"), None, None).is_err());

        let shown = Arc::clone(&here.found);
        fake::serve(&here.itm.paths.herdr_config, "work", move |method, _| match method {
            "session.snapshot" => json!({"snapshot": snapshot_of(&shown.lock().unwrap())}),
            _ => json!({}),
        });
        here.itm.client_socket = Some(here.itm.herdr.socket("work"));
        let added = here.itm.add_workspace(Some("x"), None, None).unwrap();
        assert_eq!(added.said, ["work is not a task, so no task got the project"]);
        assert_eq!(here.project("x").1.path, Some(here.code.display().to_string()));
    }

    /// R10.5: a workspace whose folder Herdr does not report cannot be added.
    #[test]
    fn r10_5_a_workspace_without_a_known_folder() {
        let here = here();
        here.found.lock().unwrap()[1].folder = None;
        let err = here.itm.add_workspace(Some("x"), None, None).unwrap_err().to_string();
        assert_eq!(err, "cannot tell which workspace this is");
    }

    /// R10.5, R10.2: a linked worktree's workspace is its checkout's project, not a new one.
    #[test]
    fn r10_5_a_worktree_is_its_checkouts_project() {
        let here = here();
        let api = checkout(&here.itm, "code/api", None);
        let id = here.itm.add_project(ProjectKind::Dir, &api.to_string_lossy(), Some("api")).unwrap();
        let tree = here.itm.paths.home.join("worktrees/fix");
        git(&api, &["worktree", "add", "-q", &tree.to_string_lossy(), "-b", "fix"]);
        here.found.lock().unwrap()[1] = Workspace { focused: true, ..workspace("w2", "fix", Some(&tree)) };
        here.itm.add_workspace(None, None, None).unwrap();
        assert_eq!(projects(&here.itm).keys().collect::<Vec<_>>(), [&id]);
        assert_eq!(here.renames(), [renamed("w2", "api")]);
    }

    // -------------------------------------------------------------- a project without a folder gets one

    /// R10.5: a project without a folder that the workspace is named after
    /// is asked about, and given the folder on a yes.
    #[test]
    fn r10_5_a_workspace_named_after_a_project_without_a_folder() {
        let here = here();
        let id = here.itm.add_project(ProjectKind::Name, "My Thing", None).unwrap();
        let answers = Answers { give: Some(true), ..Answers::default() };
        let added = here.itm.add_workspace(None, None, Some(&answers)).unwrap();
        assert_eq!(*answers.asked.borrow(), ["give My Thing ~/code/new-thing"]);
        assert_eq!(added.project.path, Some(here.code.display().to_string()));
        assert_eq!(added.said, ["My Thing is now in ~/code/new-thing", "added to 'Here'"]);
        assert_eq!(here.task().projects, [id]);
        assert_eq!(here.renames(), [renamed("w2", "My Thing")]);
    }

    /// R10.5: an unnamed workspace named like it (its folder's name) finds it too, not a second project.
    #[test]
    fn r10_5_an_unnamed_workspace_named_like_it() {
        let here = here();
        let id = here.itm.add_project(ProjectKind::Name, "New-Thing", None).unwrap();
        here.found.lock().unwrap()[1].label = "new-thing".into(); // Herdr's own label
        here.itm.add_workspace(None, None, Some(&Answers { give: Some(true), ..Answers::default() })).unwrap();
        assert_eq!(projects(&here.itm)[&id].path, Some(here.code.display().to_string()));
        assert_eq!(projects(&here.itm).len(), 1);
    }

    /// R10.5: n keeps it without a folder, still given to the task; Esc changes nothing at all.
    #[test]
    fn r10_5_declined_or_cancelled() {
        let here = here();
        let id = here.itm.add_project(ProjectKind::Name, "My Thing", None).unwrap();
        let before = fs::read_to_string(here.itm.paths.tasks_file()).unwrap();
        let esc = Answers::default();
        assert_eq!(here.itm.add_workspace(None, None, Some(&esc)).unwrap_err(), Error::Cancelled);
        assert_eq!(fs::read_to_string(here.itm.paths.tasks_file()).unwrap(), before);
        assert!(here.renames().is_empty(), "the workspace is not renamed either");

        let added =
            here.itm.add_workspace(None, None, Some(&Answers { give: Some(false), ..Answers::default() })).unwrap();
        assert_eq!(added.said, ["My Thing kept without a folder", "added to 'Here'"]);
        assert_eq!(projects(&here.itm)[&id].path, None);
        assert_eq!(here.task().projects, [id]);
        assert_eq!(here.renames(), [renamed("w2", "My Thing")]);
    }

    /// R10.5: without a terminal, or with a name given, nothing asks; the result says how.
    #[test]
    fn r10_5_without_a_question_it_says_how() {
        let here = here();
        let id = here.itm.add_project(ProjectKind::Name, "My Thing", None).unwrap();
        let added = here.itm.add_workspace(None, None, None).unwrap();
        let hint = format!(
            "My Thing kept without a folder; `itm project set 'My Thing' --dir {}` gives it this folder",
            here.code.display()
        );
        assert_eq!(added.said, [hint.clone(), "added to 'Here'".into()]);
        assert_eq!(added.warnings().collect::<Vec<_>>(), [&hint]);
        let answers = Answers { give: Some(true), ..Answers::default() };
        let added = here.itm.add_workspace(Some("Other"), None, Some(&answers)).unwrap();
        assert!(answers.asked.borrow().is_empty(), "a name given: no question");
        assert!(added.said[0].starts_with("My Thing kept without a folder; `itm project set"), "{:?}", added.said);
        assert_eq!(projects(&here.itm)[&id].path, None);
    }

    /// R10.5: a project removed while the question is open ends with an error, not a crash.
    #[test]
    fn r10_5_removed_while_asking() {
        let here = Arc::new(here());
        let id = here.itm.add_project(ProjectKind::Name, "My Thing", None).unwrap();
        let (gone, remover) = (id.clone(), Arc::clone(&here));
        let meanwhile = move || drop(remover.itm.remove_project(&gone, crate::projects::CloneChoice::Keep).unwrap());
        let answers = Answers { give: Some(true), meanwhile: Some(Box::new(meanwhile)), ..Answers::default() };
        let err = here.itm.add_workspace(None, None, Some(&answers)).unwrap_err();
        assert_eq!(err.to_string(), "the project no longer exists");
    }

    /// R10.5: a folder it must not get is not asked about; the project is
    /// still given to the task, and what went wrong is a warning.
    #[test]
    fn r10_5_folders_a_project_must_not_get() {
        let here = here();
        let id = here.itm.add_project(ProjectKind::Name, "My Thing", None).unwrap();
        here.found.lock().unwrap()[1].folder = Some(here.itm.paths.home.clone()); // where tasks start
        let answers = Answers { give: Some(true), ..Answers::default() };
        let added = here.itm.add_workspace(None, None, Some(&answers)).unwrap();
        assert!(answers.asked.borrow().is_empty(), "not asked");
        assert!(added.said[0].starts_with("My Thing kept without a folder: ~ holds every workspace under it"));
        assert_eq!(added.warnings().count(), 1);
        assert_eq!(here.task().projects, std::slice::from_ref(&id));
        assert_eq!(projects(&here.itm)[&id].path, None);

        let api = add_dir(&here.itm, "code/api", "api");
        let err = here.itm.give_folder(&id, &folder(&here.itm, "code/api/src")).unwrap_err().to_string();
        assert_eq!(err, "~/code/api/src is in project api");
        folder(&here.itm, "elsewhere");
        here.itm.set_project_folder("My Thing", Some("~/elsewhere")).unwrap();
        let err = here.itm.give_folder(&id, &here.code).unwrap_err().to_string();
        assert_eq!(err, "My Thing has a folder already", "given one meanwhile: not replaced");
        assert!(projects(&here.itm).contains_key(&api));
    }
}
