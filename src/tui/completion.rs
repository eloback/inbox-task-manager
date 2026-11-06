//! Folder completion in the project finder and the add dialog (R1.6,
//! DESIGN.md section 4): each screen keeps the listings it asked for while
//! it is open, each folder read once, in a thread of its own (the event
//! loop's), and filters them in memory on every key. While a folder is
//! read there is nothing to complete from it, so Enter and Tab never act
//! on rows left from earlier text; a listing that arrives late is used for
//! what is typed then.

use std::collections::{BTreeMap, BTreeSet};

use crate::folders::{self, Listing, folder_parent, looks_like_path};

#[derive(Debug, Clone, Default)]
pub struct Folders {
    /// By the parent folder as typed; None when it could not be read.
    listings: BTreeMap<String, Option<Listing>>,
    reading: BTreeSet<String>,
}

impl Folders {
    /// The listing of `parent` as typed: `Some(None)` when it cannot be
    /// read, None while it is read or before it was asked for.
    pub fn get(&self, parent: &str) -> Option<&Option<Listing>> {
        self.listings.get(parent)
    }

    /// `parent` when it still has to be read, marked as being read: each folder is read once.
    pub fn ask(&mut self, parent: &str) -> Option<String> {
        let new = !self.listings.contains_key(parent) && self.reading.insert(parent.to_string());
        new.then(|| parent.to_string())
    }

    /// A listing arrived; kept only when this screen asked for it.
    pub fn arrived(&mut self, parent: &str, listing: &Option<Listing>) {
        if self.reading.remove(parent) {
            self.listings.insert(parent.to_string(), listing.clone());
        }
    }

    /// Folders completing `text`; None while its parent is read.
    pub fn complete(&self, text: &str) -> Option<Vec<String>> {
        match folder_parent(text) {
            Some((parent, _)) if !matches!(text, "~" | "." | "..") => {
                let listing = self.get(parent)?.clone();
                Some(folders::complete(text, |_| listing))
            }
            _ => Some(folders::complete(text, |_| None)),
        }
    }

    /// The parent of `text` to read for completing it, if one is needed and not asked for yet.
    pub fn wanted(&mut self, text: &str) -> Option<String> {
        if !looks_like_path(text) {
            return None;
        }
        let (parent, _) = folder_parent(text)?;
        self.ask(parent)
    }

    /// Ghost text for a folder being typed (`~/co` → `~/code/`): a
    /// completion that starts with exactly what was typed (it is drawn
    /// after it, and → or Tab take it), none after a trailing "/" (Tab
    /// would otherwise keep going down into the first subfolder).
    pub fn ghost(&self, typed: &str) -> Option<String> {
        if !looks_like_path(typed) || typed.ends_with('/') {
            return None;
        }
        self.complete(typed)?.into_iter().find(|c| c.starts_with(typed) && c != typed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn listing(names: &[&str]) -> Option<Listing> {
        let names = names.iter().map(|n| n.to_string()).collect();
        Some(Listing { real: PathBuf::from("/r/home"), names, links: BTreeMap::new() })
    }

    /// R1.6: each folder is read once; completion waits for its listing,
    /// and a listing nobody asked for (a closed dialog's) is dropped.
    #[test]
    fn r1_6_each_folder_is_read_once_and_completion_waits_for_it() {
        let mut folders = Folders::default();
        assert_eq!(folders.wanted("~/co"), Some("~/".into()));
        assert_eq!(folders.wanted("~/cod"), None, "asked already");
        assert_eq!(folders.complete("~/co"), None, "being read");
        assert_eq!(folders.ghost("~/co"), None);
        folders.arrived("/elsewhere/", &listing(&["x"]));
        assert_eq!(folders.get("/elsewhere/"), None, "not asked for");
        folders.arrived("~/", &listing(&["code", "Config", "decode"]));
        assert_eq!(folders.complete("~/co").unwrap(), ["~/code/", "~/Config/", "~/decode/"]);
        assert_eq!(folders.wanted("~/co"), None, "read once");
        assert_eq!(folders.wanted("api"), None, "not a path");
    }

    /// DESIGN.md: ghost text starts with exactly what was typed (no
    /// casefolding) and offers nothing after a trailing "/".
    #[test]
    fn r1_6_ghost_text_starts_with_exactly_what_was_typed() {
        let mut folders = Folders::default();
        folders.ask("~/");
        folders.arrived("~/", &listing(&["Config", "code", "projects"]));
        assert_eq!(folders.ghost("~/co").as_deref(), Some("~/code/"));
        assert_eq!(folders.ghost("~/Co").as_deref(), Some("~/Config/"));
        assert_eq!(folders.ghost("~/CO"), None, "no completion starts with it");
        assert_eq!(folders.ghost("~/"), None, "nothing after a trailing /");
        assert_eq!(folders.ghost("~/code/"), None);
        assert_eq!(folders.ghost("~/pro").as_deref(), Some("~/projects/"));
        assert_eq!(folders.ghost("api"), None);
        assert_eq!(folders.ghost("~").as_deref(), Some("~/"));
        folders.ask("~/missing/");
        folders.arrived("~/missing/", &None);
        assert_eq!(folders.complete("~/missing/a"), Some(vec![]), "unreadable: nothing to complete");
    }
}
