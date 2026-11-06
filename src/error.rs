//! The one error type: a message for the user. itm's messages are part of its
//! interface (tests and scripts match them), so every error carries a sentence
//! written where it happens rather than a chain of causes.

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub struct Error(pub String);

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// `return Err(Error(format!(...)))`.
macro_rules! bail {
    ($($arg:tt)*) => { return Err($crate::error::Error(format!($($arg)*))) };
}
pub(crate) use bail;

/// Adds the user-facing sentence to a lower-level error: `.context(|| format!("cannot write {path}"))`
/// gives "cannot write …: <cause>".
pub trait Context<T> {
    fn context(self, what: impl FnOnce() -> String) -> Result<T>;
}

impl<T, E: fmt::Display> Context<T> for std::result::Result<T, E> {
    fn context(self, what: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|e| Error(format!("{}: {e}", what())))
    }
}
