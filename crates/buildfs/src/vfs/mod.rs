//! Synchronous filesystem core: inodes, directories, links, xattrs and data.
//!
//! Every mutation runs in a single redb write transaction committed with
//! `Durability::None`; a background flusher turns those into durable commits
//! periodically and on demand (NFS COMMIT), coalescing concurrent requests.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use embednfs::{AuthContext, FsError, RequestContext, SetAttrs, SetTime, XattrSetMode};
use parking_lot::{Condvar, Mutex};
use redb::{Database, Durability, ReadableDatabase, ReadableTable, Table};
use tracing::{error, info, warn};

use crate::chunks::{self, ChunkStore, Hash};
use crate::hot::HotStore;
use crate::meta::*;
use crate::wal::{self, Mutation, Wal};

mod tier;

pub type Result<T> = std::result::Result<T, FsError>;

const MAX_NAME_BYTES: usize = 255;
/// Matches the NFS `maxfilesize` we advertise.
pub const MAX_FILE_SIZE: u64 = 1 << 50;

// ---------------------------------------------------------------------------
// Error plumbing
// ---------------------------------------------------------------------------

fn db_err(e: impl std::fmt::Display) -> FsError {
    error!("metadata store error: {e}");
    FsError::Io
}

fn io_err(e: std::io::Error) -> FsError {
    match e.raw_os_error() {
        Some(libc::ENOSPC) | Some(libc::EDQUOT) => FsError::NoSpace,
        Some(libc::EFBIG) => FsError::FileTooLarge,
        _ => {
            error!("data store error: {e}");
            FsError::Io
        }
    }
}

macro_rules! from_db {
    ($($t:ty),*) => {$(
        impl From<$t> for DbError { fn from(e: $t) -> Self { DbError(db_err(e)) } }
    )*};
}

/// Adapter so `?` works on redb results inside closures returning `Result`.
struct DbError(FsError);
from_db!(
    redb::Error,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    redb::SetDurabilityError,
    redb::DatabaseError
);

trait DbResultExt<T> {
    fn db(self) -> Result<T>;
}
impl<T, E> DbResultExt<T> for std::result::Result<T, E>
where
    DbError: From<E>,
{
    fn db(self) -> Result<T> {
        self.map_err(|e| DbError::from(e).0)
    }
}

// ---------------------------------------------------------------------------
// Caller identity
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Caller {
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
}

impl Caller {
    pub const NOBODY: u32 = 65534;

    pub fn from_ctx(ctx: &RequestContext) -> Self {
        match &ctx.auth {
            AuthContext::Sys {
                uid,
                gid,
                supplemental_gids,
            } => Self {
                uid: *uid,
                gid: *gid,
                groups: supplemental_gids.clone(),
            },
            _ => Self {
                uid: Self::NOBODY,
                gid: Self::NOBODY,
                groups: Vec::new(),
            },
        }
    }

    pub fn root() -> Self {
        Self {
            uid: 0,
            gid: 0,
            groups: Vec::new(),
        }
    }

    fn in_group(&self, gid: u32) -> bool {
        self.gid == gid || self.groups.contains(&gid)
    }
}

/// NFSv4 ACCESS bits (RFC 8881 §18.1).
pub mod access {
    pub const READ: u32 = 1;
    pub const LOOKUP: u32 = 2;
    pub const MODIFY: u32 = 4;
    pub const EXTEND: u32 = 8;
    pub const DELETE: u32 = 16;
    pub const EXECUTE: u32 = 32;
}

// ---------------------------------------------------------------------------
// Group-commit flusher
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FlushState {
    requested: u64,
    completed: u64,
    shutdown: bool,
}

struct Flusher {
    state: Mutex<FlushState>,
    cv: Condvar,
}

impl Flusher {
    fn new() -> Self {
        Self {
            state: Mutex::new(FlushState::default()),
            cv: Condvar::new(),
        }
    }

    /// Blocks until a durable commit that started after this call completes.
    fn barrier(&self) {
        let mut s = self.state.lock();
        s.requested += 1;
        let target = s.requested;
        self.cv.notify_all();
        while s.completed < target && !s.shutdown {
            self.cv.wait(&mut s);
        }
    }
}

// ---------------------------------------------------------------------------
// Write-transaction helper
// ---------------------------------------------------------------------------

struct W<'t> {
    inodes: Table<'t, u64, &'static [u8]>,
    dirents: Table<'t, (u64, &'static str), (u64, u64)>,
    cookies: Table<'t, (u64, u64), &'static str>,
    xattrs: Table<'t, (u64, &'static str), &'static [u8]>,
    counters: Table<'t, &'static str, u64>,
    inline: Table<'t, u64, &'static [u8]>,
    chunks: Table<'t, &'static [u8], (u64, u32, u8)>,
    chunkdata: Table<'t, &'static [u8], &'static [u8]>,
    chunkmap: Table<'t, (u64, u32), &'static [u8]>,
    change: &'t AtomicU64,
    open_counts: &'t Mutex<std::collections::HashMap<Ino, u32>>,
    /// Data files to delete once the transaction has committed.
    drop_data: Vec<DropData>,
    /// Redo record for the WAL; every table write goes through a helper
    /// below so nothing is missed.
    log: Vec<Mutation>,
}

