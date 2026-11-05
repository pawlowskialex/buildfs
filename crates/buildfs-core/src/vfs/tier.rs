//! Tiering: hot files -> compressed, deduplicated chunks (and back).
//!
//! ## Chunk lifetime
//!
//! A chunk's reference count lives in the `chunks` table and is changed
//! only inside metadata transactions. Large chunks also have a file, which
//! is written *before* the transaction that first references it and deleted
//! *after* the transaction that drops the last reference. Two rules keep
//! those file operations safe against each other:
//!
//! - deleting a chunk file happens under `chunk_lock`, after re-checking
//!   (in a fresh read transaction) that no reference exists;
//! - a compaction commits under `chunk_lock` too, and when it creates the
//!   first reference to a chunk it verifies the file still exists with the
//!   expected length, aborting (to be retried later) if a deletion or a
//!   concurrent compaction won the race.

use std::sync::Arc;
use std::time::Duration;

use crate::types::FsError;
use parking_lot::Mutex;
use redb::{ReadableDatabase, ReadableTable};
use tracing::{debug, error};

use super::*;
use crate::chunks::{self, CHUNK_SIZE, Encoded, Hash, SMALL_CHUNK_MAX};

/// How a compaction will reference one chunk.
enum Planned {
    /// Already stored when we looked (dedup hit); nothing to write.
    Existing,
    /// New, small: bytes go into `chunkdata` inside the transaction.
    Small(Encoded),
    /// New to this store, large: file already there (written, or linked from
    /// the shared pool); record its stored size.
    Large { stored_len: u32, raw: bool },
}

impl W<'_> {
    /// Adds a reference to an existing chunk. Returns its stored length,
    /// or `None` if the chunk doesn't exist (anymore).
    fn chunk_add_ref(&mut self, h: &Hash) -> Result<Option<u32>> {
        let Some((rc, len, raw)) = self.chunks.get(h.as_slice()).db()?.map(|v| v.value()) else {
            return Ok(None);
        };
        let _ = self.chunks.insert(h.as_slice(), (rc + 1, len, raw)).db()?;
        self.log
            .push(Mutation::ChunkPut(h.to_vec(), rc + 1, len, raw));
        Ok(Some(len))
    }

    fn chunk_create(&mut self, h: &Hash, stored_len: u32, raw: bool, small: Option<&[u8]>) -> Result<()> {
        let raw = u8::from(raw);
        let _ = self.chunks.insert(h.as_slice(), (1, stored_len, raw)).db()?;
        self.log
            .push(Mutation::ChunkPut(h.to_vec(), 1, stored_len, raw));
        if let Some(bytes) = small {
            let _ = self.chunkdata.insert(h.as_slice(), bytes).db()?;
            self.log
                .push(Mutation::ChunkDataPut(h.to_vec(), bytes.to_vec()));
        }
        Ok(())
    }

    fn chunk_drop_ref(&mut self, h: &Hash) -> Result<()> {
        let Some((rc, len, raw)) = self.chunks.get(h.as_slice()).db()?.map(|v| v.value()) else {
            warn!("chunk {} referenced but missing", chunks::hex(h));
            return Ok(());
        };
        if rc > 1 {
            let _ = self.chunks.insert(h.as_slice(), (rc - 1, len, raw)).db()?;
            self.log
                .push(Mutation::ChunkPut(h.to_vec(), rc - 1, len, raw));
            return Ok(());
        }
        let _ = self.chunks.remove(h.as_slice()).db()?;
        self.log.push(Mutation::ChunkDel(h.to_vec()));
        if self.chunkdata.remove(h.as_slice()).db()?.is_some() {
            self.log.push(Mutation::ChunkDataDel(h.to_vec()));
        } else {
            self.drop_data.push(DropData::Chunk(*h));
        }
        Ok(())
    }

    fn chunkmap_put(&mut self, ino: Ino, idx: u32, h: &Hash) -> Result<()> {
        let _ = self.chunkmap.insert((ino, idx), h.as_slice()).db()?;
        self.log.push(Mutation::ChunkMapPut(ino, idx, h.to_vec()));
        Ok(())
    }

    /// Drops every chunk reference of `ino` and its chunk map.
    pub(super) fn release_chunks(&mut self, ino: Ino) -> Result<()> {
        let rows: Vec<(u32, Hash)> = self
            .chunkmap
            .range((ino, 0)..=(ino, u32::MAX))
            .db()?
            .map(|r| {
                r.map(|(k, v)| {
                    let mut h = [0u8; 32];
                    h.copy_from_slice(v.value());
                    (k.value().1, h)
                })
            })
            .collect::<std::result::Result<_, _>>()
            .db()?;
        for (idx, h) in rows {
            let _ = self.chunkmap.remove((ino, idx)).db()?;
            self.log.push(Mutation::ChunkMapDel(ino, idx));
            self.chunk_drop_ref(&h)?;
        }
        Ok(())
    }
}

