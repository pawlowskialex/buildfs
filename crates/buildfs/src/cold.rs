//! Cold data tier: compressed, immutable, randomly readable file images.
//!
//! A cold file `cold/<shard>/<ino>-<gen>` holds a header, a chunk index and
//! the chunks. Each `CHUNK_SIZE` slice of the logical file is compressed with
//! zstd independently (or stored raw when compression doesn't help), so a
//! read only decompresses the chunks it touches. Files are written once to a
//! temp path and renamed into place, and never modified afterwards; a new
//! version gets a new generation number, which also keys the chunk cache.
//!
//! Layout (little endian):
//! ```text
//! magic "BFSC" | version u8 | codec u8 | reserved u16 | chunk_size u32
//! size u64 | chunk_count u32
//! chunk_count * (offset u64, stored_len u32, raw u8)
//! chunk data...
//! ```

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::meta::Ino;

pub const CHUNK_SIZE: u32 = 128 * 1024;
const MAGIC: &[u8; 4] = b"BFSC";
const VERSION: u8 = 1;
const CODEC_ZSTD: u8 = 1;
const HEADER_LEN: usize = 4 + 1 + 1 + 2 + 4 + 8 + 4;
const ENTRY_LEN: usize = 8 + 4 + 1;

/// Store a chunk raw unless compression saves at least 1/16th.
fn worth_compressing(raw: usize, compressed: usize) -> bool {
    compressed + raw / 16 < raw
}

#[derive(Debug, Clone, Copy)]
struct ChunkEntry {
    offset: u64,
    stored_len: u32,
    raw: bool,
}

struct ColdFile {
    file: File,
    chunk_size: u32,
    size: u64,
    index: Vec<ChunkEntry>,
}

/// Result of compressing a file into the cold tier.
pub struct Written {
    /// Temp path; rename to [`ColdStore::path`] to publish.
    pub temp: PathBuf,
    /// Bytes allocated on the backing filesystem.
    pub disk_bytes: u64,
}

/// `(inode, generation, chunk index)`.
type ChunkKey = (Ino, u64, u32);

/// Small bounded LRU of decompressed chunks (value, last-use tick).
struct ChunkCache {
    map: HashMap<ChunkKey, (Arc<Vec<u8>>, u64)>,
    tick: u64,
    bytes: usize,
    max_bytes: usize,
}

