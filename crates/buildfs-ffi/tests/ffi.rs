//! Exercises the C API the way the FSKit extension calls it.

use std::ffi::{CString, c_void};
use std::ptr;

use buildfs_ffi::*;

struct Store {
    _dir: tempfile::TempDir,
    path: CString,
    v: *mut BfsVfs,
}

impl Store {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = CString::new(dir.path().to_str().unwrap()).unwrap();
        let mut s = Self { _dir: dir, path, v: ptr::null_mut() };
        s.open();
        s
    }
    fn open(&mut self) {
        // Compression is queued but never due during a test; tests drive
        // it with bfs_compact_all.
        let opts = BfsOptions { compress_after_ms: 3_600_000, ..Default::default() };
        assert_eq!(unsafe { bfs_open(self.path.as_ptr(), &opts, &mut self.v) }, 0);
    }
    fn reopen(&mut self) {
        unsafe { bfs_close(self.v) };
        self.open();
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        unsafe { bfs_close(self.v) };
    }
}

fn me() -> BfsCaller {
    BfsCaller { uid: 501, gid: 20, groups: [20u32, 12].as_ptr(), ngroups: 2 }
}

fn create(s: &Store, dir: u64, name: &str, kind: u32) -> Result<BfsAttr, i32> {
    let who = me();
    let (mut a, mut d) = (BfsAttr::default(), BfsAttr::default());
    let rc = unsafe {
        bfs_create(s.v, &who, dir, name.as_ptr(), name.len(), kind, 0, 0, ptr::null(), ptr::null(), 0, &mut a, &mut d)
    };
    if rc == 0 { Ok(a) } else { Err(rc) }
}

fn lookup(s: &Store, dir: u64, name: &str) -> Result<BfsAttr, i32> {
    let mut a = BfsAttr::default();
    let rc = unsafe { bfs_lookup(s.v, dir, name.as_ptr(), name.len(), &mut a) };
    if rc == 0 { Ok(a) } else { Err(rc) }
}

fn getattr(s: &Store, ino: u64) -> Result<BfsAttr, i32> {
    let mut a = BfsAttr::default();
    let rc = unsafe { bfs_getattr(s.v, ino, &mut a) };
    if rc == 0 { Ok(a) } else { Err(rc) }
}

fn write(s: &Store, ino: u64, off: u64, data: &[u8]) -> BfsAttr {
    let (mut n, mut a) = (0usize, BfsAttr::default());
    assert_eq!(unsafe { bfs_write(s.v, ino, off, data.as_ptr(), data.len(), &mut n, &mut a) }, 0);
    assert_eq!(n, data.len());
    a
}

fn read(s: &Store, ino: u64, off: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let mut n = 0usize;
    assert_eq!(unsafe { bfs_read(s.v, ino, off, buf.as_mut_ptr(), len, &mut n) }, 0);
    buf.truncate(n);
    buf
}

fn take(buf: BfsBuf) -> Vec<u8> {
    let v = if buf.ptr.is_null() { Vec::new() } else { unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) }.to_vec() };
    unsafe { bfs_buf_free(buf) };
    v
}

extern "C" fn collect(
    ctx: *mut c_void,
    name: *const u8,
    len: usize,
    _ino: u64,
    kind: u32,
    cookie: u64,
    attr: *const BfsAttr,
) -> bool {
    let out = unsafe { &mut *(ctx as *mut (Vec<(String, u32, u64, bool)>, usize)) };
    let n = String::from_utf8(unsafe { std::slice::from_raw_parts(name, len) }.to_vec()).unwrap();
    out.0.push((n, kind, cookie, !attr.is_null()));
    out.0.len() < out.1
}

fn readdir(s: &Store, dir: u64, cookie: u64, max: usize, attrs: bool) -> Vec<(String, u32, u64, bool)> {
    let mut ctx = (Vec::new(), max);
    let rc = unsafe { bfs_readdir(s.v, dir, cookie, attrs, collect, (&mut ctx as *mut _) as *mut c_void) };
    assert_eq!(rc, 0);
    ctx.0
}

