//! Inspecting and creating mounts (NFS and FSKit) without touching possibly
//! dead ones.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

/// Default `mount_nfs` options for buildfs mounts (root and attachments).
pub fn mount_options(port: u16) -> String {
    // namedattr: xattrs are stored natively instead of in `._` files.
    format!("vers=4.1,tcp,port={port},nobrowse,namedattr,rsize=1048576,wsize=1048576")
}

/// Mounts `export` (`/` or a store subtree) of the server on `port` at `dir`.
pub fn mount_nfs(port: u16, export: &str, dir: &Path, extra: Option<&str>) -> anyhow::Result<()> {
    let mut opts = mount_options(port);
    if let Some(extra) = extra {
        opts.push(',');
        opts.push_str(extra);
    }
    let status = Command::new("/sbin/mount_nfs")
        .args(["-o", &opts, &format!("127.0.0.1:{export}")])
        .arg(dir)
        .status()?;
    if !status.success() {
        bail!("mount_nfs {export} on {} failed: {status}", dir.display());
    }
    Ok(())
}

/// Options every FSKit mount gets. `owners`: without it XNU silently drops
/// chown/chgrp on the volume. `nobrowse`: keep it out of Finder.
const FSKIT_OPTIONS: &str = "owners,nobrowse";

/// Prefix of the stores behind FSKit attachments (see `attach`).
pub const FSKIT_ATTACH_PREFIX: &str = "att-";

/// Checks a store name: it becomes the host of `buildfs://<name>`.
pub fn validate_store_name(name: &str) -> anyhow::Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok {
        bail!("store name {name:?} may only contain a-z, 0-9 and '-' (64 at most), and can't start with '-'");
    }
    Ok(())
}

/// The URL of an FSKit store.
pub fn fskit_url(store: &str) -> String {
    format!("buildfs://{store}")
}

/// The store behind an FSKit mount, from its `f_mntfromname`.
pub fn fskit_store(from: &str) -> Option<&str> {
    from.strip_prefix("buildfs://").map(|s| s.trim_end_matches('/'))
}

/// Mounts FSKit store `store` at `dir`. `fskitd` starts the extension;
/// no process of ours stays behind. Retries a few times: the system may
/// still be starting up (login) or registering the module (reinstall).
pub fn mount_fskit(store: &str, dir: &Path, extra: &[String]) -> anyhow::Result<()> {
    let mut opts = FSKIT_OPTIONS.to_owned();
    for e in extra {
        opts.push(',');
        opts.push_str(e);
    }
    let url = fskit_url(store);
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        let status = Command::new("/sbin/mount")
            .args(["-F", "-t", "buildfs", "-o", &opts, &url])
            .arg(dir)
            .status()?;
        if status.success() {
            return Ok(());
        }
    }
    bail!(
        "mounting {url} on {} failed. Is the module installed (fskit/install.sh) and enabled \
         (System Settings > General > Login Items & Extensions > File System Extensions)?",
        dir.display()
    )
}