impl ChunkCache {
    fn get(&mut self, key: ChunkKey) -> Option<Arc<Vec<u8>>> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(&key).map(|(v, t)| {
            *t = tick;
            v.clone()
        })
    }

    fn insert(&mut self, key: ChunkKey, value: Arc<Vec<u8>>) {
        self.tick += 1;
        self.bytes += value.len();
        if let Some((old, _)) = self.map.insert(key, (value, self.tick)) {
            self.bytes -= old.len();
        }
        if self.bytes > self.max_bytes {
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

    fn forget(&mut self, ino: Ino) {
        let mut freed = 0;
        self.map.retain(|k, (v, _)| {
            let keep = k.0 != ino;
            if !keep {
                freed += v.len();
            }
            keep
        });
        self.bytes -= freed;
    }
}

pub struct ColdStore {
    root: PathBuf,
    level: i32,
    files: Mutex<HashMap<(Ino, u64), Arc<ColdFile>>>,
    cache: Mutex<ChunkCache>,
}

impl ColdStore {
    pub fn open(root: impl Into<PathBuf>, level: i32, cache_bytes: usize) -> io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(root.join("tmp"))?;
        // Leftover temp files are never referenced by metadata.
        for e in std::fs::read_dir(root.join("tmp"))? {
            let _ = std::fs::remove_file(e?.path());
        }
        Ok(Self {
            root,
            level,
            files: Mutex::new(HashMap::new()),
            cache: Mutex::new(ChunkCache {
                map: HashMap::new(),
                tick: 0,
                bytes: 0,
                max_bytes: cache_bytes,
            }),
        })
    }

    pub fn path(&self, ino: Ino, gen_: u64) -> PathBuf {
        self.root
            .join(format!("{:02x}", ino & 0xff))
            .join(format!("{ino:016x}-{gen_:x}"))
    }

    /// Compresses `size` bytes obtained from `read(offset, len)` into a new
    /// temp file. The caller publishes it by renaming to [`Self::path`].
    pub fn write(
        &self,
        ino: Ino,
        size: u64,
        mut read: impl FnMut(u64, usize) -> io::Result<Vec<u8>>,
    ) -> io::Result<Written> {
        let chunk_count = size.div_ceil(u64::from(CHUNK_SIZE)) as usize;
        let data_start = (HEADER_LEN + chunk_count * ENTRY_LEN) as u64;
        let temp = self.root.join("tmp").join(format!(
            "{ino:016x}-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let file = File::create(&temp)?;
        let result = (|| {
            // Chunks are streamed after a placeholder header + index, which
            // are filled in once all chunk offsets are known.
            let mut out = std::io::BufWriter::with_capacity(1 << 20, &file);
            out.write_all(&vec![0u8; data_start as usize])?;
            let mut index = Vec::with_capacity(chunk_count);
            let mut pos = data_start;
            let mut compressor = zstd::bulk::Compressor::new(self.level)?;
            for i in 0..chunk_count {
                let off = i as u64 * u64::from(CHUNK_SIZE);
                let len = (size - off).min(u64::from(CHUNK_SIZE)) as usize;
                let raw = read(off, len)?;
                let compressed = compressor.compress(&raw)?;
                let (stored, is_raw) = if worth_compressing(raw.len(), compressed.len()) {
                    (compressed.as_slice(), false)
                } else {
                    (raw.as_slice(), true)
                };
                out.write_all(stored)?;
                index.push(ChunkEntry {
                    offset: pos,
                    stored_len: stored.len() as u32,
                    raw: is_raw,
                });
                pos += stored.len() as u64;
            }
            out.flush()?;
            drop(out);

            let mut head = Vec::with_capacity(data_start as usize);
            head.extend_from_slice(MAGIC);
            head.push(VERSION);
            head.push(CODEC_ZSTD);
            head.extend_from_slice(&0u16.to_le_bytes());
            head.extend_from_slice(&CHUNK_SIZE.to_le_bytes());
            head.extend_from_slice(&size.to_le_bytes());
            head.extend_from_slice(&(chunk_count as u32).to_le_bytes());
            for e in &index {
                head.extend_from_slice(&e.offset.to_le_bytes());
                head.extend_from_slice(&e.stored_len.to_le_bytes());
                head.push(u8::from(e.raw));
            }
            file.write_all_at(&head, 0)?;
            Ok(file.metadata()?.blocks() * 512)
        })();
        match result {
            Ok(disk_bytes) => Ok(Written { temp, disk_bytes }),
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                Err(e)
            }
        }
    }

    /// Discards an unpublished temp file.
    pub fn discard(&self, written: Written) {
        let _ = std::fs::remove_file(written.temp);
    }

    /// Moves a written temp file to its final location.
    pub fn publish(&self, written: &Written, ino: Ino, gen_: u64) -> io::Result<()> {
        let dst = self.path(ino, gen_);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&written.temp, dst)
    }

    fn open_file(&self, ino: Ino, gen_: u64) -> io::Result<Arc<ColdFile>> {
        if let Some(f) = self.files.lock().get(&(ino, gen_)) {
            return Ok(f.clone());
        }
        let file = File::open(self.path(ino, gen_))?;
        let mut header = [0u8; HEADER_LEN];
        file.read_exact_at(&mut header, 0)?;
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "corrupt cold file header");
        if &header[0..4] != MAGIC || header[4] != VERSION || header[5] != CODEC_ZSTD {
            return Err(bad());
        }
        let u32_at = |o: usize| u32::from_le_bytes([header[o], header[o + 1], header[o + 2], header[o + 3]]);
        let chunk_size = u32_at(8);
        let size = u64::from_le_bytes(header[12..20].try_into().map_err(|_| bad())?);
        let count = u32_at(20) as usize;
        if chunk_size == 0 || size.div_ceil(u64::from(chunk_size)) != count as u64 {
            return Err(bad());
        }
        let mut raw_index = vec![0u8; count * ENTRY_LEN];
        file.read_exact_at(&mut raw_index, HEADER_LEN as u64)?;
        let index = raw_index
            .chunks_exact(ENTRY_LEN)
            .map(|e| ChunkEntry {
                offset: u64::from_le_bytes(e[0..8].try_into().unwrap_or_default()),
                stored_len: u32::from_le_bytes(e[8..12].try_into().unwrap_or_default()),
                raw: e[12] != 0,
            })
            .collect();
        let cf = Arc::new(ColdFile {
            file,
            chunk_size,
            size,
            index,
        });
        let mut files = self.files.lock();
        if files.len() >= 1024
            && let Some(&victim) = files.keys().next()
        {
            let _ = files.remove(&victim);
        }
        let _ = files.insert((ino, gen_), cf.clone());
        Ok(cf)
    }

    fn chunk(&self, ino: Ino, gen_: u64, cf: &ColdFile, idx: u32) -> io::Result<Arc<Vec<u8>>> {
        if let Some(c) = self.cache.lock().get((ino, gen_, idx)) {
            return Ok(c);
        }
        let e = cf
            .index
            .get(idx as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "chunk out of range"))?;
        let mut stored = vec![0u8; e.stored_len as usize];
        cf.file.read_exact_at(&mut stored, e.offset)?;
        let logical = (cf.size - u64::from(idx) * u64::from(cf.chunk_size))
            .min(u64::from(cf.chunk_size)) as usize;
        let data = if e.raw {
            stored
        } else {
            zstd::bulk::decompress(&stored, logical)?
        };
        if data.len() != logical {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "chunk size mismatch"));
        }
        let data = Arc::new(data);
        self.cache.lock().insert((ino, gen_, idx), data.clone());
        Ok(data)
    }

    /// Reads `len` bytes at `offset` (caller clamps to the logical size).
    pub fn read_at(&self, ino: Ino, gen_: u64, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let cf = self.open_file(ino, gen_)?;
        let mut out = Vec::with_capacity(len);
        let cs = u64::from(cf.chunk_size);
        let mut pos = offset;
        let end = (offset + len as u64).min(cf.size);
        while pos < end {
            let idx = (pos / cs) as u32;
            let chunk = self.chunk(ino, gen_, &cf, idx)?;
            let within = (pos % cs) as usize;
            let take = ((end - pos) as usize).min(chunk.len() - within);
            out.extend_from_slice(&chunk[within..within + take]);
            pos += take as u64;
        }
        out.resize(len, 0);
        Ok(out)
    }

    pub fn remove(&self, ino: Ino, gen_: u64) -> io::Result<()> {
        let _ = self.files.lock().remove(&(ino, gen_));
        self.cache.lock().forget(ino);
        match std::fs::remove_file(self.path(ino, gen_)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    /// Lists `(ino, gen)` of every published cold file (for GC).
    pub fn list(&self) -> io::Result<Vec<(Ino, u64)>> {
        let mut out = Vec::new();
        for shard in std::fs::read_dir(&self.root)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() || shard.file_name() == "tmp" {
                continue;
            }
            for entry in std::fs::read_dir(shard.path())? {
                let name = entry?.file_name();
                let Some((ino, gen_)) = name.to_str().and_then(|n| n.split_once('-')) else {
                    continue;
                };
                if let (Ok(ino), Ok(gen_)) =
                    (u64::from_str_radix(ino, 16), u64::from_str_radix(gen_, 16))
                {
                    out.push((ino, gen_));
                }
            }
        }
        Ok(out)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Compresses a small file image for inline storage in the metadata db.
pub fn compress_inline(data: &[u8], level: i32) -> io::Result<Vec<u8>> {
    zstd::bulk::compress(data, level)
}

pub fn decompress_inline(stored: &[u8], size: usize) -> io::Result<Vec<u8>> {
    zstd::bulk::decompress(stored, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_random_access() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ColdStore::open(tmp.path(), 3, 1 << 20).unwrap();
        // Mix of compressible text and incompressible noise, > 2 chunks.
        let mut data = Vec::new();
        for i in 0..40_000u32 {
            data.extend_from_slice(format!("symbol_{i}_rlib_metadata ").as_bytes());
        }
        let mut x = 0x1234_5678u32;
        for _ in 0..200_000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            data.push(x as u8);
        }
        let size = data.len() as u64;
        let w = store
            .write(7, size, |off, len| Ok(data[off as usize..off as usize + len].to_vec()))
            .unwrap();
        store.publish(&w, 7, 1).unwrap();
        assert!(w.disk_bytes < size);
        for (off, len) in [(0u64, 10usize), (131_000, 5000), (size - 3, 3), (500_000, 300_000)] {
            let got = store.read_at(7, 1, off, len).unwrap();
            let end = (off as usize + len).min(data.len());
            let mut want = data[off as usize..end].to_vec();
            want.resize(len, 0);
            assert_eq!(got, want, "offset {off}");
        }
        assert_eq!(store.list().unwrap(), vec![(7, 1)]);
        store.remove(7, 1).unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn empty_and_tiny() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ColdStore::open(tmp.path(), 3, 1 << 20).unwrap();
        let w = store.write(1, 0, |_, _| Ok(vec![])).unwrap();
        store.publish(&w, 1, 1).unwrap();
        assert_eq!(store.read_at(1, 1, 0, 0).unwrap(), Vec::<u8>::new());
        let packed = compress_inline(b"hello hello hello", 3).unwrap();
        assert_eq!(decompress_inline(&packed, 17).unwrap(), b"hello hello hello");
    }
}
