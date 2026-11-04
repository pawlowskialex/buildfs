//! launchd agent management (`~/Library/LaunchAgents`).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

use crate::StoreArgs;

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

pub fn install(args: &StoreArgs, mountpoint: &Path, port: u16, name: &str) -> anyhow::Result<()> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("agent name may only contain letters, digits, '-' and '_'");
    }
    let exe = std::env::current_exe()?.canonicalize()?;
    let store = absolute(&args.store)?;
    let mountpoint = absolute(mountpoint)?;
    std::fs::create_dir_all(&store)?;
    std::fs::create_dir_all(&mountpoint)?;
    let logs = home()?.join("Library/Logs");
    std::fs::create_dir_all(&logs)?;
    let log = logs.join(format!("buildfs-{name}.log"));

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
    let program_args: String = argv
        .iter()
        .map(|a| format!("        <string>{}</string>\n", xml_escape(a)))
        .collect();
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{program_args}    </array>
    <key>RunAtLoad</key>
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
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        label = label(name),
        log = xml_escape(&log.display().to_string()),
    );
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
    println!("mounting {} at {}; log: {}", store.display(), mountpoint.display(), log.display());
    Ok(())
}

/// The mountpoint recorded in an installed agent's plist (last argument).
fn installed_mountpoint(path: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(path).ok()?;
    let args = text.split("<key>ProgramArguments</key>").nth(1)?;
    let args = args.split("</array>").next()?;
    let last = args.rsplit("<string>").next()?.split("</string>").next()?;
    let unescaped = last
        .replace("&quot;", "\"")
        .replace("&gt;", ">")
        .replace("&lt;", "<")
        .replace("&amp;", "&");
    Some(PathBuf::from(unescaped))
}

pub fn uninstall(name: &str) -> anyhow::Result<()> {
    let path = plist_path(name)?;
    // Unmount from this (interactive) process while the agent still serves,
    // so the unmount can complete; then stop the agent.
    if let Some(mnt) = installed_mountpoint(&path) {
        for a in crate::attach::load(&mnt) {
            if crate::mounts::mounted_from(&a.path).is_some() {
                let _ = crate::mounts::unmount(&a.path);
            }
        }
        let ok = Command::new("/sbin/umount").arg(&mnt).status().is_ok_and(|s| s.success());
        if !ok {
            let _ = Command::new("/sbin/umount").arg("-f").arg(&mnt).status();
        }
    }
    let _ = Command::new("/bin/launchctl")
        .stderr(std::process::Stdio::null())
        .arg("bootout")
        .arg(format!("gui/{}/{}", uid(), label(name)))
        .status();
    match std::fs::remove_file(&path) {
        Ok(()) => println!("removed {}", label(name)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("{} was not installed", label(name)),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
