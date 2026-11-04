//! embednfs adapter: exposes [`Vfs`] as an NFSv4.1 filesystem.
//!
//! The core is synchronous (redb + pread/pwrite), so every call hops onto
//! tokio's blocking pool.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use embednfs::{
    AccessMask, Attrs, CommitSupport, CreateKind, CreateRequest, CreateResult, DirEntry, DirPage,
    FileSystem, FsCapabilities, FsError, FsLimits, FsResult, FsStats, HardLinks, ObjectType,
    ReadResult, RequestContext, SetAttrs, StableHandles, Symlinks, Timestamp, WriteResult,
    WriteStability, XattrSetMode, Xattrs,
};

use crate::meta::{FLAG_ARCHIVE, FLAG_HIDDEN, FLAG_SYSTEM, Ino, Inode, Kind, ROOT_INO, Ts};
use crate::vfs::{Caller, MAX_FILE_SIZE, NewNode, Vfs};

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
        F: FnOnce(&Vfs) -> FsResult<T> + Send + 'static,
    {
        let vfs = self.vfs.clone();
        tokio::task::spawn_blocking(move || f(&vfs))
            .await
            .map_err(|_| FsError::ServerFault)?
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
        self.run(|vfs| {
            let st = vfs.statfs()?;
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
        .await
    }

    async fn getattr(&self, _ctx: &RequestContext, h: &Ino) -> FsResult<Attrs> {
        let ino = *h;
        self.run(move |vfs| Ok(Self::attrs(vfs, ino, &vfs.getattr(ino)?)))
            .await
    }

    async fn access(
        &self,
        ctx: &RequestContext,
        h: &Ino,
        requested: AccessMask,
    ) -> FsResult<AccessMask> {
        let (ino, caller) = (*h, Caller::from_ctx(ctx));
        self.run(move |vfs| vfs.access(&caller, ino, requested.bits()).map(AccessMask))
            .await
    }

    async fn lookup(&self, _ctx: &RequestContext, parent: &Ino, name: &str) -> FsResult<Ino> {
        let (parent, name) = (*parent, name.to_owned());
        self.run(move |vfs| {
            let r = vfs.lookup(parent, &name);
            tracing::debug!(target: "buildfs::ops", "lookup {parent}/{name} -> {r:?}");
            r
        })
        .await
    }

    async fn parent(&self, _ctx: &RequestContext, dir: &Ino) -> FsResult<Option<Ino>> {
        let dir = *dir;
        self.run(move |vfs| vfs.parent(dir)).await
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
        self.run(move |vfs| {
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
        .await
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
            let stability = match requested {
                WriteStability::Unstable => WriteStability::Unstable,
                WriteStability::DataSync | WriteStability::FileSync => {
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
        let (parent, name, caller) = (*parent, name.to_owned(), Caller::from_ctx(ctx));
        self.run(move |vfs| {
            let node = match req.kind {
                CreateKind::File => NewNode::File,
                CreateKind::Directory => NewNode::Dir,
                CreateKind::Fifo => NewNode::Fifo,
                CreateKind::Socket => NewNode::Socket,
                CreateKind::CharDevice { major, minor } => NewNode::CharDev(major, minor),
                CreateKind::BlockDevice { major, minor } => NewNode::BlockDev(major, minor),
            };
            let r = vfs.create(&caller, parent, &name, node, &req.attrs);
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
        let (ino, attrs, caller) = (*h, attrs.clone(), Caller::from_ctx(ctx));
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
        let ino = *h;
        self.run(move |vfs| vfs.list_xattrs(ino)).await
    }

    async fn get_xattr(&self, _ctx: &RequestContext, h: &Ino, name: &str) -> FsResult<Bytes> {
        let (ino, name) = (*h, name.to_owned());
        self.run(move |vfs| vfs.get_xattr(ino, &name).map(Bytes::from))
            .await
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
        self.run(move |vfs| vfs.set_xattr(ino, &name, &value, mode))
            .await
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
            attrs.clone(),
            Caller::from_ctx(ctx),
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
        let ino = *h;
        self.run(move |vfs| vfs.readlink(ino)).await
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
