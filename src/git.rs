//! git, for repository projects (R8): one form for every way of writing a
//! repository's URL (R8.4), whether a folder is a checkout of a repository,
//! the work only a clone has (R8.7), and the clone itself, which a signal
//! stops without leaving anything behind (DESIGN.md section 2, Projects).

use std::ffi::OsString;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::LazyLock;
use std::thread;
use std::time::Duration;

use regex_lite::Regex;

use crate::error::{Error, Result};
use crate::system::{ExitSignals, output_within, stop_child};

/// How long git may take to stop its helpers after SIGTERM.
const STOP_PATIENCE: Duration = Duration::from_secs(10);
/// How long git may take to say which repository a folder is in.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// The `git` program. Tests run it with settings (`-c …`) or run a fake.
pub struct Git {
    /// The program and its first arguments.
    command: Vec<OsString>,
}

impl Default for Git {
    fn default() -> Git {
        Git { command: vec!["git".into()] }
    }
}

impl Git {
    #[cfg(test)]
    pub fn with(command: &[&str]) -> Git {
        Git { command: command.iter().map(OsString::from).collect() }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.command[0]);
        command.args(&self.command[1..]);
        command
    }

    /// git's output for `args`, stdin closed. A missing git is an error of its own.
    fn output(&self, args: &[&std::ffi::OsStr]) -> Result<Output> {
        self.command().args(args).stdin(Stdio::null()).output().map_err(missing_git)
    }

    /// [`normalize_git_url`] after git's own rewriting (`url.<base>.insteadOf`),
    /// so a URL written with a shortcut is the same repository. Without git,
    /// nothing is rewritten.
    pub fn repo_key(&self, url: &str) -> Result<String> {
        let key = normalize_git_url(url)?; // refuse anything else before git sees it
        let asked =
            self.command().args(["ls-remote", "--get-url", url.trim()]).current_dir("/").stdin(Stdio::null()).output();
        let expanded = match asked {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
            _ => String::new(),
        };
        if expanded.is_empty() { Ok(key) } else { normalize_git_url(&expanded) }
    }

    /// Whether `path` is a checkout of `url`'s repository (by its `origin`),
    /// and not a link to one.
    pub fn is_checkout_of(&self, path: &Path, url: &str) -> Result<bool> {
        if path.is_symlink() || !path.join(".git").exists() {
            return Ok(false);
        }
        let out =
            self.output(&["-C".as_ref(), path.as_os_str(), "remote".as_ref(), "get-url".as_ref(), "origin".as_ref()])?;
        if !out.status.success() {
            return Ok(false);
        }
        let origin = normalize_git_url(&String::from_utf8_lossy(&out.stdout));
        Ok(matches!((origin, self.repo_key(url)), (Ok(origin), Ok(key)) if origin == key))
    }

    /// What exists only in this checkout, if anything (R8.7): uncommitted or
    /// untracked files (whatever `status.showUntrackedFiles` says), commits on
    /// no remote (branches, tags, a detached HEAD), stashes.
    pub fn local_work(&self, path: &Path) -> Result<Option<String>> {
        let checks: [(&str, &[&str]); 3] = [
            ("uncommitted changes", &["status", "--porcelain", "--untracked-files=normal"]),
            (
                "unpushed commits",
                &["rev-list", "--ignore-missing", "HEAD", "--branches", "--tags", "--not", "--remotes", "-n", "1"],
            ),
            ("stashes", &["stash", "list"]),
        ];
        let mut found = vec![];
        for (label, args) in checks {
            let mut argv: Vec<&std::ffi::OsStr> = vec!["-C".as_ref(), path.as_os_str()];
            argv.extend(args.iter().map(std::ffi::OsStr::new));
            let out = self.output(&argv)?;
            if !out.status.success() {
                found.push(format!("{label} (git could not tell)"));
            } else if !String::from_utf8_lossy(&out.stdout).trim().is_empty() {
                found.push(label.to_string());
            }
        }
        Ok(Some(found.join(", ")).filter(|f| !f.is_empty()))
    }

    /// `git clone --progress -- <url> <dest>`, its progress on this process's
    /// output (`--progress`: git says nothing when that is a pipe, as for a
    /// TUI's child). When one of `stop` arrives first, git gets SIGTERM (so
    /// it stops its helpers too) and SIGKILL if it is still there 10 s later,
    /// and the result is the exit status that signal means (128 + its number).
    pub fn clone(&self, url: &str, dest: &Path, stop: &mut ExitSignals) -> Result<()> {
        let mut git = self.command().args(["clone", "--progress", "--", url]).arg(dest).spawn().map_err(missing_git)?;
        loop {
            if let Some(status) = git.try_wait().map_err(|e| Error::Other(format!("cannot wait for git: {e}")))? {
                return if status.success() { Ok(()) } else { Err(Error::Other(format!("could not clone {url}"))) };
            }
            if let Some(signal) = stop.arrived() {
                let _ = stop_child(&mut git, STOP_PATIENCE);
                return Err(Error::Exit(128 + signal as u8));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

/// The git repository a folder is in, as workspace matching needs it (R10.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    /// Its main work tree: for a linked worktree, the checkout it was made
    /// from. None for a bare repository.
    pub root: Option<PathBuf>,
    /// Its `origin` remote, normalized (R8.4); None without one.
    pub origin: Option<String>,
}

impl Git {
    /// The repository `folder` is in; None outside one, or without git.
    /// Asked without the network (`ls-remote --get-url` only applies
    /// `url.<base>.insteadOf`), stdin closed, each question within 5 s.
    pub fn repository(&self, folder: &Path) -> Option<Repository> {
        let ask = |args: &[&str]| {
            let mut git = self.command();
            git.arg("-C").arg(folder).args(args);
            match output_within(&mut git, LOOKUP_TIMEOUT) {
                Ok(Some((status, out))) if status.success() => Some(out.trim().to_string()),
                _ => None,
            }
        };
        let common = PathBuf::from(ask(&["rev-parse", "--path-format=absolute", "--git-common-dir"])?);
        // A bare repository's common folder is the repository itself: it has no work tree.
        let root =
            (common.file_name() == Some(".git".as_ref())).then(|| common.parent().map(Path::to_path_buf)).flatten();
        // Without such a remote, git prints the name back.
        let url = ask(&["ls-remote", "--get-url", "origin"]).filter(|url| !url.is_empty() && url != "origin");
        Some(Repository { root, origin: url.and_then(|url| normalize_git_url(&url).ok()) })
    }
}

fn missing_git(e: io::Error) -> Error {
    match e.kind() {
        ErrorKind::NotFound => Error::Missing("git"),
        _ => Error::Other(format!("cannot run git: {e}")),
    }
}

/// `host/owner/repo` for a git URL: the same for every form of one
/// repository (R8.4), whatever the host. An error for anything that is not
/// a remote repository's URL (paths, `file://`, options, git's
/// `<transport>::<address>`).
pub fn normalize_git_url(url: &str) -> Result<String> {
    static SCHEME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?i)[a-z][a-z0-9+.-]*://").unwrap());
    static SCP_LIKE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:[^@/:]+@)?([^@/:]+):(.+)$").unwrap());
    let bad = || Error::Other(format!("not a git URL: {url}"));
    let text = url.trim();
    if text.starts_with('-') || text.contains("::") {
        return Err(bad());
    }
    let (host, path) = if SCHEME.is_match(text) {
        split_url(text).ok_or_else(bad)?
    } else {
        let parts = SCP_LIKE.captures(text).ok_or_else(bad)?;
        (parts[1].to_string(), parts[2].to_string())
    };
    let mut segments: Vec<String> = path.split('/').filter(|s| !s.is_empty()).map(unquote).collect();
    if let Some(last) = segments.last_mut()
        && let Some(repo) = last.strip_suffix(".git")
    {
        *last = repo.to_string();
    }
    let host = host.to_lowercase();
    let (host, segments) = if segments.is_empty() { (host, segments) } else { azure_repo(host, segments) };
    let valid_host = |h: &str| {
        let mut chars = h.chars();
        chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.-".contains(c))
    };
    if !valid_host(&host) || segments.is_empty() || !segments.iter().all(|s| valid_segment(s)) {
        return Err(bad());
    }
    Ok(format!("{host}/{}", segments.join("/")))
}