#[test]
fn create_write_read_lookup() {
    let s = Store::new();
    let f = create(&s, BFS_ROOT_INO, "a.txt", BFS_KIND_FILE).unwrap();
    assert_eq!(f.kind, BFS_KIND_FILE);
    assert_eq!(f.uid, 501);
    assert_eq!(f.nlink, 1);
    assert!(f.ino >= 16);
    let after = write(&s, f.ino, 0, b"hello world");
    assert_eq!(after.size, 11);
    assert_eq!(read(&s, f.ino, 6, 100), b"world");
    assert_eq!(lookup(&s, BFS_ROOT_INO, "a.txt").unwrap().ino, f.ino);
    assert_eq!(lookup(&s, BFS_ROOT_INO, "missing"), Err(libc::ENOENT));
    assert_eq!(create(&s, BFS_ROOT_INO, "a.txt", BFS_KIND_FILE), Err(libc::EEXIST));
    // Non-UTF-8 names are refused, not mangled.
    let bad = [0x66u8, 0xff];
    let mut a = BfsAttr::default();
    assert_eq!(unsafe { bfs_lookup(s.v, BFS_ROOT_INO, bad.as_ptr(), bad.len(), &mut a) }, libc::EILSEQ);
    // Over-long names: ENAMETOOLONG, also for lookups.
    let long = "x".repeat(256);
    assert_eq!(lookup(&s, BFS_ROOT_INO, &long), Err(libc::ENAMETOOLONG));
    assert_eq!(create(&s, BFS_ROOT_INO, &long, BFS_KIND_FILE), Err(libc::ENAMETOOLONG));
    assert!(create(&s, BFS_ROOT_INO, &"y".repeat(255), BFS_KIND_FILE).is_ok());
    // Root: directory, its own parent.
    let root = getattr(&s, BFS_ROOT_INO).unwrap();
    assert_eq!(root.kind, BFS_KIND_DIR);
    assert_eq!(root.parent, BFS_ROOT_INO);
}

#[test]
fn directories_and_readdir_paging() {
    let s = Store::new();
    let d = create(&s, BFS_ROOT_INO, "d", BFS_KIND_DIR).unwrap();
    assert_eq!(d.parent, BFS_ROOT_INO);
    for i in 0..300 {
        let _ = create(&s, d.ino, &format!("f{i}"), BFS_KIND_FILE).unwrap();
    }
    let _ = create(&s, d.ino, "sub", BFS_KIND_DIR).unwrap();
    // Page through with a small consumer, deleting entries already seen
    // (what Rust's remove_dir_all does): nothing may be skipped.
    let mut seen = Vec::new();
    let mut cookie = 0;
    loop {
        let page = readdir(&s, d.ino, cookie, 7, false);
        if page.is_empty() {
            break;
        }
        for (name, kind, c, has_attr) in &page {
            assert!(!has_attr);
            if name != "sub" {
                assert_eq!(*kind, BFS_KIND_FILE);
                let (mut a, mut dir) = (BfsAttr::default(), BfsAttr::default());
                assert_eq!(unsafe { bfs_remove(s.v, d.ino, name.as_ptr(), name.len(), &mut a, &mut dir) }, 0);
                assert_eq!(a.nlink, 0);
            } else {
                assert_eq!(*kind, BFS_KIND_DIR);
            }
            seen.push(name.clone());
            cookie = *c;
        }
    }
    assert_eq!(seen.len(), 301);
    assert_eq!(readdir(&s, d.ino, 0, 1000, true), vec![("sub".to_owned(), BFS_KIND_DIR, readdir(&s, d.ino, 0, 1, false)[0].2, true)]);
    // rmdir of a non-empty directory fails.
    let (mut a, mut dir) = (BfsAttr::default(), BfsAttr::default());
    assert_eq!(unsafe { bfs_remove(s.v, BFS_ROOT_INO, "d".as_ptr(), 1, &mut a, &mut dir) }, libc::ENOTEMPTY);
}