impl W<'_> {
    fn get(&self, ino: Ino) -> Result<Inode> {
        let v = self.inodes.get(ino).db()?.ok_or(FsError::Stale)?;
        Inode::decode(v.value()).map_err(db_err)
    }

    fn put(&mut self, ino: Ino, inode: &Inode) -> Result<()> {
        let bytes = inode.encode();
        let _ = self.inodes.insert(ino, bytes.as_slice()).db()?;
        self.log.push(Mutation::InodePut(ino, bytes));
        Ok(())
    }

    fn del_inode(&mut self, ino: Ino) -> Result<()> {
        let _ = self.inodes.remove(ino).db()?;
        self.log.push(Mutation::InodeDel(ino));
        Ok(())
    }

    fn put_xattr(&mut self, ino: Ino, name: &str, value: &[u8]) -> Result<()> {
        let _ = self.xattrs.insert((ino, name), value).db()?;
        self.log
            .push(Mutation::XattrPut(ino, name.to_owned(), value.to_vec()));
        Ok(())
    }

    fn put_inline(&mut self, ino: Ino, bytes: &[u8]) -> Result<()> {
        let _ = self.inline.insert(ino, bytes).db()?;
        self.log.push(Mutation::InlinePut(ino, bytes.to_vec()));
        Ok(())
    }

    fn del_inline(&mut self, ino: Ino) -> Result<()> {
        if self.inline.remove(ino).db()?.is_some() {
            self.log.push(Mutation::InlineDel(ino));
        }
        Ok(())
    }

    /// Releases whatever stores the bytes of `inode` (row now, files after
    /// commit). The caller updates `inode.data`.
    fn release_data(&mut self, ino: Ino, data: &Data) -> Result<()> {
        match *data {
            Data::Empty => {}
            Data::Hot => self.drop_data.push(DropData::Hot(ino)),
            Data::Chunked { .. } => self.release_chunks(ino)?,
            Data::Inline { .. } => self.del_inline(ino)?,
        }
        Ok(())
    }

    /// Returns whether the xattr existed.
    fn del_xattr(&mut self, ino: Ino, name: &str) -> Result<bool> {
        let existed = self.xattrs.remove((ino, name)).db()?.is_some();
        if existed {
            self.log.push(Mutation::XattrDel(ino, name.to_owned()));
        }
        Ok(existed)
    }

    fn next_change(&self) -> u64 {
        self.change.fetch_add(1, Ordering::Relaxed)
    }

    fn alloc(&mut self, key: &str, first: u64) -> Result<u64> {
        let next = self.counters.get(key).db()?.map_or(first, |v| v.value());
        let _ = self.counters.insert(key, next + 1).db()?;
        self.log.push(Mutation::Counter(key.to_owned(), next + 1));
        Ok(next)
    }

    fn lookup(&self, dir: Ino, name: &str) -> Result<Option<(Ino, u64)>> {
        Ok(self.dirents.get((dir, name)).db()?.map(|v| v.value()))
    }

    fn link_entry(&mut self, dir: Ino, name: &str, child: Ino) -> Result<()> {
        let cookie = self.alloc(NEXT_COOKIE, FIRST_COOKIE)?;
        let _ = self.dirents.insert((dir, name), (child, cookie)).db()?;
        let _ = self.cookies.insert((dir, cookie), name).db()?;
        self.log
            .push(Mutation::DirentPut(dir, name.to_owned(), child, cookie));
        self.log
            .push(Mutation::CookiePut(dir, cookie, name.to_owned()));
        Ok(())
    }

    fn unlink_entry(&mut self, dir: Ino, name: &str, cookie: u64) -> Result<()> {
        let _ = self.dirents.remove((dir, name)).db()?;
        let _ = self.cookies.remove((dir, cookie)).db()?;
        self.log.push(Mutation::DirentDel(dir, name.to_owned()));
        self.log.push(Mutation::CookieDel(dir, cookie));
        Ok(())
    }

    /// Bumps mtime/ctime/change on a directory after its entries changed.
    fn touch(&self, inode: &mut Inode) {
        let now = Ts::now();
        inode.mtime = now;
        inode.ctime = now;
        inode.change = self.next_change();
    }

    fn touch_ctime(&self, inode: &mut Inode) {
        inode.ctime = Ts::now();
        inode.change = self.next_change();
    }

    /// Drops one link to `ino`; frees it when the last link goes away.
    fn drop_link(&mut self, ino: Ino, mut inode: Inode) -> Result<()> {
        if inode.kind == Kind::Dir {
            inode.nlink = 0;
        } else {
            inode.nlink = inode.nlink.saturating_sub(1);
        }
        if inode.nlink > 0 {
            self.touch_ctime(&mut inode);
            return self.put(ino, &inode);
        }
        // Still open somewhere: keep the inode (nlink 0) until the last
        // open state ends. Checked inside the write transaction, which
        // serializes against `Vfs::open_released`.
        if inode.kind != Kind::Dir && self.open_counts.lock().contains_key(&ino) {
            self.touch_ctime(&mut inode);
            return self.put(ino, &inode);
        }
        self.free_inode(ino, inode)
    }

    /// Deletes an inode with no links left, with its xattrs and data.
    fn free_inode(&mut self, ino: Ino, inode: Inode) -> Result<()> {
        self.del_inode(ino)?;
        if inode.xattr_count > 0 {
            let names: Vec<String> = self
                .xattrs
                .range((ino, "")..(ino + 1, ""))
                .db()?
                .map(|r| r.map(|(k, _)| k.value().1.to_owned()))
                .collect::<std::result::Result<_, _>>()
                .db()?;
            for name in names {
                let _ = self.del_xattr(ino, &name)?;
            }
        }
        self.release_data(ino, &inode.data)?;
        Ok(())
    }
}

enum DropData {
    Hot(Ino),
    /// A chunk whose last reference went away; its file is deleted after
    /// re-checking under `chunk_lock` (see `tier.rs`).
    Chunk(Hash),
}

// ---------------------------------------------------------------------------
// The filesystem
// ---------------------------------------------------------------------------

/// How much an NFS COMMIT (fsync/close) guarantees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncMode {
    /// Data and metadata reach the kernel page cache (pwrite + WAL append)
    /// before replying, so they survive a crash of buildfs itself. Durable
    /// redb commits happen in the background every second. Suits build
    /// artifacts, and lets writes reply FILE_SYNC so clients skip COMMIT.
    #[default]
    Relaxed,
    /// COMMIT fsyncs the file and waits for a durable metadata commit:
    /// survives power loss at the cost of ~10 ms per closed file.
    Strict,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub sync: SyncMode,
    /// Period of background durable commits; `None` disables them (tests).
    pub flush_interval: Option<Duration>,
    /// zstd level for compressed data.
    pub level: i32,
    /// Compress files once they haven't been written for this long.
    /// `None` disables background compression.
    pub compress_after: Option<Duration>,
    /// Background compression threads.
    pub compress_workers: usize,
    /// Decompressed-chunk cache size.
    pub cache_bytes: usize,
}

/// Files whose compressed image is at most this big live in redb.
const INLINE_MAX: usize = 3072;
/// Only files up to this size are tried for inline storage.
const INLINE_PROBE_MAX: u64 = 256 * 1024;
/// Upper bound for the per-file compression delay backoff.
const MAX_COMPRESS_DELAY: Duration = Duration::from_secs(600);



impl Default for Options {
    fn default() -> Self {
        Self {
            sync: SyncMode::default(),
            flush_interval: Some(Duration::from_secs(1)),
            level: 3,
            compress_after: Some(Duration::from_secs(15)),
            compress_workers: std::thread::available_parallelism()
                .map_or(2, |n| (n.get() / 4).max(1)),
            cache_bytes: 256 << 20,
        }
    }
}

/// Pending compression for a file: when it was last modified and how long
/// to wait (doubles each time the file is rehydrated, to avoid churning on
/// files that are rewritten in place).
#[derive(Clone, Copy)]
struct Pending {
    since: std::time::Instant,
    delay: Duration,
}

#[derive(Default)]
struct CompactorState {
    pending: std::collections::HashMap<Ino, Pending>,
    /// Per-inode delay multiplier carried across rehydrations.
    backoff: std::collections::HashMap<Ino, Duration>,
    shutdown: bool,
}

