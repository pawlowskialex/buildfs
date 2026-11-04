//! Attachments: store subtrees mounted at per-project artifact directories.
//!
//! Tools without a "put your output elsewhere" setting (`node_modules`,
//! `.next`, SwiftPM's `.build`, Gradle/CMake `build/`, `.venv`, ...) write
//! into a directory inside the project. Symlinks don't survive many of them
//! (npm replaces a symlinked `node_modules` with a real directory), so the
//! directory itself becomes an NFS mount of `/.buildfs/attach/<id>`: tools
//! see an ordinary directory, and `.gitignore` rules keep working.
//!
//! The registry lives inside the filesystem (`/.buildfs/attachments.json`),
//! so `buildfs mount` can restore every attachment at startup.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::mounts::{self, find_root_mount, mounted_from};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Attachment {
    /// Absolute path of the attached directory.
    pub path: PathBuf,
    /// Export path below the store root, e.g. `/.buildfs/attach/web-node_modules-1a2b3c4d`.
    pub export: String,
}

fn registry_path(root: &Path) -> PathBuf {
    root.join(".buildfs/attachments.json")
}

pub fn load(root: &Path) -> Vec<Attachment> {
    std::fs::read(registry_path(root))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save(root: &Path, list: &[Attachment]) -> anyhow::Result<()> {
    let path = registry_path(root);
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
    let (root, port) = find_root_mount(mount)?;
    let mut list = load(&root);
    for dir in dirs {
        let path = mounts::resolve_mountpoint(dir)?;
        if path.starts_with(&root) {
            bail!("{} is already on the buildfs mount", path.display());
        }
        if let Some(from) = mounted_from(&path) {
            println!("{} is already mounted ({from}), skipping", path.display());
            continue;
        }
        let export = export_for(&path);
        let subtree = root.join(export.trim_start_matches('/'));
        std::fs::create_dir_all(&subtree)?;
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
                // ditto preserves hard links, xattrs and permissions.
                run(Command::new("/usr/bin/ditto").arg(&path).arg(&subtree))?;
                for e in std::fs::read_dir(&path)? {
                    let p = e?.path();
                    if p.is_dir() && !p.is_symlink() {
                        std::fs::remove_dir_all(&p)?;
                    } else {
                        std::fs::remove_file(&p)?;
                    }
                }
            }
        } else {
            std::fs::create_dir_all(&path)?;
        }
        mounts::mount_nfs(port, &export, &path, None)?;
        list.retain(|a| a.path != path);
        list.push(Attachment {
            path: path.clone(),
            export,
        });
        save(&root, &list)?;
        println!("attached {}", path.display());
    }
    Ok(())
}

pub fn detach(dirs: &[PathBuf], mount: Option<&Path>, keep: bool) -> anyhow::Result<()> {
    let (root, _) = find_root_mount(mount)?;
    let mut list = load(&root);
    for dir in dirs {
        let path = mounts::resolve_mountpoint(dir)?;
        let Some(entry) = list.iter().find(|a| a.path == path).cloned() else {
            println!("{} is not attached", path.display());
            continue;
        };
        if mounted_from(&path).is_some() {
            mounts::unmount(&path)?;
        }
        let subtree = root.join(entry.export.trim_start_matches('/'));
        if keep && subtree.exists() {
            println!("copying contents back to {}...", path.display());
            std::fs::create_dir_all(&path)?;
            run(Command::new("/usr/bin/ditto").arg(&subtree).arg(&path))?;
        }
        if subtree.exists() {
            std::fs::remove_dir_all(&subtree)?;
        }
        list.retain(|a| a.path != path);
        save(&root, &list)?;
        println!("detached {}", path.display());
    }
    Ok(())
}

pub fn print_list(mount: Option<&Path>) -> anyhow::Result<()> {
    let (root, _) = find_root_mount(mount)?;
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
pub fn restore(root: &Path, port: u16) {
    for a in load(root) {
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
            .and_then(|()| mounts::mount_nfs(port, &a.export, &a.path, None))
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
    fn export_ids_are_stable_and_readable() {
        let a = export_for(Path::new("/Users/me/code/web app/node_modules"));
        assert!(a.starts_with("/.buildfs/attach/web_app-node_modules-"), "{a}");
        assert_eq!(a, export_for(Path::new("/Users/me/code/web app/node_modules")));
        assert_ne!(a, export_for(Path::new("/Users/me/other/web app/node_modules")));
    }
}