#[test]
fn links_and_open_unlink() {
    let s = Store::new();
    let f = create(&s, BFS_ROOT_INO, "f", BFS_KIND_FILE).unwrap();
    let _ = write(&s, f.ino, 0, b"data");
    let (mut a, mut d) = (BfsAttr::default(), BfsAttr::default());
    assert_eq!(unsafe { bfs_link(s.v, f.ino, BFS_ROOT_INO, "g".as_ptr(), 1, &mut a, &mut d) }, 0);
    assert_eq!(a.nlink, 2);
    // Open, then unlink both names: the data must stay readable.
    unsafe { bfs_open_acquired(s.v, f.ino) };
    for n in ["f", "g"] {
        assert_eq!(unsafe { bfs_remove(s.v, BFS_ROOT_INO, n.as_ptr(), 1, &mut a, &mut d) }, 0);
    }
    assert_eq!(a.nlink, 0);
    assert_eq!(read(&s, f.ino, 0, 10), b"data");
    unsafe { bfs_open_released(s.v, f.ino) };
    assert_eq!(getattr(&s, f.ino), Err(libc::ESTALE));
}

#[test]
fn rename_over_reports_replaced_item() {
    let s = Store::new();
    let a = create(&s, BFS_ROOT_INO, "a", BFS_KIND_FILE).unwrap();
    let b = create(&s, BFS_ROOT_INO, "b", BFS_KIND_FILE).unwrap();
    let (mut item, mut fd, mut td, mut over) = Default::default();
    let mut replaced = false;
    let rc = unsafe {
        bfs_rename(s.v, BFS_ROOT_INO, "a".as_ptr(), 1, BFS_ROOT_INO, "b".as_ptr(), 1, &mut item, &mut fd, &mut td, &mut over, &mut replaced)
    };
    assert_eq!(rc, 0);
    assert!(replaced);
    assert_eq!((item.ino, over.ino, over.nlink), (a.ino, b.ino, 0));
    assert_eq!(lookup(&s, BFS_ROOT_INO, "b").unwrap().ino, a.ino);
    // Renaming onto a fresh name replaces nothing.
    let rc = unsafe {
        bfs_rename(s.v, BFS_ROOT_INO, "b".as_ptr(), 1, BFS_ROOT_INO, "c".as_ptr(), 1, &mut item, &mut fd, &mut td, &mut over, &mut replaced)
    };
    assert_eq!(rc, 0);
    assert!(!replaced);
}

#[test]
fn setattr_and_flags() {
    let s = Store::new();
    let f = create(&s, BFS_ROOT_INO, "f", BFS_KIND_FILE).unwrap();
    let who = me();
    let mut out = BfsAttr::default();
    let req = BfsSetattr {
        valid: BFS_SET_MODE | BFS_SET_SIZE | BFS_SET_MTIME | BFS_SET_FLAGS | BFS_SET_GID,
        mode: 0o600,
        size: 5000,
        mtime: BfsTime { sec: 1_000_000, nsec: 5 },
        flags: BFS_UF_HIDDEN,
        gid: 12, // a supplementary group of the owner: allowed
        ..Default::default()
    };
    assert_eq!(unsafe { bfs_setattr(s.v, &who, f.ino, &req, &mut out) }, 0);
    assert_eq!((out.mode, out.size, out.mtime, out.flags, out.gid), (0o600, 5000, BfsTime { sec: 1_000_000, nsec: 5 }, BFS_UF_HIDDEN, 12));
    assert_eq!(read(&s, f.ino, 4990, 100), vec![0u8; 10]);
    // Flags the store can't represent are refused.
    let req = BfsSetattr { valid: BFS_SET_FLAGS, flags: 0x2 /* UF_IMMUTABLE */, ..Default::default() };
    assert_eq!(unsafe { bfs_setattr(s.v, &who, f.ino, &req, &mut out) }, libc::ENOTSUP);
    // Somebody else can't chmod.
    let other = BfsCaller { uid: 777, gid: 777, groups: ptr::null(), ngroups: 0 };
    let req = BfsSetattr { valid: BFS_SET_MODE, mode: 0o777, ..Default::default() };
    assert_eq!(unsafe { bfs_setattr(s.v, &other, f.ino, &req, &mut out) }, libc::EPERM);
}

