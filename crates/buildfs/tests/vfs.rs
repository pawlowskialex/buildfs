use buildfs::meta::{Kind, ROOT_INO};
use buildfs::vfs::{Caller, NewNode, Options, Vfs};
use embednfs::{FsError, SetAttrs, XattrSetMode};

fn user() -> Caller {
    Caller {
        uid: 501,
        gid: 20,
        groups: vec![20],
    }
}

fn attrs() -> SetAttrs {
    SetAttrs::default()
}

fn mkfile(vfs: &Vfs, dir: u64, name: &str) -> u64 {
    vfs.create(&user(), dir, name, NewNode::File, &attrs())
        .unwrap()
        .0
}

fn mkdir(vfs: &Vfs, dir: u64, name: &str) -> u64 {
    vfs.create(&user(), dir, name, NewNode::Dir, &attrs())
        .unwrap()
        .0
}

fn names(vfs: &Vfs, dir: u64) -> Vec<String> {
    let (items, eof) = vfs.readdir(dir, 0, 10_000, false).unwrap();
    assert!(eof);
    let mut n: Vec<_> = items.into_iter().map(|i| i.name).collect();
    n.sort();
    n
}

#[test]
fn create_lookup_readdir() {
    let tmp = tempfile::tempdir().unwrap();
    let vfs = Vfs::open(tmp.path()).unwrap();
    let a = mkfile(&vfs, ROOT_INO, "a");
    let d = mkdir(&vfs, ROOT_INO, "d");
    assert_eq!(vfs.lookup(ROOT_INO, "a").unwrap(), a);
    assert_eq!(names(&vfs, ROOT_INO), ["a", "d"]);
    assert_eq!(
        vfs.create(&user(), ROOT_INO, "a", NewNode::File, &attrs())
            .err(),
        Some(FsError::AlreadyExists)
    );
    assert_eq!(vfs.getattr(ROOT_INO).unwrap().nlink, 3);
    assert_eq!(vfs.parent(d).unwrap(), Some(ROOT_INO));
    assert_eq!(vfs.parent(ROOT_INO).unwrap(), None);
    assert_eq!(vfs.lookup(a, "x").err(), Some(FsError::NotDirectory));
    // BSD group inheritance from parent directory.
    assert_eq!(vfs.getattr(a).unwrap().gid, vfs.getattr(ROOT_INO).unwrap().gid);
}

#[test]
fn readdir_paging_survives_deletes() {
    let tmp = tempfile::tempdir().unwrap();
    let vfs = Vfs::open(tmp.path()).unwrap();
    for i in 0..10 {
        mkfile(&vfs, ROOT_INO, &format!("f{i}"));
    }
    let (page1, eof) = vfs.readdir(ROOT_INO, 0, 4, false).unwrap();
    assert!(!eof);
    assert_eq!(page1.len(), 4);
    // Delete an entry already returned and one not yet returned.
    vfs.remove(ROOT_INO, &page1[0].name).unwrap();
    vfs.remove(ROOT_INO, "f9").unwrap();
    let (page2, eof) = vfs
        .readdir(ROOT_INO, page1.last().unwrap().cookie, 100, true)
        .unwrap();
    assert!(eof);
    assert_eq!(page2.len(), 5);
    assert!(page2.iter().all(|e| e.attrs.is_some()));
}

#[test]
fn write_read_truncate_sparse() {
    let tmp = tempfile::tempdir().unwrap();
    let vfs = Vfs::open(tmp.path()).unwrap();
    let f = mkfile(&vfs, ROOT_INO, "f");
    assert_eq!(vfs.read(f, 0, 100).unwrap(), (vec![], true));

    vfs.write_data(f, 0, b"hello world").unwrap();
    assert_eq!(vfs.read(f, 0, 5).unwrap(), (b"hello".to_vec(), false));
    assert_eq!(vfs.read(f, 6, 100).unwrap(), (b"world".to_vec(), true));

    // Shrink then extend: the cut-off bytes must read back as zeros.
    let shrink = SetAttrs {
        size: Some(5),
        ..attrs()
    };
    vfs.setattr(&user(), f, &shrink).unwrap();
    vfs.write_data(f, 8, b"!").unwrap();
    assert_eq!(vfs.read(f, 0, 100).unwrap().0, b"hello\0\0\0!".to_vec());

    // Sparse file never written: reads are zeros, no hot data.
    let s = mkfile(&vfs, ROOT_INO, "sparse");
    vfs.setattr(
        &user(),
        s,
        &SetAttrs {
            size: Some(4096),
            ..attrs()
        },
    )
    .unwrap();
    let (data, eof) = vfs.read(s, 4000, 1000).unwrap();
    assert!(eof);
    assert_eq!(data, vec![0u8; 96]);
    vfs.write_data(s, 4096, b"x").unwrap();
    assert_eq!(vfs.getattr(s).unwrap().size, 4097);
    assert_eq!(vfs.read(s, 0, 1).unwrap().0, vec![0]);
}

