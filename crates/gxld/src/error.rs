//! Linker errors. Every input-derived failure is a value, never a panic.

use alloc::string::String;
use core::fmt;

/// Why a link failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// An input could not be parsed as an ELF64 x86_64 relocatable or an
    /// `ar` archive of them.
    Input {
        /// Input name as given on the command line (archive members as
        /// `archive(member)`).
        input: String,
        /// What was wrong.
        detail: String,
    },
    /// A live section references a symbol nothing defines.
    Undefined {
        /// Symbol name.
        symbol: String,
        /// `input(section)` holding the relocation.
        referenced_by: String,
    },
    /// Two inputs define the same strong symbol.
    Duplicate {
        /// Symbol name.
        symbol: String,
        /// First definer.
        first: String,
        /// Second definer.
        second: String,
    },
    /// A construct outside the v0 scope (`docs/LINKER.md`).
    Unsupported {
        /// What was met.
        what: String,
        /// Where.
        input: String,
    },
    /// A relocation result does not fit its field.
    Overflow {
        /// ELF relocation type number.
        rtype: u32,
        /// Target symbol (or section) name.
        symbol: String,
        /// `input(section)` holding the relocation.
        input: String,
    },
    /// The entry symbol is not defined.
    MissingEntry(String),
    /// Command-line problem.
    Args(String),
}

/// `Result` with the linker error.
pub type Result<T> = core::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Input { input, detail } => write!(f, "{input}: {detail}"),
            Error::Undefined {
                symbol,
                referenced_by,
            } => write!(
                f,
                "undefined symbol `{symbol}` referenced by {referenced_by}"
            ),
            Error::Duplicate {
                symbol,
                first,
                second,
            } => write!(
                f,
                "duplicate symbol `{symbol}`: defined in {first} and {second}"
            ),
            Error::Unsupported { what, input } => write!(f, "{input}: unsupported: {what}"),
            Error::Overflow {
                rtype,
                symbol,
                input,
            } => write!(
                f,
                "{input}: relocation type {rtype} against `{symbol}` out of range"
            ),
            Error::MissingEntry(entry) => write!(f, "entry symbol `{entry}` is not defined"),
            Error::Args(msg) => write!(f, "{msg}"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {}
