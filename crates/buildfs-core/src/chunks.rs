//! Content-addressed chunk store (the compressed tier).
//!
//! Files are split into `CHUNK_SIZE` pieces identified by the BLAKE3 hash
//! of their uncompressed bytes. Each distinct chunk is stored once, zstd
//! compressed (or raw when that doesn't help), so identical content in
//! different files, builds or projects shares storage. Reference counts and
//! small chunks live in redb (see `vfs.rs`); chunks above
//! [`SMALL_CHUNK_MAX`] are files `chunks/<hh>/<hash>` written once via
//! rename and deleted when their count drops to zero.
//!
//! ## Shared pool
//!
//! Stores can't share a metadata database (redb allows one process), but they
//! can share chunk files. With a pool directory on the same file system, a
//! store publishes each chunk file as a hard link `<pool>/<hh>/<hash>`, and
//! another store needing that chunk links it into its own `chunks/` instead
//! of compressing it again. The file's link count is the reference count
//! across stores, so no coordination is needed.
//!
//! **A store only ever uses its own link.** The pool is an index for finding
//! chunks, so anything that happens to a pool entry costs at most a duplicate
//! or an orphan, never data. Orphans (link count 1) are swept at startup and
//! when a store is deleted. Entries are verified against the chunk's bytes
//! before being adopted, so damage (power loss; chunk files aren't fsynced)
//! doesn't spread to another store through the pool.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

pub const CHUNK_SIZE: u32 = 128 * 1024;
/// Compressed chunks up to this size are kept in redb instead of a file
/// (a file costs at least one 4 KiB block).
pub const SMALL_CHUNK_MAX: usize = 2048;

pub type Hash = [u8; 32];

pub fn hash(data: &[u8]) -> Hash {
    *blake3::hash(data).as_bytes()
}

pub fn hex(h: &Hash) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Hash> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// A chunk ready to be stored.
pub struct Encoded {
    pub bytes: Vec<u8>,
    pub raw: bool,
}

/// Compresses a chunk, falling back to raw storage when zstd saves less
/// than 1/16th.
pub fn encode(compressor: &mut zstd::bulk::Compressor<'_>, data: &[u8]) -> io::Result<Encoded> {
    let compressed = compressor.compress(data)?;
    if compressed.len() + data.len() / 16 < data.len() {
        Ok(Encoded {
            bytes: compressed,
            raw: false,
        })
    } else {
        Ok(Encoded {
            bytes: data.to_vec(),
            raw: true,
        })
    }
}

pub fn decode(stored: Vec<u8>, raw: bool, logical_len: usize) -> io::Result<Vec<u8>> {
    let data = if raw {
        stored
    } else {
        zstd::bulk::decompress(&stored, logical_len)?
    };
    if data.len() != logical_len {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "chunk size mismatch"));
    }
    Ok(data)
}

/// Bounded LRU of decompressed chunks keyed by hash (so it dedups too).
struct Cache {
    map: HashMap<Hash, (Arc<Vec<u8>>, u64)>,
    tick: u64,
    bytes: usize,
    max_bytes: usize,
}

impl Cache {
    fn get(&mut self, h: &Hash) -> Option<Arc<Vec<u8>>> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(h).map(|(v, t)| {
            *t = tick;
            v.clone()
        })
    }

    fn insert(&mut self, h: Hash, v: Arc<Vec<u8>>) {
        self.tick += 1;
        self.bytes += v.len();
        if let Some((old, _)) = self.map.insert(h, (v, self.tick)) {
            self.bytes -= old.len();
        }
        if self.bytes > self.max_bytes && !self.map.is_empty() {
            // Evict the least recently used half in one pass.
            let mut ticks: Vec<u64> = self.map.values().map(|(_, t)| *t).collect();
            ticks.sort_unstable();
            let cutoff = ticks[ticks.len() / 2];
            let mut freed = 0;
            self.map.retain(|_, (v, t)| {
                let keep = *t > cutoff;
                if !keep {
                    freed += v.len();
                }
                keep
            });
            self.bytes -= freed;
        }
    }
}

