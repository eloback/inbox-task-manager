//! Folder completion for the project finder and the add dialog (R1.6): a
//! folder being typed (`~/co`) completes from a listing of its parent,
//! spelled as typed (`~` kept), fuzzy matched. Reading a folder can block
//! on a slow filesystem (NFS, autofs, a cold /nix/store), so the TUI reads
//! listings in threads of their own; matching one is in memory.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::fuzzy::fuzzy_match;
use crate::paths::{expand_home, resolved};

/// At most this many entries are read from one folder, so a huge one answers fast, partially.
pub const SCAN_LIMIT: usize = 5000;
/// At most this many completions are offered.
pub const COMPLETIONS: usize = 30;

/// Whether typed text names a folder rather than a project: `~…`, `/…`, `./…`, `../…`, `.`, `..`.
pub fn looks_like_path(text: &str) -> bool {
    matches!(text, "." | "..") || ["/", "~", "./", "../"].iter().any(|p| text.starts_with(p))
}

/// (the parent as typed, ending in "/"; the last segment) of a folder path being typed.
pub fn folder_parent(text: &str) -> Option<(&str, &str)> {
    let at = text.rfind('/')?;
    Some((&text[..=at], &text[at + 1..]))
}

/// What a folder holds, for completion.
#[derive(Debug, Clone, PartialEq)]
pub struct Listing {
    /// The folder, its links resolved.
    pub real: PathBuf,
    /// Its subfolders' names (symlinks to folders included).
    pub names: Vec<String>,
    /// The resolved targets of the subfolders that are symlinks: a saved
    /// folder reached through a link is found.
    pub links: BTreeMap<String, PathBuf>,
}

impl Listing {
    /// Where a subfolder of this one really is, read off the listing (no
    /// I/O: a screen asks while typing). The folder is resolved and links
    /// were resolved when it was read, so any other subfolder is just its name.
    pub fn target(&self, name: &str) -> PathBuf {
        self.links.get(name).cloned().unwrap_or_else(|| self.real.join(name))
    }
}

/// The listing of a folder as typed (`~/code/`), reading at most `limit`
/// entries; None when it cannot be read. `~user` is not read (as in
/// `ITM_PROJECTS_DIR`), so it is unreadable, like a user that does not exist.
pub fn list_folders(parent: &str, home: &Path, limit: usize) -> Option<Listing> {
    if parent.starts_with('~') && !(parent == "~" || parent.starts_with("~/")) {
        return None;
    }
    let folder = expand_home(Path::new(parent), home);
    let mut names = vec![];
    let mut links = BTreeMap::new();
    for entry in fs::read_dir(&folder).ok()?.take(limit) {
        let Ok(entry) = entry else { continue };
        let Some(name) = entry.file_name().to_str().map(String::from) else { continue };
        let path = entry.path();
        if fs::metadata(&path).is_ok_and(|m| m.is_dir()) {
            if entry.file_type().is_ok_and(|t| t.is_symlink()) {
                links.insert(name.clone(), resolved(&path));
            }
            names.push(name);
        }
    }
    Some(Listing { real: resolved(&folder), names, links })
}

/// The subfolders matching `segment`, best first, spelled `parent` + name
/// + "/". Hidden folders only when the segment starts with ".".
pub fn match_folders(parent: &str, segment: &str, names: &[String], limit: usize) -> Vec<String> {
    let mut found: Vec<(u32, String, &String)> = (names.iter())
        .filter(|name| !name.starts_with('.') || segment.starts_with('.'))
        .filter_map(|name| Some((fuzzy_match(segment, name)?.0, name.to_lowercase(), name)))
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)).then_with(|| a.2.cmp(b.2)));
    found.into_iter().take(limit).map(|(_, _, name)| format!("{parent}{name}/")).collect()
}