#[test]
fn hard_links_and_unlink() {
    let tmp = tempfile::tempdir().unwrap();
    let vfs = Vfs::open(tmp.path()).unwrap();
    let d = mkdir(&vfs, ROOT_INO, "deps");
    let f = mkfile(&vfs, d, "libfoo-abc.rlib");
    vfs.write_data(f, 0, b"rlib").unwrap();
    vfs.link(f, ROOT_INO, "libfoo.rlib").unwrap();
    assert_eq!(vfs.getattr(f).unwrap().nlink, 2);
    assert_eq!(vfs.link(d, ROOT_INO, "x").err(), Some(FsError::IsDirectory));

    vfs.remove(d, "libfoo-abc.rlib").unwrap();
    assert_eq!(vfs.getattr(f).unwrap().nlink, 1);
    assert_eq!(vfs.read(f, 0, 10).unwrap().0, b"rlib".to_vec());

    vfs.remove(ROOT_INO, "libfoo.rlib").unwrap();
    assert_eq!(vfs.getattr(f).err(), Some(FsError::Stale));
    assert_eq!(vfs.remove(ROOT_INO, "deps").err(), None);
}

#[test]
fn rmdir_rules() {
    let tmp = tempfile::tempdir().unwrap();
    let vfs = Vfs::open(tmp.path()).unwrap();
    let d = mkdir(&vfs, ROOT_INO, "d");
    mkfile(&vfs, d, "f");
    assert_eq!(vfs.remove(ROOT_INO, "d").err(), Some(FsError::NotEmpty));
    vfs.remove(d, "f").unwrap();
    vfs.remove(ROOT_INO, "d").unwrap();
    assert_eq!(vfs.getattr(ROOT_INO).unwrap().nlink, 2);
    assert_eq!(vfs.getattr(ROOT_INO).unwrap().size, 0);
}

#[test]
fn rename_semantics() {
    let tmp = tempfile::tempdir().unwrap();
    let vfs = Vfs::open(tmp.path()).unwrap();
    let a = mkdir(&vfs, ROOT_INO, "a");
    let b = mkdir(&vfs, a, "b");
    let f = mkfile(&vfs, ROOT_INO, "f");
    let g = mkfile(&vfs, ROOT_INO, "g");

    // Directory into its own descendant.
    assert_eq!(
        vfs.rename(ROOT_INO, "a", b, "a").err(),
        Some(FsError::InvalidInput)
    );
    // File over directory and vice versa.
    assert_eq!(
        vfs.rename(ROOT_INO, "f", ROOT_INO, "a").err(),
        Some(FsError::IsDirectory)
    );
    assert_eq!(
        vfs.rename(a, "b", ROOT_INO, "f").err(),
        Some(FsError::NotDirectory)
    );

    // Atomic replace (the classic write-temp-then-rename pattern).
    vfs.write_data(g, 0, b"new").unwrap();
    vfs.rename(ROOT_INO, "g", ROOT_INO, "f").unwrap();
    assert_eq!(vfs.lookup(ROOT_INO, "f").unwrap(), g);
    assert_eq!(vfs.getattr(f).err(), Some(FsError::Stale));
    assert_eq!(names(&vfs, ROOT_INO), ["a", "f"]);
    assert_eq!(vfs.getattr(ROOT_INO).unwrap().size, 2);

    // Move a directory across parents: nlink and parent pointers follow.
    vfs.rename(a, "b", ROOT_INO, "b").unwrap();
    assert_eq!(vfs.getattr(a).unwrap().nlink, 2);
    assert_eq!(vfs.getattr(ROOT_INO).unwrap().nlink, 4);
    assert_eq!(vfs.parent(b).unwrap(), Some(ROOT_INO));

    // Replace an empty directory with another directory.
    vfs.rename(ROOT_INO, "b", ROOT_INO, "a").unwrap();
    assert_eq!(vfs.getattr(ROOT_INO).unwrap().nlink, 3);
    assert_eq!(vfs.getattr(a).err(), Some(FsError::Stale));
    assert_eq!(names(&vfs, ROOT_INO), ["a", "f"]);

    // Hard links to the same inode: rename is a no-op.
    vfs.link(g, ROOT_INO, "g2").unwrap();
    vfs.rename(ROOT_INO, "f", ROOT_INO, "g2").unwrap();
    assert_eq!(names(&vfs, ROOT_INO), ["a", "f", "g2"]);
}

