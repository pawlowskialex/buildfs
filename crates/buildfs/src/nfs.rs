//! embednfs adapter: exposes [`Vfs`] as an NFSv4.1 filesystem.
//!
//! The core is synchronous (redb + pread/pwrite), so every call hops onto
//! tokio's blocking pool.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use embednfs::{
    AccessMask, Attrs, CommitSupport, CreateKind, CreateRequest, CreateResult, DirEntry, DirPage,
    AuthContext, FileSystem, FsCapabilities, FsError, FsLimits, FsResult, FsStats, HardLinks,
    ObjectType, OpenTracking, ReadResult, RequestContext, SetAttrs, SetTime, StableHandles,
    Symlinks, Timestamp, WriteResult, WriteStability, XattrSetMode, Xattrs,
};

use buildfs_core::meta::{FLAG_ARCHIVE, FLAG_HIDDEN, FLAG_SYSTEM, Ino, Inode, Kind, ROOT_INO, Ts};
use buildfs_core::vfs::{self, Caller, MAX_FILE_SIZE, NewNode, SyncMode, Vfs};

// ---------------------------------------------------------------------------
// Conversions between embednfs's types and the core's
// ---------------------------------------------------------------------------

fn fs_error(e: buildfs_core::FsError) -> FsError {
    use buildfs_core::FsError as E;
    match e {
        E::PermissionDenied => FsError::PermissionDenied,
        E::NotFound => FsError::NotFound,
        E::Io => FsError::Io,
        E::AlreadyExists => FsError::AlreadyExists,
        E::NotDirectory => FsError::NotDirectory,
        E::IsDirectory => FsError::IsDirectory,
        E::InvalidInput => FsError::InvalidInput,
        E::FileTooLarge => FsError::FileTooLarge,
        E::NoSpace => FsError::NoSpace,
        E::NameTooLong => FsError::NameTooLong,
        E::NotEmpty => FsError::NotEmpty,
        E::Stale => FsError::Stale,
    }
}

fn set_time(t: SetTime) -> buildfs_core::SetTime {
    match t {
        SetTime::ServerNow => buildfs_core::SetTime::ServerNow,
        SetTime::Client(t) => buildfs_core::SetTime::Client(buildfs_core::Timestamp {
            seconds: t.seconds,
            nanos: t.nanos,
        }),
    }
}

fn set_attrs(a: &SetAttrs) -> buildfs_core::SetAttrs {
    buildfs_core::SetAttrs {
        size: a.size,
        archive: a.archive,
        hidden: a.hidden,
        mode: a.mode,
        uid: a.uid,
        gid: a.gid,
        system: a.system,
        atime: a.atime.map(set_time),
        mtime: a.mtime.map(set_time),
        birthtime: a.birthtime.map(set_time),
    }
}

fn xattr_mode(m: XattrSetMode) -> buildfs_core::XattrSetMode {
    match m {
        XattrSetMode::CreateOrReplace => buildfs_core::XattrSetMode::CreateOrReplace,
        XattrSetMode::CreateOnly => buildfs_core::XattrSetMode::CreateOnly,
        XattrSetMode::ReplaceOnly => buildfs_core::XattrSetMode::ReplaceOnly,
    }
}

fn caller(ctx: &RequestContext) -> Caller {
    match &ctx.auth {
        AuthContext::Sys {
            uid,
            gid,
            supplemental_gids,
        } => Caller {
            uid: *uid,
            gid: *gid,
            groups: supplemental_gids.clone(),
        },
        _ => Caller {
            uid: Caller::NOBODY,
            gid: Caller::NOBODY,
            groups: Vec::new(),
        },
    }
}

#[derive(Clone)]
pub struct BuildFs {
    vfs: Arc<Vfs>,
}

impl BuildFs {
    pub fn new(vfs: Arc<Vfs>) -> Self {
        Self { vfs }
    }