/// Folders completing `text` given the listing of its parent (`listing`
/// asks for it), each ending in "/"; `~`, `.` and `..` complete to
/// themselves with a "/".
pub fn complete(text: &str, listing: impl FnOnce(&str) -> Option<Listing>) -> Vec<String> {
    if matches!(text, "~" | "." | "..") {
        return vec![format!("{text}/")];
    }
    let Some((parent, segment)) = folder_parent(text) else { return vec![] };
    listing(parent).map_or_else(Vec::new, |l| match_folders(parent, segment, &l.names, COMPLETIONS))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A home with folders, a file and a link.
    fn home() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let home = resolved(&tmp.path().join("home"));
        for name in ["code", "Config", "docs", ".hidden", "decode", "cxoxdxe"] {
            fs::create_dir_all(home.join(name)).unwrap();
        }
        fs::write(home.join("code.txt"), "a file").unwrap();
        std::os::unix::fs::symlink(home.join("docs"), home.join("linked")).unwrap();
        (tmp, home)
    }

    fn completions(text: &str, home: &Path) -> Vec<String> {
        complete(text, |parent| list_folders(parent, home, SCAN_LIMIT))
    }

    /// R1.6: prefix matches first, spelled as typed.
    #[test]
    fn r1_6_prefix_matches_come_first_and_keep_the_spelling() {
        let (_tmp, home) = home();
        assert_eq!(completions("~/co", &home), ["~/code/", "~/Config/", "~/decode/", "~/cxoxdxe/"]);
        assert_eq!(completions("~/cde", &home), ["~/code/", "~/cxoxdxe/", "~/decode/"]);
        let base = home.display().to_string();
        assert_eq!(completions(&format!("{base}/do"), &home), [format!("{base}/docs/"), format!("{base}/decode/")]);
    }

    #[test]
    fn r1_6_hidden_folders_symlinks_and_files() {
        let (_tmp, home) = home();
        assert!(!completions("~/", &home).contains(&"~/.hidden/".to_string()));
        assert_eq!(completions("~/.h", &home), ["~/.hidden/"]);
        assert!(completions("~/", &home).contains(&"~/linked/".to_string()));
        assert!(!completions("~/code", &home).contains(&"~/code.txt/".to_string()));
    }

    #[test]
    fn r1_6_listings_resolve_symlinked_folders() {
        let (_tmp, home) = home();
        let listing = list_folders("~/", &home, SCAN_LIMIT).unwrap();
        assert_eq!(listing.real, home);
        assert!(listing.names.contains(&"linked".to_string()));
        assert_eq!(listing.links, BTreeMap::from([("linked".to_string(), home.join("docs"))]));
        assert_eq!(listing.target("linked"), home.join("docs"));
        assert_eq!(listing.target("code"), home.join("code"));
        assert_eq!(list_folders("~/missing/", &home, SCAN_LIMIT), None);
    }

    #[test]
    fn r1_6_odd_input() {
        let (_tmp, home) = home();
        assert_eq!(completions("~", &home), ["~/"]);
        assert_eq!(completions("~no-such-user-x/a", &home), Vec::<String>::new());
        assert_eq!(completions("~/missing/a", &home), Vec::<String>::new());
        assert_eq!(completions("plain", &home), Vec::<String>::new());
    }

    #[test]
    fn r1_6_limits() {
        let (_tmp, home) = home();
        let names = list_folders("~/", &home, SCAN_LIMIT).unwrap().names;
        assert_eq!(match_folders("~/", "", &names, 2).len(), 2);
        assert!(list_folders("~/", &home, 1).unwrap().names.len() <= 1);
    }

    #[test]
    fn r1_6_what_looks_like_a_path() {
        for text in ["/", "~", "~/x", "./x", "../x", ".", ".."] {
            assert!(looks_like_path(text), "{text}");
        }
        for text in ["api", "my api", ".x", "git@github.com:a/b"] {
            assert!(!looks_like_path(text), "{text}");
        }
        assert_eq!(folder_parent("~/code/ap"), Some(("~/code/", "ap")));
        assert_eq!(folder_parent("~/code/"), Some(("~/code/", "")));
        assert_eq!(folder_parent("api"), None);
    }
}