#[test]
fn xattrs() {
    let tmp = tempfile::tempdir().unwrap();
    let vfs = Vfs::open(tmp.path()).unwrap();
    let f = mkfile(&vfs, ROOT_INO, "f");
    vfs.set_xattr(f, "com.apple.quarantine", b"q", XattrSetMode::CreateOnly)
        .unwrap();
    assert_eq!(
        vfs.set_xattr(f, "com.apple.quarantine", b"q", XattrSetMode::CreateOnly)
            .err(),
        Some(FsError::AlreadyExists)
    );
    assert_eq!(
        vfs.set_xattr(f, "user.missing", b"", XattrSetMode::ReplaceOnly)
            .err(),
        Some(FsError::NotFound)
    );
    vfs.set_xattr(f, "user.a", b"1", XattrSetMode::CreateOrReplace)
        .unwrap();
    assert_eq!(
        vfs.list_xattrs(f).unwrap(),
        ["com.apple.quarantine", "user.a"]
    );
    assert_eq!(vfs.getattr(f).unwrap().xattr_count, 2);
    vfs.remove_xattr(f, "user.a").unwrap();
    assert_eq!(vfs.get_xattr(f, "user.a").err(), Some(FsError::NotFound));
    // Xattrs of other inodes are not visible.
    let g = mkfile(&vfs, ROOT_INO, "g");
    assert!(vfs.list_xattrs(g).unwrap().is_empty());
    // Removing the file drops its xattrs.
    vfs.remove(ROOT_INO, "f").unwrap();
    assert_eq!(vfs.list_xattrs(f).err(), Some(FsError::Stale));
}

#[test]
fn special_files_and_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let vfs = Vfs::open(tmp.path()).unwrap();
    let (l, li) = vfs
        .create(
            &user(),
            ROOT_INO,
            "link",
            NewNode::Symlink("../target".into()),
            &attrs(),
        )
        .unwrap();
    assert_eq!(li.kind, Kind::Symlink);
    assert_eq!(vfs.readlink(l).unwrap(), "../target");
    assert_eq!(vfs.getattr(l).unwrap().size, 9);
    let (_, fifo) = vfs
        .create(&user(), ROOT_INO, "pipe", NewNode::Fifo, &attrs())
        .unwrap();
    assert_eq!(fifo.kind, Kind::Fifo);
    let (_, dev) = vfs
        .create(&Caller::root(), ROOT_INO, "null", NewNode::CharDev(3, 2), &attrs())
        .unwrap();
    assert_eq!(dev.rdev, (3, 2));
}

#[test]
fn permissions() {
    let tmp = tempfile::tempdir().unwrap();
    let vfs = Vfs::open(tmp.path()).unwrap();
    let f = mkfile(&vfs, ROOT_INO, "f");
    let other = Caller {
        uid: 777,
        gid: 777,
        groups: vec![],
    };
    use buildfs::vfs::access::*;
    assert_eq!(vfs.access(&other, f, READ | MODIFY).unwrap(), READ);
    assert_eq!(vfs.access(&user(), f, READ | MODIFY).unwrap(), READ | MODIFY);
    assert_eq!(
        vfs.setattr(
            &other,
            f,
            &SetAttrs {
                mode: Some(0o777),
                ..attrs()
            }
        )
        .err(),
        Some(FsError::PermissionDenied)
    );
    assert_eq!(
        vfs.setattr(
            &user(),
            f,
            &SetAttrs {
                uid: Some(0),
                ..attrs()
            }
        )
        .err(),
        Some(FsError::PermissionDenied)
    );
    // Writing clears setuid.
    vfs.setattr(
        &user(),
        f,
        &SetAttrs {
            mode: Some(0o4755),
            ..attrs()
        },
    )
    .unwrap();
    vfs.write_data(f, 0, b"x").unwrap();
    assert_eq!(vfs.getattr(f).unwrap().mode, 0o755);
}

