//! Error types shared across oxenDB.

use std::fmt;
use std::io;

/// Convenience alias for results returned by oxenDB.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors produced by oxenDB.
///
/// Variants describe *what kind* of failure happened so callers can react to
/// it (retry an I/O error, refuse to open a corrupt file). The attached
/// message is for humans and carries the specifics.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// An operating system I/O call failed.
    Io(io::Error),
    /// On-disk data failed validation: bad checksum, bad magic number,
    /// impossible field value. The file should not be trusted.
    Corruption(String),
    /// The file was written by an incompatible version of oxenDB.
    UnsupportedFormat(String),
    /// The caller passed an argument that violates an API contract.
    InvalidArgument(String),
    /// A fixed-size resource (for example, buffer pool frames) is exhausted.
    ResourceExhausted(String),
}

impl Error {
    /// Builds a [`Error::Corruption`] from anything printable.
    pub fn corruption(msg: impl Into<String>) -> Self {
        Error::Corruption(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(err) => write!(f, "I/O error: {err}"),
            Error::Corruption(msg) => write!(f, "data corruption detected: {msg}"),
            Error::UnsupportedFormat(msg) => write!(f, "unsupported file format: {msg}"),
            Error::InvalidArgument(msg) => write!(f, "invalid argument: {msg}"),
            Error::ResourceExhausted(msg) => write!(f, "resource exhausted: {msg}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn display_includes_kind_and_detail() {
        let err = Error::corruption("page 7 checksum mismatch");
        assert_eq!(
            err.to_string(),
            "data corruption detected: page 7 checksum mismatch"
        );
    }

    #[test]
    fn io_errors_keep_their_source() {
        let err: Error = io::Error::new(io::ErrorKind::NotFound, "missing").into();
        assert!(matches!(err, Error::Io(_)));
        assert!(err.source().is_some());
    }
}