/// Whether `stored` is `data`, raw or compressed; `Some(raw)` if so.
fn verify_stored(stored: &[u8], data: &[u8]) -> Option<bool> {
    if stored == data {
        return Some(true);
    }
    match decode(stored.to_vec(), false, data.len()) {
        Ok(d) if d == data => Some(false),
        _ => None,
    }
}

fn pool_entry(pool: &Path, h: &Hash) -> PathBuf {
    let name = hex(h);
    pool.join(&name[..2]).join(name)
}

/// Removes pool entries no store links to any more. Safe at any time: a
/// store that is linked has link count 2 or more, and one that loses a race
/// with this simply writes its own copy.
pub fn sweep_pool(pool: &Path) -> io::Result<usize> {
    let mut removed = 0;
    let shards = match std::fs::read_dir(pool) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    for shard in shards {
        let shard = shard?;
        if !shard.file_type()?.is_dir() {
            continue;
        }
        for e in std::fs::read_dir(shard.path())? {
            let e = e?;
            if e.metadata().is_ok_and(|m| m.nlink() == 1) && std::fs::remove_file(e.path()).is_ok() {
                removed += 1;
            }
        }
    }
    Ok(removed)
}

pub struct ChunkStore {
    root: PathBuf,
    cache: Mutex<Cache>,
    pool: Option<PathBuf>,
}

