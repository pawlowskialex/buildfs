//! Attachments: store subtrees mounted at per-project artifact directories.
//!
//! Tools without a "put your output elsewhere" setting (`node_modules`,
//! `.next`, SwiftPM's `.build`, Gradle/CMake `build/`, `.venv`, ...) write
//! into a directory inside the project. Symlinks don't survive many of them
//! (npm replaces a symlinked `node_modules` with a real directory), so the
//! directory itself becomes a mount: tools see an ordinary directory, and
//! `.gitignore` rules keep working.
//!
//! On NFS that is a mount of the store subtree `/.buildfs/attach/<id>`, and
//! the registry lives inside the filesystem (`/.buildfs/attachments.json`),
//! so `buildfs mount` can restore every attachment at startup. The server reads
//! it from the store rather than through the mount (see `load_from_store`).
//!
//! FSKit can't mount a subtree: each volume is one extension process with
//! exclusive access to its store. So an attachment is a store of its own
//! (no dedup against other stores). The registry lives outside the volume,
//! because the launchd agent restores attachments and launchd jobs can
//! mount volumes but aren't allowed to read them.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use buildfs_core::meta::ROOT_INO;
use buildfs_core::vfs::Vfs;

use crate::mounts::{self, Backend, Root, find_root_mount, mounted_from};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Attachment {
    /// Absolute path of the attached directory.
    pub path: PathBuf,
    /// NFS: export path below the store root, e.g.
    /// `/.buildfs/attach/web-node_modules-1a2b3c4d`.
    /// FSKit: store name, e.g. `att-web-node-modules-1a2b3c4d`.
    pub export: String,
}

fn registry_path(root: &Root) -> anyhow::Result<PathBuf> {
    Ok(match &root.backend {
        Backend::Nfs { .. } => root.path.join(".buildfs/attachments.json"),
        Backend::Fskit { store } => std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?
            .join("Library/Application Support/buildfs")
            .join(format!("{store}-attachments.json")),
    })
}