    async fn run<T, F>(&self, f: F) -> FsResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Vfs) -> vfs::Result<T> + Send + 'static,
    {
        let vfs = self.vfs.clone();
        tokio::task::spawn_blocking(move || f(&vfs))
            .await
            .map_err(|_| FsError::ServerFault)?
            .map_err(fs_error)
    }

    /// Runs a cheap read-only operation directly on the async worker.
    ///
    /// redb reads are MVCC snapshots that never wait for writers and the
    /// hot data is page-cached, so these finish in microseconds; skipping
    /// the blocking-pool handoff roughly halves metadata round-trip latency.
    fn inline<T>(&self, f: impl FnOnce(&Vfs) -> vfs::Result<T>) -> FsResult<T> {
        f(&self.vfs).map_err(fs_error)
    }

    fn attrs(vfs: &Vfs, ino: Ino, inode: &Inode) -> Attrs {
        let ts = |t: Ts| Timestamp {
            seconds: t.sec,
            nanos: t.nsec,
        };
        let object_type = match inode.kind {
            Kind::File => ObjectType::File,
            Kind::Dir => ObjectType::Directory,
            Kind::Symlink => ObjectType::Symlink,
            Kind::Fifo => ObjectType::Fifo,
            Kind::Socket => ObjectType::Socket,
            Kind::CharDev => ObjectType::CharDevice,
            Kind::BlockDev => ObjectType::BlockDevice,
        };
        let mut a = Attrs::new(object_type, ino);
        a.change = inode.change;
        a.size = inode.size;
        a.space_used = vfs.space_used(ino, inode);
        a.link_count = inode.nlink;
        a.mode = inode.mode;
        a.uid = inode.uid;
        a.gid = inode.gid;
        a.atime = ts(inode.atime);
        a.mtime = ts(inode.mtime);
        a.ctime = ts(inode.ctime);
        a.birthtime = ts(inode.btime);
        a.hidden = inode.flags & FLAG_HIDDEN != 0;
        a.archive = inode.flags & FLAG_ARCHIVE != 0;
        a.system = inode.flags & FLAG_SYSTEM != 0;
        a.has_named_attrs = inode.xattr_count > 0;
        a.rdev_major = inode.rdev.0;
        a.rdev_minor = inode.rdev.1;
        a
    }
}

#[async_trait]
impl FileSystem for BuildFs {
    type Handle = Ino;

    fn root(&self) -> Ino {
        ROOT_INO
    }

    fn capabilities(&self) -> FsCapabilities {
        FsCapabilities {
            symlinks: true,
            hard_links: true,
            xattrs: true,
            explicit_sync: true,
            case_sensitive: true,
            case_preserving: true,
            stable_readdir_cookies: true,
        }
    }

    fn limits(&self) -> FsLimits {
        FsLimits {
            max_name_bytes: 255,
            max_read: 1 << 20,
            max_write: 1 << 20,
            max_file_size: MAX_FILE_SIZE,
        }
    }

    async fn statfs(&self, _ctx: &RequestContext) -> FsResult<FsStats> {
        self.inline(|vfs| {
            let st = vfs.statfs()?;
            #[allow(clippy::unnecessary_cast, reason = "f_frsize width differs across platforms")]
            let frsize = st.f_frsize as u64;
            Ok(FsStats {
                total_bytes: st.f_blocks as u64 * frsize,
                free_bytes: st.f_bfree as u64 * frsize,
                avail_bytes: st.f_bavail as u64 * frsize,
                total_files: 1 << 40,
                free_files: 1 << 39,
                avail_files: 1 << 39,
            })
        })
    }

    async fn getattr(&self, _ctx: &RequestContext, h: &Ino) -> FsResult<Attrs> {
        let ino = *h;
        self.inline(|vfs| Ok(Self::attrs(vfs, ino, &vfs.getattr(ino)?)))
    }

    async fn access(
        &self,
        ctx: &RequestContext,
        h: &Ino,
        requested: AccessMask,
    ) -> FsResult<AccessMask> {
        let caller = caller(ctx);
        self.inline(|vfs| vfs.access(&caller, *h, requested.bits()).map(AccessMask))
    }

    async fn lookup(&self, _ctx: &RequestContext, parent: &Ino, name: &str) -> FsResult<Ino> {
        self.inline(|vfs| {
            let r = vfs.lookup(*parent, name);
            tracing::trace!(target: "buildfs::ops", "lookup {parent}/{name} -> {r:?}");
            r
        })
    }

    async fn parent(&self, _ctx: &RequestContext, dir: &Ino) -> FsResult<Option<Ino>> {
        self.inline(|vfs| vfs.parent(*dir))
    }

