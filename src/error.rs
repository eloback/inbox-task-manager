//! itm's errors. Their messages are part of its interface (scripts and the
//! Python itm's users read them), so each one is a sentence for the user,
//! worded like the Python itm's. The variants are the errors a caller has
//! to tell apart: which Herdr failure, which lookup failed, and why a
//! project could not be added (R8.4, R8.5: the add dialog keeps you on the
//! name field only when the name is the problem). Everything else is
//! [`Error::Other`], written where it happens.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// `herdr` is not on `PATH` (pip installs bring none).
    HerdrMissing,
    /// A request to a Herdr session failed.
    Herdr {
        method: &'static str,
        failure: HerdrFailure,
    },
    /// No task has this session name, session name without the prefix, or title.
    NoSuchTask(String),
    /// Several tasks have this title; their sessions.
    AmbiguousTask {
        query: String,
        sessions: Vec<String>,
    },
    /// The task was deleted while an edit waited for the store lock.
    TaskGone(String),
    NoSuchProject(String),
    /// The folder or repository is saved already, as this project (R8.4).
    ProjectExists {
        name: String,
    },
    /// Another project has the name, ignoring case; `place` says where it is (R8.5).
    ProjectNameTaken {
        name: String,
        place: String,
    },
    /// A project without a folder has the name: give it the folder instead (R8.2).
    ProjectWithoutFolder {
        name: String,
        folder: String,
    },
    /// Anything else.
    Other(String),
}

/// How a request to a Herdr session failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HerdrFailure {
    /// Nothing listens on the session's socket.
    NotRunning,
    /// No connection, or no answer, within the timeout.
    TimedOut,
    /// Herdr answered with an error.
    Refused { code: String, message: String },
    /// An answer itm cannot read (what was wrong with it).
    BadAnswer(&'static str),
    /// Any other failure, as the system described it.
    Io(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::HerdrMissing => f.write_str("herdr not found; install it or add it to PATH"),
            Error::Herdr { method, failure } => match failure {
                HerdrFailure::NotRunning => write!(f, "herdr {method}: session not running"),
                HerdrFailure::TimedOut => write!(f, "herdr {method} timed out"),
                HerdrFailure::Refused { code, message } => write!(f, "herdr {method} failed: {message} ({code})"),
                HerdrFailure::BadAnswer(what) => write!(f, "herdr {method} returned {what}"),
                HerdrFailure::Io(e) => write!(f, "herdr {method}: {e}"),
            },
            Error::NoSuchTask(query) => write!(f, "no task named '{query}'"),
            Error::AmbiguousTask { query, sessions } => {
                write!(f, "several tasks are titled '{query}': {}", sessions.join(", "))
            }
            Error::TaskGone(name) => write!(f, "task {name} no longer exists"),
            Error::NoSuchProject(query) => write!(f, "no project named '{query}'; add it with `itm project add`"),
            Error::ProjectExists { name } => write!(f, "project already exists: {name}"),
            Error::ProjectNameTaken { name, place } => write!(
                f,
                "project already exists: {name} ({place}); choose another name, or rename it with `itm project rename {} <name>`",
                shell_quote(name)
            ),
            Error::ProjectWithoutFolder { name, folder } => write!(
                f,
                "project {name} has no folder yet; give it this one with `itm project set {} --dir {}`",
                shell_quote(name),
                shell_quote(folder)
            ),
            Error::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

/// `return Err(Error::Other(format!(...)))`.
macro_rules! bail {
    ($($arg:tt)*) => { return Err($crate::error::Error::Other(format!($($arg)*))) };
}
pub(crate) use bail;

/// Adds the user-facing sentence to a lower-level error: `.context(|| format!("cannot write {path}"))`
/// gives "cannot write …: <cause>".
pub trait Context<T> {
    fn context(self, what: impl FnOnce() -> String) -> Result<T>;
}

impl<T, E: fmt::Display> Context<T> for std::result::Result<T, E> {
    fn context(self, what: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|e| Error::Other(format!("{}: {e}", what())))
    }
}

/// A word as a shell reads it back, quoted like Python's `shlex.quote`, for
/// commands that messages suggest.
pub fn shell_quote(word: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if !word.is_empty() && word.chars().all(safe) {
        return word.into();
    }
    format!("'{}'", word.replace('\'', r#"'"'"'"#))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_are_quoted_like_python_shlex() {
        assert_eq!(shell_quote("api"), "api");
        assert_eq!(shell_quote("~/src/api"), "'~/src/api'");
        assert_eq!(shell_quote("Client X"), "'Client X'");
        assert_eq!(shell_quote("it's"), r#"'it'"'"'s'"#);
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("café"), "'café'");
    }

    #[test]
    fn messages_are_the_python_itms() {
        let taken = Error::ProjectNameTaken { name: "api".into(), place: "~/code/api".into() };
        assert_eq!(
            taken.to_string(),
            "project already exists: api (~/code/api); choose another name, or rename it with `itm project rename api <name>`"
        );
        let refused = HerdrFailure::Refused { code: "invalid_params".into(), message: "title is empty".into() };
        let herdr = Error::Herdr { method: "notification.show", failure: refused };
        assert_eq!(herdr.to_string(), "herdr notification.show failed: title is empty (invalid_params)");
        let several = Error::AmbiguousTask { query: "Fix".into(), sessions: vec!["task-a".into(), "task-b".into()] };
        assert_eq!(several.to_string(), "several tasks are titled 'Fix': task-a, task-b");
    }
}