#[test]
fn xattrs_and_symlinks() {
    let s = Store::new();
    let f = create(&s, BFS_ROOT_INO, "f", BFS_KIND_FILE).unwrap();
    let n = "com.apple.provenance";
    let val = [1u8, 2, 3];
    let set = |mode| unsafe { bfs_set_xattr(s.v, f.ino, n.as_ptr(), n.len(), val.as_ptr(), val.len(), mode) };
    assert_eq!(set(BFS_XATTR_REPLACE), libc::ENOATTR);
    assert_eq!(set(BFS_XATTR_CREATE), 0);
    assert_eq!(set(BFS_XATTR_CREATE), libc::EEXIST);
    assert_eq!(set(BFS_XATTR_SET), 0);
    let mut buf = BfsBuf { ptr: ptr::null_mut(), len: 0 };
    assert_eq!(unsafe { bfs_get_xattr(s.v, f.ino, n.as_ptr(), n.len(), &mut buf) }, 0);
    assert_eq!(take(buf), val);
    let mut buf = BfsBuf { ptr: ptr::null_mut(), len: 0 };
    assert_eq!(unsafe { bfs_list_xattrs(s.v, f.ino, &mut buf) }, 0);
    assert_eq!(take(buf), b"com.apple.provenance\0");
    assert_eq!(unsafe { bfs_remove_xattr(s.v, f.ino, n.as_ptr(), n.len()) }, 0);
    assert_eq!(unsafe { bfs_remove_xattr(s.v, f.ino, n.as_ptr(), n.len()) }, libc::ENOATTR);
    let mut buf = BfsBuf { ptr: ptr::null_mut(), len: 0 };
    assert_eq!(unsafe { bfs_get_xattr(s.v, f.ino, n.as_ptr(), n.len(), &mut buf) }, libc::ENOATTR);

    let who = me();
    let (mut l, mut d) = (BfsAttr::default(), BfsAttr::default());
    let target = "../some/where";
    let rc = unsafe {
        bfs_create(s.v, &who, BFS_ROOT_INO, "l".as_ptr(), 1, BFS_KIND_SYMLINK, 0, 0, ptr::null(), target.as_ptr(), target.len(), &mut l, &mut d)
    };
    assert_eq!(rc, 0);
    assert_eq!((l.kind, l.size), (BFS_KIND_SYMLINK, target.len() as u64));
    let mut buf = BfsBuf { ptr: ptr::null_mut(), len: 0 };
    assert_eq!(unsafe { bfs_readlink(s.v, l.ino, &mut buf) }, 0);
    assert_eq!(take(buf), target.as_bytes());
    let fifo = create(&s, BFS_ROOT_INO, "p", BFS_KIND_FIFO).unwrap();
    assert_eq!(fifo.kind, BFS_KIND_FIFO);
}

#[test]
fn persists_and_compresses_across_reopen() {
    let mut s = Store::new();
    let f = create(&s, BFS_ROOT_INO, "big", BFS_KIND_FILE).unwrap();
    let data: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
    let _ = write(&s, f.ino, 0, &data);
    let mut n = 0u64;
    assert_eq!(unsafe { bfs_compact_all(s.v, &mut n) }, 0);
    assert_eq!(n, 1);
    let mut st = BfsCompressStats::default();
    assert_eq!(unsafe { bfs_compress_stats(s.v, &mut st) }, 0);
    assert!(st.stored_bytes < st.logical_bytes / 10, "{st:?}");
    assert_eq!(unsafe { bfs_sync(s.v) }, 0);
    s.reopen();
    let a = lookup(&s, BFS_ROOT_INO, "big").unwrap();
    assert_eq!((a.ino, a.size), (f.ino, data.len() as u64));
    assert!(a.space_used < 100_000, "space_used {}", a.space_used);
    assert_eq!(read(&s, f.ino, 0, data.len()), data);
    let mut fs = BfsStatfs::default();
    assert_eq!(unsafe { bfs_statfs(s.v, &mut fs) }, 0);
    assert!(fs.total_bytes > 0 && fs.block_size > 0);
}

#[test]
fn bad_handles_are_rejected() {
    let mut a = BfsAttr::default();
    assert_eq!(unsafe { bfs_getattr(ptr::null(), 1, &mut a) }, libc::EINVAL);
    let mut v = ptr::null_mut();
    assert_eq!(unsafe { bfs_open(ptr::null(), ptr::null(), &mut v) }, libc::EINVAL);
    unsafe { bfs_close(ptr::null_mut()) };
}
