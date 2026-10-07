//! Compile errors for the gxr subset.

use std::fmt;

/// A frontend error with an optional byte offset into the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// Human-readable message.
    pub message: String,
    /// Byte offset in the source, if known.
    pub offset: Option<usize>,
}

impl Error {
    /// Error without a source location.
    pub fn msg(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            offset: None,
        }
    }

    /// Error at a byte offset.
    pub fn at(offset: usize, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            offset: Some(offset),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.offset {
            Some(off) => write!(f, "{} (at byte {})", self.message, off),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for Error {}

/// Result alias for the frontend.
pub type Result<T> = std::result::Result<T, Error>;
