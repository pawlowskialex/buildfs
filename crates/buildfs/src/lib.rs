//! buildfs: a userspace NFSv4.1 filesystem for build artifacts.
//!
//! Layers:
//! - [`nfs`]: embednfs adapter (wire protocol lives in `vendor/embednfs`)
//! - [`vfs`]: POSIX semantics over a redb metadata store
//! - [`hot`]: uncompressed data tier for files being written
//! - [`chunks`]: content-addressed zstd chunk store (dedup + compression)
//! - [`wal`]: metadata redo log (process-crash safety without fsync)

pub mod chunks;
pub mod hot;
pub mod ids;
pub mod meta;
pub mod nfs;
pub mod vfs;
pub mod wal;

/// Raises this process's open-file limit (soft `RLIMIT_NOFILE`) so the data
/// tiers' descriptor caches fit. Processes started by launchd (the agent,
/// the FSKit extension) get a soft limit of 256. Returns the new limit.
pub fn raise_fd_limit() -> std::io::Result<u64> {
    const WANT: libc::rlim_t = 65536;
    let mut rl = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: `rl` is a valid out pointer.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if rl.rlim_cur < WANT {
        // macOS rejects values above kern.maxfilesperproc, even when the hard
        // limit is "unlimited": step down until one is accepted.
        let mut want = WANT.min(rl.rlim_max);
        while want > rl.rlim_cur {
            let new = libc::rlimit { rlim_cur: want, rlim_max: rl.rlim_max };
            // SAFETY: `new` is a valid rlimit.
            if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &new) } == 0 {
                rl.rlim_cur = want;
                break;
            }
            want /= 2;
        }
    }
    #[allow(clippy::unnecessary_cast, reason = "rlim_t width differs across platforms")]
    Ok(rl.rlim_cur as u64)
}
