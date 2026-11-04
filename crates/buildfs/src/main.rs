use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};
use embednfs::NfsServer;
use tokio::net::TcpListener;
use tracing::{error, info, warn};

mod agent;

use buildfs::ids::SystemIdMapper;
use buildfs::nfs::BuildFs;
use buildfs::vfs::{Options, SyncMode, Vfs};

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum Sync {
    /// Survive buildfs crashes; no fsync on close (default, fast).
    Relaxed,
    /// fsync on close/fsync; survive power loss (slow).
    Strict,
}

impl From<Sync> for SyncMode {
    fn from(s: Sync) -> Self {
        match s {
            Sync::Relaxed => SyncMode::Relaxed,
            Sync::Strict => SyncMode::Strict,
        }
    }
}

#[derive(Parser)]
#[command(version, about = "Compressing NFSv4.1 filesystem for build artifacts")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone)]
pub(crate) struct StoreArgs {
    /// Directory holding metadata and data (must not be inside the mount).
    #[arg(long)]
    pub(crate) store: PathBuf,
    /// Durability of fsync/close.
    #[arg(long, value_enum, default_value = "relaxed")]
    pub(crate) sync: Sync,
    /// zstd compression level (1-19).
    #[arg(long, default_value_t = 3)]
    pub(crate) level: i32,
    /// Compress files this many seconds after their last write (0 = never).
    #[arg(long, default_value_t = 15)]
    pub(crate) compress_after: u64,
    /// Background compression threads (default: a quarter of the cores).
    #[arg(long)]
    pub(crate) workers: Option<usize>,
}