/// Counters exposed for `buildfs stats` and tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct CompressStats {
    pub compacted: u64,
    pub inlined: u64,
    pub rehydrated: u64,
    pub aborted: u64,
    pub logical_bytes: u64,
    pub stored_bytes: u64,
    /// Chunks (and their logical bytes) that were already stored.
    pub dedup_chunks: u64,
    pub dedup_bytes: u64,
}

pub struct Vfs {
    db: Database,
    hot: HotStore,
    chunks: ChunkStore,
    /// Serializes chunk-file deletion against new references to the same
    /// chunk (see `tier.rs`).
    chunk_lock: Mutex<()>,
    level: i32,
    compress_after: Option<Duration>,
    compactor: Mutex<CompactorState>,
    compactor_cv: Condvar,
    compactor_threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    rehydrate_lock: Mutex<()>,
    stats: Mutex<CompressStats>,
    /// Active NFS open states per inode (see `open_acquired`).
    open_counts: Mutex<std::collections::HashMap<Ino, u32>>,
    wal: Mutex<Wal>,
    sync_mode: SyncMode,
    change: AtomicU64,
    dirty: AtomicBool,
    flusher: Flusher,
    flusher_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// What to create in [`Vfs::create`].
#[derive(Debug, Clone)]
pub enum NewNode {
    File,
    Dir,
    Symlink(String),
    Fifo,
    Socket,
    CharDev(u32, u32),
    BlockDev(u32, u32),
}

pub struct DirItem {
    pub name: String,
    pub ino: Ino,
    pub cookie: u64,
    pub attrs: Option<Inode>,
}

impl Vfs {
    pub fn open(store: &Path) -> anyhow::Result<Arc<Self>> {
        Self::open_with(store, Options::default())
    }

    pub fn open_with(store: &Path, options: Options) -> anyhow::Result<Arc<Self>> {
        let sync_mode = options.sync;
        std::fs::create_dir_all(store)?;
        let db = Database::create(store.join("meta.redb"))?;
        let hot = HotStore::open(store.join("hot"))?;
        let chunks = ChunkStore::open(store.join("chunks"), options.cache_bytes)?;
        let wal_dir = store.join("wal");
        let pending = wal::segments(&wal_dir)?;
        let seed = {
            let d = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            d.as_nanos() as u64
        };
        let vfs = Arc::new(Self {
            db,
            hot,
            chunks,
            chunk_lock: Mutex::new(()),
            level: options.level,
            compress_after: options.compress_after,
            compactor: Mutex::new(CompactorState::default()),
            compactor_cv: Condvar::new(),
            compactor_threads: Mutex::new(Vec::new()),
            rehydrate_lock: Mutex::new(()),
            stats: Mutex::new(CompressStats::default()),
            open_counts: Mutex::new(std::collections::HashMap::new()),
            // Placeholder segment numbering is fixed up after replay below.
            wal: Mutex::new(Wal::open(&wal_dir)?),
            sync_mode,
            change: AtomicU64::new(seed),
            dirty: AtomicBool::new(false),
            flusher: Flusher::new(),
            flusher_thread: Mutex::new(None),
        });
        vfs.init_schema().map_err(|e| anyhow::anyhow!("init failed: {e:?}"))?;
        vfs.replay_wal(&pending)?;
        vfs.free_orphans().map_err(|e| anyhow::anyhow!("orphan cleanup failed: {e:?}"))?;
        vfs.gc_data().map_err(|e| anyhow::anyhow!("gc failed: {e:?}"))?;
        Self::spawn_flusher(&vfs, options.flush_interval);
        if vfs.compress_after.is_some() {
            Self::spawn_compactor(&vfs, options.compress_workers.max(1));
        }
        Ok(vfs)
    }