/// (host, path) of a URL with a scheme, as Python's `urlsplit` reads them:
/// the host is the network location without user and port; the path stops
/// at a query or fragment. None for a location with IPv6 brackets (Python
/// refuses them or finds a host no git URL here has).
fn split_url(text: &str) -> Option<(String, String)> {
    let text: String = text.chars().filter(|c| !"\t\r\n".contains(*c)).collect();
    let rest = &text[text.find("://")? + 3..];
    let (netloc, after) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    let path = after.split(['?', '#']).next().unwrap_or("");
    if netloc.contains(['[', ']']) {
        return None;
    }
    let hostinfo = netloc.rsplit_once('@').map_or(netloc, |(_, host)| host);
    let host = hostinfo.split(':').next().unwrap_or("");
    Some((host.to_string(), path.to_string()))
}

/// `%XX` escapes decoded, the result read as UTF-8 (a bad sequence becomes
/// U+FFFD), like Python's `urllib.parse.unquote`.
fn unquote(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (bytes[i], bytes.get(i + 1).copied().and_then(hex), bytes.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(high), Some(low)) => {
                out.push((high * 16 + low) as u8);
                i += 3;
            }
            (byte, ..) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A path segment a repository URL may have: not empty, `.`, `..` or
/// `.git`, and without a `/` or control character (once unescaped).
fn valid_segment(segment: &str) -> bool {
    !["", ".", ".."].contains(&segment)
        && segment.to_lowercase() != ".git"
        && !segment.contains('/')
        && !segment.chars().any(char::is_control)
}

/// Azure DevOps names one repository with several URL shapes (ssh `v3/`,
/// https `_git/`, legacy `<org>.visualstudio.com`): all of them become
/// `dev.azure.com/<org>/<project>/<repo>`, the organization in lower case.
fn azure_repo(host: String, segments: Vec<String>) -> (String, Vec<String>) {
    let (host, mut segments) =
        if ["ssh.dev.azure.com", "vs-ssh.visualstudio.com"].contains(&host.as_str()) && segments[0] == "v3" {
            ("dev.azure.com".to_string(), segments[1..].to_vec())
        } else if let Some(organization) = host.strip_suffix(".visualstudio.com") {
            let rest = if segments[0].to_lowercase() == "defaultcollection" { &segments[1..] } else { &segments[..] };
            ("dev.azure.com".to_string(), [vec![organization.to_string()], rest.to_vec()].concat())
        } else {
            (host, segments)
        };
    if host != "dev.azure.com" || segments.is_empty() {
        return (host, segments);
    }
    let inner = segments.len().saturating_sub(1);
    if segments.get(1..inner).is_some_and(|middle| middle.iter().any(|s| s == "_git")) {
        let at = segments.iter().position(|s| s == "_git").expect("found above");
        // org/_git/repo: the project is named like the repository.
        let project = if at > 1 { segments[1..at].to_vec() } else { segments[at + 1..].to_vec() };
        segments = [vec![segments[0].clone()], project, segments[at + 1..].to_vec()].concat();
    }
    segments[0] = segments[0].to_lowercase();
    (host, segments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn forms_of_one_repository_are_equal() {
        for url in [
            "git@github.com:acme/api.git",
            "github.com:acme/api",
            "https://github.com/acme/api",
            "https://github.com/acme/api.git/",
            "ssh://git@GitHub.com:22/acme/api.git",
            "  https://user@github.com/acme/api  ",
            "https://github.com/acme/api?tab=readme#top",
        ] {
            assert_eq!(normalize_git_url(url).unwrap(), "github.com/acme/api", "{url}");
        }
    }

    #[test]
    fn gitlab_subgroups_and_self_hosted() {
        for url in [
            "git@gitlab.com:group/sub/app.git",
            "https://gitlab.com/group/sub/app",
            "ssh://git@gitlab.com:2222/group/sub/app",
        ] {
            assert_eq!(normalize_git_url(url).unwrap(), "gitlab.com/group/sub/app", "{url}");
        }
        assert_eq!(normalize_git_url("git@git.example.org:team/tool.git").unwrap(), "git.example.org/team/tool");
    }

    /// R8.4: Azure DevOps ssh (`v3/`), https (`_git/`) and legacy URLs are one repository.
    #[test]
    fn azure_devops_forms_are_one_repository() {
        for url in [
            "https://dev.azure.com/acme/Platform/_git/api",
            "https://acme@dev.azure.com/acme/Platform/_git/api",
            "git@ssh.dev.azure.com:v3/acme/Platform/api",
            "ssh://git@ssh.dev.azure.com/v3/acme/Platform/api",
            "https://acme.visualstudio.com/Platform/_git/api",
            "https://acme.visualstudio.com/DefaultCollection/Platform/_git/api",
            "acme@vs-ssh.visualstudio.com:v3/acme/Platform/api",
        ] {
            assert_eq!(normalize_git_url(url).unwrap(), "dev.azure.com/acme/Platform/api", "{url}");
        }
    }

    #[test]
    fn azure_devops_organization_ignores_case() {
        assert_eq!(
            normalize_git_url("https://Contoso.visualstudio.com/Platform/_git/api").unwrap(),
            normalize_git_url("https://Contoso@dev.azure.com/Contoso/Platform/_git/api").unwrap()
        );
    }

    #[test]
    fn azure_devops_short_form_and_escapes() {
        assert_eq!(
            normalize_git_url("https://dev.azure.com/acme/_git/api").unwrap(),
            normalize_git_url("git@ssh.dev.azure.com:v3/acme/api/api").unwrap()
        );
        for url in
            ["https://dev.azure.com/acme/My%20Project/_git/api", "git@ssh.dev.azure.com:v3/acme/My%20Project/api"]
        {
            assert_eq!(normalize_git_url(url).unwrap(), "dev.azure.com/acme/My Project/api", "{url}");
        }
    }

    /// R8.4: forms deliberately not recognized as the same repository get their own clone.
    #[test]
    fn host_aliases_and_bitbucket_server_forms_stay_apart() {
        let real = normalize_git_url("git@github.com:acme/api.git").unwrap();
        assert_ne!(normalize_git_url("git@my-alias:acme/api.git").unwrap(), real);
        assert_ne!(
            normalize_git_url("https://bitbucket.example.com/scm/team/api.git").unwrap(),
            normalize_git_url("ssh://git@bitbucket.example.com:7999/team/api.git").unwrap()
        );
    }

    #[test]
    fn what_is_not_a_git_url() {
        for url in [
            "",
            "api",
            "/home/me/repo",
            "./repo",
            "https://github.com/",
            "git@github.com:../x",
            "file:///tmp/repo",
            "x:.git",
            "..:foo/bar",
            "https://../foo",
            "-c:foo/bar",
            "https://host/a/%2e%2e/b",
            "https://host/a%2Fb/c",
            "https://host/a/b%00",
            "git@ssh.dev.azure.com:v3",
            "git@host:a/.git/hooks",
            "ext::sh -c touch% /tmp/x",
            "fd::3",
            "https://[::1]/a/b",
        ] {
            let err = normalize_git_url(url).unwrap_err();
            assert_eq!(err.to_string(), format!("not a git URL: {url}"));
        }
    }

    #[test]
    fn escapes_are_read_as_utf8() {
        assert_eq!(unquote("caf%C3%A9"), "café");
        assert_eq!(unquote("100%"), "100%");
        assert_eq!(unquote("%zz%4"), "%zz%4");
        assert_eq!(unquote("%ff"), "\u{fffd}");
    }

    /// R8.4: URLs compare after git's `insteadOf` rewriting.
    #[test]
    fn repo_key_follows_insteadof() {
        let git = Git::with(&["git", "-c", "url.git@github.com:.insteadOf=gh:"]);
        assert_eq!(git.repo_key("gh:acme/api").unwrap(), "github.com/acme/api");
        let missing = Git::with(&["/nowhere/git"]);
        assert_eq!(missing.repo_key("git@github.com:acme/api").unwrap(), "github.com/acme/api", "no git: as written");
        assert!(missing.repo_key("/a/path").is_err());
        assert_eq!(missing.local_work(Path::new("/")).unwrap_err(), Error::Missing("git"));
    }

    /// git in `repo`, without the user's settings (signing, hooks).
    fn git_in(repo: &Path, args: &[&str]) {
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

    /// R8.7: what only a clone has is local work, whatever git's settings hide.
    #[test]
    fn local_work() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let git = Git::default();
        git_in(&repo, &["init", "-q"]);
        assert_eq!(git.local_work(&repo).unwrap(), None, "an empty clone");
        fs::write(repo.join("notes.txt"), "draft").unwrap();
        assert_eq!(git.local_work(&repo).unwrap().as_deref(), Some("uncommitted changes"));
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "-qm", "draft"]);
        assert_eq!(git.local_work(&repo).unwrap().as_deref(), Some("unpushed commits"));
        git_in(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]); // as if pushed
        assert_eq!(git.local_work(&repo).unwrap(), None);
        git_in(&repo, &["config", "status.showUntrackedFiles", "no"]);
        fs::write(repo.join("new.txt"), "new").unwrap();
        assert_eq!(git.local_work(&repo).unwrap().as_deref(), Some("uncommitted changes"));
        fs::remove_file(repo.join("new.txt")).unwrap();
        git_in(&repo, &["checkout", "-q", "--detach"]);
        git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "detached"]);
        assert_eq!(git.local_work(&repo).unwrap().as_deref(), Some("unpushed commits"));
        git_in(&repo, &["tag", "only-here"]);
        git_in(&repo, &["checkout", "-q", "-"]);
        assert_eq!(git.local_work(&repo).unwrap().as_deref(), Some("unpushed commits"), "reachable from the tag only");
        let none = tmp.path().join("not-a-repo");
        fs::create_dir(&none).unwrap();
        assert!(git.local_work(&none).unwrap().unwrap().contains("git could not tell"));
    }

    #[test]
    fn a_checkout_is_known_by_its_origin_and_never_through_a_link() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("api");
        fs::create_dir(&repo).unwrap();
        let git = Git::default();
        git_in(&repo, &["init", "-q"]);
        assert!(!git.is_checkout_of(&repo, "https://github.com/acme/api").unwrap(), "no origin");
        git_in(&repo, &["remote", "add", "origin", "git@github.com:acme/api.git"]);
        assert!(git.is_checkout_of(&repo, "https://github.com/acme/api").unwrap());
        assert!(!git.is_checkout_of(&repo, "https://github.com/other/api").unwrap());
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&repo, &link).unwrap();
        assert!(!git.is_checkout_of(&link, "https://github.com/acme/api").unwrap());
        assert!(!git.is_checkout_of(tmp.path(), "https://github.com/acme/api").unwrap(), "not a checkout");
    }
}
