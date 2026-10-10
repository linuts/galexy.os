//! `io::Error` ↔ `SysError`. The raw OS error code of an `io::Error` on
//! this target is the ABI error code (`SysError as i32`), so a program
//! can round-trip `err.raw_os_error()` through `SysError::from_code`.

use crate::fmt;
use crate::io::ErrorKind;
use crate::sys::pal::abi::SysError;

pub fn errno() -> i32 {
    0
}

pub fn is_interrupted(code: i32) -> bool {
    code == SysError::Interrupted as i32
}

pub fn decode_error_kind(code: i32) -> ErrorKind {
    match SysError::from_code(code as u64) {
        SysError::BadCap => ErrorKind::InvalidInput,
        SysError::AccessDenied => ErrorKind::PermissionDenied,
        SysError::BadBuffer => ErrorKind::InvalidInput,
        SysError::Unsupported => ErrorKind::Unsupported,
        SysError::BadValue => ErrorKind::InvalidInput,
        SysError::NotFound => ErrorKind::NotFound,
        SysError::NoResource => ErrorKind::QuotaExceeded,
        SysError::Interrupted => ErrorKind::Interrupted,
        SysError::Locked => ErrorKind::ResourceBusy,
        SysError::TimedOut => ErrorKind::TimedOut,
    }
}

pub fn format_error(code: i32, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let name = match SysError::from_code(code as u64) {
        SysError::BadCap => "bad capability",
        SysError::AccessDenied => "access denied",
        SysError::BadBuffer => "bad buffer",
        SysError::Unsupported => "unsupported",
        SysError::BadValue => "bad value",
        SysError::NotFound => "not found",
        SysError::NoResource => "no resource",
        SysError::Interrupted => "interrupted",
        SysError::Locked => "locked",
        SysError::TimedOut => "timed out",
    };
    f.write_str(name)
}
