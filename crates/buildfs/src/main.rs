use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};
use embednfs::NfsServer;
use tokio::net::TcpListener;
use tracing::{error, info, warn};

mod agent;
mod attach;
mod integrate;
mod mounts;

use mounts::{mounted_from, nfs_mount_port, resolve_mountpoint};

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
    /// Point toolchains' artifact/cache locations at the mount (rust, xcode,
    /// go, uv, or `all`). Without tools: show status and what would change.
    Integrate {
        tools: Vec<String>,
        /// Mount to use (default: the only buildfs mount).
        #[arg(long)]
        mount: Option<PathBuf>,
        /// Remove settings that point at buildfs.
        #[arg(long)]
        revert: bool,
    },
    /// Store a project directory (node_modules, .build, build/, ...) on
    /// buildfs by mounting a store subtree there.
    Attach {
        #[arg(required = true)]
        dirs: Vec<PathBuf>,
        #[arg(long)]
        mount: Option<PathBuf>,
        /// Move existing contents into buildfs instead of refusing.
        #[arg(long = "move")]
        move_contents: bool,
    },
    /// Undo `attach`; the directory is left empty unless --keep.
    Detach {
        #[arg(required = true)]
        dirs: Vec<PathBuf>,
        #[arg(long)]
        mount: Option<PathBuf>,
        /// Copy the contents back into the (local) directory.
        #[arg(long)]
        keep: bool,
    },
    /// List attached directories.
    Attachments {
        #[arg(long)]
        mount: Option<PathBuf>,
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

            let mut opts = mounts::mount_options(port);
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
            {
                // Mounting attachments talks to our own server, which runs
                // on the runtime's worker threads.
                let (root, port) = (mountpoint.clone(), port);
                let _ = tokio::task::spawn_blocking(move || attach::restore(&root, port)).await;
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
                // Attachments first: they keep the server busy otherwise.
                for a in attach::load(&mountpoint) {
                    if mounted_from(&a.path).is_some() {
                        let _ = tokio::task::spawn_blocking(move || mounts::unmount(&a.path)).await;
                    }
                }
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
        Cmd::Integrate {
            tools,
            mount,
            revert,
        } => integrate::run(&tools, mount.as_deref(), revert)?,
        Cmd::Attach {
            dirs,
            mount,
            move_contents,
        } => attach::attach(&dirs, mount.as_deref(), move_contents)?,
        Cmd::Detach { dirs, mount, keep } => attach::detach(&dirs, mount.as_deref(), keep)?,
        Cmd::Attachments { mount } => attach::print_list(mount.as_deref())?,
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
