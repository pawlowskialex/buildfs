//! Content-addressed chunk store (the compressed tier).
//!
//! Files are split into `CHUNK_SIZE` pieces identified by the BLAKE3 hash
//! of their uncompressed bytes. Each distinct chunk is stored once, zstd
//! compressed (or raw when that doesn't help), so identical content in
//! different files, builds or projects shares storage. Reference counts and
//! small chunks live in redb (see `vfs.rs`); chunks above
//! [`SMALL_CHUNK_MAX`] are files `chunks/<hh>/<hash>` written once via
//! rename and deleted when their count drops to zero.

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

pub struct ChunkStore {
    root: PathBuf,
    cache: Mutex<Cache>,
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
        })
    }

    pub fn path(&self, h: &Hash) -> PathBuf {
        let name = hex(h);
        self.root.join(&name[..2]).join(name)
    }

    pub fn file_exists(&self, h: &Hash) -> bool {
        self.path(h).exists()
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
        match std::fs::remove_file(self.path(h)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
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
}
