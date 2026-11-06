//! Settings (R10.4): `$XDG_CONFIG_HOME/inbox-task-manager/config.toml`, the
//! user's file (DESIGN.md section 2, Settings).
//!
//! It is read on every use, the daemon's every pass included, so a toggle
//! applies without a restart; a missing file is the defaults. An unknown
//! setting, a value that is not true or false, or a file that cannot be
//! read is an error: a typo must not silently do nothing. A change edits
//! the TOML document (toml_edit), so everything else in the file, comments
//! included, stays as it was; it is written through a symlink to its target
//! (home-manager, stow).

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use toml_edit::{DocumentMut, Item, Table, Value};

use crate::error::{Context, Error, Result};
use crate::itm::Itm;
use crate::paths::resolved;
use crate::store::{flocked, write_atomic};

/// The table the settings live in.
const SECTION: &str = "herdr";

/// One setting: a key in `[herdr]`, true or false.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    /// "Projects follow workspaces" (R10.3).
    ProjectsFromWorkspaces,
    /// "Remove a project when its workspace closes" (R10.3).
    RemoveProjectsWithoutWorkspace,
}

impl Setting {
    pub const ALL: [Setting; 2] = [Setting::ProjectsFromWorkspaces, Setting::RemoveProjectsWithoutWorkspace];

    pub fn key(self) -> &'static str {
        match self {
            Setting::ProjectsFromWorkspaces => "projects_from_workspaces",
            Setting::RemoveProjectsWithoutWorkspace => "remove_projects_without_workspace",
        }
    }

    /// What the Settings screen calls it.
    pub fn label(self) -> &'static str {
        match self {
            Setting::ProjectsFromWorkspaces => "Projects follow workspaces",
            Setting::RemoveProjectsWithoutWorkspace => "Remove a project when its workspace closes",
        }
    }

    /// What the Settings screen says it does.
    pub fn help(self) -> &'static str {
        match self {
            Setting::ProjectsFromWorkspaces => {
                "A running task's projects are the saved projects its workspaces are in (the folder Herdr names a workspace after). Off: projects change only when you edit them."
            }
            Setting::RemoveProjectsWithoutWorkspace => {
                "With the above: a project found in a workspace leaves the task once its workspace closed; projects you added yourself stay. Off: projects are only added."
            }
        }
    }

    fn named(key: &str) -> Option<Setting> {
        Setting::ALL.into_iter().find(|s| s.key() == key)
    }
}

/// The settings, each on by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub projects_from_workspaces: bool,
    pub remove_projects_without_workspace: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { projects_from_workspaces: true, remove_projects_without_workspace: true }
    }
}

impl Settings {
    pub fn get(&self, setting: Setting) -> bool {
        match setting {
            Setting::ProjectsFromWorkspaces => self.projects_from_workspaces,
            Setting::RemoveProjectsWithoutWorkspace => self.remove_projects_without_workspace,
        }
    }

    pub fn set(&mut self, setting: Setting, on: bool) {
        match setting {
            Setting::ProjectsFromWorkspaces => self.projects_from_workspaces = on,
            Setting::RemoveProjectsWithoutWorkspace => self.remove_projects_without_workspace = on,
        }
    }

    /// What a broken file means until it is fixed: nothing follows
    /// workspaces, so nothing is added or removed (the safe choice).
    pub fn broken() -> Settings {
        Settings { projects_from_workspaces: false, ..Settings::default() }
    }
}

/// The settings in `text` over the defaults. `shown` is the file as errors name it.
pub fn parse(text: &str, shown: &str) -> Result<Settings> {
    let doc: DocumentMut =
        text.parse().map_err(|e| Error::Other(format!("{shown} is not valid TOML: {}", toml_error(&e, text))))?;
    let mut settings = Settings::default();
    for (section, item) in doc.iter() {
        let values = match item.as_table_like() {
            Some(values) if section == SECTION => values,
            _ => return Err(Error::Other(format!("{shown}: unknown setting {section}"))),
        };
        for (key, value) in values.iter() {
            let setting =
                Setting::named(key).ok_or_else(|| Error::Other(format!("{shown}: unknown setting {section}.{key}")))?;
            let on = value
                .as_bool()
                .ok_or_else(|| Error::Other(format!("{shown}: {section}.{key} must be true or false")))?;
            settings.set(setting, on);
        }
    }
    Ok(settings)
}

/// A parse error on one line: toml_edit's own spans several, with a drawing of the line.
fn toml_error(e: &toml_edit::TomlError, text: &str) -> String {
    let message = e.message().lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("; ");
    let Some(span) = e.span() else { return message };
    let before = &text[..span.start.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().unwrap_or_default().chars().count() + 1;
    format!("{message} (at line {line}, column {column})")
}

/// The file as text; a missing one is empty. Any other failure, a file that
/// is not UTF-8 included, is an error.
fn read_text(path: &Path, shown: &str) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).context(|| format!("cannot read {shown}")),
    }
}