    async fn readdir(
        &self,
        _ctx: &RequestContext,
        dir: &Ino,
        cookie: u64,
        max_entries: u32,
        with_attrs: bool,
    ) -> FsResult<DirPage<Ino>> {
        let dir = *dir;
        self.inline(|vfs| {
            let (items, eof) = vfs.readdir(dir, cookie, max_entries as usize, with_attrs)?;
            let entries = items
                .into_iter()
                .map(|it| DirEntry {
                    attrs: it.attrs.as_ref().map(|a| Self::attrs(vfs, it.ino, a)),
                    name: it.name,
                    handle: it.ino,
                    cookie: it.cookie,
                })
                .collect();
            Ok(DirPage { entries, eof })
        })
    }

    async fn read(
        &self,
        _ctx: &RequestContext,
        h: &Ino,
        offset: u64,
        count: u32,
    ) -> FsResult<ReadResult> {
        let ino = *h;
        self.run(move |vfs| {
            let (data, eof) = vfs.read(ino, offset, count)?;
            Ok(ReadResult {
                data: Bytes::from(data),
                eof,
            })
        })
        .await
    }

    async fn write(
        &self,
        _ctx: &RequestContext,
        h: &Ino,
        offset: u64,
        data: Bytes,
        requested: WriteStability,
    ) -> FsResult<WriteResult> {
        let ino = *h;
        self.run(move |vfs| {
            let written = vfs.write_data(ino, offset, &data)?;
            let stability = match (vfs.sync_mode(), requested) {
                // Acknowledged writes are as durable as relaxed mode gets, so
                // say so: the client then skips COMMIT on close entirely.
                (SyncMode::Relaxed, _) => WriteStability::FileSync,
                (SyncMode::Strict, WriteStability::Unstable) => WriteStability::Unstable,
                (SyncMode::Strict, _) => {
                    vfs.commit(ino)?;
                    WriteStability::FileSync
                }
            };
            Ok(WriteResult { written, stability })
        })
        .await
    }

    async fn create(
        &self,
        ctx: &RequestContext,
        parent: &Ino,
        name: &str,
        req: CreateRequest,
    ) -> FsResult<CreateResult<Ino>> {
        let (parent, name, caller, attrs) = (*parent, name.to_owned(), caller(ctx), set_attrs(&req.attrs));
        self.run(move |vfs| {
            let node = match req.kind {
                CreateKind::File => NewNode::File,
                CreateKind::Directory => NewNode::Dir,
                CreateKind::Fifo => NewNode::Fifo,
                CreateKind::Socket => NewNode::Socket,
                CreateKind::CharDevice { major, minor } => NewNode::CharDev(major, minor),
                CreateKind::BlockDevice { major, minor } => NewNode::BlockDev(major, minor),
            };
            let r = vfs.create(&caller, parent, &name, node, &attrs);
            tracing::debug!(target: "buildfs::ops", "create {parent}/{name} -> {:?}", r.as_ref().map(|x| x.0));
            let (ino, inode) = r?;
            Ok(CreateResult {
                handle: ino,
                attrs: Self::attrs(vfs, ino, &inode),
            })
        })
        .await
    }

    async fn remove(&self, _ctx: &RequestContext, parent: &Ino, name: &str) -> FsResult<()> {
        let (parent, name) = (*parent, name.to_owned());
        self.run(move |vfs| {
            let r = vfs.remove(parent, &name);
            tracing::debug!(target: "buildfs::ops", "remove {parent}/{name}: {r:?}");
            r
        })
        .await
    }

    async fn rename(
        &self,
        _ctx: &RequestContext,
        from_dir: &Ino,
        from_name: &str,
        to_dir: &Ino,
        to_name: &str,
    ) -> FsResult<()> {
        let (fd, fname, td, tname) = (
            *from_dir,
            from_name.to_owned(),
            *to_dir,
            to_name.to_owned(),
        );
        self.run(move |vfs| {
            let r = vfs.rename(fd, &fname, td, &tname);
            tracing::debug!(target: "buildfs::ops", "rename {fd}/{fname} -> {td}/{tname}: {r:?}");
            r
        })
        .await
    }

    async fn setattr(&self, ctx: &RequestContext, h: &Ino, attrs: &SetAttrs) -> FsResult<Attrs> {
        let (ino, attrs, caller) = (*h, set_attrs(attrs), caller(ctx));
        self.run(move |vfs| {
            let inode = vfs.setattr(&caller, ino, &attrs)?;
            Ok(Self::attrs(vfs, ino, &inode))
        })
        .await
    }

    fn xattrs(&self) -> Option<&dyn Xattrs<Ino>> {
        Some(self)
    }