impl Vfs {
    /// Post-commit half of dropping a chunk's last reference.
    pub(super) fn delete_chunk_file_if_unused(&self, h: &Hash) {
        let _guard = self.chunk_lock.lock();
        let unused = (|| -> Result<bool> {
            let txn = self.db.begin_read().db()?;
            let t = txn.open_table(CHUNKS).db()?;
            Ok(t.get(h.as_slice()).db()?.is_none())
        })();
        match unused {
            Ok(true) => {
                if let Err(e) = self.chunks.remove_file(h) {
                    warn!("failed to remove chunk {}: {e}", chunks::hex(h));
                }
            }
            Ok(false) => {} // re-referenced meanwhile
            Err(e) => warn!("chunk liveness check failed: {e:?}"),
        }
    }

    fn load_chunk(&self, h: &Hash, logical_len: usize) -> std::io::Result<Arc<Vec<u8>>> {
        if let Some(c) = self.chunks.cached(h) {
            return Ok(c);
        }
        let not_found = || std::io::Error::from(std::io::ErrorKind::NotFound);
        let (stored, raw) = {
            let txn = self.db.begin_read().map_err(std::io::Error::other)?;
            let t = txn.open_table(CHUNKS).map_err(std::io::Error::other)?;
            let (_, len, raw) = t
                .get(h.as_slice())
                .map_err(std::io::Error::other)?
                .ok_or_else(not_found)?
                .value();
            let small = txn.open_table(CHUNKDATA).map_err(std::io::Error::other)?;
            let stored = match small.get(h.as_slice()).map_err(std::io::Error::other)? {
                Some(v) => v.value().to_vec(),
                None => self.chunks.read_file(h, len as usize)?,
            };
            (stored, raw != 0)
        };
        let data = Arc::new(chunks::decode(stored, raw, logical_len)?);
        self.chunks.cache_insert(*h, data.clone());
        Ok(data)
    }

    /// Reads `len` bytes at `offset` of a chunked file of logical `size`.
    pub(super) fn read_chunked(&self, ino: Ino, size: u64, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        let cs = u64::from(CHUNK_SIZE);
        let end = (offset + len as u64).min(size);
        let mut out = Vec::with_capacity(len);
        if offset < end {
            let first = (offset / cs) as u32;
            let last = ((end - 1) / cs) as u32;
            let hashes: Vec<Hash> = {
                let txn = self.db.begin_read().map_err(std::io::Error::other)?;
                let t = txn.open_table(CHUNKMAP).map_err(std::io::Error::other)?;
                let mut v = Vec::with_capacity((last - first + 1) as usize);
                for r in t.range((ino, first)..=(ino, last)).map_err(std::io::Error::other)? {
                    let (_, h) = r.map_err(std::io::Error::other)?;
                    let mut a = [0u8; 32];
                    a.copy_from_slice(h.value());
                    v.push(a);
                }
                v
            };
            if hashes.len() != (last - first + 1) as usize {
                // Rehydrated or freed concurrently; the caller retries.
                return Err(std::io::ErrorKind::NotFound.into());
            }
            let mut pos = offset;
            for (i, h) in hashes.iter().enumerate() {
                let idx = u64::from(first) + i as u64;
                let chunk_start = idx * cs;
                let chunk_len = (size - chunk_start).min(cs) as usize;
                let chunk = self.load_chunk(h, chunk_len)?;
                let within = (pos - chunk_start) as usize;
                let take = ((end - pos) as usize).min(chunk.len() - within);
                out.extend_from_slice(&chunk[within..within + take]);
                pos += take as u64;
            }
        }
        out.resize(len, 0);
        Ok(out)
    }

