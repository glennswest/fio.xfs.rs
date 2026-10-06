//! Errors.

/// Anything that can go wrong reading a filesystem.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The device, or a stream being written to, failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// The device does not hold an XFS filesystem.
    #[error("not an XFS filesystem: {0}")]
    NotXfs(String),

    /// A structure failed its magic number, its checksum or a bounds check.
    #[error("corrupt filesystem: {0}")]
    Corrupt(String),

    /// The filesystem was not cleanly unmounted: its log holds changes that
    /// may not be on disk, so what is on disk may be stale. Mounting it once
    /// replays the log; [`crate::Volume::open_norecovery`] reads it anyway.
    #[error("the log is dirty (head {head}, tail {tail}): the filesystem was not cleanly unmounted, and what is on disk may be stale; mount it once to replay the log")]
    DirtyLog {
        /// Where the next log write would go, in 512-byte blocks into the log.
        head: u64,
        /// The oldest record whose changes may not be on disk.
        tail: u64,
    },

    /// The filesystem uses something this implementation cannot read.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// No such file or directory.
    #[error("no such file or directory: {0}")]
    NotFound(String),

    /// A path component that should have been a directory was not.
    #[error("not a directory: {0}")]
    NotADirectory(String),

    /// The target is a directory and the operation wanted a file.
    #[error("is a directory: {0}")]
    IsADirectory(String),

    /// The target is not a symbolic link.
    #[error("not a symbolic link: {0}")]
    NotASymlink(String),

    /// Symbolic links nested too deeply, or in a loop.
    #[error("too many levels of symbolic links: {0}")]
    SymlinkLoop(String),

    /// The path was malformed.
    #[error("invalid path: {0}")]
    InvalidPath(String),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// A `Corrupt` error from anything printable.
pub(crate) fn corrupt(what: impl Into<String>) -> Error {
    Error::Corrupt(what.into())
}
