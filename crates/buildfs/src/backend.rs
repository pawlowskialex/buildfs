//! Which front end a mount uses, and where NFS keeps what FSKit doesn't need.
//!
//! macOS 27 and later with the module installed use FSKit; anything else uses
//! NFS. Being installed is all that is tested here: `pluginkit` reports a
//! disabled module as elected, so only trying the mount can tell. A disabled
//! module is an error (see `mounts::mount_fskit`), never a reason to use NFS:
//! that would put data in a different store with no warning.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use clap::ValueEnum;

/// `DataCacheHandler`, which FSKit's speed depends on, is new in macOS 27.
const FSKIT_MIN_MACOS: u32 = 27;
const FSKIT_MODULE_ID: &str = "com.pawlowskialex.buildfs.fsmodule";

/// The port an NFS server uses unless told otherwise.
pub const NFS_DEFAULT_PORT: u16 = 20490;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BackendKind {
    /// The FSKit module (macOS 27 and later; much faster).
    Fskit,
    /// A local NFSv4.1 server.
    Nfs,
}

/// What decides the backend when none is requested.
#[derive(Debug, Clone, Copy)]
pub struct Host {
    /// `None` if the version couldn't be read.
    pub macos_major: Option<u32>,
    pub module_installed: bool,
}

impl Host {
    /// Reads this Mac. `BUILDFS_TEST_MACOS_VERSION` and
    /// `BUILDFS_TEST_FSKIT_MODULE=missing` replace the answers, to test on
    /// one Mac what happens on others.
    pub fn detect() -> Self {
        let version = std::env::var("BUILDFS_TEST_MACOS_VERSION").ok().or_else(|| {
            let out = Command::new("/usr/sbin/sysctl").args(["-n", "kern.osproductversion"]).output().ok()?;
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        });
        let module_installed = std::env::var("BUILDFS_TEST_FSKIT_MODULE").map_or_else(
            |_| {
                Command::new("/usr/bin/pluginkit")
                    .args(["-m", "-i", FSKIT_MODULE_ID])
                    .output()
                    .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(FSKIT_MODULE_ID))
            },
            |v| v != "missing",
        );
        Self {
            macos_major: version.as_deref().and_then(parse_major),
            module_installed,
        }
    }
}

fn parse_major(version: &str) -> Option<u32> {
    version.trim().split('.').next()?.parse().ok()
}

/// The backend for `host`, and a note if the user should know why it isn't
/// the fast one.
pub fn choose(host: Host) -> (BackendKind, Option<String>) {
    match host {
        Host { macos_major: Some(v), module_installed: true } if v >= FSKIT_MIN_MACOS => (BackendKind::Fskit, None),
        Host { macos_major: Some(v), .. } if v >= FSKIT_MIN_MACOS => (
            BackendKind::Nfs,
            Some("note: using NFS because the FSKit module isn't installed (fskit/install.sh), which would be much faster".into()),
        ),
        Host { macos_major: Some(v), .. } => (
            BackendKind::Nfs,
            Some(format!("note: using NFS because FSKit needs macOS {FSKIT_MIN_MACOS} (this is macOS {v})")),
        ),
        Host { macos_major: None, .. } => (
            BackendKind::Nfs,
            Some("note: using NFS because the macOS version couldn't be read".into()),
        ),
    }
}

/// The backend the flags ask for, if any. `--store` has always meant NFS.
pub fn requested(fskit: bool, backend: Option<BackendKind>, store: Option<&Path>) -> anyhow::Result<Option<BackendKind>> {
    Ok(match (fskit, backend, store) {
        (_, Some(BackendKind::Fskit), Some(_)) => {
            bail!("--store is for NFS: FSKit stores live in the module's container, and --name picks one")
        }
        (true, ..) | (_, Some(BackendKind::Fskit), _) => Some(BackendKind::Fskit),
        (_, Some(BackendKind::Nfs), _) | (_, None, Some(_)) => Some(BackendKind::Nfs),
        (false, None, None) => None,
    })
}

/// Where the NFS store called `name` lives when `--store` doesn't say.
pub fn default_store(name: &str) -> anyhow::Result<PathBuf> {
    Ok(std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?
        .join(".buildfs/stores")
        .join(name))
}

/// Whether nothing listens on `port` on localhost.
pub fn port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// The first free port from the default up, so that several NFS mounts can
/// run without anyone having to number them.
pub fn first_free_port() -> anyhow::Result<u16> {
    (NFS_DEFAULT_PORT..NFS_DEFAULT_PORT + 100)
        .find(|p| port_free(*p))
        .context("no free port for the NFS server; pick one with --port")
}

/// Whether a server has `store` open. redb holds a `flock` on its database for
/// as long as it is open, so this needs no records that could go stale.
pub fn store_in_use(store: &Path) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(f) = std::fs::File::open(store.join("meta.redb")) else {
        return false;
    };
    // SAFETY: `f` is an open descriptor; closing it releases the lock.
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(macos_major: Option<u32>, module_installed: bool) -> Host {
        Host { macos_major, module_installed }
    }

    #[test]
    fn fskit_needs_macos_27_and_the_module() {
        assert_eq!(choose(host(Some(27), true)), (BackendKind::Fskit, None));
        assert_eq!(choose(host(Some(28), true)), (BackendKind::Fskit, None));
        for h in [host(Some(27), false), host(Some(26), true), host(Some(15), false), host(None, true)] {
            let (kind, note) = choose(h);
            assert_eq!(kind, BackendKind::Nfs);
            assert!(note.is_some(), "falling back to NFS must say so");
        }
    }

    #[test]
    fn macos_versions() {
        assert_eq!(parse_major("27.2\n"), Some(27));
        assert_eq!(parse_major("26"), Some(26));
        assert_eq!(parse_major("15.7.1"), Some(15));
        assert_eq!(parse_major(""), None);
        assert_eq!(parse_major("x"), None);
    }

    #[test]
    fn flags_override_the_choice() {
        let s = Some(Path::new("/s"));
        assert_eq!(requested(false, None, None).unwrap(), None);
        assert_eq!(requested(true, None, None).unwrap(), Some(BackendKind::Fskit));
        assert_eq!(requested(false, Some(BackendKind::Fskit), None).unwrap(), Some(BackendKind::Fskit));
        assert_eq!(requested(false, Some(BackendKind::Nfs), None).unwrap(), Some(BackendKind::Nfs));
        assert_eq!(requested(false, Some(BackendKind::Nfs), s).unwrap(), Some(BackendKind::Nfs));
        assert_eq!(requested(false, None, s).unwrap(), Some(BackendKind::Nfs));
        assert!(requested(false, Some(BackendKind::Fskit), s).is_err());
    }

    #[test]
    fn a_held_port_is_not_free() {
        let held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        assert!(!port_free(held.local_addr().unwrap().port()));
    }

    #[test]
    fn an_open_store_is_in_use() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!store_in_use(dir.path()), "no database yet");
        let vfs = buildfs_core::vfs::Vfs::open(dir.path()).unwrap();
        assert!(store_in_use(dir.path()), "redb's lock is what this relies on");
        vfs.shutdown();
        drop(vfs);
        assert!(!store_in_use(dir.path()));
    }
}