    /// Brings a compressed file back to the hot tier (for in-place writes).
    pub(super) fn rehydrate(&self, ino: Ino) -> Result<()> {
        let _guard = self.rehydrate_lock.lock();
        let inode = self.getattr(ino)?;
        if !matches!(inode.data, Data::Chunked { .. } | Data::Inline { .. }) {
            return Ok(());
        }
        // Any hot file present now is a stray from a racing write.
        self.hot.remove(ino).map_err(io_err)?;
        self.hot.create(ino, 0).map_err(io_err)?;
        let step = u64::from(CHUNK_SIZE) * 8;
        let mut off = 0;
        while off < inode.size {
            let len = (inode.size - off).min(step) as usize;
            let bytes = self.read_data(ino, &inode, off, len).map_err(io_err)?;
            self.hot.write_at(ino, off, &bytes).map_err(io_err)?;
            off += len as u64;
        }
        self.hot.set_len(ino, inode.size).map_err(io_err)?;
        let swapped = self.write(|w| {
            let mut cur = w.get(ino)?;
            if cur.data != inode.data {
                return Ok(false);
            }
            let old = std::mem::replace(&mut cur.data, Data::Hot);
            w.release_data(ino, &old)?;
            w.put(ino, &cur)?;
            Ok(true)
        });
        match swapped {
            Ok(true) => {
                self.stats.lock().rehydrated += 1;
                let mut c = self.compactor.lock();
                let b = c.backoff.entry(ino).or_insert(Duration::ZERO);
                *b = (*b * 2)
                    .max(self.compress_after.unwrap_or_default())
                    .min(MAX_COMPRESS_DELAY);
                drop(c);
                self.note_modified(ino);
                Ok(())
            }
            other => {
                // Truncated to zero or deleted meanwhile: our copy is junk.
                let still_hot = self.getattr(ino).is_ok_and(|i| i.data == Data::Hot);
                if !still_hot {
                    let _ = self.hot.remove(ino);
                }
                other.map(|_| ())
            }
        }
    }

    /// Schedules `ino` for compression once it has been idle long enough.
    pub(super) fn note_modified(&self, ino: Ino) {
        let Some(base) = self.compress_after else { return };
        let mut c = self.compactor.lock();
        let delay = c.backoff.get(&ino).copied().unwrap_or(base).max(base);
        let _ = c.pending.insert(
            ino,
            Pending {
                since: std::time::Instant::now(),
                delay,
            },
        );
    }

    pub fn compress_stats(&self) -> CompressStats {
        *self.stats.lock()
    }

