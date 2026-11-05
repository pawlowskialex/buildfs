//! The core's API types. Front ends convert to and from their own: none of
//! these is stored on disk (`meta` and `wal` define the on-disk format).

/// What a core operation can fail with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FsError {
    #[error("permission denied")]
    PermissionDenied,
    #[error("not found")]
    NotFound,
    #[error("i/o error")]
    Io,
    #[error("already exists")]
    AlreadyExists,
    #[error("not a directory")]
    NotDirectory,
    #[error("is a directory")]
    IsDirectory,
    #[error("invalid input")]
    InvalidInput,
    #[error("file too large")]
    FileTooLarge,
    #[error("no space left")]
    NoSpace,
    #[error("name too long")]
    NameTooLong,
    #[error("directory not empty")]
    NotEmpty,
    #[error("stale handle")]
    Stale,
}

/// A time supplied by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timestamp {
    pub seconds: i64,
    pub nanos: u32,
}

/// A requested time update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetTime {
    /// The current time.
    ServerNow,
    Client(Timestamp),
}

/// A partial attribute update.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetAttrs {
    pub size: Option<u64>,
    pub archive: Option<bool>,
    pub hidden: Option<bool>,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub system: Option<bool>,
    pub atime: Option<SetTime>,
    pub mtime: Option<SetTime>,
    pub birthtime: Option<SetTime>,
}

/// How an extended attribute is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XattrSetMode {
    CreateOrReplace,
    /// Fails if the attribute exists.
    CreateOnly,
    /// Fails if the attribute doesn't exist.
    ReplaceOnly,
}
