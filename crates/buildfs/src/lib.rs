//! buildfs: a userspace NFSv4.1 filesystem for build artifacts.
//!
//! Layers:
//! - [`nfs`]: embednfs adapter (wire protocol lives in `vendor/embednfs`)
//! - [`vfs`]: POSIX semantics over a redb metadata store
//! - [`hot`]: uncompressed data tier for files being written
//! - [`cold`]: zstd-compressed, randomly readable data tier for idle files
//! - [`wal`]: metadata redo log (process-crash safety without fsync)

pub mod cold;
pub mod hot;
pub mod ids;
pub mod meta;
pub mod nfs;
pub mod vfs;
pub mod wal;