/// `text` with `setting` changed to `on`, the rest kept as it is: the
/// comment after the value too (assigning a new value would drop it). A new
/// key goes after the last one in its table; a missing table is added.
fn changed(text: &str, setting: Setting, on: bool) -> Option<String> {
    let mut doc: DocumentMut = text.parse().ok()?;
    if !doc.contains_key(SECTION) {
        // Any other table is an error, so the file holds comments at most: they
        // stay before the new table, then one blank line, as the Python itm writes it.
        let mut before = doc.trailing().as_str().unwrap_or_default().to_string();
        if !before.is_empty() && !before.ends_with('\n') {
            before.push('\n');
        }
        if before.lines().last().is_some_and(|l| !l.trim().is_empty()) {
            before.push('\n');
        }
        let mut table = Table::new();
        table.decor_mut().set_prefix(before);
        doc.set_trailing("");
        doc.insert(SECTION, Item::Table(table));
    }
    let table = doc.get_mut(SECTION)?.as_table_like_mut()?;
    match table.get_mut(setting.key()) {
        Some(Item::Value(value)) => {
            let decor = value.decor().clone();
            *value = Value::from(on);
            *value.decor_mut() = decor;
        }
        Some(_) => return None,
        None => drop(table.insert(setting.key(), toml_edit::value(on))),
    }
    let mut new = doc.to_string();
    if !new.ends_with('\n') {
        new.push('\n');
    }
    Some(new)
}

impl Itm {
    /// The settings; a missing file means the defaults.
    pub fn settings(&self) -> Result<Settings> {
        let path = self.paths.config_file();
        let shown = self.paths.display(&path);
        parse(&read_text(&path, &shown)?, &shown)
    }

    /// Whether running tasks' projects follow their workspaces (R10.3). A
    /// broken settings file means no, as for the daemon.
    pub fn follows_workspaces(&self) -> bool {
        self.settings().is_ok_and(|s| s.projects_from_workspaces)
    }