pub fn load(root: &Root) -> Vec<Attachment> {
    let Ok(path) = registry_path(root) else {
        return Vec::new();
    };
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// The NFS registry, read by the server that owns the store. Reading it through
/// the mount would never return under launchd: privacy rules block its `open`
/// on a network volume, and the blocked open also wedges `umount`.
pub fn load_from_store(vfs: &Vfs) -> Vec<Attachment> {
    let read = || -> buildfs_core::vfs::Result<Vec<u8>> {
        let dir = vfs.lookup(ROOT_INO, ".buildfs")?;
        let ino = vfs.lookup(dir, "attachments.json")?;
        let mut bytes = Vec::new();
        loop {
            let (chunk, eof) = vfs.read(ino, bytes.len() as u64, 1 << 20)?;
            let done = eof || chunk.is_empty();
            bytes.extend_from_slice(&chunk);
            if done {
                return Ok(bytes);
            }
        }
    };
    read().ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(root: &Root, list: &[Attachment]) -> anyhow::Result<()> {
    let path = registry_path(root)?;
    std::fs::create_dir_all(path.parent().context("registry dir")?)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(list)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Stable, readable id: `<project>-<dir>-<hash of the full path>`.
fn export_for(path: &Path) -> String {
    let clean = |s: Option<&std::ffi::OsStr>| -> String {
        s.and_then(|s| s.to_str())
            .unwrap_or("x")
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
            .collect()
    };
    let project = clean(path.parent().and_then(Path::file_name));
    let name = clean(path.file_name());
    let hash = blake3::hash(path.as_os_str().as_encoded_bytes()).to_hex();
    format!("/.buildfs/attach/{project}-{name}-{}", &hash[..8])
}

/// FSKit store name for an attachment: `att-<project>-<dir>-<hash>`.
fn store_for(path: &Path) -> String {
    let clean = |s: Option<&std::ffi::OsStr>| -> String {
        s.and_then(|s| s.to_str())
            .unwrap_or("x")
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
            .take(20)
            .collect()
    };
    let project = clean(path.parent().and_then(Path::file_name));
    let name = clean(path.file_name());
    let hash = blake3::hash(path.as_os_str().as_encoded_bytes()).to_hex();
    format!("{}{project}-{name}-{}", mounts::FSKIT_ATTACH_PREFIX, &hash[..8])
}

fn mount_attachment(root: &Root, a: &Attachment) -> anyhow::Result<()> {
    match &root.backend {
        Backend::Nfs { port } => mounts::mount_nfs(*port, &a.export, &a.path, None),
        Backend::Fskit { .. } => mounts::mount_fskit(&a.export, &a.path, &[]),
    }
}

/// `dir` with `suffix` added to its name, next to it (same filesystem).
fn sibling(dir: &Path, suffix: &str) -> PathBuf {
    let mut name = dir.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    dir.with_file_name(name)
}

/// Removes the contents of `dir`.
fn empty_dir(dir: &Path) -> anyhow::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if p.is_dir() && !p.is_symlink() {
            std::fs::remove_dir_all(&p)?;
        } else {
            std::fs::remove_file(&p)?;
        }
    }
    Ok(())
}

/// Attaches a non-empty FSKit directory: the contents move aside, the empty
/// store goes on top, and they are copied in. On failure the original
/// directory is put back.
fn move_into_fskit(a: &Attachment) -> anyhow::Result<()> {
    let aside = sibling(&a.path, ".buildfs-moving");
    if aside.exists() {
        bail!("{} exists, left by an interrupted attach; resolve it first", aside.display());
    }
    std::fs::rename(&a.path, &aside)?;
    std::fs::create_dir(&a.path)?;
    let copied = mounts::mount_fskit(&a.export, &a.path, &[]).and_then(|()| {
        // ditto preserves hard links, xattrs and permissions.
        run(Command::new("/usr/bin/ditto").arg(&aside).arg(&a.path)).inspect_err(|_| {
            let _ = mounts::unmount(&a.path);
        })
    });
    if let Err(e) = copied {
        let _ = std::fs::remove_dir(&a.path);
        std::fs::rename(&aside, &a.path)?;
        return Err(e);
    }
    std::fs::remove_dir_all(&aside)?;
    Ok(())
}

fn is_empty_dir(p: &Path) -> anyhow::Result<bool> {
    Ok(std::fs::read_dir(p)?.next().is_none())
}

fn run(cmd: &mut Command) -> anyhow::Result<()> {
    let status = cmd.status()?;
    if !status.success() {
        bail!("{cmd:?} failed: {status}");
    }
    Ok(())
}

pub fn attach(dirs: &[PathBuf], mount: Option<&Path>, move_contents: bool) -> anyhow::Result<()> {
    let root = find_root_mount(mount)?;
    let mut list = load(&root);
    for dir in dirs {
        let path = mounts::resolve_mountpoint(dir)?;
        if path.starts_with(&root.path) {
            bail!("{} is already on the buildfs mount", path.display());
        }
        if let Some(from) = mounted_from(&path) {
            println!("{} is already mounted ({from}), skipping", path.display());
            continue;
        }
        let fskit = matches!(root.backend, Backend::Fskit { .. });
        let att = Attachment {
            path: path.clone(),
            export: if fskit { store_for(&path) } else { export_for(&path) },
        };
        let mut mounted = false;
        if path.exists() {
            if !path.is_dir() {
                bail!("{} is not a directory", path.display());
            }
            if !is_empty_dir(&path)? {
                if !move_contents {
                    bail!(
                        "{} is not empty; pass --move to move its contents into buildfs \
                         (or delete it if it is regenerable)",
                        path.display()
                    );
                }
                println!("moving {} into buildfs...", path.display());
                if fskit {
                    move_into_fskit(&att)?;
                    mounted = true;
                } else {
                    let subtree = root.path.join(att.export.trim_start_matches('/'));
                    std::fs::create_dir_all(&subtree)?;
                    // ditto preserves hard links, xattrs and permissions.
                    run(Command::new("/usr/bin/ditto").arg(&path).arg(&subtree))?;
                    empty_dir(&path)?;
                }
            }
        } else {
            std::fs::create_dir_all(&path)?;
        }
        if !mounted {
            if !fskit {
                std::fs::create_dir_all(root.path.join(att.export.trim_start_matches('/')))?;
            }
            mount_attachment(&root, &att)?;
        }
        list.retain(|a| a.path != path);
        list.push(att);
        save(&root, &list)?;
        println!("attached {}", path.display());
    }
    Ok(())
}

pub fn detach(dirs: &[PathBuf], mount: Option<&Path>, keep: bool) -> anyhow::Result<()> {
    let root = find_root_mount(mount)?;
    let mut list = load(&root);
    for dir in dirs {
        let path = mounts::resolve_mountpoint(dir)?;
        let Some(entry) = list.iter().find(|a| a.path == path).cloned() else {
            println!("{} is not attached", path.display());
            continue;
        };
        match &root.backend {
            Backend::Nfs { .. } => {
                if mounted_from(&path).is_some() {
                    mounts::unmount(&path)?;
                }
                let subtree = root.path.join(entry.export.trim_start_matches('/'));
                if keep && subtree.exists() {
                    println!("copying contents back to {}...", path.display());
                    std::fs::create_dir_all(&path)?;
                    run(Command::new("/usr/bin/ditto").arg(&subtree).arg(&path))?;
                }
                if subtree.exists() {
                    std::fs::remove_dir_all(&subtree)?;
                }
            }
            Backend::Fskit { .. } => detach_fskit(&entry, keep)?,
        }
        list.retain(|a| a.path != path);
        save(&root, &list)?;
        println!("detached {}", path.display());
    }
    Ok(())
}

/// Detaches an FSKit attachment. The contents, if kept, are copied out while
/// the store is still mounted (only the extension can read the store).
fn detach_fskit(a: &Attachment, keep: bool) -> anyhow::Result<()> {
    let kept = sibling(&a.path, ".buildfs-detaching");
    if keep {
        if mounted_from(&a.path).is_none() {
            mounts::mount_fskit(&a.export, &a.path, &[])?;
        }
        println!("copying contents back to {}...", a.path.display());
        if kept.exists() {
            bail!("{} exists, left by an interrupted detach; resolve it first", kept.display());
        }
        run(Command::new("/usr/bin/ditto").arg(&a.path).arg(&kept))?;
    }
    if mounted_from(&a.path).is_some() {
        mounts::unmount(&a.path)?;
    }
    if keep {
        std::fs::remove_dir(&a.path)?;
        std::fs::rename(&kept, &a.path)?;
    }
    mounts::delete_store(&a.export)
}

pub fn print_list(mount: Option<&Path>) -> anyhow::Result<()> {
    let root = find_root_mount(mount)?;
    let list = load(&root);
    if list.is_empty() {
        println!("no attachments");
    }
    for a in list {
        let state = match mounted_from(&a.path) {
            Some(_) => "mounted",
            None if a.path.exists() => "not mounted",
            None => "missing",
        };
        println!("{:12} {}", state, a.path.display());
    }
    Ok(())
}

/// Mounts every registered attachment (called by `buildfs mount` once the
/// root is up). Never mounts over a non-empty directory.
pub fn restore(root: &Root) {
    restore_list(root, load(root));
}

/// Mounts those of `list` that aren't mounted.
pub fn restore_list(root: &Root, list: Vec<Attachment>) {
    for a in list {
        if mounted_from(&a.path).is_some() {
            continue; // still mounted from before a restart: adopted
        }
        let parent_exists = a.path.parent().is_some_and(Path::exists);
        if !parent_exists {
            warn!("attachment {} skipped: parent directory is gone", a.path.display());
            continue;
        }
        if a.path.exists() && !is_empty_dir(&a.path).unwrap_or(false) {
            warn!(
                "attachment {} skipped: directory is not empty (detach and re-attach with --move)",
                a.path.display()
            );
            continue;
        }
        if let Err(e) = std::fs::create_dir_all(&a.path)
            .map_err(anyhow::Error::from)
            .and_then(|()| mount_attachment(root, &a))
        {
            warn!("attachment {} failed: {e:#}", a.path.display());
        } else {
            info!("attached {}", a.path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_reads_the_registry_from_its_store() {
        use buildfs_core::SetAttrs;
        use buildfs_core::vfs::{Caller, NewNode};

        let dir = tempfile::tempdir().unwrap();
        let vfs = Vfs::open(dir.path()).unwrap();
        assert_eq!(load_from_store(&vfs), [], "no registry yet");

        let list = vec![Attachment { path: "/p/node_modules".into(), export: "/.buildfs/attach/x".into() }];
        let (me, attrs) = (Caller::root(), SetAttrs::default());
        let (d, _) = vfs.create(&me, ROOT_INO, ".buildfs", NewNode::Dir, &attrs).unwrap();
        let (f, _) = vfs.create(&me, d, "attachments.json", NewNode::File, &attrs).unwrap();
        vfs.write_data(f, 0, &serde_json::to_vec_pretty(&list).unwrap()).unwrap();
        assert_eq!(load_from_store(&vfs), list);
        vfs.shutdown();
    }

    #[test]
    fn export_ids_are_stable_and_readable() {
        let a = export_for(Path::new("/Users/me/code/web app/node_modules"));
        assert!(a.starts_with("/.buildfs/attach/web_app-node_modules-"), "{a}");
        assert_eq!(a, export_for(Path::new("/Users/me/code/web app/node_modules")));
        assert_ne!(a, export_for(Path::new("/Users/me/other/web app/node_modules")));
    }

    #[test]
    fn fskit_store_names_are_valid_and_marked() {
        for p in [
            "/Users/me/code/web app/node_modules",
            "/Users/me/Äpfel_und.Birnen/.build",
            "/Users/me/a-very-long-project-name-that-goes-on-and-on/an-equally-long-directory-name",
        ] {
            let s = store_for(Path::new(p));
            mounts::validate_store_name(&s).unwrap();
            assert!(s.starts_with(mounts::FSKIT_ATTACH_PREFIX), "{s}");
        }
        assert_ne!(store_for(Path::new("/a/x/y")), store_for(Path::new("/b/x/y")));
    }

    #[test]
    fn fskit_registry_is_outside_the_volume() {
        let root = Root {
            path: PathBuf::from("/mnt/b"),
            backend: Backend::Fskit { store: "default".into() },
        };
        let p = registry_path(&root).unwrap();
        assert!(!p.starts_with("/mnt/b"), "{}", p.display());
        assert!(p.ends_with("Library/Application Support/buildfs/default-attachments.json"));
    }
}