impl ChunkStore {
    pub fn open(root: impl Into<PathBuf>, cache_bytes: usize) -> io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(root.join("tmp"))?;
        for e in std::fs::read_dir(root.join("tmp"))? {
            let _ = std::fs::remove_file(e?.path());
        }
        Ok(Self {
            root,
            cache: Mutex::new(Cache {
                map: HashMap::new(),
                tick: 0,
                bytes: 0,
                max_bytes: cache_bytes,
            }),
            pool: None,
        })
    }

    /// Shares chunk files with other stores through `pool` (see the module
    /// docs). It must be on the same file system as the store.
    pub fn with_pool(mut self, pool: PathBuf) -> io::Result<Self> {
        std::fs::create_dir_all(&pool)?;
        self.pool = Some(pool);
        Ok(self)
    }

    pub fn pool(&self) -> Option<&Path> {
        self.pool.as_deref()
    }

    /// Finds `data` as an existing chunk file: our own (another compaction
    /// may have just written it) or, via a new hard link, the pool's.
    /// Returns its stored length and whether it's raw. `None`: compress it.
    pub fn adopt(&self, h: &Hash, data: &[u8]) -> Option<(u32, bool)> {
        let pool = self.pool.as_ref()?;
        let check = |p: &Path| -> Option<(u32, bool)> {
            let stored = std::fs::read(p).ok()?;
            let raw = verify_stored(&stored, data)?;
            Some((u32::try_from(stored.len()).ok()?, raw))
        };
        let own = self.path(h);
        if own.exists() {
            return check(&own);
        }
        let entry = pool_entry(pool, h);
        let Some(found) = check(&entry) else {
            if entry.exists() {
                // Damaged. Other stores still have their own links, so
                // dropping the name is safe, and lets a good copy replace it.
                tracing::warn!("removing damaged pool chunk {}", hex(h));
                let _ = std::fs::remove_file(&entry);
            }
            return None;
        };
        if let Some(parent) = own.parent() {
            std::fs::create_dir_all(parent).ok()?;
        }
        match std::fs::hard_link(&entry, &own) {
            Ok(()) => Some(found),
            // Written meanwhile by another compaction here: use that.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => check(&own),
            // Removed meanwhile (or anything else): the pool is best effort.
            Err(_) => None,
        }
    }

    /// Offers our chunk file to other stores (best effort).
    pub fn publish(&self, h: &Hash) {
        let Some(pool) = &self.pool else { return };
        let entry = pool_entry(pool, h);
        if let Some(parent) = entry.parent()
            && std::fs::create_dir_all(parent).is_ok()
        {
            let _ = std::fs::hard_link(self.path(h), entry);
        }
    }

    /// Our link to `h` is gone: drop the pool entry if nobody else has one.
    fn pool_release(&self, h: &Hash) {
        let Some(pool) = &self.pool else { return };
        let entry = pool_entry(pool, h);
        if std::fs::metadata(&entry).is_ok_and(|m| m.nlink() == 1) {
            let _ = std::fs::remove_file(entry);
        }
    }

    pub fn path(&self, h: &Hash) -> PathBuf {
        let name = hex(h);
        self.root.join(&name[..2]).join(name)
    }

    /// Length of the chunk file, if it exists.
    pub fn file_len(&self, h: &Hash) -> Option<u64> {
        std::fs::metadata(self.path(h)).ok().map(|m| m.len())
    }

    /// Writes a large chunk file (idempotent: same hash, same content).
    /// Returns the bytes allocated on disk.
    pub fn write_file(&self, h: &Hash, bytes: &[u8]) -> io::Result<u64> {
        let dst = self.path(h);
        if let Ok(md) = std::fs::metadata(&dst) {
            return Ok(md.blocks() * 512);
        }
        // Unique per writer: concurrent compactions often store the very
        // same chunk at the same moment.
        static NEXT_TMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tmp = self.root.join("tmp").join(format!(
            "{}-{}-{}",
            hex(h),
            std::process::id(),
            NEXT_TMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        let disk = f.metadata()?.blocks() * 512;
        drop(f);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&tmp, &dst)?;
        Ok(disk)
    }

    pub fn read_file(&self, h: &Hash, stored_len: usize) -> io::Result<Vec<u8>> {
        let f = File::open(self.path(h))?;
        let mut buf = vec![0u8; stored_len];
        f.read_exact_at(&mut buf, 0)?;
        Ok(buf)
    }

    pub fn remove_file(&self, h: &Hash) -> io::Result<()> {
        let _ = self.cache.lock().map.remove(h);
        let r = match std::fs::remove_file(self.path(h)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        };
        self.pool_release(h);
        r
    }

    pub fn cached(&self, h: &Hash) -> Option<Arc<Vec<u8>>> {
        self.cache.lock().get(h)
    }

    pub fn cache_insert(&self, h: Hash, data: Arc<Vec<u8>>) {
        self.cache.lock().insert(h, data);
    }

    /// Hashes of all chunk files on disk (for GC).
    pub fn list_files(&self) -> io::Result<Vec<Hash>> {
        let mut out = Vec::new();
        for shard in std::fs::read_dir(&self.root)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() || shard.file_name() == "tmp" {
                continue;
            }
            for e in std::fs::read_dir(shard.path())? {
                if let Some(h) = e?.file_name().to_str().and_then(unhex) {
                    out.push(h);
                }
            }
        }
        Ok(out)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip_and_encoding() {
        let h = hash(b"chunk");
        assert_eq!(unhex(&hex(&h)), Some(h));
        let mut c = zstd::bulk::Compressor::new(3).unwrap();
        let text = b"abcabcabcabc".repeat(1000);
        let e = encode(&mut c, &text).unwrap();
        assert!(!e.raw && e.bytes.len() < text.len());
        assert_eq!(decode(e.bytes, false, text.len()).unwrap(), text);
        let mut x = 0x2545_f491u32;
        let noise: Vec<u8> = (0..4096)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        let e = encode(&mut c, &noise).unwrap();
        assert!(e.raw);
    }

    #[test]
    fn files_are_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let s = ChunkStore::open(tmp.path(), 1 << 20).unwrap();
        let h = hash(b"x");
        let _ = s.write_file(&h, b"payload").unwrap();
        let _ = s.write_file(&h, b"payload").unwrap();
        assert_eq!(s.read_file(&h, 7).unwrap(), b"payload");
        assert_eq!(s.list_files().unwrap(), vec![h]);
        s.remove_file(&h).unwrap();
        assert!(s.list_files().unwrap().is_empty());
    }

    fn pooled(dir: &Path, name: &str) -> ChunkStore {
        ChunkStore::open(dir.join(name), 1 << 20)
            .unwrap()
            .with_pool(dir.join("pool"))
            .unwrap()
    }

    fn compressed(data: &[u8], level: i32) -> Encoded {
        encode(&mut zstd::bulk::Compressor::new(level).unwrap(), data).unwrap()
    }

    #[test]
    fn pool_shares_files_and_counts_links() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (pooled(tmp.path(), "a"), pooled(tmp.path(), "b"));
        let data = b"shared chunk contents ".repeat(500);
        let h = hash(&data);
        assert_eq!(b.adopt(&h, &data), None, "nothing published yet");
        let enc = compressed(&data, 3);
        let _ = a.write_file(&h, &enc.bytes).unwrap();
        a.publish(&h);
        assert_eq!(b.adopt(&h, &data), Some((enc.bytes.len() as u32, false)));
        let ino = |s: &ChunkStore| std::fs::metadata(s.path(&h)).unwrap().ino();
        assert_eq!(ino(&a), ino(&b));
        assert_eq!(std::fs::metadata(a.path(&h)).unwrap().nlink(), 3);
        // A store leaving doesn't touch the others, and the last one out
        // takes the pool entry along.
        a.remove_file(&h).unwrap();
        assert_eq!(b.read_file(&h, enc.bytes.len()).unwrap(), enc.bytes);
        assert!(pool_entry(&tmp.path().join("pool"), &h).exists());
        b.remove_file(&h).unwrap();
        assert!(!pool_entry(&tmp.path().join("pool"), &h).exists());
    }

    #[test]
    fn pool_detects_raw_chunks_and_foreign_levels() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (pooled(tmp.path(), "a"), pooled(tmp.path(), "b"));
        // Raw: incompressible, stored as is.
        let mut x = 0x2545_f491u32;
        let noise: Vec<u8> = (0..8192)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        let h = hash(&noise);
        let _ = a.write_file(&h, &noise).unwrap();
        a.publish(&h);
        assert_eq!(b.adopt(&h, &noise), Some((noise.len() as u32, true)));
        // Another store's compression level: its length, not ours.
        let data: Vec<u8> = (0..64_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                b"abcdefgh"[(x % 8) as usize]
            })
            .collect();
        let h = hash(&data);
        let enc = compressed(&data, 19);
        assert_ne!(enc.bytes.len(), compressed(&data, 1).bytes.len());
        let _ = a.write_file(&h, &enc.bytes).unwrap();
        a.publish(&h);
        assert_eq!(b.adopt(&h, &data), Some((enc.bytes.len() as u32, false)));
    }

    #[test]
    fn pool_rejects_and_removes_damaged_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (pooled(tmp.path(), "a"), pooled(tmp.path(), "b"));
        let data = b"will be damaged ".repeat(1000);
        let h = hash(&data);
        let entry = pool_entry(&tmp.path().join("pool"), &h);
        std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
        // Truncated, and wrong content under the right name.
        for bad in [&b""[..], b"garbage", &compressed(b"other", 3).bytes] {
            std::fs::write(&entry, bad).unwrap();
            assert_eq!(b.adopt(&h, &data), None);
            assert!(!entry.exists());
            assert!(!b.path(&h).exists());
        }
        // A good copy can take its place.
        let enc = compressed(&data, 3);
        let _ = a.write_file(&h, &enc.bytes).unwrap();
        a.publish(&h);
        assert!(b.adopt(&h, &data).is_some());
    }

    #[test]
    fn pool_sweep_removes_only_orphans() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = tmp.path().join("pool");
        assert_eq!(sweep_pool(&pool).unwrap(), 0, "no pool yet");
        let a = pooled(tmp.path(), "a");
        let (keep, gone) = (hash(b"keep"), hash(b"gone"));
        for h in [keep, gone] {
            let _ = a.write_file(&h, b"x").unwrap();
            a.publish(&h);
        }
        // What deleting a store leaves: an entry nobody else links to.
        std::fs::remove_file(a.path(&gone)).unwrap();
        assert_eq!(sweep_pool(&pool).unwrap(), 1);
        assert!(pool_entry(&pool, &keep).exists());
        assert!(!pool_entry(&pool, &gone).exists());
    }

    #[test]
    fn without_a_pool_nothing_is_adopted() {
        let tmp = tempfile::tempdir().unwrap();
        let s = ChunkStore::open(tmp.path(), 1 << 20).unwrap();
        let h = hash(b"x");
        let _ = s.write_file(&h, b"x").unwrap();
        s.publish(&h);
        assert_eq!(s.adopt(&h, b"x"), None);
        assert_eq!(s.pool(), None);
    }
}