#[test]
fn persists_across_reopen_and_gcs_orphans() {
    let tmp = tempfile::tempdir().unwrap();
    let f;
    {
        let vfs = Vfs::open(tmp.path()).unwrap();
        let d = mkdir(&vfs, ROOT_INO, "target");
        f = mkfile(&vfs, d, "out.o");
        vfs.write_data(f, 0, b"object code").unwrap();
        vfs.set_xattr(f, "user.k", b"v", XattrSetMode::CreateOrReplace)
            .unwrap();
        vfs.shutdown();
    }
    // A leftover hot file for an inode that doesn't exist.
    let orphan = tmp.path().join("hot/ff/00000000000000ff");
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, b"junk").unwrap();

    let vfs = Vfs::open(tmp.path()).unwrap();
    let d = vfs.lookup(ROOT_INO, "target").unwrap();
    assert_eq!(vfs.lookup(d, "out.o").unwrap(), f);
    assert_eq!(vfs.read(f, 0, 100).unwrap().0, b"object code".to_vec());
    assert_eq!(vfs.get_xattr(f, "user.k").unwrap(), b"v");
    assert!(!orphan.exists());
    // Inode numbers keep increasing after reopen (never reused).
    assert!(mkfile(&vfs, ROOT_INO, "new") > f);
}

#[test]
fn commit_is_durable() {
    let tmp = tempfile::tempdir().unwrap();
    let vfs = Vfs::open(tmp.path()).unwrap();
    let f = mkfile(&vfs, ROOT_INO, "f");
    vfs.write_data(f, 0, b"data").unwrap();
    vfs.commit(f).unwrap();
}

/// Copies the on-disk state of a live store, which is what a crash of the
/// buildfs process would leave behind (no Drop, no final durable commit).
fn snapshot_store(from: &std::path::Path, to: &std::path::Path) {
    fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap() {
            let e = e.unwrap();
            let dst = to.join(e.file_name());
            if e.file_type().unwrap().is_dir() {
                copy_dir(&e.path(), &dst);
            } else {
                std::fs::copy(e.path(), dst).unwrap();
            }
        }
    }
    copy_dir(from, to);
}

#[test]
fn wal_recovers_metadata_after_process_crash() {
    let live = tempfile::tempdir().unwrap();
    let crashed = tempfile::tempdir().unwrap();
    let opts = Options {
        flush_interval: None, // no background durable commits
        ..Options::default()
    };
    let vfs = Vfs::open_with(live.path(), opts.clone()).unwrap();
    let d = mkdir(&vfs, ROOT_INO, "target");
    let f = mkfile(&vfs, d, "lib.rlib");
    vfs.write_data(f, 0, b"artifact").unwrap();
    vfs.link(f, ROOT_INO, "lib.rlib").unwrap();
    vfs.set_xattr(f, "user.x", b"1", XattrSetMode::CreateOrReplace)
        .unwrap();
    let gone = mkfile(&vfs, d, "tmp");
    vfs.remove(d, "tmp").unwrap();
    vfs.rename(d, "lib.rlib", d, "renamed.rlib").unwrap();

    snapshot_store(live.path(), crashed.path());

    let rec = Vfs::open_with(crashed.path(), opts).unwrap();
    let d2 = rec.lookup(ROOT_INO, "target").unwrap();
    assert_eq!(d2, d);
    assert_eq!(rec.lookup(d, "renamed.rlib").unwrap(), f);
    assert_eq!(rec.lookup(d, "lib.rlib").err(), Some(FsError::NotFound));
    assert_eq!(rec.getattr(f).unwrap().nlink, 2);
    assert_eq!(rec.read(f, 0, 100).unwrap().0, b"artifact".to_vec());
    assert_eq!(rec.get_xattr(f, "user.x").unwrap(), b"1");
    assert_eq!(rec.getattr(gone).err(), Some(FsError::Stale));
    // Counters were recovered too: no inode number reuse.
    assert!(mkfile(&rec, ROOT_INO, "next") > gone);
    // Replayed segments are gone; a clean reopen has nothing to replay.
    rec.shutdown();
    drop(rec);
    let again = Vfs::open_with(crashed.path(), Options::default()).unwrap();
    assert_eq!(again.lookup(d, "renamed.rlib").unwrap(), f);
    vfs.shutdown();
}


