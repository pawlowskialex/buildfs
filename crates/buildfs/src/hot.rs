//! Hot data tier: uncompressed per-inode files on the backing filesystem.
//!
//! Files that are being written (or were written recently) live here so
//! random writes, mmap-based linkers and truncation stay cheap. A later
//! phase moves idle files into the compressed cold tier.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::meta::Ino;

/// Upper bound on cached open file descriptors.
const FD_CACHE_CAPACITY: usize = 1024;

pub struct HotStore {
    root: PathBuf,
    fds: Mutex<HashMap<Ino, Arc<File>>>,
}

impl HotStore {
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            fds: Mutex::new(HashMap::new()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `hot/<low byte>/<ino>`: shards directories so none grows huge.
    pub fn path(&self, ino: Ino) -> PathBuf {
        self.root
            .join(format!("{:02x}", ino & 0xff))
            .join(format!("{ino:016x}"))
    }

    fn file(&self, ino: Ino, create: bool) -> io::Result<Arc<File>> {
        if let Some(f) = self.fds.lock().get(&ino) {
            return Ok(f.clone());
        }
        let path = self.path(ino);
        let open = |path: &Path| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(create)
                .truncate(false)
                .open(path)
        };
        let file = match open(&path) {
            Err(e) if create && e.kind() == io::ErrorKind::NotFound => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                open(&path)?
            }
            other => other?,
        };
        let file = Arc::new(file);
        let mut fds = self.fds.lock();
        if fds.len() >= FD_CACHE_CAPACITY
            && let Some(&victim) = fds.keys().next()
        {
            let _ = fds.remove(&victim);
        }
        let _ = fds.insert(ino, file.clone());
        Ok(file)
    }

    /// Creates (or opens) the backing file and sets its length.
    pub fn create(&self, ino: Ino, len: u64) -> io::Result<()> {
        let f = self.file(ino, true)?;
        if f.metadata()?.len() != len {
            f.set_len(len)?;
        }
        Ok(())
    }

    /// Reads up to `len` bytes; bytes past the backing file's end read as
    /// zeros (the caller has already clamped `len` to the logical size).
    pub fn read_at(&self, ino: Ino, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let f = self.file(ino, false)?;
        let mut buf = vec![0u8; len];
        let mut done = 0;
        while done < len {
            let n = f.read_at(&mut buf[done..], offset + done as u64)?;
            if n == 0 {
                break; // short backing file: remainder stays zero
            }
            done += n;
        }
        Ok(buf)
    }

    pub fn write_at(&self, ino: Ino, offset: u64, data: &[u8]) -> io::Result<()> {
        self.file(ino, true)?.write_all_at(data, offset)
    }

    pub fn set_len(&self, ino: Ino, len: u64) -> io::Result<()> {
        self.file(ino, true)?.set_len(len)
    }

    /// Bytes actually allocated on the backing filesystem.
    pub fn allocated(&self, ino: Ino) -> io::Result<u64> {
        Ok(self.file(ino, false)?.metadata()?.blocks() * 512)
    }

    pub fn sync(&self, ino: Ino) -> io::Result<()> {
        match self.file(ino, false) {
            Ok(f) => f.sync_data(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn remove(&self, ino: Ino) -> io::Result<()> {
        let _ = self.fds.lock().remove(&ino);
        match std::fs::remove_file(self.path(ino)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    /// Lists every inode that currently has a backing file (for GC).
    pub fn list(&self) -> io::Result<Vec<Ino>> {
        let mut out = Vec::new();
        for shard in std::fs::read_dir(&self.root)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(shard.path())? {
                let entry = entry?;
                if let Some(ino) = entry
                    .file_name()
                    .to_str()
                    .and_then(|n| u64::from_str_radix(n, 16).ok())
                {
                    out.push(ino);
                }
            }
        }
        Ok(out)
    }
}
