use std::fmt;

/// What went wrong, in words fit to show a person. Callers that want more match on nothing:
/// every failure of an update check is "no update, and here is why".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(String);

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self(message)
    }
}

impl From<&str> for Error {
    fn from(message: &str) -> Self {
        Self(message.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

macro_rules! fail {
    ($($arg:tt)*) => { return Err($crate::Error::new(format!($($arg)*))) };
}
pub(crate) use fail;
