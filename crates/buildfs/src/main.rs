use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};
use embednfs::NfsServer;
use tokio::net::TcpListener;
use tracing::{info, warn};

use buildfs::nfs::BuildFs;
use buildfs::vfs::{Options, SyncMode, Vfs};

#[derive(Clone, Copy, ValueEnum)]
enum Sync {
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
struct StoreArgs {
    /// Directory holding metadata and data (must not be inside the mount).
    #[arg(long)]
    store: PathBuf,
    /// Durability of fsync/close.
    #[arg(long, value_enum, default_value = "relaxed")]
    sync: Sync,
    /// zstd compression level (1-19).
    #[arg(long, default_value_t = 3)]
    level: i32,
    /// Compress files this many seconds after their last write (0 = never).
    #[arg(long, default_value_t = 15)]
    compress_after: u64,
    /// Background compression threads (default: a quarter of the cores).
    #[arg(long)]
    workers: Option<usize>,
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
        } => {
            let store = std::fs::canonicalize(&args.store)
                .or_else(|_| {
                    std::fs::create_dir_all(&args.store)?;
                    std::fs::canonicalize(&args.store)
                })
                .context("store directory")?;
            let mountpoint = std::fs::canonicalize(&mountpoint).context("mountpoint")?;
            if store.starts_with(&mountpoint) {
                bail!("store must not live inside the mountpoint");
            }
            let vfs = Vfs::open_with(&store, args.options())?;
            let listener = TcpListener::bind(("127.0.0.1", port)).await?;
            let server = NfsServer::builder(BuildFs::new(vfs.clone()))
                .server_identity(Vfs::server_identity(&store)?)
                .mandatory_io_locks(false)
                .build();
            let serve = tokio::spawn(server.serve(listener));

            // namedattr: xattrs are stored natively instead of in `._` files.
            let mut opts =
                format!("vers=4.1,tcp,port={port},nobrowse,namedattr,rsize=1048576,wsize=1048576");
            if let Some(extra) = options {
                opts.push(',');
                opts.push_str(&extra);
            }
            let status = tokio::process::Command::new("/sbin/mount_nfs")
                .args(["-o", &opts, "127.0.0.1:/"])
                .arg(&mountpoint)
                .status()
                .await?;
            if !status.success() {
                serve.abort();
                vfs.shutdown();
                bail!("mount_nfs failed: {status}");
            }
            info!(
                "mounted {} at {} (store {})",
                "127.0.0.1:/",
                mountpoint.display(),
                store.display()
            );
            tokio::select! {
                r = serve => warn!("server exited: {r:?}"),
                () = shutdown_signal() => info!("unmounting"),
            }
            unmount(&mountpoint).await;
            log_stats(&vfs);
            vfs.shutdown();
        }
    }
    Ok(())
}

fn log_stats(vfs: &Vfs) {
    let st = vfs.compress_stats();
    if st.compacted + st.inlined > 0 {
        info!(
            "compressed {} files ({} inline): {:.1} MiB -> {:.1} MiB ({:.2}x); {} rehydrated",
            st.compacted + st.inlined,
            st.inlined,
            st.logical_bytes as f64 / 1048576.0,
            st.stored_bytes as f64 / 1048576.0,
            st.logical_bytes as f64 / st.stored_bytes.max(1) as f64,
            st.rehydrated
        );
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
