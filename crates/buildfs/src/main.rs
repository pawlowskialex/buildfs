use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};
use embednfs::NfsServer;
use tokio::net::TcpListener;
use tracing::{error, info, warn};

mod agent;
mod attach;
mod backend;
mod integrate;
mod mounts;

use backend::BackendKind;
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
    /// Mount a store and return, using FSKit on macOS 27 and later if the
    /// module is installed, and NFS otherwise. Safe to repeat.
    ///
    /// With --store or --foreground the NFS server stays in the foreground,
    /// and unmounts on Ctrl-C / SIGTERM.
    Mount {
        /// Directory holding metadata and data, for NFS (must not be inside
        /// the mount). Default: ~/.buildfs/stores/<name>. FSKit stores live in
        /// the module's container.
        #[arg(long, conflicts_with = "fskit")]
        store: Option<PathBuf>,
        #[command(flatten)]
        tuning: Tuning,
        mountpoint: PathBuf,
        /// NFS server port. Default: 20490, or with neither --store nor
        /// --foreground, the first free port from there.
        #[arg(long)]
        port: Option<u16>,
        /// Extra mount options, appended to the defaults.
        #[arg(long, short = 'o')]
        options: Option<String>,
        /// The same as --backend fskit.
        #[arg(long, conflicts_with = "backend")]
        fskit: bool,
        /// Use this backend, rather than the one that suits this Mac. Each
        /// backend has its own stores.
        #[arg(long, value_enum)]
        backend: Option<BackendKind>,
        /// Store name.
        #[arg(long, default_value = "default", conflicts_with = "store")]
        name: String,
        /// Keep the NFS server in the foreground.
        #[arg(long, conflicts_with = "fskit")]
        foreground: bool,
        /// Leave the mount in place on shutdown (used by the launchd agent;
        /// a restarted server is adopted by the existing mount).
        #[arg(long)]
        keep_mounted: bool,
        /// Created once mounted, for `mount` to know when to return.
        #[arg(long, hide = true)]
        ready_file: Option<PathBuf>,
    },
    /// Install a launchd agent that keeps a mount running across logins and
    /// restarts it if it crashes. The backend is chosen as for `mount`, once:
    /// upgrading macOS doesn't change what an installed agent mounts.
    InstallAgent {
        /// Directory holding metadata and data, for NFS. Default:
        /// ~/.buildfs/stores/<name>.
        #[arg(long, conflicts_with = "fskit")]
        store: Option<PathBuf>,
        #[command(flatten)]
        tuning: Tuning,
        mountpoint: PathBuf,
        /// NFS server port. Default: 20490 with --store, otherwise the first
        /// free port from there.
        #[arg(long)]
        port: Option<u16>,
        /// Agent name (label suffix), to run several mounts. Without --store
        /// this is also the store's name.
        #[arg(long, default_value = "default")]
        name: String,
        /// The same as --backend fskit.
        #[arg(long, conflicts_with = "backend")]
        fskit: bool,
        /// Use this backend, rather than the one that suits this Mac.
        #[arg(long, value_enum)]
        backend: Option<BackendKind>,
    },
    /// Unmount a volume and its attachments.
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
    /// Delete a store and everything in it. FSKit stores live in the module's
    /// sandbox container, which nothing else can touch.
    DeleteStore {
        /// Store name (the --name it was mounted with).
        name: String,
        /// The backend whose store to delete (default: the one that `mount`
        /// would use on this Mac).
        #[arg(long, value_enum)]
        backend: Option<BackendKind>,
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
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
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
            store,
            tuning,
            mountpoint,
            port,
            options,
            fskit,
            backend,
            name,
            foreground,
            keep_mounted,
            ready_file,
        } => {
            let requested = backend::requested(fskit, backend, store.as_deref())?;
            if let Some(parent) = mountpoint.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent).context("mountpoint parent")?;
            }
            let (kind, note) = choose_for_mount(requested, &mountpoint)?;
            if let Some(note) = note {
                eprintln!("{note}");
            }
            match kind {
                BackendKind::Fskit => {
                    if foreground {
                        bail!("--foreground is for NFS: with FSKit there is no server to run");
                    }
                    if requested.is_none() {
                        note_unused_nfs_store(&name);
                    }
                    mount_fskit(&name, &tuning, &mountpoint, options.as_deref())?;
                }
                BackendKind::Nfs if store.is_some() || foreground || keep_mounted => {
                    let store = match store {
                        Some(s) => s,
                        None => {
                            mounts::validate_store_name(&name)?;
                            backend::default_store(&name)?
                        }
                    };
                    let port = port.unwrap_or(backend::NFS_DEFAULT_PORT);
                    let options = options.as_deref();
                    let run = NfsRun { store: &store, tuning: &tuning, mountpoint: &mountpoint, port, options, keep_mounted };
                    serve_and_mount(run, ready_file.as_deref()).await?;
                }
                BackendKind::Nfs => mount_nfs_detached(&name, &tuning, &mountpoint, port, options.as_deref())?,
            }
        }
        Cmd::InstallAgent {
            store,
            tuning,
            mountpoint,
            port,
            name,
            fskit,
            backend,
        } => {
            let requested = backend::requested(fskit, backend, store.as_deref())?;
            let kind = requested.unwrap_or_else(|| {
                let (kind, note) = backend::choose(backend::Host::detect());
                if let Some(note) = note {
                    eprintln!("{note}");
                }
                kind
            });
            match kind {
                BackendKind::Fskit => {
                    probe_fskit(&name, &tuning, &mountpoint)?;
                    agent::install(agent::Kind::Fskit, &tuning, &mountpoint, &name)?;
                }
                BackendKind::Nfs => {
                    let port = match (port, &store) {
                        (Some(p), _) => p,
                        (None, Some(_)) => backend::NFS_DEFAULT_PORT,
                        (None, None) => backend::first_free_port()?,
                    };
                    let store = match store {
                        Some(s) => s,
                        None => {
                            mounts::validate_store_name(&name)?;
                            backend::default_store(&name)?
                        }
                    };
                    agent::install(agent::Kind::Nfs { store: &store, port }, &tuning, &mountpoint, &name)?;
                }
            }
        }
        Cmd::UninstallAgent { name } => agent::uninstall(&name)?,
        Cmd::Unmount { mount } => unmount_root(mount.as_deref())?,
        Cmd::DeleteStore { name, backend } => {
            mounts::validate_store_name(&name)?;
            let kind = backend.unwrap_or_else(|| backend::choose(backend::Host::detect()).0);
            match kind {
                BackendKind::Fskit => {
                    let url = mounts::fskit_url(&name);
                    if let Some((on, _)) = mounts::all_mounts().into_iter().find(|(_, from)| *from == url) {
                        bail!("{url} is mounted at {}; unmount it first", on.display());
                    }
                    if agent::fskit_agent_installed(&name) {
                        bail!("an agent mounts {url}; run uninstall-agent --name {name} first");
                    }
                    mounts::discard_fskit_store(&name)?;
                    if backend.is_none() {
                        note_unused_nfs_store(&name);
                    }
                }
                BackendKind::Nfs => {
                    let store = backend::default_store(&name)?;
                    if !store.exists() {
                        bail!("there is no NFS store called {name} ({})", store.display());
                    }
                    if backend::store_in_use(&store) {
                        bail!("{} is in use; unmount it first", store.display());
                    }
                    std::fs::remove_dir_all(&store).context("delete store")?;
                }
            }
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

/// The backend for a mount at `mountpoint`, and a note for the user: what the
/// flags ask for, else what is already mounted there, else what suits this Mac.
fn choose_for_mount(
    requested: Option<BackendKind>,
    mountpoint: &Path,
) -> anyhow::Result<(BackendKind, Option<String>)> {
    if let Some(kind) = requested {
        return Ok((kind, None));
    }
    let existing = mounted_from(&resolve_mountpoint(mountpoint)?);
    Ok(match existing.as_deref() {
        Some(from) if mounts::fskit_store(from).is_some() => (BackendKind::Fskit, None),
        Some(from) if mounts::is_nfs_root(from) => (BackendKind::Nfs, None),
        _ => backend::choose(backend::Host::detect()),
    })
}

/// FSKit was chosen for `name`, but an NFS store has the same name: say that
/// it isn't what's in use (which happens on upgrading to macOS 27).
fn note_unused_nfs_store(name: &str) {
    if let Ok(store) = backend::default_store(name)
        && store.exists()
    {
        eprintln!(
            "note: using FSKit, whose stores are separate. The NFS store {} isn't used: \
             reach it with --backend nfs, or remove it with delete-store --backend nfs {name}",
            store.display()
        );
    }
}

/// Fails if the module can't mount, rather than installing an agent that can
/// only log the problem every 30 s. Leaves the mounting to the agent.
fn probe_fskit(name: &str, tuning: &Tuning, mountpoint: &Path) -> anyhow::Result<()> {
    mounts::validate_store_name(name)?;
    let options = tuning.fskit_options()?;
    std::fs::create_dir_all(mountpoint).context("create mountpoint")?;
    let real = resolve_mountpoint(mountpoint)?;
    if mounted_from(&real).is_none() {
        mounts::mount_fskit(name, &real, &options)?;
        mounts::unmount(&real)?;
    }
    Ok(())
}

/// `buildfs mount` on NFS: runs the server in the background and returns once
/// the mount and its attachments are up, as FSKit does. Safe to repeat. The
/// server is this program again, in the foreground under a new session, and it
/// exits when the mount goes away.
fn mount_nfs_detached(
    name: &str,
    tuning: &Tuning,
    mountpoint: &Path,
    port: Option<u16>,
    extra: Option<&str>,
) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    mounts::validate_store_name(name)?;
    let mountpoint = resolve_mountpoint(mountpoint)?;
    let store = backend::default_store(name)?;
    if store.starts_with(&mountpoint) {
        bail!("store must not live inside the mountpoint");
    }
    let port = match mounted_from(&mountpoint) {
        None => port.map_or_else(backend::first_free_port, Ok)?,
        Some(from) if mounts::is_nfs_root(&from) => {
            let existing = nfs_mount_port(&mountpoint).context("could not determine the server port")?;
            if !backend::port_free(existing) {
                if !backend::store_in_use(&store) {
                    bail!("{} is already mounted, from another store", mountpoint.display());
                }
                attach::restore(&Root {
                    path: mountpoint,
                    backend: Backend::Nfs { port: existing },
                });
                return Ok(());
            }
            // The server died. Serving on the same port lets the kernel's
            // client recover (see serve_and_mount).
            existing
        }
        Some(from) => bail!("{} is already a mount point ({from})", mountpoint.display()),
    };

    let log_path = agent::log_path(name)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path).context("server log")?;
    let ready = std::env::temp_dir().join(format!("buildfs-ready-{}", std::process::id()));
    let _ = std::fs::remove_file(&ready);
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("mount").args(agent::nfs_server_args(&store, port, tuning)).arg("--ready-file").arg(&ready);
    if let Some(extra) = extra {
        cmd.args(["-o", extra]);
    }
    cmd.arg(&mountpoint).stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    // SAFETY: setsid is async-signal-safe and touches no memory. A session of
    // its own is what lets the server outlive this terminal.
    unsafe {
        let _ = cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut server = cmd.spawn().context("start the NFS server")?;

    let started = std::time::Instant::now();
    let result = loop {
        if ready.exists() {
            break Ok(());
        }
        if let Some(status) = server.try_wait()? {
            let text = std::fs::read_to_string(&log_path).unwrap_or_default();
            let tail: Vec<&str> = text.lines().rev().take(5).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            break Err(anyhow::anyhow!("the NFS server exited ({status}):\n{}", tail.join("\n")));
        }
        if started.elapsed() > Duration::from_secs(60) {
            break Err(anyhow::anyhow!("the NFS server isn't ready after 60 s; see {}", log_path.display()));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = std::fs::remove_file(&ready);
    result?;
    info!("mounted {} at {} (NFS, port {port}; log: {})", store.display(), mountpoint.display(), log_path.display());
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

fn unmount_root(mount: Option<&Path>) -> anyhow::Result<()> {
    let root = mounts::find_root_mount(mount)?;
    match &root.backend {
        Backend::Fskit { store } => {
            if agent::fskit_agent_installed(store) {
                warn!("an agent is installed for {store} and will mount it again within 30 s; use uninstall-agent to stop it");
            }
        }
        Backend::Nfs { .. } => {
            // Its server stays up while the mount is gone, and would serve
            // nothing until it was restarted.
            if let Some(name) = agent::agent_for_mountpoint(&root.path) {
                bail!("an agent serves {}; use uninstall-agent --name {name}", root.path.display());
            }
        }
    }
    for a in attach::load(&root) {
        if mounted_from(&a.path).is_some() {
            mounts::unmount(&a.path)?;
        }
    }
    mounts::unmount(&root.path)?;
    if let Backend::Nfs { port } = root.backend {
        // The server exits when it sees the mount go. Wait for it, so that the
        // store can be mounted again as soon as this returns.
        let started = std::time::Instant::now();
        while !backend::port_free(port) {
            if started.elapsed() > Duration::from_secs(30) {
                warn!("the NFS server on port {port} is still running");
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    println!("unmounted {}", root.path.display());
    Ok(())
}

/// How to run an NFS server and its mount in the foreground.
struct NfsRun<'a> {
    store: &'a Path,
    tuning: &'a Tuning,
    mountpoint: &'a Path,
    port: u16,
    options: Option<&'a str>,
    keep_mounted: bool,
}

/// What ended a server.
enum ServerEnd {
    Crashed,
    Signal,
    MountGone,
}

/// Returns once nothing is mounted at `mountpoint`. Uses cached mount info, so
/// it never contacts the server it runs in.
async fn mount_gone(mountpoint: &Path) {
    while mounted_from(mountpoint).is_some() {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Serves `run.store`, mounts it, and stays until a signal arrives or the mount
/// goes away.
async fn serve_and_mount(run: NfsRun<'_>, ready_file: Option<&Path>) -> anyhow::Result<()> {
    let NfsRun { store: store_arg, tuning: args, mountpoint, port, options, keep_mounted } = run;
    let store = std::fs::canonicalize(store_arg)
        .or_else(|_| {
            std::fs::create_dir_all(store_arg)?;
            std::fs::canonicalize(store_arg)
        })
        .context("store directory")?;
    // Never stat the mountpoint itself: if a previous instance died,
    // it is a dead `hard` NFS mount and any access would hang.
    let mountpoint = resolve_mountpoint(mountpoint)?;
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
        opts.push_str(extra);
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
    if adopt {
        let mut mounts_to_wake = vec![mountpoint.clone()];
        let wake = tokio::task::spawn_blocking(move || {
            mounts_to_wake.extend(mounts::nfs_attachments_on_port(port));
            mounts_to_wake.iter().for_each(|m| mounts::wake_nfs_client(m));
        });
        // Bounded: recovery is the kernel's business, and it mustn't keep a
        // server from running. The client normally takes up to about 4 s.
        if tokio::time::timeout(Duration::from_secs(30), wake).await.is_err() {
            warn!("the NFS client hasn't recovered after 30 s");
        }
    }
    {
        let root = Root {
            path: mountpoint.clone(),
            backend: Backend::Nfs { port },
        };
        // Mounting attachments talks to our own server, which runs
        // on the runtime's worker threads.
        let (root, list) = (root.clone(), attach::load_from_store(&vfs));
        let _ = tokio::task::spawn_blocking(move || attach::restore_list(&root, list)).await;
    }
    info!(
        "{} 127.0.0.1:/ at {} (store {})",
        if adopt { "serving existing mount" } else { "mounted" },
        mountpoint.display(),
        store.display()
    );
    if let Some(ready) = ready_file {
        std::fs::write(ready, b"").context("ready file")?;
    }
    let end = tokio::select! {
        r = serve => {
            error!("server exited unexpectedly: {r:?}");
            ServerEnd::Crashed
        }
        () = shutdown_signal() => {
            info!("unmounting");
            ServerEnd::Signal
        }
        // An agent's server must stay: launchd jobs can't reliably unmount,
        // and `uninstall-agent` unmounts while it still serves.
        () = mount_gone(&mountpoint), if !keep_mounted => {
            info!("{} was unmounted; exiting", mountpoint.display());
            ServerEnd::MountGone
        }
    };
    if !keep_mounted {
        // Attachments first: they keep the server busy otherwise.
        let attachments = if matches!(end, ServerEnd::MountGone) {
            // The registry is gone with the root, so find them by server.
            tokio::task::spawn_blocking(move || mounts::nfs_attachments_on_port(port)).await?
        } else {
            attach::load_from_store(&vfs).into_iter().map(|a| a.path).collect()
        };
        for path in attachments {
            if mounted_from(&path).is_some() {
                let _ = tokio::task::spawn_blocking(move || mounts::unmount(&path)).await;
            }
        }
        if !matches!(end, ServerEnd::MountGone) {
            unmount(&mountpoint).await;
        }
    }
    log_stats(&vfs);
    vfs.shutdown();
    if matches!(end, ServerEnd::Crashed) {
        // Non-zero so launchd's KeepAlive restarts us.
        std::process::exit(1);
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn mount_flags() {
        let ok = |args: &[&str]| Cli::try_parse_from(args).is_ok();
        for cmd in ["mount", "install-agent"] {
            assert!(ok(&["buildfs", cmd, "/m"]));
            assert!(ok(&["buildfs", cmd, "--store", "/s", "/m"]), "existing flags keep working");
            assert!(ok(&["buildfs", cmd, "--fskit", "/m"]), "existing flags keep working");
            assert!(ok(&["buildfs", cmd, "--backend", "fskit", "/m"]));
            assert!(ok(&["buildfs", cmd, "--backend", "nfs", "/m"]));
            assert!(ok(&["buildfs", cmd, "--backend", "nfs", "--store", "/s", "/m"]));
            assert!(!ok(&["buildfs", cmd, "--backend", "smb", "/m"]));
            assert!(!ok(&["buildfs", cmd, "--fskit", "--store", "/s", "/m"]));
            assert!(!ok(&["buildfs", cmd, "--fskit", "--backend", "nfs", "/m"]));
        }
        assert!(!ok(&["buildfs", "mount", "--name", "x", "--store", "/s", "/m"]));
        assert!(ok(&["buildfs", "mount", "--foreground", "/m"]));
        assert!(!ok(&["buildfs", "mount", "--foreground", "--fskit", "/m"]));
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
