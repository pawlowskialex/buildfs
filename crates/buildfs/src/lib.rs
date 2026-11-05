//! buildfs's NFSv4.1 front end. The storage engine is in `buildfs-core`; the
//! FSKit front end is in `buildfs-ffi` and `fskit/`.
//!
//! - [`nfs`]: embednfs adapter (wire protocol lives in `vendor/embednfs`)
//! - [`ids`]: NFSv4 owner strings <-> local uids/gids

pub mod ids;
pub mod nfs;
