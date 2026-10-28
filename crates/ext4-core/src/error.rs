use std::fmt;
use std::io;

/// Errors returned by the ext4 engine. Every variant maps to a POSIX errno
/// through [`Error::errno`] so that platform shims can report it directly.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// On-disk data is inconsistent. The string names the structure.
    Corrupt(String),
    /// A feature or on-disk construct this engine does not handle.
    Unsupported(String),
    /// A write was requested on a read-only mount.
    ReadOnly,
    NotFound,
    NotDir,
    IsDir,
    Exists,
    NotEmpty,
    NoSpace,
    NameTooLong,
    TooManyLinks,
    InvalidArgument(String),
    FileTooLarge,
    NoAttribute,
    Range,
    PermissionDenied,
    Busy,
}
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn corrupt(what: impl Into<String>) -> Self {
        Error::Corrupt(what.into())
    }

    pub fn unsupported(what: impl Into<String>) -> Self {
        Error::Unsupported(what.into())
    }

    pub fn invalid(what: impl Into<String>) -> Self {
        Error::InvalidArgument(what.into())
    }

    /// POSIX errno values (Darwin numbering, which matches Linux for these).
    pub fn errno(&self) -> i32 {
        match self {
            Error::Io(e) => e.raw_os_error().unwrap_or(5),
            Error::Corrupt(_) => 5,          // EIO
            Error::Unsupported(_) => 45,     // ENOTSUP (Darwin)
            Error::ReadOnly => 30,           // EROFS
            Error::NotFound => 2,            // ENOENT
            Error::NotDir => 20,             // ENOTDIR
            Error::IsDir => 21,              // EISDIR
            Error::Exists => 17,             // EEXIST
            Error::NotEmpty => 66,           // ENOTEMPTY (Darwin)
            Error::NoSpace => 28,            // ENOSPC
            Error::NameTooLong => 63,        // ENAMETOOLONG (Darwin)
            Error::TooManyLinks => 31,       // EMLINK
            Error::InvalidArgument(_) => 22, // EINVAL
            Error::FileTooLarge => 27,       // EFBIG
            Error::NoAttribute => 93,        // ENOATTR (Darwin)
            Error::Range => 34,              // ERANGE
            Error::PermissionDenied => 1,    // EPERM
            Error::Busy => 16,               // EBUSY
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Corrupt(s) => write!(f, "file system is corrupt: {s}"),
            Error::Unsupported(s) => write!(f, "unsupported: {s}"),
            Error::ReadOnly => f.write_str("file system is mounted read-only"),
            Error::NotFound => f.write_str("no such file or directory"),
            Error::NotDir => f.write_str("not a directory"),
            Error::IsDir => f.write_str("is a directory"),
            Error::Exists => f.write_str("file exists"),
            Error::NotEmpty => f.write_str("directory not empty"),
            Error::NoSpace => f.write_str("no space left on device"),
            Error::NameTooLong => f.write_str("file name too long"),
            Error::TooManyLinks => f.write_str("too many links"),
            Error::InvalidArgument(s) => write!(f, "invalid argument: {s}"),
            Error::FileTooLarge => f.write_str("file too large"),
            Error::NoAttribute => f.write_str("attribute not found"),
            Error::Range => f.write_str("result too large"),
            Error::PermissionDenied => f.write_str("operation not permitted"),
            Error::Busy => f.write_str("resource busy"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}