    /// Change one setting in `config.toml`, keeping the rest of the file
    /// (comments too), under `.config.lock`. A symlinked file is written
    /// through the link; the result must read back with the new value, or
    /// nothing is written.
    pub fn change_setting(&self, setting: Setting, on: bool) -> Result<()> {
        let config = self.paths.config_file();
        let lock = self.paths.config.join(".config.lock");
        let _lock = flocked(&lock).context(|| format!("cannot lock {}", self.paths.display(&lock)))?;
        let path = if config.is_symlink() { resolved(&config) } else { config.clone() };
        let shown = self.paths.display(&path);
        let text = read_text(&path, &shown)?;
        let hand = || Error::Other(format!("could not change {SECTION}.{} in {shown}; edit it by hand", setting.key()));
        let new = changed(&text, setting, on).ok_or_else(hand)?;
        match parse(&new, &self.paths.display(&config)) {
            Ok(settings) if settings.get(setting) == on => {}
            _ => return Err(hand()),
        }
        write_atomic(&path, &new).context(|| format!("cannot write {shown}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::itm::tests::itm;

    fn write(itm: &Itm, text: &str) {
        fs::create_dir_all(&itm.paths.config).unwrap();
        fs::write(itm.paths.config_file(), text).unwrap();
    }

    fn read(itm: &Itm) -> String {
        fs::read_to_string(itm.paths.config_file()).unwrap()
    }

    #[test]
    fn r10_4_defaults_without_a_file() {
        let (_tmp, itm) = itm();
        assert_eq!(itm.settings().unwrap(), Settings::default());
        assert!(Settings::default().projects_from_workspaces && Settings::default().remove_projects_without_workspace);
        assert!(itm.follows_workspaces());
    }

    /// R10.4: a change keeps the rest of the file, comments included.
    #[test]
    fn r10_4_a_change_keeps_the_rest_of_the_file() {
        let (_tmp, itm) = itm();
        write(&itm, "# mine\n[herdr]\n# why\nprojects_from_workspaces = true  # keep\n");
        itm.change_setting(Setting::ProjectsFromWorkspaces, false).unwrap();
        itm.change_setting(Setting::RemoveProjectsWithoutWorkspace, false).unwrap();
        assert_eq!(
            read(&itm),
            "# mine\n[herdr]\n# why\nprojects_from_workspaces = false  # keep\nremove_projects_without_workspace = false\n"
        );
        assert!(!itm.settings().unwrap().remove_projects_without_workspace);
        assert!(!itm.follows_workspaces());
    }

    #[test]
    fn r10_4_a_change_creates_the_file() {
        let (_tmp, itm) = itm();
        itm.change_setting(Setting::RemoveProjectsWithoutWorkspace, false).unwrap();
        assert_eq!(read(&itm), "[herdr]\nremove_projects_without_workspace = false\n");
        // After other content, the new table follows a blank line, as the Python itm writes it.
        for (text, written) in [
            ("# settings\n", "# settings\n\n[herdr]\nprojects_from_workspaces = false\n"),
            ("# settings\n\n", "# settings\n\n[herdr]\nprojects_from_workspaces = false\n"),
            ("# settings", "# settings\n\n[herdr]\nprojects_from_workspaces = false\n"),
            ("\n", "\n[herdr]\nprojects_from_workspaces = false\n"),
        ] {
            write(&itm, text);
            itm.change_setting(Setting::ProjectsFromWorkspaces, false).unwrap();
            assert_eq!(read(&itm), written, "from {text:?}");
        }
    }

    /// R10.4: a mistake is an error that says which setting and where.
    #[test]
    fn r10_4_mistakes_are_errors() {
        let error = |text: &str| parse(text, "~/c.toml").unwrap_err().to_string();
        assert_eq!(error("[herdr\n"), "~/c.toml is not valid TOML: unclosed table, expected `]` (at line 1, column 7)");
        assert_eq!(error("[herdr]\nnope = true\n"), "~/c.toml: unknown setting herdr.nope");
        assert_eq!(error("[other]\n"), "~/c.toml: unknown setting other");
        assert_eq!(error("herdr = 1\n"), "~/c.toml: unknown setting herdr");
        assert_eq!(error("[[herdr]]\n"), "~/c.toml: unknown setting herdr");
        assert_eq!(
            error("[herdr]\nprojects_from_workspaces = 1\n"),
            "~/c.toml: herdr.projects_from_workspaces must be true or false"
        );
        assert_eq!(
            error("[herdr]\nprojects_from_workspaces = \"false\"\n"),
            "~/c.toml: herdr.projects_from_workspaces must be true or false"
        );
        // Every way TOML writes a table counts.
        let off = Settings { remove_projects_without_workspace: false, ..Settings::default() };
        assert_eq!(parse("herdr.remove_projects_without_workspace = false\n", "c").unwrap(), off);
        assert_eq!(parse("herdr = { remove_projects_without_workspace = false }\n", "c").unwrap(), off);
        assert_eq!(parse("# nothing\n[herdr]\n", "c").unwrap(), Settings::default());
    }

    /// R10.4: a file that cannot be read is an error, and itm then does not follow workspaces.
    #[test]
    fn r10_4_odd_files() {
        let (_tmp, itm) = itm();
        fs::create_dir_all(&itm.paths.config).unwrap();
        fs::write(itm.paths.config_file(), b"# n\xe3o\n").unwrap(); // Latin-1
        let err = itm.settings().unwrap_err().to_string();
        assert!(err.starts_with(&format!("cannot read {}: ", itm.paths.config_file().display())), "{err}");
        assert!(!itm.follows_workspaces(), "a broken file is off");
        let err = itm.change_setting(Setting::ProjectsFromWorkspaces, false).unwrap_err().to_string();
        assert!(err.starts_with("cannot read "), "{err}");

        // What the Python itm refused ("edit it by hand"), the document edit follows.
        write(&itm, "[herdr]\n\"projects_from_workspaces\" = true\n");
        itm.change_setting(Setting::ProjectsFromWorkspaces, false).unwrap();
        assert_eq!(read(&itm), "[herdr]\n\"projects_from_workspaces\" = false\n");
        write(&itm, "herdr.projects_from_workspaces = true\n");
        itm.change_setting(Setting::ProjectsFromWorkspaces, false).unwrap();
        assert_eq!(read(&itm), "herdr.projects_from_workspaces = false\n");
        write(&itm, "herdr = { projects_from_workspaces = true }\n");
        itm.change_setting(Setting::RemoveProjectsWithoutWorkspace, false).unwrap();
        assert!(!itm.settings().unwrap().remove_projects_without_workspace);

        // A file it cannot read back with the change is left alone.
        for text in ["[herdr\n", "[herdr]\nnope = true\n", "[herdr.projects_from_workspaces]\n"] {
            write(&itm, text);
            let err = itm.change_setting(Setting::ProjectsFromWorkspaces, false).unwrap_err().to_string();
            assert!(err.ends_with("; edit it by hand"), "{err}");
            assert_eq!(read(&itm), text);
        }
    }

    /// R10.4: through a symlink (home-manager, stow) the target changes and the link stays.
    #[test]
    fn r10_4_a_symlinked_file_is_written_through_the_link() {
        use std::os::unix::fs::PermissionsExt;
        let (tmp, itm) = itm();
        let target = tmp.path().join("dotfiles/itm.toml");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "[herdr]\nprojects_from_workspaces = true\n").unwrap();
        fs::create_dir_all(&itm.paths.config).unwrap();
        std::os::unix::fs::symlink(&target, itm.paths.config_file()).unwrap();
        itm.change_setting(Setting::ProjectsFromWorkspaces, false).unwrap();
        assert!(itm.paths.config_file().is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), "[herdr]\nprojects_from_workspaces = false\n");

        let dir = target.parent().unwrap();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
        let unwritable = fs::File::create(dir.join("probe")).is_err(); // root writes anyway
        let result = itm.change_setting(Setting::ProjectsFromWorkspaces, true);
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
        if unwritable {
            let err = result.unwrap_err().to_string();
            assert!(err.starts_with(&format!("cannot write {}: ", target.display())), "{err}");
        }
    }
}