    fn symlinks(&self) -> Option<&dyn Symlinks<Ino>> {
        Some(self)
    }

    fn hard_links(&self) -> Option<&dyn HardLinks<Ino>> {
        Some(self)
    }

    fn commit_support(&self) -> Option<&dyn CommitSupport<Ino>> {
        Some(self)
    }

    fn stable_handles(&self) -> Option<&dyn StableHandles<Ino>> {
        Some(self)
    }

    fn open_tracking(&self) -> Option<&dyn OpenTracking<Ino>> {
        Some(self)
    }
}

impl OpenTracking<Ino> for BuildFs {
    fn opened(&self, handle: &Ino) {
        self.vfs.open_acquired(*handle);
    }

    fn released(&self, handle: &Ino) {
        // Freeing an unlinked file is a metadata transaction plus file
        // deletion: keep it off the async worker.
        let (vfs, ino) = (self.vfs.clone(), *handle);
        drop(tokio::task::spawn_blocking(move || vfs.open_released(ino)));
    }
}

impl StableHandles<Ino> for BuildFs {
    fn id_of(&self, handle: &Ino) -> u64 {
        *handle
    }

    fn handle_of(&self, id: u64) -> Option<Ino> {
        Some(id)
    }
}

#[async_trait]
impl Xattrs<Ino> for BuildFs {
    async fn list_xattrs(&self, _ctx: &RequestContext, h: &Ino) -> FsResult<Vec<String>> {
        self.inline(|vfs| vfs.list_xattrs(*h))
    }

    async fn get_xattr(&self, _ctx: &RequestContext, h: &Ino, name: &str) -> FsResult<Bytes> {
        self.inline(|vfs| vfs.get_xattr(*h, name).map(Bytes::from))
    }

    async fn set_xattr(
        &self,
        _ctx: &RequestContext,
        h: &Ino,
        name: &str,
        value: Bytes,
        mode: XattrSetMode,
    ) -> FsResult<()> {
        let (ino, name) = (*h, name.to_owned());
        self.run(move |vfs| vfs.set_xattr(ino, &name, &value, xattr_mode(mode)))
            .await
    }

    async fn xattr_exists(&self, _ctx: &RequestContext, h: &Ino, name: &str) -> FsResult<bool> {
        self.inline(|vfs| vfs.xattr_exists(*h, name))
    }

    async fn remove_xattr(&self, _ctx: &RequestContext, h: &Ino, name: &str) -> FsResult<()> {
        let (ino, name) = (*h, name.to_owned());
        self.run(move |vfs| vfs.remove_xattr(ino, &name)).await
    }
}

#[async_trait]
impl Symlinks<Ino> for BuildFs {
    async fn create_symlink(
        &self,
        ctx: &RequestContext,
        parent: &Ino,
        name: &str,
        target: &str,
        attrs: &SetAttrs,
    ) -> FsResult<CreateResult<Ino>> {
        let (parent, name, target, attrs, caller) = (
            *parent,
            name.to_owned(),
            target.to_owned(),
            set_attrs(attrs),
            caller(ctx),
        );
        self.run(move |vfs| {
            let (ino, inode) =
                vfs.create(&caller, parent, &name, NewNode::Symlink(target), &attrs)?;
            Ok(CreateResult {
                handle: ino,
                attrs: Self::attrs(vfs, ino, &inode),
            })
        })
        .await
    }

    async fn readlink(&self, _ctx: &RequestContext, h: &Ino) -> FsResult<String> {
        self.inline(|vfs| vfs.readlink(*h))
    }
}

#[async_trait]
impl HardLinks<Ino> for BuildFs {
    async fn link(
        &self,
        _ctx: &RequestContext,
        source: &Ino,
        parent: &Ino,
        name: &str,
    ) -> FsResult<()> {
        let (src, parent, name) = (*source, *parent, name.to_owned());
        self.run(move |vfs| {
            let r = vfs.link(src, parent, &name);
            tracing::debug!(target: "buildfs::ops", "link {src} -> {parent}/{name}: {r:?}");
            r
        })
        .await
    }
}

#[async_trait]
impl CommitSupport<Ino> for BuildFs {
    async fn commit(
        &self,
        _ctx: &RequestContext,
        h: &Ino,
        _offset: u64,
        _count: u32,
    ) -> FsResult<()> {
        let ino = *h;
        self.run(move |vfs| vfs.commit(ino)).await
    }
}
