//! Metadata schema stored in redb.
//!
//! Layout:
//! - `inodes`:      ino -> postcard(Inode)
//! - `dirents`:     (dir ino, name) -> (child ino, cookie)
//! - `dircookies`:  (dir ino, cookie) -> name      (stable readdir order/resume)
//! - `xattrs`:      (ino, name) -> value
//! - `counters`:    name -> u64                    (next inode / cookie)
//! - `inline`:      ino -> zstd bytes              (tiny compressed files)
//!
//! Inode numbers and readdir cookies are allocated from monotonic counters and
//! never reused, so NFS filehandles that embed an inode number can never
//! silently alias a newer file.

use redb::TableDefinition;
use serde::{Deserialize, Serialize};

pub type Ino = u64;

/// Root directory inode.
pub const ROOT_INO: Ino = 1;
/// First inode handed out for regular objects.
pub const FIRST_INO: Ino = 16;
/// NFSv4 reserves readdir cookies 0, 1 and 2.
pub const FIRST_COOKIE: u64 = 3;

pub const INODES: TableDefinition<u64, &[u8]> = TableDefinition::new("inodes");
pub const DIRENTS: TableDefinition<(u64, &str), (u64, u64)> = TableDefinition::new("dirents");
pub const DIRCOOKIES: TableDefinition<(u64, u64), &str> = TableDefinition::new("dircookies");
pub const XATTRS: TableDefinition<(u64, &str), &[u8]> = TableDefinition::new("xattrs");
pub const COUNTERS: TableDefinition<&str, u64> = TableDefinition::new("counters");
/// ino -> zstd frame of the whole file, for files that compress tiny.
pub const INLINE: TableDefinition<u64, &[u8]> = TableDefinition::new("inline");

pub const NEXT_INO: &str = "next_ino";
pub const NEXT_COOKIE: &str = "next_cookie";
pub const FORMAT_VERSION_KEY: &str = "format_version";
pub const FORMAT_VERSION: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Fifo,
    Socket,
    CharDev,
    BlockDev,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Ts {
    pub sec: i64,
    pub nsec: u32,
}

impl Ts {
    pub fn now() -> Self {
        let d = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            sec: d.as_secs() as i64,
            nsec: d.subsec_nanos(),
        }
    }
}

/// Where a regular file's bytes live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Data {
    /// No backing storage; reads return zeros up to `size`.
    Empty,
    /// Uncompressed file in the hot store, named after the inode.
    Hot,
    /// Compressed image in the cold store (`cold/<ino>-<gen>`).
    Cold { gen_: u64, disk: u64 },
    /// Compressed bytes in the `inline` table (`disk` = stored length).
    Inline { disk: u32 },
}

pub const FLAG_HIDDEN: u8 = 1;
pub const FLAG_ARCHIVE: u8 = 2;
pub const FLAG_SYSTEM: u8 = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inode {
    pub kind: Kind,
    /// Permission bits only (`0o7777`); the type lives in `kind`.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    /// Logical size. For directories: number of entries.
    pub size: u64,
    pub atime: Ts,
    pub mtime: Ts,
    pub ctime: Ts,
    pub btime: Ts,
    /// NFS change attribute; strictly increases on every modification.
    pub change: u64,
    pub flags: u8,
    pub rdev: (u32, u32),
    /// Parent directory (directories only; root points at itself).
    pub parent: Ino,
    pub xattr_count: u32,
    /// Symlink target bytes.
    pub target: Vec<u8>,
    pub data: Data,
}

impl Inode {
    pub fn new(kind: Kind, mode: u32, uid: u32, gid: u32, change: u64) -> Self {
        let now = Ts::now();
        Self {
            kind,
            mode: mode & 0o7777,
            uid,
            gid,
            nlink: if kind == Kind::Dir { 2 } else { 1 },
            size: 0,
            atime: now,
            mtime: now,
            ctime: now,
            btime: now,
            change,
            flags: 0,
            rdev: (0, 0),
            parent: 0,
            xattr_count: 0,
            target: Vec::new(),
            data: Data::Empty,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        // Serializing plain structs into a Vec cannot fail.
        postcard::to_stdvec(self).unwrap_or_default()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inode_roundtrip() {
        let mut inode = Inode::new(Kind::Symlink, 0o777, 501, 20, 9);
        inode.target = b"../lib".to_vec();
        inode.rdev = (1, 2);
        let decoded = Inode::decode(&inode.encode()).unwrap();
        assert_eq!(decoded, inode);
    }
}
