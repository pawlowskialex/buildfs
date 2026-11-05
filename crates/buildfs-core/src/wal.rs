//! Metadata redo log.
//!
//! redb commits made with `Durability::None` only become persistent with the
//! next durable commit, so a crash of the buildfs process would lose the last
//! second of namespace changes while their data (already `pwrite`n into the
//! kernel page cache) survives. To close that gap without paying an fsync per
//! operation, every write transaction appends its mutations here with a plain
//! `write(2)` before committing. That makes metadata exactly as durable as
//! file data: it survives process crashes, not kernel panics or power loss.
//!
//! The flusher rotates the log inside the write transaction it uses for each
//! durable commit; once that commit is on disk the old segment is deleted.
//! On startup any remaining segments are replayed in order.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One table mutation. Values are stored exactly as written to redb, so
/// replaying a record is idempotent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mutation {
    InodePut(u64, Vec<u8>),
    InodeDel(u64),
    DirentPut(u64, String, u64, u64),
    DirentDel(u64, String),
    CookiePut(u64, u64, String),
    CookieDel(u64, u64),
    XattrPut(u64, String, Vec<u8>),
    XattrDel(u64, String),
    Counter(String, u64),
    InlinePut(u64, Vec<u8>),
    InlineDel(u64),
    ChunkPut(Vec<u8>, u64, u32, u8),
    ChunkDel(Vec<u8>),
    ChunkDataPut(Vec<u8>, Vec<u8>),
    ChunkDataDel(Vec<u8>),
    ChunkMapPut(u64, u32, Vec<u8>),
    ChunkMapDel(u64, u32),
}

pub struct Wal {
    dir: PathBuf,
    seq: u64,
    file: File,
}

fn segment_path(dir: &Path, seq: u64) -> PathBuf {
    dir.join(format!("{seq:016x}.wal"))
}

/// Lists existing segments in replay order.
pub fn segments(dir: &Path) -> io::Result<Vec<(u64, PathBuf)>> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".wal")) else {
            continue;
        };
        if let Ok(seq) = u64::from_str_radix(stem, 16) {
            out.push((seq, entry.path()));
        }
    }
    out.sort();
    Ok(out)
}

/// Reads every intact record of a segment. A torn or corrupt tail (possible
/// only if the process died mid-append) ends the segment.
pub fn read_segment(path: &Path) -> io::Result<Vec<Vec<Mutation>>> {
    let mut bytes = Vec::new();
    let _ = File::open(path)?.read_to_end(&mut bytes)?;
    let mut records = Vec::new();
    let mut rest = bytes.as_slice();
    while rest.len() >= 8 {
        let (header, tail) = rest.split_at(8);
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let crc = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        if tail.len() < len {
            break;
        }
        let (payload, tail) = tail.split_at(len);
        if crc32fast::hash(payload) != crc {
            break;
        }
        match postcard::from_bytes::<Vec<Mutation>>(payload) {
            Ok(record) => records.push(record),
            Err(_) => break,
        }
        rest = tail;
    }
    Ok(records)
}

impl Wal {
    /// Starts a fresh segment numbered after any existing ones.
    pub fn open(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let seq = segments(dir)?.last().map_or(1, |(s, _)| s + 1);
        let file = Self::create_segment(dir, seq)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            seq,
            file,
        })
    }

    fn create_segment(dir: &Path, seq: u64) -> io::Result<File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(segment_path(dir, seq))
    }

    /// Appends one transaction's mutations with a single `write(2)`.
    pub fn append(&mut self, record: &[Mutation]) -> io::Result<()> {
        if record.is_empty() {
            return Ok(());
        }
        let payload = postcard::to_stdvec(record).map_err(io::Error::other)?;
        let mut buf = Vec::with_capacity(payload.len() + 8);
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&crc32fast::hash(&payload).to_le_bytes());
        buf.extend_from_slice(&payload);
        self.file.write_all(&buf)
    }

    /// Switches to a new segment; returns the path of the previous one, which
    /// may be deleted once a durable commit covering it has completed.
    pub fn rotate(&mut self) -> io::Result<PathBuf> {
        let old = segment_path(&self.dir, self.seq);
        self.file = Self::create_segment(&self.dir, self.seq + 1)?;
        self.seq += 1;
        Ok(old)
    }

    /// fsyncs the current segment (strict durability).
    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_data()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_read_and_torn_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(tmp.path()).unwrap();
        let r1 = vec![Mutation::InodePut(5, vec![1, 2, 3]), Mutation::Counter("x".into(), 9)];
        let r2 = vec![Mutation::DirentDel(1, "a".into())];
        wal.append(&r1).unwrap();
        wal.append(&r2).unwrap();
        let (_, path) = segments(tmp.path()).unwrap().pop().unwrap();
        assert_eq!(read_segment(&path).unwrap(), vec![r1.clone(), r2]);

        // Simulate a torn append.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&[200, 0, 0, 0, 1, 2]).unwrap();
        assert_eq!(read_segment(&path).unwrap().len(), 2);

        let old = wal.rotate().unwrap();
        assert_eq!(old, path);
        assert_eq!(segments(tmp.path()).unwrap().len(), 2);
    }
}