/// Deletes an FSKit store. Its directory is in the extension's sandbox
/// container, which only the extension can touch, so this mounts the store
/// emptied, with the option that deletes it on close.
pub fn discard_fskit_store(store: &str) -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join(format!("buildfs-discard-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let r = mount_fskit(store, &dir, &["fresh".into(), "discard".into()]).and_then(|()| unmount(&dir));
    let _ = std::fs::remove_dir(&dir);
    r
}

/// Unmounts `dir`, forcing if a plain unmount fails.
pub fn unmount(dir: &Path) -> anyhow::Result<()> {
    if Command::new("/sbin/umount").arg(dir).status()?.success() {
        return Ok(());
    }
    if Command::new("/sbin/umount").arg("-f").arg(dir).status()?.success() {
        return Ok(());
    }
    bail!("could not unmount {}", dir.display())
}

/// All mounts as `(mounted on, mounted from)`, from cached kernel info.
pub fn all_mounts() -> Vec<(PathBuf, String)> {
    let mut list: *mut libc::statfs = std::ptr::null_mut();
    // SAFETY: getmntinfo fills `list` with a pointer to an internal array of
    // `n` entries that stays valid until the next call; we only read it.
    let n = unsafe { libc::getmntinfo(&mut list, libc::MNT_NOWAIT) };
    if n <= 0 || list.is_null() {
        return Vec::new();
    }
    // SAFETY: as above, `list` points to `n` initialized entries.
    let mounts = unsafe { std::slice::from_raw_parts(list, n as usize) };
    mounts
        .iter()
        .map(|m| {
            // SAFETY: f_mntonname/f_mntfromname are NUL-terminated C strings.
            let on = unsafe { std::ffi::CStr::from_ptr(m.f_mntonname.as_ptr()) };
            // SAFETY: see above.
            let from = unsafe { std::ffi::CStr::from_ptr(m.f_mntfromname.as_ptr()) };
            (
                PathBuf::from(on.to_string_lossy().into_owned()),
                from.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

/// How a buildfs root is served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// Our NFS server on this port.
    Nfs { port: u16 },
    /// The FSKit module, serving this store.
    Fskit { store: String },
}

/// A mounted buildfs root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub path: PathBuf,
    pub backend: Backend,
}

/// Whether `from` is a root rather than an attachment or something else.
fn is_root(from: &str) -> bool {
    from == "127.0.0.1:/"
        || from == "localhost:/"
        || fskit_store(from).is_some_and(|s| !s.starts_with(FSKIT_ATTACH_PREFIX))
}

/// The buildfs root mount to operate on: `explicit` if given, otherwise the
/// only root mounted (NFS or FSKit).
pub fn find_root_mount(explicit: Option<&Path>) -> anyhow::Result<Root> {
    let mounted: Vec<(PathBuf, String)> = all_mounts().into_iter().filter(|(_, from)| is_root(from)).collect();
    let roots: Vec<PathBuf> = mounted.iter().map(|(on, _)| on.clone()).collect();
    let mnt = match explicit {
        Some(p) => {
            let p = resolve_mountpoint(p)?;
            if !roots.contains(&p) {
                bail!("{} is not a mounted buildfs root", p.display());
            }
            p
        }
        None => match roots.as_slice() {
            [one] => one.clone(),
            [] => bail!("no buildfs mount found (start one with `buildfs mount` or `install-agent`)"),
            _ => bail!("several buildfs mounts found; pick one with --mount"),
        },
    };
    let from = mounted.iter().find(|(on, _)| *on == mnt).map_or("", |(_, from)| from.as_str());
    let backend = match fskit_store(from) {
        Some(store) => Backend::Fskit { store: store.to_owned() },
        None => Backend::Nfs {
            port: nfs_mount_port(&mnt).context("could not determine the server port")?,
        },
    };
    Ok(Root { path: mnt, backend })
}

/// Absolute mountpoint path without touching the mountpoint itself.
pub fn resolve_mountpoint(p: &std::path::Path) -> anyhow::Result<PathBuf> {
    let name = p.file_name().context("mountpoint needs a final component")?;
    let parent = match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    Ok(std::fs::canonicalize(&parent).context("mountpoint parent")?.join(name))
}

/// The `f_mntfromname` of a mount at exactly `path`, using cached mount
/// info (MNT_NOWAIT) so dead mounts are never contacted.
pub fn mounted_from(path: &std::path::Path) -> Option<String> {
    let mut list: *mut libc::statfs = std::ptr::null_mut();
    // SAFETY: getmntinfo fills `list` with a pointer to an internal array of
    // `n` entries that stays valid until the next call; we only read it.
    let n = unsafe { libc::getmntinfo(&mut list, libc::MNT_NOWAIT) };
    if n <= 0 || list.is_null() {
        return None;
    }
    // SAFETY: as above, `list` points to `n` initialized entries.
    let mounts = unsafe { std::slice::from_raw_parts(list, n as usize) };
    let want = path.as_os_str().as_encoded_bytes();
    mounts.iter().find_map(|m| {
        // SAFETY: f_mntonname/f_mntfromname are NUL-terminated C strings.
        let on = unsafe { std::ffi::CStr::from_ptr(m.f_mntonname.as_ptr()) };
        (on.to_bytes() == want).then(|| {
            // SAFETY: see above.
            unsafe { std::ffi::CStr::from_ptr(m.f_mntfromname.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        })
    })
}

/// Server port of an NFS mount, from `nfsstat -m` (kernel mount info only;
/// never contacts the server).
pub fn nfs_mount_port(mountpoint: &std::path::Path) -> Option<u16> {
    let out = std::process::Command::new("/usr/bin/nfsstat").arg("-m").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let header = format!("{} from ", mountpoint.display());
    let lines = text.lines().skip_while(|l| !l.starts_with(&header)).skip(1);
    lines
        .take_while(|l| l.starts_with(' '))
        .find(|l| l.contains("NFS parameters:"))
        .and_then(|l| l.split([',', ' ']).find_map(|kv| kv.strip_prefix("port=")))
        .and_then(|p| p.parse().ok())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_names() {
        assert!(validate_store_name("default").is_ok());
        assert!(validate_store_name("build-2").is_ok());
        for bad in ["", "-x", "Upper", "a_b", "a.b", "a/b", &"x".repeat(65)] {
            assert!(validate_store_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn roots_exclude_attachments() {
        assert!(is_root("127.0.0.1:/"));
        assert!(is_root("buildfs://default"));
        assert!(!is_root("buildfs://att-web-node_modules-1a2b"));
        assert!(!is_root("127.0.0.1:/.buildfs/attach/x"));
        assert!(!is_root("/dev/disk3s1"));
        assert_eq!(fskit_store("buildfs://x"), Some("x"));
        assert_eq!(fskit_store("127.0.0.1:/"), None);
    }
}