// ----- compression tiers ----------------------------------------------------

fn manual_compaction() -> Options {
    Options {
        compress_after: None, // tests drive compaction explicitly
        ..Options::default()
    }
}

fn artifact(len: usize) -> Vec<u8> {
    // Compressible like real object code: repetitive symbols plus noise.
    let mut out = Vec::with_capacity(len);
    let mut x = 0x9e37_79b9u32;
    let mut i = 0u32;
    while out.len() < len {
        out.extend_from_slice(format!("_ZN4core3fmt{i}Display3fmt").as_bytes());
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        out.extend_from_slice(&x.to_le_bytes()[..(x % 4) as usize]);
        i = i.wrapping_add(1) % 500;
    }
    out.truncate(len);
    out
}

fn read_all(vfs: &Vfs, ino: u64) -> Vec<u8> {
    let size = vfs.getattr(ino).unwrap().size;
    vfs.read(ino, 0, size as u32).unwrap().0
}

#[test]
fn compacts_large_file_to_cold_and_reads_randomly() {
    use buildfs::meta::Data;
    let tmp = tempfile::tempdir().unwrap();
    let vfs = Vfs::open_with(tmp.path(), manual_compaction()).unwrap();
    let f = mkfile(&vfs, ROOT_INO, "libbig.rlib");
    let data = artifact(3 * 1024 * 1024 + 12345);
    for (i, chunk) in data.chunks(1 << 20).enumerate() {
        vfs.write_data(f, (i << 20) as u64, chunk).unwrap();
    }
    let before = vfs.getattr(f).unwrap();
    assert!(vfs.compact(f).unwrap());
    let after = vfs.getattr(f).unwrap();
    assert!(matches!(after.data, Data::Cold { .. }));
    // Compression must not look like a modification to NFS clients.
    assert_eq!(after.change, before.change);
    assert_eq!(after.mtime, before.mtime);
    let used = vfs.space_used(f, &after);
    assert!(used * 2 < data.len() as u64, "used {used} of {}", data.len());
    assert!(!tmp.path().join("hot/").read_dir().unwrap().any(|e| e
        .unwrap()
        .path()
        .read_dir()
        .unwrap()
        .next()
        .is_some()));

    assert_eq!(read_all(&vfs, f), data);
    for (off, len) in [(0usize, 1usize), (131_071, 2), (1_000_000, 300_000), (data.len() - 5, 50)] {
        let (got, eof) = vfs.read(f, off as u64, len as u32).unwrap();
        let end = (off + len).min(data.len());
        assert_eq!(got, data[off..end], "offset {off}");
        assert_eq!(eof, end == data.len());
    }
}

#[test]
fn tiny_files_go_inline_and_empty_files_drop_data() {
    use buildfs::meta::Data;
    let tmp = tempfile::tempdir().unwrap();
    let vfs = Vfs::open_with(tmp.path(), manual_compaction()).unwrap();
    let d = mkfile(&vfs, ROOT_INO, "foo.d");
    let dep = b"/target/debug/deps/foo.rmeta: src/lib.rs src/a.rs src/b.rs\n".repeat(20);
    vfs.write_data(d, 0, &dep).unwrap();
    assert!(vfs.compact(d).unwrap());
    let inode = vfs.getattr(d).unwrap();
    assert!(matches!(inode.data, Data::Inline { .. }), "{:?}", inode.data);
    assert_eq!(read_all(&vfs, d), dep);
    assert_eq!(vfs.read(d, 5, 10).unwrap().0, dep[5..15].to_vec());

    let e = mkfile(&vfs, ROOT_INO, "empty");
    vfs.write_data(e, 0, b"x").unwrap();
    vfs.setattr(&user(), e, &SetAttrs { size: Some(0), ..attrs() }).unwrap();
    assert!(vfs.compact(e).unwrap());
    assert_eq!(vfs.getattr(e).unwrap().data, Data::Empty);
}