    /// Stable identity for this store, used as the NFS server owner/scope so
    /// clients recognize a restarted server. Unique per store.
    pub fn server_identity(store: &Path) -> anyhow::Result<String> {
        let path = store.join("server-id");
        if let Ok(id) = std::fs::read_to_string(&path) {
            let id = id.trim();
            if !id.is_empty() {
                return Ok(id.to_owned());
            }
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let id = format!("buildfs-{:x}-{:x}", nanos, std::process::id());
        std::fs::write(&path, &id)?;
        Ok(id)
    }

    fn init_schema(&self) -> Result<()> {
        let mut txn = self.db.begin_write().db()?;
        txn.set_durability(Durability::Immediate).db()?;
        {
            let mut counters = txn.open_table(COUNTERS).db()?;
            let mut inodes = txn.open_table(INODES).db()?;
            let _ = txn.open_table(DIRENTS).db()?;
            let _ = txn.open_table(DIRCOOKIES).db()?;
            let _ = txn.open_table(XATTRS).db()?;
            let _ = txn.open_table(INLINE).db()?;
            let _ = txn.open_table(CHUNKS).db()?;
            let _ = txn.open_table(CHUNKDATA).db()?;
            let _ = txn.open_table(CHUNKMAP).db()?;
            let version = counters.get(FORMAT_VERSION_KEY).db()?.map(|v| v.value());
            match version {
                Some(FORMAT_VERSION) => {}
                Some(other) => {
                    error!(
                        "store format version {other} is not supported (expected {FORMAT_VERSION}); \
                         use a new store directory"
                    );
                    return Err(FsError::Io);
                }
                None => {
                    let _ = counters.insert(FORMAT_VERSION_KEY, FORMAT_VERSION).db()?;
                }
            }
            if inodes.get(ROOT_INO).db()?.is_none() {
                // SAFETY: getuid/getgid have no preconditions.
                let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
                let mut root = Inode::new(Kind::Dir, 0o755, uid, gid, 1);
                root.parent = ROOT_INO;
                let _ = inodes.insert(ROOT_INO, root.encode().as_slice()).db()?;
            }
        }
        txn.commit().db()?;
        Ok(())
    }

    /// Re-applies metadata changes that were in the WAL but not yet durable
    /// in redb when the previous process died.
    fn replay_wal(&self, segments: &[(u64, std::path::PathBuf)]) -> anyhow::Result<()> {
        if segments.is_empty() {
            return Ok(());
        }
        let mut applied = 0usize;
        let mut txn = self.db.begin_write()?;
        txn.set_durability(Durability::Immediate)?;
        {
            let mut inodes = txn.open_table(INODES)?;
            let mut dirents = txn.open_table(DIRENTS)?;
            let mut cookies = txn.open_table(DIRCOOKIES)?;
            let mut xattrs = txn.open_table(XATTRS)?;
            let mut counters = txn.open_table(COUNTERS)?;
            let mut inline = txn.open_table(INLINE)?;
            let mut chunk_t = txn.open_table(CHUNKS)?;
            let mut chunkdata = txn.open_table(CHUNKDATA)?;
            let mut chunkmap = txn.open_table(CHUNKMAP)?;
            for (_, path) in segments {
                for record in wal::read_segment(path)? {
                    for m in record {
                        match m {
                            Mutation::InodePut(k, v) => {
                                let _ = inodes.insert(k, v.as_slice())?;
                            }
                            Mutation::InodeDel(k) => {
                                let _ = inodes.remove(k)?;
                            }
                            Mutation::DirentPut(d, n, c, ck) => {
                                let _ = dirents.insert((d, n.as_str()), (c, ck))?;
                            }
                            Mutation::DirentDel(d, n) => {
                                let _ = dirents.remove((d, n.as_str()))?;
                            }
                            Mutation::CookiePut(d, ck, n) => {
                                let _ = cookies.insert((d, ck), n.as_str())?;
                            }
                            Mutation::CookieDel(d, ck) => {
                                let _ = cookies.remove((d, ck))?;
                            }
                            Mutation::XattrPut(i, n, v) => {
                                let _ = xattrs.insert((i, n.as_str()), v.as_slice())?;
                            }
                            Mutation::XattrDel(i, n) => {
                                let _ = xattrs.remove((i, n.as_str()))?;
                            }
                            Mutation::Counter(k, v) => {
                                let _ = counters.insert(k.as_str(), v)?;
                            }
                            Mutation::InlinePut(i, v) => {
                                let _ = inline.insert(i, v.as_slice())?;
                            }
                            Mutation::InlineDel(i) => {
                                let _ = inline.remove(i)?;
                            }
                            Mutation::ChunkPut(h, rc, len, raw) => {
                                let _ = chunk_t.insert(h.as_slice(), (rc, len, raw))?;
                            }
                            Mutation::ChunkDel(h) => {
                                let _ = chunk_t.remove(h.as_slice())?;
                            }
                            Mutation::ChunkDataPut(h, v) => {
                                let _ = chunkdata.insert(h.as_slice(), v.as_slice())?;
                            }
                            Mutation::ChunkDataDel(h) => {
                                let _ = chunkdata.remove(h.as_slice())?;
                            }
                            Mutation::ChunkMapPut(i, idx, h) => {
                                let _ = chunkmap.insert((i, idx), h.as_slice())?;
                            }
                            Mutation::ChunkMapDel(i, idx) => {
                                let _ = chunkmap.remove((i, idx))?;
                            }
                        }
                    }
                    applied += 1;
                }
            }
        }
        txn.commit()?;
        for (_, path) in segments {
            std::fs::remove_file(path)?;
        }
        if applied > 0 {
            warn!("recovered {applied} metadata transactions from the WAL");
        }
        Ok(())
    }

    /// Frees files that were unlinked while open when the previous process
    /// stopped. Open state never survives a restart.
    fn free_orphans(&self) -> Result<()> {
        let orphans: Vec<Ino> = {
            let txn = self.db.begin_read().db()?;
            let inodes = txn.open_table(INODES).db()?;
            let mut out = Vec::new();
            for entry in inodes.iter().db()? {
                let (k, v) = entry.db()?;
                if Inode::decode(v.value()).is_ok_and(|i| i.nlink == 0 && i.kind != Kind::Dir) {
                    out.push(k.value());
                }
            }
            out
        };
        if orphans.is_empty() {
            return Ok(());
        }
        let n = orphans.len();
        self.write(|w| {
            for ino in orphans {
                let inode = w.get(ino)?;
                w.free_inode(ino, inode)?;
            }
            Ok(())
        })?;
        info!("freed {n} files that were unlinked while open");
        Ok(())
    }

    // ----- open tracking ------------------------------------------------------

    /// An NFS open state was created for `ino`.
    pub fn open_acquired(&self, ino: Ino) {
        *self.open_counts.lock().entry(ino).or_insert(0) += 1;
    }

    /// An NFS open state for `ino` ended; frees the inode if it was the last
    /// one and the file has been unlinked meanwhile.
    pub fn open_released(&self, ino: Ino) {
        {
            let mut counts = self.open_counts.lock();
            match counts.get_mut(&ino) {
                Some(n) if *n > 1 => {
                    *n -= 1;
                    return;
                }
                Some(_) => {
                    let _ = counts.remove(&ino);
                }
                None => return,
            }
        }
        let r = self.write(|w| {
            let inode = match w.get(ino) {
                Ok(i) => i,
                Err(FsError::Stale) => return Ok(()),
                Err(e) => return Err(e),
            };
            if inode.nlink == 0 && inode.kind != Kind::Dir && !w.open_counts.lock().contains_key(&ino) {
                w.free_inode(ino, inode)?;
            }
            Ok(())
        });
        if let Err(e) = r {
            warn!("failed to free unlinked inode {ino}: {e:?}");
        }
    }

    /// Deletes data files no inode references (crash leftovers) and queues
    /// hot files left over from the previous run for compression.
    fn gc_data(&self) -> Result<()> {
        let txn = self.db.begin_read().db()?;
        let inodes = txn.open_table(INODES).db()?;
        let data_of = |ino: Ino| -> Result<Option<Data>> {
            Ok(match inodes.get(ino).db()? {
                Some(v) => Inode::decode(v.value()).ok().map(|i| i.data),
                None => None,
            })
        };
        let mut removed = 0usize;
        for ino in self.hot.list().map_err(io_err)? {
            if data_of(ino)? != Some(Data::Hot) {
                self.hot.remove(ino).map_err(io_err)?;
                removed += 1;
            }
        }
        let chunk_t = txn.open_table(CHUNKS).db()?;
        for h in self.chunks.list_files().map_err(io_err)? {
            if chunk_t.get(h.as_slice()).db()?.is_none() {
                self.chunks.remove_file(&h).map_err(io_err)?;
                removed += 1;
            }
        }
        if removed > 0 {
            warn!("removed {removed} orphaned data files");
        }
        if self.compress_after.is_some() {
            let mut queued = 0usize;
            for entry in inodes.iter().db()? {
                let (k, v) = entry.db()?;
                if Inode::decode(v.value()).is_ok_and(|i| i.data == Data::Hot) {
                    self.note_modified(k.value());
                    queued += 1;
                }
            }
            if queued > 0 {
                info!("queued {queued} uncompressed files from the previous run");
            }
        }
        Ok(())
    }

    fn spawn_flusher(vfs: &Arc<Self>, interval: Option<Duration>) {
        let weak = Arc::downgrade(vfs);
        let handle = std::thread::Builder::new()
            .name("buildfs-flush".into())
            .spawn(move || {
                loop {
                    let Some(vfs) = weak.upgrade() else { return };
                    let target = {
                        let mut s = vfs.flusher.state.lock();
                        if s.requested == s.completed && !s.shutdown {
                            match interval {
                                Some(i) => {
                                    let _ = vfs.flusher.cv.wait_for(&mut s, i);
                                }
                                None => vfs.flusher.cv.wait(&mut s),
                            }
                        }
                        if s.shutdown {
                            return;
                        }
                        s.requested
                    };
                    if vfs.dirty.swap(false, Ordering::AcqRel)
                        && let Err(e) = vfs.durable_commit()
                    {
                        error!("durable commit failed: {e:?}");
                        vfs.dirty.store(true, Ordering::Release);
                    }
                    let mut s = vfs.flusher.state.lock();
                    s.completed = s.completed.max(target);
                    vfs.flusher.cv.notify_all();
                }
            });
        match handle {
            Ok(h) => *vfs.flusher_thread.lock() = Some(h),
            Err(e) => error!("failed to start flusher thread: {e}"),
        }
    }

    fn durable_commit(&self) -> Result<()> {
        let mut txn = self.db.begin_write().db()?;
        txn.set_durability(Durability::Immediate).db()?;
        // Holding the write transaction excludes appends, so the old segment
        // contains exactly the transactions this commit makes durable.
        let old = self.wal.lock().rotate().map_err(io_err)?;
        txn.commit().db()?;
        if let Err(e) = std::fs::remove_file(&old) {
            warn!("failed to remove WAL segment {}: {e}", old.display());
        }
        Ok(())
    }

    /// Makes every metadata change so far durable (group-committed).
    pub fn sync_metadata(&self) {
        self.flusher.barrier();
    }

    /// Stops background work and makes metadata durable. Call before exit.
    pub fn shutdown(&self) {
        {
            let mut c = self.compactor.lock();
            c.shutdown = true;
            self.compactor_cv.notify_all();
        }
        let threads = std::mem::take(&mut *self.compactor_threads.lock());
        for h in threads {
            if h.thread().id() != std::thread::current().id() {
                let _ = h.join();
            }
        }
        self.flusher.barrier();
        {
            let mut s = self.flusher.state.lock();
            s.shutdown = true;
            self.flusher.cv.notify_all();
        }
        if let Some(h) = self.flusher_thread.lock().take()
            && h.thread().id() != std::thread::current().id()
        {
            let _ = h.join();
        }
    }

    fn write<R>(&self, f: impl FnOnce(&mut W<'_>) -> Result<R>) -> Result<R> {
        let mut txn = self.db.begin_write().db()?;
        txn.set_durability(Durability::None).db()?;
        let (result, drop_data) = {
            let mut w = W {
                inodes: txn.open_table(INODES).db()?,
                dirents: txn.open_table(DIRENTS).db()?,
                cookies: txn.open_table(DIRCOOKIES).db()?,
                xattrs: txn.open_table(XATTRS).db()?,
                counters: txn.open_table(COUNTERS).db()?,
                inline: txn.open_table(INLINE).db()?,
                chunks: txn.open_table(CHUNKS).db()?,
                chunkdata: txn.open_table(CHUNKDATA).db()?,
                chunkmap: txn.open_table(CHUNKMAP).db()?,
                change: &self.change,
                open_counts: &self.open_counts,
                drop_data: Vec::new(),
                log: Vec::new(),
            };
            let r = f(&mut w)?; // early return aborts the transaction
            // Appended while holding the write lock, so WAL order == commit order.
            self.wal.lock().append(&w.log).map_err(io_err)?;
            (r, std::mem::take(&mut w.drop_data))
        };
        txn.commit().db()?;
        self.dirty.store(true, Ordering::Release);
        for d in drop_data {
            match d {
                DropData::Hot(ino) => {
                    if let Err(e) = self.hot.remove(ino) {
                        warn!("failed to remove data for inode {ino}: {e}");
                    }
                }
                DropData::Chunk(h) => self.delete_chunk_file_if_unused(&h),
            }
        }
        Ok(result)
    }

    // ----- reads ------------------------------------------------------------

    pub fn getattr(&self, ino: Ino) -> Result<Inode> {
        let txn = self.db.begin_read().db()?;
        let t = txn.open_table(INODES).db()?;
        let v = t.get(ino).db()?.ok_or(FsError::Stale)?;
        Inode::decode(v.value()).map_err(db_err)
    }

    /// Bytes this inode occupies in the store (what `du` reports).
    pub fn space_used(&self, ino: Ino, inode: &Inode) -> u64 {
        match inode.data {
            Data::Hot => self.hot.allocated(ino).unwrap_or(inode.size),
            Data::Empty => 0,
            Data::Chunked { disk } => disk,
            Data::Inline { disk } => u64::from(disk),
        }
    }

    pub fn lookup(&self, dir: Ino, name: &str) -> Result<Ino> {
        let txn = self.db.begin_read().db()?;
        let inodes = txn.open_table(INODES).db()?;
        let d = inodes.get(dir).db()?.ok_or(FsError::Stale)?;
        if Inode::decode(d.value()).map_err(db_err)?.kind != Kind::Dir {
            return Err(FsError::NotDirectory);
        }
        let dirents = txn.open_table(DIRENTS).db()?;
        let v = dirents.get((dir, name)).db()?.ok_or(FsError::NotFound)?;
        Ok(v.value().0)
    }

    pub fn parent(&self, dir: Ino) -> Result<Option<Ino>> {
        let inode = self.getattr(dir)?;
        if inode.kind != Kind::Dir {
            return Err(FsError::NotDirectory);
        }
        Ok((dir != ROOT_INO).then_some(inode.parent))
    }

    pub fn readdir(
        &self,
        dir: Ino,
        cookie: u64,
        max: usize,
        with_attrs: bool,
    ) -> Result<(Vec<DirItem>, bool)> {
        let txn = self.db.begin_read().db()?;
        let inodes = txn.open_table(INODES).db()?;
        let d = inodes.get(dir).db()?.ok_or(FsError::Stale)?;
        if Inode::decode(d.value()).map_err(db_err)?.kind != Kind::Dir {
            return Err(FsError::NotDirectory);
        }
        let cookies = txn.open_table(DIRCOOKIES).db()?;
        let dirents = txn.open_table(DIRENTS).db()?;
        let start = cookie.saturating_add(1).max(FIRST_COOKIE);
        let mut out = Vec::new();
        let mut eof = true;
        for entry in cookies.range((dir, start)..=(dir, u64::MAX)).db()? {
            let (k, v) = entry.db()?;
            if out.len() >= max {
                eof = false;
                break;
            }
            let name = v.value().to_owned();
            let Some(de) = dirents.get((dir, name.as_str())).db()? else {
                continue;
            };
            let (ino, _) = de.value();
            let attrs = if with_attrs {
                match inodes.get(ino).db()? {
                    Some(v) => Some(Inode::decode(v.value()).map_err(db_err)?),
                    None => continue,
                }
            } else {
                None
            };
            out.push(DirItem {
                name,
                ino,
                cookie: k.value().1,
                attrs,
            });
        }
        Ok((out, eof))
    }

    pub fn readlink(&self, ino: Ino) -> Result<String> {
        let inode = self.getattr(ino)?;
        if inode.kind != Kind::Symlink {
            return Err(FsError::InvalidInput);
        }
        String::from_utf8(inode.target).map_err(|_| FsError::Io)
    }

    pub fn read(&self, ino: Ino, offset: u64, count: u32) -> Result<(Vec<u8>, bool)> {
        // A file can migrate between tiers between reading its inode and
        // touching its data; the old data file then disappears and the read
        // retries against the new location.
        let mut attempt = 0;
        loop {
            let inode = self.getattr(ino)?;
            match inode.kind {
                Kind::File => {}
                Kind::Dir => return Err(FsError::IsDirectory),
                _ => return Err(FsError::InvalidInput),
            }
            if offset >= inode.size {
                return Ok((Vec::new(), true));
            }
            let len = (inode.size - offset).min(u64::from(count)) as usize;
            match self.read_data(ino, &inode, offset, len) {
                Ok(data) => return Ok((data, offset + len as u64 >= inode.size)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && attempt < 5 => {
                    attempt += 1;
                }
                Err(e) => return Err(io_err(e)),
            }
        }
    }

    fn read_data(&self, ino: Ino, inode: &Inode, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        match inode.data {
            Data::Hot => self.hot.read_at(ino, offset, len),
            Data::Empty => Ok(vec![0; len]),
            Data::Chunked { .. } => self.read_chunked(ino, inode.size, offset, len),
            Data::Inline { .. } => {
                let stored = {
                    let txn = self.db.begin_read().map_err(std::io::Error::other)?;
                    let t = txn.open_table(INLINE).map_err(std::io::Error::other)?;
                    let v = t
                        .get(ino)
                        .map_err(std::io::Error::other)?
                        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
                    v.value().to_vec()
                };
                let full = chunks::decode(stored, false, inode.size as usize)?;
                let start = (offset as usize).min(full.len());
                let end = (start + len).min(full.len());
                let mut out = full[start..end].to_vec();
                out.resize(len, 0);
                Ok(out)
            }
        }
    }

    pub fn access(&self, caller: &Caller, ino: Ino, requested: u32) -> Result<u32> {
        let inode = self.getattr(ino)?;
        Ok(requested & allowed_access(caller, &inode))
    }

    pub fn statfs(&self) -> Result<libc::statvfs> {
        let path = std::ffi::CString::new(self.hot.root().as_os_str().as_encoded_bytes())
            .map_err(|_| FsError::Io)?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `path` is NUL-terminated and `st` is a valid out pointer.
        let rc = unsafe { libc::statvfs(path.as_ptr(), &mut st) };
        if rc != 0 {
            return Err(io_err(std::io::Error::last_os_error()));
        }
        Ok(st)
    }

    // ----- namespace mutations ---------------------------------------------

    pub fn create(
        &self,
        caller: &Caller,
        parent: Ino,
        name: &str,
        node: NewNode,
        attrs: &SetAttrs,
    ) -> Result<(Ino, Inode)> {
        validate_name(name)?;
        self.write(|w| {
            let mut p = w.get(parent)?;
            if p.kind != Kind::Dir {
                return Err(FsError::NotDirectory);
            }
            if p.nlink == 0 {
                return Err(FsError::NotFound); // directory was removed
            }
            if w.lookup(parent, name)?.is_some() {
                return Err(FsError::AlreadyExists);
            }
            let (kind, default_mode) = match &node {
                NewNode::File => (Kind::File, 0o644),
                NewNode::Dir => (Kind::Dir, 0o755),
                NewNode::Symlink(_) => (Kind::Symlink, 0o777),
                NewNode::Fifo => (Kind::Fifo, 0o644),
                NewNode::Socket => (Kind::Socket, 0o755),
                NewNode::CharDev(..) => (Kind::CharDev, 0o644),
                NewNode::BlockDev(..) => (Kind::BlockDev, 0o644),
            };
            let ino = w.alloc(NEXT_INO, FIRST_INO)?;
            // BSD semantics: new objects inherit the parent directory's group.
            let mut inode = Inode::new(
                kind,
                attrs.mode.unwrap_or(default_mode),
                caller.uid,
                p.gid,
                w.next_change(),
            );
            match node {
                NewNode::Dir => inode.parent = parent,
                NewNode::Symlink(target) => {
                    inode.size = target.len() as u64;
                    inode.target = target.into_bytes();
                }
                NewNode::CharDev(ma, mi) | NewNode::BlockDev(ma, mi) => inode.rdev = (ma, mi),
                _ => {}
            }
            let mut initial = attrs.clone();
            initial.mode = None;
            if kind == Kind::Symlink {
                initial.size = None;
            }
            apply_setattr(&self.hot, ino, &mut inode, &initial, caller, true)?;
            w.put(ino, &inode)?;
            w.link_entry(parent, name, ino)?;
            p.size += 1;
            if kind == Kind::Dir {
                p.nlink += 1;
            }
            w.touch(&mut p);
            w.put(parent, &p)?;
            Ok((ino, inode))
        })
    }

    pub fn link(&self, ino: Ino, parent: Ino, name: &str) -> Result<()> {
        validate_name(name)?;
        self.write(|w| {
            let mut inode = w.get(ino)?;
            if inode.kind == Kind::Dir {
                return Err(FsError::IsDirectory);
            }
            if inode.nlink == 0 {
                return Err(FsError::Stale);
            }
            let mut p = w.get(parent)?;
            if p.kind != Kind::Dir {
                return Err(FsError::NotDirectory);
            }
            if w.lookup(parent, name)?.is_some() {
                return Err(FsError::AlreadyExists);
            }
            inode.nlink += 1;
            w.touch_ctime(&mut inode);
            w.put(ino, &inode)?;
            w.link_entry(parent, name, ino)?;
            p.size += 1;
            w.touch(&mut p);
            w.put(parent, &p)
        })
    }

    pub fn remove(&self, parent: Ino, name: &str) -> Result<()> {
        self.write(|w| {
            let mut p = w.get(parent)?;
            if p.kind != Kind::Dir {
                return Err(FsError::NotDirectory);
            }
            let (child, cookie) = w.lookup(parent, name)?.ok_or(FsError::NotFound)?;
            let c = w.get(child)?;
            if c.kind == Kind::Dir {
                if c.size > 0 {
                    return Err(FsError::NotEmpty);
                }
                p.nlink = p.nlink.saturating_sub(1);
            }
            w.unlink_entry(parent, name, cookie)?;
            w.drop_link(child, c)?;
            p.size = p.size.saturating_sub(1);
            w.touch(&mut p);
            w.put(parent, &p)
        })
    }

    pub fn rename(&self, from_dir: Ino, from: &str, to_dir: Ino, to: &str) -> Result<()> {
        validate_name(to)?;
        self.write(|w| {
            let (src, src_cookie) = w.lookup(from_dir, from)?.ok_or(FsError::NotFound)?;
            if from_dir == to_dir && from == to {
                return Ok(());
            }
            let mut s = w.get(src)?;
            let mut fp = w.get(from_dir)?;
            if fp.kind != Kind::Dir {
                return Err(FsError::NotDirectory);
            }
            let same_dir = from_dir == to_dir;
            let mut tp = if same_dir { None } else { Some(w.get(to_dir)?) };
            if tp.as_ref().is_some_and(|t| t.kind != Kind::Dir) {
                return Err(FsError::NotDirectory);
            }

            // A directory may not move underneath itself.
            if s.kind == Kind::Dir && !same_dir {
                let mut cur = to_dir;
                loop {
                    if cur == src {
                        return Err(FsError::InvalidInput);
                    }
                    if cur == ROOT_INO {
                        break;
                    }
                    cur = w.get(cur)?.parent;
                }
            }

            let mut replaced_dir = false;
            if let Some((dst, dst_cookie)) = w.lookup(to_dir, to)? {
                if dst == src {
                    // Both names are links to the same inode: POSIX says no-op.
                    return Ok(());
                }
                let d = w.get(dst)?;
                match (s.kind == Kind::Dir, d.kind == Kind::Dir) {
                    (true, false) => return Err(FsError::NotDirectory),
                    (false, true) => return Err(FsError::IsDirectory),
                    (true, true) if d.size > 0 => return Err(FsError::NotEmpty),
                    _ => {}
                }
                replaced_dir = d.kind == Kind::Dir;
                w.unlink_entry(to_dir, to, dst_cookie)?;
                w.drop_link(dst, d)?;
            } else {
                // New name: the target directory gains an entry.
                tp.as_mut().unwrap_or(&mut fp).size += 1;
            }

            w.unlink_entry(from_dir, from, src_cookie)?;
            w.link_entry(to_dir, to, src)?;
            fp.size = fp.size.saturating_sub(1);

            let tpr = tp.as_mut().unwrap_or(&mut fp);
            if replaced_dir {
                tpr.nlink = tpr.nlink.saturating_sub(1);
            }
            if s.kind == Kind::Dir && !same_dir {
                s.parent = to_dir;
                fp.nlink = fp.nlink.saturating_sub(1);
                if let Some(t) = tp.as_mut() {
                    t.nlink += 1;
                }
            }
            w.touch_ctime(&mut s);
            w.put(src, &s)?;
            w.touch(&mut fp);
            w.put(from_dir, &fp)?;
            if let Some(mut t) = tp {
                w.touch(&mut t);
                w.put(to_dir, &t)?;
            }
            Ok(())
        })
    }

    // ----- attributes & data ------------------------------------------------

    pub fn setattr(&self, caller: &Caller, ino: Ino, attrs: &SetAttrs) -> Result<Inode> {
        loop {
            let done = self.write(|w| {
                let mut inode = w.get(ino)?;
                if let Some(size) = attrs.size
                    && size != inode.size
                    && matches!(inode.data, Data::Chunked { .. } | Data::Inline { .. })
                {
                    if size != 0 {
                        return Ok(None); // needs the bytes back first
                    }
                    // Truncate to zero (O_TRUNC): no need to decompress.
                    let old = std::mem::replace(&mut inode.data, Data::Empty);
                    w.release_data(ino, &old)?;
                }
                apply_setattr(&self.hot, ino, &mut inode, attrs, caller, false)?;
                inode.change = w.next_change();
                w.put(ino, &inode)?;
                Ok(Some(inode))
            })?;
            match done {
                Some(inode) => {
                    if attrs.size.is_some() && inode.data == Data::Hot {
                        self.note_modified(ino);
                    }
                    return Ok(inode);
                }
                None => self.rehydrate(ino)?,
            }
        }
    }

    pub fn write_data(&self, ino: Ino, offset: u64, data: &[u8]) -> Result<u32> {
        let end = offset
            .checked_add(data.len() as u64)
            .filter(|&e| e <= MAX_FILE_SIZE)
            .ok_or(FsError::FileTooLarge)?;
        loop {
            let inode = self.getattr(ino)?;
            match inode.kind {
                Kind::File => {}
                Kind::Dir => return Err(FsError::IsDirectory),
                _ => return Err(FsError::InvalidInput),
            }
            match inode.data {
                Data::Hot => {}
                Data::Empty => {
                    // Materialize the hot file (preserving any sparse length).
                    self.write(|w| {
                        let mut inode = w.get(ino)?;
                        if inode.data == Data::Empty {
                            self.hot.create(ino, inode.size).map_err(io_err)?;
                            inode.data = Data::Hot;
                            w.put(ino, &inode)?;
                        }
                        Ok(())
                    })?;
                    continue;
                }
                Data::Chunked { .. } | Data::Inline { .. } => {
                    self.rehydrate(ino)?;
                    continue;
                }
            }
            // Data goes to the page cache outside the metadata lock.
            self.hot.write_at(ino, offset, data).map_err(io_err)?;
            let committed = self.write(|w| {
                let mut inode = w.get(ino)?;
                if inode.data != Data::Hot {
                    // Compressed concurrently: our bytes may have missed the
                    // compressed copy. Rehydrate and write again.
                    return Ok(false);
                }
                if end > inode.size {
                    inode.size = end;
                }
                let now = Ts::now();
                inode.mtime = now;
                inode.ctime = now;
                // Writing clears setuid/setgid like local filesystems do.
                inode.mode &= !0o6000;
                inode.change = w.next_change();
                w.put(ino, &inode)?;
                Ok(true)
            })?;
            if committed {
                self.note_modified(ino);
                return Ok(data.len() as u32);
            }
        }
    }

    pub fn sync_mode(&self) -> SyncMode {
        self.sync_mode
    }

    /// NFS COMMIT / fsync. In relaxed mode everything acknowledged is
    /// already in the page cache + WAL, so there is nothing to wait for.
    pub fn commit(&self, ino: Ino) -> Result<()> {
        if self.sync_mode == SyncMode::Relaxed {
            return Ok(());
        }
        self.hot.sync(ino).map_err(io_err)?;
        self.sync_metadata();
        Ok(())
    }

    // ----- xattrs -----------------------------------------------------------

    pub fn list_xattrs(&self, ino: Ino) -> Result<Vec<String>> {
        let _ = self.getattr(ino)?;
        let txn = self.db.begin_read().db()?;
        let t = txn.open_table(XATTRS).db()?;
        t.range((ino, "")..(ino + 1, ""))
            .db()?
            .map(|r| r.map(|(k, _)| k.value().1.to_owned()))
            .collect::<std::result::Result<_, _>>()
            .db()
    }

    pub fn xattr_exists(&self, ino: Ino, name: &str) -> Result<bool> {
        match self.get_xattr(ino, name) {
            Ok(_) => Ok(true),
            Err(FsError::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub fn get_xattr(&self, ino: Ino, name: &str) -> Result<Vec<u8>> {
        let _ = self.getattr(ino)?;
        let txn = self.db.begin_read().db()?;
        let t = txn.open_table(XATTRS).db()?;
        let v = t.get((ino, name)).db()?.ok_or(FsError::NotFound)?;
        Ok(v.value().to_vec())
    }

    pub fn set_xattr(&self, ino: Ino, name: &str, value: &[u8], mode: XattrSetMode) -> Result<()> {
        if name.is_empty() || name.len() > MAX_NAME_BYTES {
            return Err(FsError::InvalidInput);
        }
        self.write(|w| {
            let mut inode = w.get(ino)?;
            let exists = w.xattrs.get((ino, name)).db()?.is_some();
            match (mode, exists) {
                (XattrSetMode::CreateOnly, true) => return Err(FsError::AlreadyExists),
                (XattrSetMode::ReplaceOnly, false) => return Err(FsError::NotFound),
                _ => {}
            }
            w.put_xattr(ino, name, value)?;
            if !exists {
                inode.xattr_count += 1;
            }
            w.touch_ctime(&mut inode);
            w.put(ino, &inode)
        })
    }

    pub fn remove_xattr(&self, ino: Ino, name: &str) -> Result<()> {
        self.write(|w| {
            let mut inode = w.get(ino)?;
            if !w.del_xattr(ino, name)? {
                return Err(FsError::NotFound);
            }
            inode.xattr_count = inode.xattr_count.saturating_sub(1);
            w.touch_ctime(&mut inode);
            w.put(ino, &inode)
        })
    }
}

impl Drop for Vfs {
    fn drop(&mut self) {
        if self.dirty.load(Ordering::Acquire)
            && let Err(e) = self.durable_commit()
        {
            error!("final durable commit failed: {e:?}");
        }
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0')
    {
        return Err(FsError::InvalidInput);
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(FsError::NameTooLong);
    }
    Ok(())
}

fn resolve_time(t: SetTime) -> Ts {
    match t {
        SetTime::ServerNow => Ts::now(),
        SetTime::Client(ts) => Ts {
            sec: ts.seconds,
            nsec: ts.nanos,
        },
    }
}

/// Applies a SETATTR (or the initial attributes of a CREATE) to `inode`.
fn apply_setattr(
    hot: &HotStore,
    ino: Ino,
    inode: &mut Inode,
    attrs: &SetAttrs,
    caller: &Caller,
    creating: bool,
) -> Result<()> {
    let is_owner = caller.uid == 0 || caller.uid == inode.uid;

    if let Some(uid) = attrs.uid
        && uid != inode.uid
        && caller.uid != 0
        && !creating
    {
        return Err(FsError::PermissionDenied);
    }
    if let Some(gid) = attrs.gid
        && gid != inode.gid
        && !creating
        && caller.uid != 0
        && !(caller.uid == inode.uid && caller.in_group(gid))
    {
        return Err(FsError::PermissionDenied);
    }
    if attrs.mode.is_some() && !is_owner && !creating {
        return Err(FsError::PermissionDenied);
    }

    let mut data_changed = false;
    if let Some(size) = attrs.size {
        match inode.kind {
            Kind::File => {}
            Kind::Dir => return Err(FsError::IsDirectory),
            _ => return Err(FsError::InvalidInput),
        }
        if size > MAX_FILE_SIZE {
            return Err(FsError::FileTooLarge);
        }
        if size != inode.size {
            match inode.data {
                Data::Hot => hot.set_len(ino, size).map_err(io_err)?,
                Data::Empty => {} // reads synthesize zeros up to `size`
                // Callers rehydrate (or drop) compressed data before resizing.
                Data::Chunked { .. } | Data::Inline { .. } => return Err(FsError::Io),
            }
            inode.size = size;
            data_changed = true;
        }
    }

    if let Some(mode) = attrs.mode {
        let mut mode = mode & 0o7777;
        // Non-root callers can't set setgid on files outside their groups.
        if caller.uid != 0 && inode.kind != Kind::Dir && !caller.in_group(inode.gid) {
            mode &= !0o2000;
        }
        inode.mode = mode;
    }
    if let Some(uid) = attrs.uid {
        if uid != inode.uid && !creating && caller.uid != 0 {
            return Err(FsError::PermissionDenied);
        }
        if uid != inode.uid && inode.kind != Kind::Dir {
            inode.mode &= !0o6000;
        }
        inode.uid = uid;
    }
    if let Some(gid) = attrs.gid {
        if gid != inode.gid && inode.kind != Kind::Dir && caller.uid != 0 {
            inode.mode &= !0o6000;
        }
        inode.gid = gid;
    }
    if let Some(t) = attrs.atime {
        inode.atime = resolve_time(t);
    }
    if let Some(t) = attrs.mtime {
        inode.mtime = resolve_time(t);
    } else if data_changed {
        inode.mtime = Ts::now();
    }
    if let Some(t) = attrs.birthtime {
        inode.btime = resolve_time(t);
    }
    let set_flag = |flags: &mut u8, bit: u8, v: Option<bool>| {
        if let Some(v) = v {
            if v {
                *flags |= bit;
            } else {
                *flags &= !bit;
            }
        }
    };
    set_flag(&mut inode.flags, FLAG_HIDDEN, attrs.hidden);
    set_flag(&mut inode.flags, FLAG_ARCHIVE, attrs.archive);
    set_flag(&mut inode.flags, FLAG_SYSTEM, attrs.system);
    if !creating {
        inode.ctime = Ts::now();
    }
    Ok(())
}

fn allowed_access(caller: &Caller, inode: &Inode) -> u32 {
    use access::*;
    let is_dir = inode.kind == Kind::Dir;
    if caller.uid == 0 {
        let mut all = READ | LOOKUP | MODIFY | EXTEND | DELETE;
        if is_dir || inode.mode & 0o111 != 0 {
            all |= EXECUTE;
        }
        return all;
    }
    let bits = if caller.uid == inode.uid {
        (inode.mode >> 6) & 7
    } else if caller.in_group(inode.gid) {
        (inode.mode >> 3) & 7
    } else {
        inode.mode & 7
    };
    let mut out = 0;
    if bits & 4 != 0 {
        out |= READ;
    }
    if bits & 2 != 0 {
        out |= MODIFY | EXTEND;
        if is_dir {
            out |= DELETE;
        }
    }
    if bits & 1 != 0 {
        out |= if is_dir { LOOKUP } else { EXECUTE };
    }
    out
}

/// Runs background work on efficiency cores where available.
fn lower_thread_priority() {
    #[cfg(target_os = "macos")]
    // SAFETY: only adjusts the calling thread's own QoS class.
    unsafe {
        let _ = libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
    }
}
