use std::path::{Path, PathBuf};
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

use mounts::{Backend, Root, mounted_from, nfs_mount_port, resolve_mountpoint};

use buildfs::ids::SystemIdMapper;
use buildfs::nfs::BuildFs;
use buildfs_core::vfs::{Options, SyncMode, Vfs};

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
#[command(version, about = "Compressing, deduplicating filesystem for build artifacts")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone)]
pub(crate) struct Tuning {
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

impl Tuning {
    /// Mount options carrying the settings to the FSKit module.
    fn fskit_options(&self) -> anyhow::Result<Vec<String>> {
        if matches!(self.sync, Sync::Strict) {
            bail!("--sync strict needs the NFS front end: with FSKit, the kernel's write-back cache decides when data reaches buildfs");
        }
        let mut o = Vec::new();
        if self.level != 3 {
            o.push(format!("level={}", self.level));
        }
        if self.compress_after != 15 {
            o.push(format!("compress_after={}", self.compress_after));
        }
        if let Some(w) = self.workers {
            o.push(format!("workers={w}"));
        }
        Ok(o)
    }

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
        /// Directory holding metadata and data.
        #[arg(long)]
        store: PathBuf,
        #[command(flatten)]
        tuning: Tuning,
        #[arg(long, default_value = "127.0.0.1:20490")]
        listen: String,
    },
    /// Serve a store and mount it; unmounts on Ctrl-C / SIGTERM. With
    /// --fskit: mount through the FSKit module and return (nothing to serve).
    Mount {
        /// Directory holding metadata and data (must not be inside the
        /// mount). With --fskit the store lives in the module's container.
        #[arg(long, required_unless_present = "fskit", conflicts_with = "fskit")]
        store: Option<PathBuf>,
        #[command(flatten)]
        tuning: Tuning,
        mountpoint: PathBuf,
        #[arg(long, default_value_t = 20490)]
        port: u16,
        /// Extra mount options, appended to the defaults.
        #[arg(long, short = 'o')]
        options: Option<String>,
        /// Use the FSKit module instead of the NFS server (much faster).
        #[arg(long)]
        fskit: bool,
        /// FSKit store name.
        #[arg(long, default_value = "default", conflicts_with = "store")]
        name: String,
        /// Leave the mount in place on shutdown (used by the launchd agent;
        /// a restarted server is adopted by the existing mount).
        #[arg(long)]
        keep_mounted: bool,
    },
    /// Install a launchd agent that keeps a mount running across logins and
    /// restarts it if it crashes.
    InstallAgent {
        /// Directory holding metadata and data (not with --fskit).
        #[arg(long, required_unless_present = "fskit", conflicts_with = "fskit")]
        store: Option<PathBuf>,
        #[command(flatten)]
        tuning: Tuning,
        mountpoint: PathBuf,
        #[arg(long, default_value_t = 20490)]
        port: u16,
        /// Agent name (label suffix), to run several mounts. With --fskit
        /// this is also the store's name.
        #[arg(long, default_value = "default")]
        name: String,
        /// Mount through the FSKit module instead of running an NFS server.
        #[arg(long)]
        fskit: bool,
    },
    /// Unmount an FSKit volume and its attachments.
    Unmount {
        /// Mount to use (default: the only buildfs mount).
        #[arg(long)]
        mount: Option<PathBuf>,
    },
    /// Stop and remove a launchd agent installed with `install-agent`.
    UninstallAgent {
        #[arg(long, default_value = "default")]
        name: String,
    },
    /// Delete an FSKit store and everything in it. Stores live in the
    /// module's sandbox container, which nothing else can touch.
    DeleteStore {
        /// Store name (the --name it was mounted with).
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
    match buildfs_core::raise_fd_limit() {
        Ok(n) => tracing::debug!("open file limit: {n}"),
        Err(e) => tracing::warn!("could not raise the open file limit: {e}"),
    }

    match Cli::parse().cmd {
        Cmd::Serve { store, tuning, listen } => {
            let vfs = Vfs::open_with(&store, tuning.options())?;
            let listener = TcpListener::bind(&listen).await?;
            info!("serving {} on {listen}", store.display());
            let server = NfsServer::builder(BuildFs::new(vfs.clone()))
                .server_identity(Vfs::server_identity(&store)?)
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
            fskit: true,
            name,
            tuning,
            mountpoint,
            options,
            ..
        } => mount_fskit(&name, &tuning, &mountpoint, options.as_deref())?,
        Cmd::Mount {
            store,
            tuning: args,
            mountpoint,
            port,
            options,
            keep_mounted,
            ..
        } => {
            let store_arg = store.context("--store is required")?;
            let store = std::fs::canonicalize(&store_arg)
                .or_else(|_| {
                    std::fs::create_dir_all(&store_arg)?;
                    std::fs::canonicalize(&store_arg)
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
                let root = Root {
                    path: mountpoint.clone(),
                    backend: Backend::Nfs { port },
                };
                let _ = tokio::task::spawn_blocking(move || attach::restore(&root)).await;
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
                let root = Root {
                    path: mountpoint.clone(),
                    backend: Backend::Nfs { port },
                };
                for a in attach::load(&root) {
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
            store,
            tuning,
            mountpoint,
            port,
            name,
            fskit,
        } => {
            let kind = if fskit {
                let _ = tuning.fskit_options()?; // refuses what FSKit can't do
                agent::Kind::Fskit
            } else {
                agent::Kind::Nfs {
                    store: store.as_deref().context("--store is required")?,
                    port,
                }
            };
            agent::install(kind, &tuning, &mountpoint, &name)?;
        }
        Cmd::UninstallAgent { name } => agent::uninstall(&name)?,
        Cmd::Unmount { mount } => unmount_fskit(mount.as_deref())?,
        Cmd::DeleteStore { name } => {
            mounts::validate_store_name(&name)?;
            let url = mounts::fskit_url(&name);
            if let Some((on, _)) = mounts::all_mounts().into_iter().find(|(_, from)| *from == url) {
                bail!("{url} is mounted at {}; unmount it first", on.display());
            }
            if agent::fskit_agent_installed(&name) {
                bail!("an agent mounts {url}; run uninstall-agent --name {name} first");
            }
            mounts::discard_fskit_store(&name)?;
            println!("deleted store {name}");
        }
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

/// `buildfs mount --fskit`: mounts the store and its attachments if they
/// aren't mounted yet, so it is safe to repeat (the agent does, every 30 s).
/// Nothing here may read the volume: launchd jobs aren't allowed to.
fn mount_fskit(name: &str, tuning: &Tuning, mountpoint: &Path, extra: Option<&str>) -> anyhow::Result<()> {
    mounts::validate_store_name(name)?;
    if name.starts_with(mounts::FSKIT_ATTACH_PREFIX) {
        bail!("store names starting with {} are reserved for attachments", mounts::FSKIT_ATTACH_PREFIX);
    }
    let mut options = tuning.fskit_options()?;
    options.extend(extra.into_iter().flat_map(|e| e.split(',')).map(str::to_owned));
    let mountpoint = resolve_mountpoint(mountpoint)?;
    match mounted_from(&mountpoint) {
        Some(from) if mounts::fskit_store(&from) == Some(name) => {}
        Some(from) => bail!("{} is already a mount point ({from})", mountpoint.display()),
        None => {
            std::fs::create_dir_all(&mountpoint).context("create mountpoint")?;
            mounts::mount_fskit(name, &mountpoint, &options)?;
            info!("mounted {} at {}", mounts::fskit_url(name), mountpoint.display());
        }
    }
    attach::restore(&Root {
        path: mountpoint,
        backend: Backend::Fskit { store: name.to_owned() },
    });
    Ok(())
}

fn unmount_fskit(mount: Option<&Path>) -> anyhow::Result<()> {
    let root = mounts::find_root_mount(mount)?;
    let Backend::Fskit { store } = &root.backend else {
        bail!("{} is an NFS mount: stop its server (Ctrl-C, or uninstall-agent)", root.path.display());
    };
    if agent::fskit_agent_installed(store) {
        warn!("an agent is installed for {store} and will mount it again within 30 s; use uninstall-agent to stop it");
    }
    for a in attach::load(&root) {
        if mounted_from(&a.path).is_some() {
            mounts::unmount(&a.path)?;
        }
    }
    mounts::unmount(&root.path)?;
    println!("unmounted {}", root.path.display());
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn mount_needs_store_unless_fskit() {
        assert!(Cli::try_parse_from(["buildfs", "mount", "/m"]).is_err());
        assert!(Cli::try_parse_from(["buildfs", "mount", "--store", "/s", "/m"]).is_ok());
        assert!(Cli::try_parse_from(["buildfs", "mount", "--fskit", "/m"]).is_ok());
        assert!(Cli::try_parse_from(["buildfs", "mount", "--fskit", "--store", "/s", "/m"]).is_err());
        assert!(Cli::try_parse_from(["buildfs", "mount", "--name", "x", "--store", "/s", "/m"]).is_err());
        assert!(Cli::try_parse_from(["buildfs", "install-agent", "/m"]).is_err());
        assert!(Cli::try_parse_from(["buildfs", "install-agent", "--fskit", "/m"]).is_ok());
    }

    #[test]
    fn fskit_options_carry_only_non_defaults() {
        let t = |level, compress_after, workers, sync| Tuning { sync, level, compress_after, workers };
        assert!(t(3, 15, None, Sync::Relaxed).fskit_options().unwrap().is_empty());
        assert_eq!(
            t(9, 0, Some(2), Sync::Relaxed).fskit_options().unwrap(),
            ["level=9", "compress_after=0", "workers=2"]
        );
        assert!(t(3, 15, None, Sync::Strict).fskit_options().is_err());
    }
}