#[test]
fn writes_rehydrate_and_truncate_drops_cold_data() {
    use buildfs::meta::Data;
    let tmp = tempfile::tempdir().unwrap();
    let vfs = Vfs::open_with(tmp.path(), manual_compaction()).unwrap();
    let f = mkfile(&vfs, ROOT_INO, "out");
    let mut data = artifact(600_000);
    vfs.write_data(f, 0, &data).unwrap();
    assert!(vfs.compact(f).unwrap());

    // In-place write into the middle of a cold file.
    vfs.write_data(f, 200_000, b"PATCHED").unwrap();
    data[200_000..200_007].copy_from_slice(b"PATCHED");
    assert_eq!(vfs.getattr(f).unwrap().data, Data::Hot);
    assert_eq!(read_all(&vfs, f), data);
    assert_eq!(vfs.compress_stats().rehydrated, 1);

    // Recompress, then shrink (needs the bytes) and truncate (doesn't).
    assert!(vfs.compact(f).unwrap());
    vfs.setattr(&user(), f, &SetAttrs { size: Some(1000), ..attrs() }).unwrap();
    assert_eq!(read_all(&vfs, f), data[..1000].to_vec());
    assert!(vfs.compact(f).unwrap());
    vfs.setattr(&user(), f, &SetAttrs { size: Some(0), ..attrs() }).unwrap();
    assert_eq!(vfs.getattr(f).unwrap().data, Data::Empty);
    vfs.write_data(f, 0, b"fresh").unwrap();
    assert_eq!(read_all(&vfs, f), b"fresh".to_vec());
}

#[test]
fn unlink_frees_cold_and_inline_data() {
    let tmp = tempfile::tempdir().unwrap();
    let vfs = Vfs::open_with(tmp.path(), manual_compaction()).unwrap();
    let big = mkfile(&vfs, ROOT_INO, "big");
    vfs.write_data(big, 0, &artifact(500_000)).unwrap();
    let small = mkfile(&vfs, ROOT_INO, "small");
    vfs.write_data(small, 0, b"tiny").unwrap();
    assert!(vfs.compact(big).unwrap());
    assert!(vfs.compact(small).unwrap());
    let cold_files = || {
        walk(&tmp.path().join("cold"))
            .into_iter()
            .filter(|p| !p.to_string_lossy().contains("/tmp/"))
            .count()
    };
    assert_eq!(cold_files(), 1);
    vfs.remove(ROOT_INO, "big").unwrap();
    vfs.remove(ROOT_INO, "small").unwrap();
    assert_eq!(cold_files(), 0);
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[test]
fn cold_data_survives_crash_and_orphans_are_collected() {
    let live = tempfile::tempdir().unwrap();
    let crashed = tempfile::tempdir().unwrap();
    let opts = Options {
        flush_interval: None,
        ..manual_compaction()
    };
    let vfs = Vfs::open_with(live.path(), opts.clone()).unwrap();
    let f = mkfile(&vfs, ROOT_INO, "a.rlib");
    let data = artifact(400_000);
    vfs.write_data(f, 0, &data).unwrap();
    assert!(vfs.compact(f).unwrap());
    let g = mkfile(&vfs, ROOT_INO, "b.d");
    vfs.write_data(g, 0, b"deps: x y z").unwrap();
    assert!(vfs.compact(g).unwrap());
    snapshot_store(live.path(), crashed.path());
    // An orphaned cold image left by a crash mid-publish.
    let orphan = crashed.path().join("cold/ff/00000000000000ff-1");
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, b"junk").unwrap();

    let rec = Vfs::open_with(crashed.path(), opts).unwrap();
    assert_eq!(read_all(&rec, f), data);
    assert_eq!(read_all(&rec, g), b"deps: x y z".to_vec());
    assert!(!orphan.exists());
    vfs.shutdown();
}