    /// Compresses `ino` (inline for tiny files, chunks otherwise) if it is
    /// still hot and unmodified since we looked at it. Returns whether it
    /// moved. Compaction never changes the NFS change attribute.
    pub fn compact(&self, ino: Ino) -> Result<bool> {
        let inode = match self.getattr(ino) {
            Ok(i) => i,
            Err(FsError::Stale) => return Ok(false),
            Err(e) => return Err(e),
        };
        if inode.kind != Kind::File || inode.data != Data::Hot {
            return Ok(false);
        }
        let change0 = inode.change;
        let size = inode.size;
        let unchanged = |cur: &Inode| cur.change == change0 && cur.data == Data::Hot;

        // Empty and tiny files go to redb: a separate file would occupy at
        // least one 4 KiB block on the backing filesystem.
        if size <= INLINE_PROBE_MAX {
            let raw = self.hot.read_at(ino, 0, size as usize).map_err(io_err)?;
            let packed = if size == 0 {
                Vec::new()
            } else {
                zstd::bulk::compress(&raw, self.level).map_err(io_err)?
            };
            if packed.len() <= INLINE_MAX {
                let moved = self.write(|w| {
                    let mut cur = w.get(ino)?;
                    if !unchanged(&cur) {
                        return Ok(false);
                    }
                    if size == 0 {
                        cur.data = Data::Empty;
                    } else {
                        w.put_inline(ino, &packed)?;
                        cur.data = Data::Inline {
                            disk: packed.len() as u32,
                        };
                    }
                    w.drop_data.push(DropData::Hot(ino));
                    w.put(ino, &cur)?;
                    Ok(true)
                })?;
                self.record(moved, true, size, packed.len() as u64, 0, 0);
                return Ok(moved);
            }
        }

        // Plan: hash every chunk; compress and store only unknown ones.
        let mut compressor = zstd::bulk::Compressor::new(self.level).map_err(io_err)?;
        let count = size.div_ceil(u64::from(CHUNK_SIZE)) as usize;
        let mut plan: Vec<(Hash, Planned)> = Vec::with_capacity(count);
        let mut new_files: Vec<Hash> = Vec::new();
        let (mut pool_chunks, mut pool_bytes) = (0u64, 0u64);
        for i in 0..count {
            let off = i as u64 * u64::from(CHUNK_SIZE);
            let len = (size - off).min(u64::from(CHUNK_SIZE)) as usize;
            let raw = self.hot.read_at(ino, off, len).map_err(io_err)?;
            let h = chunks::hash(&raw);
            if self.chunk_exists(&h)? || plan.iter().any(|(p, _)| *p == h) {
                plan.push((h, Planned::Existing));
                continue;
            }
            if let Some((stored_len, is_raw)) = self.chunks.adopt(&h, &raw) {
                pool_chunks += 1;
                pool_bytes += raw.len() as u64;
                new_files.push(h);
                plan.push((h, Planned::Large { stored_len, raw: is_raw }));
                continue;
            }
            let enc = chunks::encode(&mut compressor, &raw).map_err(io_err)?;
            if enc.bytes.len() <= SMALL_CHUNK_MAX {
                plan.push((h, Planned::Small(enc)));
            } else {
                let _ = self.chunks.write_file(&h, &enc.bytes).map_err(io_err)?;
                self.chunks.publish(&h);
                new_files.push(h);
                plan.push((
                    h,
                    Planned::Large {
                        stored_len: enc.bytes.len() as u32,
                        raw: enc.raw,
                    },
                ));
            }
        }

        let _guard = self.chunk_lock.lock();
        let mut dedup_chunks = 0u64;
        let mut dedup_bytes = 0u64;
        let result = self.write(|w| {
            let mut cur = w.get(ino)?;
            if !unchanged(&cur) {
                return Ok(None);
            }
            let mut disk = 0u64;
            let (mut hits, mut hit_bytes) = (0u64, 0u64);
            for (idx, (h, planned)) in plan.iter().enumerate() {
                let logical = (size - idx as u64 * u64::from(CHUNK_SIZE)).min(u64::from(CHUNK_SIZE));
                if let Some(len) = w.chunk_add_ref(h)? {
                    // Existing chunk (or a duplicate within this file).
                    disk += u64::from(len);
                    if matches!(planned, Planned::Existing) {
                        hits += 1;
                        hit_bytes += logical;
                    }
                } else {
                    match planned {
                        // Was there when we planned, deleted since: retry later.
                        Planned::Existing => return Ok(None),
                        Planned::Small(enc) => {
                            w.chunk_create(h, enc.bytes.len() as u32, enc.raw, Some(&enc.bytes))?;
                            disk += enc.bytes.len() as u64;
                        }
                        Planned::Large { stored_len, raw } => {
                            if self.chunks.file_len(h) != Some(u64::from(*stored_len)) {
                                return Ok(None);
                            }
                            w.chunk_create(h, *stored_len, *raw, None)?;
                            disk += u64::from(*stored_len);
                        }
                    }
                }
                w.chunkmap_put(ino, idx as u32, h)?;
            }
            cur.data = Data::Chunked { disk };
            w.drop_data.push(DropData::Hot(ino));
            w.put(ino, &cur)?;
            Ok(Some((disk, hits, hit_bytes)))
        });
        let outcome = match result {
            Ok(Some((disk, hits, hit_bytes))) => {
                dedup_chunks = hits;
                dedup_bytes = hit_bytes;
                Ok(Some(disk))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        };
        if !matches!(outcome, Ok(Some(_))) {
            // Files we wrote that ended up unreferenced.
            for h in &new_files {
                let unused = self.chunk_exists(h).map(|e| !e).unwrap_or(false);
                if unused {
                    let _ = self.chunks.remove_file(h);
                }
            }
        }
        drop(_guard);
        match outcome {
            Ok(Some(disk)) => {
                self.record(true, false, size, disk, dedup_chunks, dedup_bytes);
                let mut st = self.stats.lock();
                st.pool_chunks += pool_chunks;
                st.pool_bytes += pool_bytes;
                Ok(true)
            }
            Ok(None) => {
                self.record(false, false, 0, 0, 0, 0);
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    fn chunk_exists(&self, h: &Hash) -> Result<bool> {
        let txn = self.db.begin_read().db()?;
        let t = txn.open_table(CHUNKS).db()?;
        Ok(t.get(h.as_slice()).db()?.is_some())
    }

    fn record(&self, moved: bool, inline: bool, logical: u64, stored: u64, dedup_chunks: u64, dedup_bytes: u64) {
        let mut st = self.stats.lock();
        if !moved {
            st.aborted += 1;
            return;
        }
        if inline {
            st.inlined += 1;
        } else {
            st.compacted += 1;
        }
        st.logical_bytes += logical;
        st.stored_bytes += stored;
        st.dedup_chunks += dedup_chunks;
        st.dedup_bytes += dedup_bytes;
    }

    /// Compresses every pending file now, ignoring idle timers (tests, CLI).
    pub fn compact_all_pending(&self) -> Result<usize> {
        let inos: Vec<Ino> = self.compactor.lock().pending.drain().map(|(k, _)| k).collect();
        let mut moved = 0;
        for ino in inos {
            if self.compact(ino)? {
                moved += 1;
            }
        }
        Ok(moved)
    }

    /// Bytes stored for chunks (files + redb), i.e. what dedup+compression
    /// actually costs on disk, and the number of distinct chunks.
    pub fn chunk_usage(&self) -> Result<(u64, u64)> {
        let txn = self.db.begin_read().db()?;
        let t = txn.open_table(CHUNKS).db()?;
        let (mut bytes, mut n) = (0u64, 0u64);
        for r in t.iter().db()? {
            let (_, v) = r.db()?;
            bytes += u64::from(v.value().1);
            n += 1;
        }
        Ok((bytes, n))
    }

    pub(super) fn spawn_compactor(vfs: &Arc<Self>, workers: usize) {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Ino>(4096);
        let rx = Arc::new(Mutex::new(rx));
        let mut handles = Vec::new();
        for i in 0..workers {
            let weak = Arc::downgrade(vfs);
            let rx = rx.clone();
            let h = std::thread::Builder::new()
                .name(format!("buildfs-compress-{i}"))
                .spawn(move || {
                    lower_thread_priority();
                    loop {
                        let next = rx.lock().recv();
                        let Ok(ino) = next else { return };
                        let Some(vfs) = weak.upgrade() else { return };
                        if let Err(e) = vfs.compact(ino) {
                            debug!("compaction of inode {ino} failed: {e:?}");
                        }
                    }
                });
            match h {
                Ok(h) => handles.push(h),
                Err(e) => error!("failed to start compression worker: {e}"),
            }
        }
        let weak = Arc::downgrade(vfs);
        let scheduler = std::thread::Builder::new()
            .name("buildfs-compress-sched".into())
            .spawn(move || {
                loop {
                    let Some(vfs) = weak.upgrade() else { return };
                    let due: Vec<Ino> = {
                        let mut c = vfs.compactor.lock();
                        if c.shutdown {
                            return; // dropping `tx` stops the workers
                        }
                        let _ = vfs
                            .compactor_cv
                            .wait_for(&mut c, Duration::from_millis(500));
                        if c.shutdown {
                            return;
                        }
                        let now = std::time::Instant::now();
                        let due: Vec<Ino> = c
                            .pending
                            .iter()
                            .filter(|(_, p)| now.duration_since(p.since) >= p.delay)
                            .map(|(k, _)| *k)
                            .take(1024)
                            .collect();
                        for ino in &due {
                            let _ = c.pending.remove(ino);
                        }
                        due
                    };
                    drop(vfs);
                    for ino in due {
                        if tx.send(ino).is_err() {
                            return;
                        }
                    }
                }
            });
        match scheduler {
            Ok(h) => handles.push(h),
            Err(e) => error!("failed to start compression scheduler: {e}"),
        }
        *vfs.compactor_threads.lock() = handles;
    }
}
