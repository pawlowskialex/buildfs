//! Inspecting and creating NFS mounts without touching possibly dead ones.

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

/// The buildfs root mount to operate on: `explicit` if given, otherwise the
/// only mount of `127.0.0.1:/`. Returns `(mountpoint, port)`.
pub fn find_root_mount(explicit: Option<&Path>) -> anyhow::Result<(PathBuf, u16)> {
    let roots: Vec<PathBuf> = all_mounts()
        .into_iter()
        .filter(|(_, from)| from == "127.0.0.1:/" || from == "localhost:/")
        .map(|(on, _)| on)
        .collect();
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
    let port = nfs_mount_port(&mnt).context("could not determine the server port")?;
    Ok((mnt, port))
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