#[test]
fn concurrent_writes_never_lose_data_to_compaction() {
    let tmp = tempfile::tempdir().unwrap();
    let vfs = Vfs::open_with(tmp.path(), manual_compaction()).unwrap();
    let f = mkfile(&vfs, ROOT_INO, "racy");
    let block = 64 * 1024;
    let blocks = 24;
    vfs.write_data(f, 0, &vec![0u8; block * blocks]).unwrap();
    std::thread::scope(|s| {
        let v = &vfs;
        s.spawn(move || {
            for _ in 0..200 {
                let _ = v.compact(f);
            }
        });
        s.spawn(move || {
            for i in 0..blocks {
                v.write_data(f, (i * block) as u64, &vec![i as u8 + 1; block])
                    .unwrap();
            }
        });
    });
    let got = read_all(&vfs, f);
    for i in 0..blocks {
        assert!(
            got[i * block..(i + 1) * block].iter().all(|&b| b == i as u8 + 1),
            "block {i} lost"
        );
    }
}

#[test]
fn background_compactor_runs_after_idle_delay() {
    use buildfs::meta::Data;
    let tmp = tempfile::tempdir().unwrap();
    let opts = Options {
        compress_after: Some(std::time::Duration::from_millis(200)),
        ..Options::default()
    };
    let vfs = Vfs::open_with(tmp.path(), opts).unwrap();
    let f = mkfile(&vfs, ROOT_INO, "x.o");
    vfs.write_data(f, 0, &artifact(300_000)).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while vfs.getattr(f).unwrap().data == Data::Hot {
        assert!(std::time::Instant::now() < deadline, "never compacted");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(matches!(vfs.getattr(f).unwrap().data, Data::Cold { .. }));
    vfs.shutdown();
}

#[test]
fn every_xattr_is_stored_including_provenance() {
    let tmp = tempfile::tempdir().unwrap();
    let vfs = Vfs::open(tmp.path()).unwrap();
    let f = mkfile(&vfs, ROOT_INO, "f");
    assert!(!vfs.xattr_exists(f, "com.apple.provenance").unwrap());
    vfs.set_xattr(f, "com.apple.provenance", b"\x01\x02", XattrSetMode::CreateOrReplace)
        .unwrap();
    assert!(vfs.xattr_exists(f, "com.apple.provenance").unwrap());
    assert_eq!(vfs.get_xattr(f, "com.apple.provenance").unwrap(), b"\x01\x02");
}

#[test]
fn unlinked_open_files_live_until_last_close() {
    let tmp = tempfile::tempdir().unwrap();
    let vfs = Vfs::open_with(tmp.path(), manual_compaction()).unwrap();
    let f = mkfile(&vfs, ROOT_INO, "tmpfile");
    vfs.write_data(f, 0, b"Hello, World!").unwrap();
    vfs.open_acquired(f);
    vfs.open_acquired(f);
    vfs.remove(ROOT_INO, "tmpfile").unwrap();
    assert_eq!(vfs.lookup(ROOT_INO, "tmpfile").err(), Some(FsError::NotFound));
    assert_eq!(vfs.getattr(f).unwrap().nlink, 0);
    assert_eq!(read_all(&vfs, f), b"Hello, World!".to_vec());
    vfs.write_data(f, 0, b"J").unwrap();
    vfs.open_released(f);
    assert_eq!(read_all(&vfs, f), b"Jello, World!".to_vec());
    vfs.open_released(f);
    assert_eq!(vfs.getattr(f).err(), Some(FsError::Stale));
    // Files closed before unlink are freed immediately, as before.
    let g = mkfile(&vfs, ROOT_INO, "g");
    vfs.open_acquired(g);
    vfs.open_released(g);
    vfs.remove(ROOT_INO, "g").unwrap();
    assert_eq!(vfs.getattr(g).err(), Some(FsError::Stale));
}

#[test]
fn orphans_are_freed_on_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let f;
    {
        let vfs = Vfs::open(tmp.path()).unwrap();
        f = mkfile(&vfs, ROOT_INO, "held");
        vfs.write_data(f, 0, &artifact(10_000)).unwrap();
        vfs.open_acquired(f);
        vfs.remove(ROOT_INO, "held").unwrap();
        vfs.shutdown();
    }
    let vfs = Vfs::open(tmp.path()).unwrap();
    assert_eq!(vfs.getattr(f).err(), Some(FsError::Stale));
    assert!(walk(&tmp.path().join("hot")).is_empty());
}
