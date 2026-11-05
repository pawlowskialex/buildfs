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

fn sync_arg(sync: crate::Sync) -> String {
    match sync {
        crate::Sync::Relaxed => "relaxed".into(),
        crate::Sync::Strict => "strict".into(),
    }
}

/// `mount` arguments for an NFS server on `store`, up to the mountpoint.
pub(crate) fn nfs_server_args(store: &Path, port: u16, args: &Tuning) -> Vec<String> {
    let mut argv = vec![
        "--store".into(),
        store.display().to_string(),
        "--port".into(),
        port.to_string(),
        "--level".into(),
        args.level.to_string(),
        "--compress-after".into(),
        args.compress_after.to_string(),
        "--sync".into(),
        sync_arg(args.sync),
    ];
    if let Some(w) = args.workers {
        argv.extend(["--workers".into(), w.to_string()]);
    }
    argv
}

fn nfs_argv(exe: &Path, store: &Path, port: u16, args: &Tuning, mountpoint: &Path) -> Vec<String> {
    let mut argv = vec![
        exe.display().to_string(),
        "mount".into(),
        // launchd-spawned processes can't reliably unmount (the kernel
        // unmount hangs or is refused), so the agent leaves the mount up;
        // a restarted agent adopts it and `uninstall-agent` unmounts.
        "--keep-mounted".into(),
    ];
    argv.extend(nfs_server_args(store, port, args));
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
        "--sync".into(),
        sync_arg(args.sync),
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

/// The log of the agent or detached server called `name`.
pub(crate) fn log_path(name: &str) -> anyhow::Result<PathBuf> {
    let logs = home()?.join("Library/Logs");
    std::fs::create_dir_all(&logs)?;
    Ok(logs.join(format!("buildfs-{name}.log")))
}

/// Installs an agent. Its plist records the backend and the store explicitly,
/// so that upgrading macOS doesn't switch stores underneath the user.
pub fn install(kind: Kind<'_>, args: &Tuning, mountpoint: &Path, name: &str) -> anyhow::Result<()> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("agent name may only contain letters, digits, '-' and '_'");
    }
    refuse_managed_agent(name)?;
    let exe = std::env::current_exe()?.canonicalize()?;
    let mountpoint = absolute(mountpoint)?;
    std::fs::create_dir_all(&mountpoint)?;
    let log = log_path(name)?;
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

/// Where a system-wide manager such as nix-darwin puts agents.
const SYSTEM_AGENTS: &str = "/Library/LaunchAgents";

/// An installed agent.
#[derive(Debug)]
struct Agent {
    name: String,
    mountpoint: Option<PathBuf>,
    store: Option<String>,
    fskit: bool,
    /// Installed by a system configuration, not by `install-agent`.
    managed: bool,
}

/// The value of an environment variable set in a plist.
fn plist_env(text: &str, var: &str) -> Option<String> {
    let after = text.split(&format!("<key>{var}</key>")).nth(1)?;
    let value = after.split("<string>").nth(1)?.split("</string>").next()?;
    Some(value.replace("&quot;", "\"").replace("&gt;", ">").replace("&lt;", "<").replace("&amp;", "&"))
}

impl Agent {
    /// Reads a plist. The Nix modules say what an agent does in its environment,
    /// because launchd wrappers make the arguments unreadable; `install-agent`
    /// plists have the mountpoint last, and name the backend and store.
    fn parse(name: String, text: &str, system: bool) -> Option<Self> {
        if let Some(mountpoint) = plist_env(text, "BUILDFS_MANAGED_MOUNTPOINT") {
            return Some(Self {
                name,
                mountpoint: Some(mountpoint.into()),
                store: plist_env(text, "BUILDFS_MANAGED_STORE"),
                fskit: plist_env(text, "BUILDFS_MANAGED_BACKEND").as_deref() == Some("fskit"),
                managed: true,
            });
        }
        let args = plist_args(text)?;
        Some(Self {
            name,
            mountpoint: args.last().map(PathBuf::from),
            store: args.windows(2).find(|w| w[0] == "--store").map(|w| w[1].clone()),
            fskit: args.iter().any(|a| a == "--fskit"),
            managed: system,
        })
    }

    fn how_to_stop(&self) -> String {
        if self.managed {
            "remove it from your system configuration".to_owned()
        } else {
            format!("run uninstall-agent --name {}", self.name)
        }
    }
}

/// The buildfs agents in `dirs`; `(directory, whether it's system-wide)`.
fn agents_in(dirs: &[(PathBuf, bool)]) -> Vec<Agent> {
    dirs.iter()
        .filter_map(|(dir, system)| Some((std::fs::read_dir(dir).ok()?, *system)))
        .flat_map(|(entries, system)| {
            entries.filter_map(Result::ok).filter_map(move |e| {
                let file = e.file_name();
                let name = file.to_str()?.strip_prefix("dev.buildfs.")?.strip_suffix(".plist")?.to_owned();
                Agent::parse(name, &std::fs::read_to_string(e.path()).ok()?, system)
            })
        })
        .collect()
}

fn agents() -> Vec<Agent> {
    let mut dirs = vec![(PathBuf::from(SYSTEM_AGENTS), true)];
    if let Ok(home) = home() {
        dirs.push((home.join("Library/LaunchAgents"), false));
    }
    agents_in(&dirs)
}

/// How to stop the agent that mounts the store called `name`, if there is one.
pub fn agent_using_store(name: &str) -> Option<String> {
    let store = crate::backend::default_store(name).ok()?.display().to_string();
    agents()
        .into_iter()
        .find(|a| (a.name == name && a.fskit) || a.store.as_deref() == Some(store.as_str()))
        .map(|a| a.how_to_stop())
}

/// How to stop the agent that serves `mountpoint`, if there is one.
pub fn agent_for_mountpoint(mountpoint: &Path) -> Option<String> {
    agents()
        .into_iter()
        .find(|a| a.mountpoint.as_deref() == Some(mountpoint))
        .map(|a| a.how_to_stop())
}

/// How to stop the FSKit agent for store `name`, if there is one (`buildfs
/// unmount` warns that it will mount the volume again).
pub fn fskit_agent_for_store(name: &str) -> Option<String> {
    agents().into_iter().find(|a| a.name == name && a.fskit).map(|a| a.how_to_stop())
}

/// Refuses to touch an agent that a system configuration manages.
fn refuse_managed_agent(name: &str) -> anyhow::Result<()> {
    if agents().iter().any(|a| a.name == name && a.managed) {
        bail!("{} is managed by your system configuration; change it there", label(name));
    }
    Ok(())
}

pub fn uninstall(name: &str) -> anyhow::Result<()> {
    refuse_managed_agent(name)?;
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
    fn agents_are_found_and_understood() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, text: String| std::fs::write(dir.path().join(format!("dev.buildfs.{name}.plist")), text).unwrap();
        // As nix-darwin and home-manager generate them: one shell string.
        let managed = |mountpoint: &str, backend: &str| {
            format!(
                "<plist><dict>\n<key>EnvironmentVariables</key>\n<dict>\n\t<key>BUILDFS_MANAGED_BACKEND</key>\n\t<string>{backend}</string>\n\
                 \t<key>BUILDFS_MANAGED_MOUNTPOINT</key>\n\t<string>{mountpoint}</string>\n\t<key>BUILDFS_MANAGED_STORE</key>\n\t<string>/h/s/x</string>\n</dict>\n\
                 <key>ProgramArguments</key>\n<array>\n\t<string>/bin/sh</string>\n\t<string>-c</string>\n\t<string>/bin/wait4path /nix/store &amp;&amp; exec /nix/b mount</string>\n</array>\n</dict></plist>\n"
            )
        };
        write("fast", managed("/m/a&amp;b", "fskit"));
        write("slow", managed("/m/slow", "nfs"));
        let argv = nfs_argv(Path::new("/b"), Path::new("/s"), 20490, &tuning(), Path::new("/m/own"));
        write("own", plist_text("own", &argv, &Kind::Nfs { store: Path::new("/s"), port: 20490 }, Path::new("/log")));
        std::fs::write(dir.path().join("dev.buildfs-setenv.plist"), "<plist/>").unwrap();

        let mut found = agents_in(&[(dir.path().to_path_buf(), false)]);
        found.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<&str> = found.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["fast", "own", "slow"]);
        let (fast, own, slow) = (&found[0], &found[1], &found[2]);
        assert!(fast.fskit && fast.managed);
        assert_eq!(fast.mountpoint.as_deref(), Some(Path::new("/m/a&b")));
        assert_eq!(fast.store.as_deref(), Some("/h/s/x"));
        assert!(fast.how_to_stop().contains("system configuration"));
        assert!(!slow.fskit && slow.managed);
        assert!(!own.fskit && !own.managed);
        assert_eq!(own.mountpoint.as_deref(), Some(Path::new("/m/own")));
        assert_eq!(own.store.as_deref(), Some("/s"));
        assert!(own.how_to_stop().contains("uninstall-agent --name own"));
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
    fn fskit_agent_keeps_strict() {
        let args = Tuning { sync: crate::Sync::Strict, ..tuning() };
        let argv = fskit_argv(Path::new("/b"), "x", &args, Path::new("/m"));
        assert!(argv.windows(2).any(|w| w[0] == "--sync" && w[1] == "strict"));
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

    /// An OS upgrade must not change what an installed agent mounts, so its
    /// arguments can't leave the backend to be chosen when it runs.
    #[test]
    fn agents_record_their_backend() {
        let fskit = fskit_argv(Path::new("/b"), "x", &tuning(), Path::new("/m"));
        assert!(fskit.iter().any(|a| a == "--fskit"));
        let nfs = nfs_argv(Path::new("/b"), Path::new("/s"), 20491, &tuning(), Path::new("/m"));
        assert!(nfs.windows(2).any(|w| w[0] == "--store" && w[1] == "/s"));
        assert!(nfs.windows(2).any(|w| w[0] == "--port" && w[1] == "20491"));
    }
}
