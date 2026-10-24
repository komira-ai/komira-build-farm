//! The errors of a [`Backend`](crate::Backend) call.

/// Why a backend call failed. Cloned, so one failure can be reported to every caller that
/// waited on it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// No object has this key.
    #[error("object {0} not found")]
    NotFound(String),
    /// A create-only put on a key that exists.
    #[error("object {0} already exists")]
    AlreadyExists(String),
    /// The backend did not answer (down, timed out, refused the connection). Retrying later
    /// may succeed.
    #[error("backend unavailable: {0}")]
    Unavailable(String),
    /// Any other failure, such as a range that starts past the end of the object.
    #[error("backend error: {0}")]
    Other(String),
}

impl Error {
    /// Whether the backend as a whole is out of reach, so that trying the next object of a
    /// batch would fail the same way.
    #[must_use]
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Error::Unavailable(_))
    }
}
