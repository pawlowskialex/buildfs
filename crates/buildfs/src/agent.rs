//! launchd agent management (`~/Library/LaunchAgents`).
//!
//! NFS: the agent is the server, kept alive by launchd.
//!
//! FSKit: `fskitd` runs the module, so there is nothing to keep alive. The
//! agent is `buildfs mount --fskit` every 30 s, which does nothing if all is
//! mounted. That also recovers from a crash: when the extension dies the
//! kernel unmounts its volume, so mounting again is all it takes. launchd
//! jobs may mount FSKit volumes but neither read nor unmount them
//! ("network volume" privacy rules), so the agent only ever mounts and
//! `uninstall-agent` unmounts from your shell.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

use crate::Tuning;
use crate::mounts::{self, Backend, Root};

fn label(name: &str) -> String {
    format!("dev.buildfs.{name}")
}

fn home() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn plist_path(name: &str) -> anyhow::Result<PathBuf> {
    Ok(home()?
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", label(name))))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn absolute(p: &Path) -> anyhow::Result<PathBuf> {
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(p))
    }
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() }
}

/// What an agent runs.
pub enum Kind<'a> {
    Nfs { store: &'a Path, port: u16 },
    Fskit,
}

/// The launchd scheduling keys for an agent.
fn schedule(kind: &Kind<'_>) -> &'static str {
    match kind {
        Kind::Nfs { .. } => {
            "    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>5</integer>
    <key>ProcessType</key>
    <string>Interactive</string>
"
        }
        Kind::Fskit => {
            "    <key>RunAtLoad</key>
    <true/>
    <key>StartInterval</key>
    <integer>30</integer>
    <key>ProcessType</key>
    <string>Background</string>
"
        }
    }
}

fn nfs_argv(exe: &Path, store: &Path, port: u16, args: &Tuning, mountpoint: &Path) -> Vec<String> {
    let mut argv = vec![
        exe.display().to_string(),
        "mount".into(),
        // launchd-spawned processes can't reliably unmount (the kernel
        // unmount hangs or is refused), so the agent leaves the mount up;
        // a restarted agent adopts it and `uninstall-agent` unmounts.
        "--keep-mounted".into(),
        "--store".into(),
        store.display().to_string(),
        "--port".into(),
        port.to_string(),
        "--level".into(),
        args.level.to_string(),
        "--compress-after".into(),
        args.compress_after.to_string(),
        "--sync".into(),
        match args.sync {
            crate::Sync::Relaxed => "relaxed".into(),
            crate::Sync::Strict => "strict".into(),
        },
    ];
    if let Some(w) = args.workers {
        argv.extend(["--workers".into(), w.to_string()]);
    }
    argv.push(mountpoint.display().to_string());
    argv
}

fn fskit_argv(exe: &Path, name: &str, args: &Tuning, mountpoint: &Path) -> Vec<String> {
    let mut argv = vec![
        exe.display().to_string(),
        "mount".into(),
        "--fskit".into(),
        "--name".into(),
        name.to_owned(),
        "--level".into(),
        args.level.to_string(),
        "--compress-after".into(),
        args.compress_after.to_string(),
    ];
    if let Some(w) = args.workers {
        argv.extend(["--workers".into(), w.to_string()]);
    }
    argv.push(mountpoint.display().to_string());
    argv
}

fn plist_text(name: &str, argv: &[String], kind: &Kind<'_>, log: &Path) -> String {
    let program_args: String = argv
        .iter()
        .map(|a| format!("        <string>{}</string>\n", xml_escape(a)))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{program_args}    </array>
{schedule}    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        label = label(name),
        schedule = schedule(kind),
        log = xml_escape(&log.display().to_string()),
    )
}

pub fn install(kind: Kind<'_>, args: &Tuning, mountpoint: &Path, name: &str) -> anyhow::Result<()> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("agent name may only contain letters, digits, '-' and '_'");
    }
    let exe = std::env::current_exe()?.canonicalize()?;
    let mountpoint = absolute(mountpoint)?;
    std::fs::create_dir_all(&mountpoint)?;
    let logs = home()?.join("Library/Logs");
    std::fs::create_dir_all(&logs)?;
    let log = logs.join(format!("buildfs-{name}.log"));
    let (argv, what) = match kind {
        Kind::Nfs { store, port } => {
            let store = absolute(store)?;
            std::fs::create_dir_all(&store)?;
            (nfs_argv(&exe, &store, port, args, &mountpoint), store.display().to_string())
        }
        Kind::Fskit => {
            // The agent's name is the store's name.
            mounts::validate_store_name(name)?;
            (fskit_argv(&exe, name, args, &mountpoint), mounts::fskit_url(name))
        }
    };
    let plist = plist_text(name, &argv, &kind, &log);
    let path = plist_path(name)?;
    std::fs::create_dir_all(path.parent().context("plist dir")?)?;
    // Replace a previous installation cleanly.
    if path.exists() {
        uninstall(name)?;
    }
    std::fs::write(&path, plist)?;
    let status = Command::new("/bin/launchctl")
        .arg("bootstrap")
        .arg(format!("gui/{}", uid()))
        .arg(&path)
        .status()?;
    if !status.success() {
        bail!("launchctl bootstrap failed: {status}");
    }
    println!("installed {} ({})", label(name), path.display());
    println!("mounting {what} at {}; log: {}", mountpoint.display(), log.display());
    Ok(())
}