impl StoreArgs {
    fn options(&self) -> Options {
        let defaults = Options::default();
        Options {
            sync: self.sync.into(),
            level: self.level,
            compress_after: (self.compress_after > 0)
                .then(|| Duration::from_secs(self.compress_after)),
            compress_workers: self.workers.unwrap_or(defaults.compress_workers),
            ..defaults
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve a store over NFSv4.1 without mounting it.
    Serve {
        #[command(flatten)]
        store: StoreArgs,
        #[arg(long, default_value = "127.0.0.1:20490")]
        listen: String,
    },
    /// Serve a store and mount it; unmounts on Ctrl-C / SIGTERM.
    Mount {
        #[command(flatten)]
        store: StoreArgs,
        mountpoint: PathBuf,
        #[arg(long, default_value_t = 20490)]
        port: u16,
        /// Extra mount_nfs options, appended to the defaults.
        #[arg(long, short = 'o')]
        options: Option<String>,
        /// Leave the mount in place on shutdown (used by the launchd agent;
        /// a restarted server is adopted by the existing mount).
        #[arg(long)]
        keep_mounted: bool,
    },
    /// Install a launchd agent that keeps a mount running across logins and
    /// restarts it if it crashes.
    InstallAgent {
        #[command(flatten)]
        store: StoreArgs,
        mountpoint: PathBuf,
        #[arg(long, default_value_t = 20490)]
        port: u16,
        /// Agent name (label suffix), to run several mounts.
        #[arg(long, default_value = "default")]
        name: String,
    },
    /// Stop and remove a launchd agent installed with `install-agent`.
    UninstallAgent {
        #[arg(long, default_value = "default")]
        name: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,embednfs=warn".into()),
        )
        .init();

    match Cli::parse().cmd {
        Cmd::Serve { store: args, listen } => {
            let vfs = Vfs::open_with(&args.store, args.options())?;
            let listener = TcpListener::bind(&listen).await?;
            info!("serving {} on {listen}", args.store.display());
            let server = NfsServer::builder(BuildFs::new(vfs.clone()))
                .server_identity(Vfs::server_identity(&args.store)?)
                .mandatory_io_locks(false)
                .id_mapper(SystemIdMapper::default())
                .build();
            tokio::select! {
                r = server.serve(listener) => r?,
                () = shutdown_signal() => info!("shutting down"),
            }
            log_stats(&vfs);
            vfs.shutdown();
        }
        Cmd::Mount {
            store: args,
            mountpoint,
            port,
            options,
            keep_mounted,
        } => {
            let store = std::fs::canonicalize(&args.store)
                .or_else(|_| {
                    std::fs::create_dir_all(&args.store)?;
                    std::fs::canonicalize(&args.store)
                })
                .context("store directory")?;
            // Never stat the mountpoint itself: if a previous instance died,
            // it is a dead `hard` NFS mount and any access would hang.
            let mountpoint = resolve_mountpoint(&mountpoint)?;
            if store.starts_with(&mountpoint) {
                bail!("store must not live inside the mountpoint");
            }
            // Binding first proves no live instance serves this port, so an
            // existing NFS mount from 127.0.0.1 here is stale.
            let listener = TcpListener::bind(("127.0.0.1", port))
                .await
                .with_context(|| format!("port {port} is in use (already mounted?)"))?;
            let mut adopt = false;
            if let Some(from) = mounted_from(&mountpoint) {
                if !(from.starts_with("127.0.0.1:") || from.starts_with("localhost:")) {
                    bail!("{} is already a mount point ({from})", mountpoint.display());
                }
                if nfs_mount_port(&mountpoint) == Some(port) {
                    // Our previous instance died; the kernel client keeps
                    // retrying this port and recovers once we serve again
                    // (stable filehandles + server identity).
                    info!("adopting existing mount at {}", mountpoint.display());
                    adopt = true;
                } else {
                    warn!("removing stale mount {from} at {}", mountpoint.display());
                    force_unmount(&mountpoint).await;
                }
            }
            if !adopt && !mountpoint.exists() {
                std::fs::create_dir_all(&mountpoint).context("create mountpoint")?;
            }
            let vfs = Vfs::open_with(&store, args.options())?;
            let server = NfsServer::builder(BuildFs::new(vfs.clone()))
                .server_identity(Vfs::server_identity(&store)?)
                .mandatory_io_locks(false)
                .id_mapper(SystemIdMapper::default())
                .build();
            let serve = tokio::spawn(server.serve(listener));

            // namedattr: xattrs are stored natively instead of in `._` files.
            let mut opts =
                format!("vers=4.1,tcp,port={port},nobrowse,namedattr,rsize=1048576,wsize=1048576");
            if let Some(extra) = options {
                opts.push(',');
                opts.push_str(&extra);
            }
            let status = if adopt {
                std::process::ExitStatus::default()
            } else {
                tokio::process::Command::new("/sbin/mount_nfs")
                    .args(["-o", &opts, "127.0.0.1:/"])
                    .arg(&mountpoint)
                    .status()
                    .await?
            };
            if !status.success() {
                serve.abort();
                vfs.shutdown();
                bail!("mount_nfs failed: {status}");
            }
            info!(
                "{} 127.0.0.1:/ at {} (store {})",
                if adopt { "serving existing mount" } else { "mounted" },
                mountpoint.display(),
                store.display()
            );
            let crashed = tokio::select! {
                r = serve => {
                    error!("server exited unexpectedly: {r:?}");
                    true
                }
                () = shutdown_signal() => {
                    info!("unmounting");
                    false
                }
            };
            if !keep_mounted {
                unmount(&mountpoint).await;
            }
            log_stats(&vfs);
            vfs.shutdown();
            if crashed {
                // Non-zero so launchd's KeepAlive restarts us.
                std::process::exit(1);
            }
        }
        Cmd::InstallAgent {
            store: args,
            mountpoint,
            port,
            name,
        } => agent::install(&args, &mountpoint, port, &name)?,
        Cmd::UninstallAgent { name } => agent::uninstall(&name)?,
    }
    Ok(())
}

fn log_stats(vfs: &Vfs) {
    let st = vfs.compress_stats();
    if st.compacted + st.inlined > 0 {
        info!(
            "compressed {} files ({} inline): {:.1} MiB -> {:.1} MiB ({:.2}x); \
             {} chunks ({:.1} MiB) deduplicated; {} rehydrated",
            st.compacted + st.inlined,
            st.inlined,
            st.logical_bytes as f64 / 1048576.0,
            st.stored_bytes as f64 / 1048576.0,
            st.logical_bytes as f64 / st.stored_bytes.max(1) as f64,
            st.dedup_chunks,
            st.dedup_bytes as f64 / 1048576.0,
            st.rehydrated
        );
    }
    if let Ok((bytes, n)) = vfs.chunk_usage() {
        info!("chunk store: {n} distinct chunks, {:.1} MiB", bytes as f64 / 1048576.0);
    }
}

/// Absolute mountpoint path without touching the mountpoint itself.
fn resolve_mountpoint(p: &std::path::Path) -> anyhow::Result<PathBuf> {
    let name = p.file_name().context("mountpoint needs a final component")?;
    let parent = match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    Ok(std::fs::canonicalize(&parent).context("mountpoint parent")?.join(name))
}

/// The `f_mntfromname` of a mount at exactly `path`, using cached mount
/// info (MNT_NOWAIT) so dead mounts are never contacted.
fn mounted_from(path: &std::path::Path) -> Option<String> {
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
fn nfs_mount_port(mountpoint: &std::path::Path) -> Option<u16> {
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

async fn force_unmount(mountpoint: &std::path::Path) {
    let ok = tokio::process::Command::new("/sbin/umount")
        .arg("-f")
        .arg(mountpoint)
        .status()
        .await
        .is_ok_and(|s| s.success());
    if !ok {
        let _ = tokio::process::Command::new("/usr/sbin/diskutil")
            .args(["unmount", "force"])
            .arg(mountpoint)
            .status()
            .await;
    }
}

async fn unmount(mountpoint: &std::path::Path) {
    for attempt in 0..5 {
        let ok = tokio::process::Command::new("/sbin/umount")
            .arg(mountpoint)
            .status()
            .await
            .is_ok_and(|s| s.success());
        if ok {
            return;
        }
        warn!("umount attempt {attempt} failed (busy?), retrying");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    warn!("forcing unmount");
    let _ = tokio::process::Command::new("/usr/sbin/diskutil")
        .args(["unmount", "force"])
        .arg(mountpoint)
        .status()
        .await;
}

async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}