/// The arguments recorded in an agent's plist.
fn plist_args(text: &str) -> Option<Vec<String>> {
    let args = text.split("<key>ProgramArguments</key>").nth(1)?;
    let args = args.split("</array>").next()?;
    Some(
        args.split("<string>")
            .skip(1)
            .filter_map(|s| s.split("</string>").next())
            .map(|s| s.replace("&quot;", "\"").replace("&gt;", ">").replace("&lt;", "<").replace("&amp;", "&"))
            .collect(),
    )
}

fn stop_agent(name: &str) {
    let _ = Command::new("/bin/launchctl")
        .stderr(std::process::Stdio::null())
        .arg("bootout")
        .arg(format!("gui/{}/{}", uid(), label(name)))
        .status();
}

/// Whether an FSKit agent is installed for store `name` (`buildfs unmount`
/// warns that it will mount the volume again).
pub fn fskit_agent_installed(name: &str) -> bool {
    plist_path(name)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| plist_args(&t))
        .is_some_and(|a| a.iter().any(|x| x == "--fskit"))
}

pub fn uninstall(name: &str) -> anyhow::Result<()> {
    let path = plist_path(name)?;
    let args = std::fs::read_to_string(&path).ok().and_then(|t| plist_args(&t)).unwrap_or_default();
    let fskit = args.iter().any(|a| a == "--fskit");
    // FSKit: stop the agent first, or it mounts everything again.
    // NFS: unmount from this (interactive) process while the agent still
    // serves, so the unmount can complete; then stop the agent.
    if fskit {
        stop_agent(name);
    }
    if let Some(mnt) = args.last().map(PathBuf::from) {
        let root = Root {
            path: mnt.clone(),
            backend: if fskit {
                Backend::Fskit { store: name.to_owned() }
            } else {
                Backend::Nfs { port: 0 }
            },
        };
        for a in crate::attach::load(&root) {
            if mounts::mounted_from(&a.path).is_some() {
                let _ = mounts::unmount(&a.path);
            }
        }
        if !fskit || mounts::mounted_from(&mnt).is_some() {
            let ok = Command::new("/sbin/umount").arg(&mnt).status().is_ok_and(|s| s.success());
            if !ok {
                let _ = Command::new("/sbin/umount").arg("-f").arg(&mnt).status();
            }
        }
    }
    if !fskit {
        stop_agent(name);
    }
    match std::fs::remove_file(&path) {
        Ok(()) => println!("removed {}", label(name)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("{} was not installed", label(name)),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tuning() -> Tuning {
        Tuning {
            sync: crate::Sync::Relaxed,
            level: 3,
            compress_after: 15,
            workers: None,
        }
    }

    #[test]
    fn fskit_plist_reruns_mount_and_roundtrips_args() {
        let argv = fskit_argv(Path::new("/opt/a&b/buildfs"), "default", &tuning(), Path::new("/mnt/my build"));
        let text = plist_text("default", &argv, &Kind::Fskit, Path::new("/log"));
        assert!(text.contains("<key>StartInterval</key>"));
        assert!(!text.contains("KeepAlive"));
        assert!(text.contains("a&amp;b"));
        assert_eq!(plist_args(&text).unwrap(), argv);
        assert_eq!(argv.last().unwrap(), "/mnt/my build");
        assert!(argv.iter().any(|a| a == "--fskit"));
    }

    #[test]
    fn nfs_plist_keeps_the_server_alive() {
        let argv = nfs_argv(Path::new("/b"), Path::new("/s"), 20490, &tuning(), Path::new("/m"));
        let text = plist_text("x", &argv, &Kind::Nfs { store: Path::new("/s"), port: 20490 }, Path::new("/log"));
        assert!(text.contains("<key>KeepAlive</key>"));
        assert!(!text.contains("StartInterval"));
        assert_eq!(plist_args(&text).unwrap(), argv);
        assert!(argv.iter().any(|a| a == "--keep-mounted"));
    }
}
