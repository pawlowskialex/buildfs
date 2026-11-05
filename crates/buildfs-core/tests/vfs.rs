use buildfs_core::meta::{Kind, ROOT_INO};
use buildfs_core::vfs::{Caller, NewNode, Options, Vfs};
use buildfs_core::{FsError, SetAttrs, XattrSetMode};

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
    use buildfs_core::vfs::access::*;
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
fn compacts_large_file_to_chunks_and_reads_randomly() {
    use buildfs_core::meta::Data;
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
    assert!(matches!(after.data, Data::Chunked { .. }));
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
    use buildfs_core::meta::Data;
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
fn writes_rehydrate_and_truncate_drops_chunks() {
    use buildfs_core::meta::Data;
    let tmp = tempfile::tempdir().unwrap();
    let vfs = Vfs::open_with(tmp.path(), manual_compaction()).unwrap();
    let f = mkfile(&vfs, ROOT_INO, "out");
    let mut data = artifact(600_000);
    vfs.write_data(f, 0, &data).unwrap();
    assert!(vfs.compact(f).unwrap());

    // In-place write into the middle of a compressed file.
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
fn unlink_frees_chunks_and_inline_data() {
    let tmp = tempfile::tempdir().unwrap();
    let vfs = Vfs::open_with(tmp.path(), manual_compaction()).unwrap();
    let big = mkfile(&vfs, ROOT_INO, "big");
    vfs.write_data(big, 0, &artifact(500_000)).unwrap();
    let small = mkfile(&vfs, ROOT_INO, "small");
    vfs.write_data(small, 0, b"tiny").unwrap();
    assert!(vfs.compact(big).unwrap());
    assert!(vfs.compact(small).unwrap());
    let cold_files = || {
        walk(&tmp.path().join("chunks"))
            .into_iter()
            .filter(|p| !p.to_string_lossy().contains("/tmp/"))
            .count()
    };
    assert!(cold_files() >= 1);
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
fn chunked_data_survives_crash_and_orphans_are_collected() {
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
    // An orphaned chunk file left by a crash before its first reference.
    let orphan = crashed
        .path()
        .join("chunks/ab/abababababababababababababababababababababababababababababababab");
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
    use buildfs_core::meta::Data;
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
    assert!(matches!(vfs.getattr(f).unwrap().data, Data::Chunked { .. }));
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

// ----- dedup ----------------------------------------------------------------

#[test]
fn identical_files_share_chunks_and_free_with_last_reference() {
    let tmp = tempfile::tempdir().unwrap();
    let vfs = Vfs::open_with(tmp.path(), manual_compaction()).unwrap();
    let data = artifact(1_000_000);
    let mut inos = Vec::new();
    for name in ["proj-a.rlib", "proj-b.rlib", "proj-c.rlib"] {
        let f = mkfile(&vfs, ROOT_INO, name);
        vfs.write_data(f, 0, &data).unwrap();
        assert!(vfs.compact(f).unwrap());
        inos.push(f);
    }
    let (bytes_after_3, chunks_after_3) = vfs.chunk_usage().unwrap();
    let single = {
        let t2 = tempfile::tempdir().unwrap();
        let v2 = Vfs::open_with(t2.path(), manual_compaction()).unwrap();
        let f = mkfile(&v2, ROOT_INO, "x");
        v2.write_data(f, 0, &data).unwrap();
        assert!(v2.compact(f).unwrap());
        v2.chunk_usage().unwrap()
    };
    assert_eq!((bytes_after_3, chunks_after_3), single, "three copies cost one");
    let st = vfs.compress_stats();
    assert_eq!(st.dedup_chunks, 2 * chunks_after_3);
    for &f in &inos {
        assert_eq!(read_all(&vfs, f), data);
    }
    // Shared chunks survive until the last file goes.
    vfs.remove(ROOT_INO, "proj-a.rlib").unwrap();
    vfs.remove(ROOT_INO, "proj-b.rlib").unwrap();
    assert_eq!(read_all(&vfs, inos[2]), data);
    assert_eq!(vfs.chunk_usage().unwrap(), single);
    vfs.remove(ROOT_INO, "proj-c.rlib").unwrap();
    assert_eq!(vfs.chunk_usage().unwrap(), (0, 0));
    assert!(walk(&tmp.path().join("chunks"))
        .iter()
        .all(|p| p.to_string_lossy().contains("/tmp/")));
}

#[test]
fn partially_shared_and_self_duplicate_content() {
    let tmp = tempfile::tempdir().unwrap();
    let vfs = Vfs::open_with(tmp.path(), manual_compaction()).unwrap();
    let block = artifact(128 * 1024);
    // File with the same chunk four times, plus a distinct tail.
    let mut a = block.repeat(4);
    a.extend_from_slice(b"tail of a");
    let mut b = block.clone();
    b.extend_from_slice(&artifact(300_000)[1000..]);
    let fa = mkfile(&vfs, ROOT_INO, "a");
    let fb = mkfile(&vfs, ROOT_INO, "b");
    vfs.write_data(fa, 0, &a).unwrap();
    vfs.write_data(fb, 0, &b).unwrap();
    assert!(vfs.compact(fa).unwrap());
    assert!(vfs.compact(fb).unwrap());
    assert_eq!(read_all(&vfs, fa), a);
    assert_eq!(read_all(&vfs, fb), b);
    // Rewriting `a` in place rehydrates it and drops its references only.
    vfs.write_data(fa, 0, b"X").unwrap();
    assert_eq!(read_all(&vfs, fb), b);
    vfs.remove(ROOT_INO, "a").unwrap();
    vfs.remove(ROOT_INO, "b").unwrap();
    assert_eq!(vfs.chunk_usage().unwrap(), (0, 0));
}

#[test]
fn shared_chunks_survive_crash_recovery() {
    let live = tempfile::tempdir().unwrap();
    let crashed = tempfile::tempdir().unwrap();
    let opts = Options {
        flush_interval: None,
        ..manual_compaction()
    };
    let vfs = Vfs::open_with(live.path(), opts.clone()).unwrap();
    let data = artifact(700_000);
    let f = mkfile(&vfs, ROOT_INO, "one");
    let g = mkfile(&vfs, ROOT_INO, "two");
    vfs.write_data(f, 0, &data).unwrap();
    vfs.write_data(g, 0, &data).unwrap();
    assert!(vfs.compact(f).unwrap());
    assert!(vfs.compact(g).unwrap());
    vfs.remove(ROOT_INO, "one").unwrap();
    snapshot_store(live.path(), crashed.path());
    let rec = Vfs::open_with(crashed.path(), opts).unwrap();
    assert_eq!(read_all(&rec, g), data);
    rec.remove(ROOT_INO, "two").unwrap();
    assert_eq!(rec.chunk_usage().unwrap(), (0, 0));
    vfs.shutdown();
}

#[test]
fn concurrent_dedup_and_deletion_never_lose_chunks() {
    let tmp = tempfile::tempdir().unwrap();
    let vfs = Vfs::open_with(tmp.path(), manual_compaction()).unwrap();
    let data = artifact(600_000);
    std::thread::scope(|s| {
        for t in 0..4 {
            let v = &vfs;
            let data = &data;
            s.spawn(move || {
                for i in 0..15 {
                    let name = format!("t{t}-{i}");
                    let f = mkfile(v, ROOT_INO, &name);
                    v.write_data(f, 0, data).unwrap();
                    let _ = v.compact(f).unwrap();
                    assert_eq!(&read_all(v, f), data, "{name}");
                    if i % 2 == 0 {
                        v.remove(ROOT_INO, &name).unwrap();
                    }
                }
            });
        }
    });
    for name in names(&vfs, ROOT_INO) {
        let f = vfs.lookup(ROOT_INO, &name).unwrap();
        assert_eq!(read_all(&vfs, f), data, "{name}");
    }
}

// ----- shared chunk pool ----------------------------------------------------

fn pooled(store: &std::path::Path, pool: &std::path::Path, level: i32) -> std::sync::Arc<Vfs> {
    Vfs::open_with(
        store,
        Options {
            chunk_pool: Some(pool.to_path_buf()),
            level,
            ..manual_compaction()
        },
    )
    .unwrap()
}

fn put_file(vfs: &Vfs, name: &str, data: &[u8]) -> u64 {
    let ino = mkfile(vfs, ROOT_INO, name);
    assert_eq!(vfs.write_data(ino, 0, data).unwrap() as usize, data.len());
    ino
}

/// Regular files under `dir`, as (inode, link count).
fn files_under(dir: &std::path::Path) -> Vec<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(files_under(&p));
        } else {
            let m = std::fs::metadata(&p).unwrap();
            out.push((m.ino(), m.nlink()));
        }
    }
    out
}

#[test]
fn pool_shares_chunks_between_stores() {
    let tmp = tempfile::tempdir().unwrap();
    let pool = tmp.path().join("pool");
    let data = artifact(2_000_000);
    let (a, b) = (pooled(&tmp.path().join("a"), &pool, 3), pooled(&tmp.path().join("b"), &pool, 3));
    let (ia, ib) = (put_file(&a, "x", &data), put_file(&b, "y", &data));
    assert!(a.compact(ia).unwrap());
    assert_eq!(a.compress_stats().pool_chunks, 0, "first store compresses");
    assert!(b.compact(ib).unwrap());
    let st = b.compress_stats();
    assert_eq!(st.pool_chunks as usize, data.len().div_ceil(128 * 1024));
    assert_eq!(st.pool_bytes as usize, data.len());

    // One inode per chunk, shared by both stores and the pool.
    let in_pool = files_under(&pool);
    assert_eq!(in_pool.len(), st.pool_chunks as usize);
    assert!(in_pool.iter().all(|&(_, links)| links == 3), "{in_pool:?}");
    let mut own: Vec<u64> = files_under(&tmp.path().join("b/chunks")).iter().map(|f| f.0).collect();
    let mut shared: Vec<u64> = in_pool.iter().map(|f| f.0).collect();
    own.sort_unstable();
    shared.sort_unstable();
    assert_eq!(own, shared);

    assert_eq!(read_all(&a, ia), data);
    assert_eq!(read_all(&b, ib), data);

    // Removing it from one store leaves the other intact; removing it from
    // both frees everything.
    a.remove(ROOT_INO, "x").unwrap();
    assert_eq!(read_all(&b, ib), data);
    assert!(files_under(&pool).iter().all(|&(_, links)| links == 2));
    b.remove(ROOT_INO, "y").unwrap();
    assert!(files_under(&pool).is_empty());
}

#[test]
fn pool_works_across_compression_levels_and_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let pool = tmp.path().join("pool");
    let data = artifact(1_000_000);
    let (sa, sb) = (tmp.path().join("a"), tmp.path().join("b"));
    let a = pooled(&sa, &pool, 19);
    let b = pooled(&sb, &pool, 1);
    let (ia, ib) = (put_file(&a, "x", &data), put_file(&b, "y", &data));
    assert!(a.compact(ia).unwrap());
    assert!(b.compact(ib).unwrap());
    assert!(b.compress_stats().pool_chunks > 0);
    a.shutdown();
    b.shutdown();
    drop((a, b));
    // Both reopen: nothing is lost, and nothing in use is swept.
    let (a, b) = (pooled(&sa, &pool, 3), pooled(&sb, &pool, 3));
    assert_eq!(read_all(&a, ia), data);
    assert_eq!(read_all(&b, ib), data);
    assert!(!files_under(&pool).is_empty());
    assert!(files_under(&pool).iter().all(|&(_, links)| links == 3));
}

#[test]
fn pool_is_filled_from_stores_that_predate_it() {
    let tmp = tempfile::tempdir().unwrap();
    let pool = tmp.path().join("pool");
    let data = artifact(1_000_000);
    let sa = tmp.path().join("a");
    let a = Vfs::open_with(&sa, manual_compaction()).unwrap();
    let ia = put_file(&a, "x", &data);
    assert!(a.compact(ia).unwrap());
    a.shutdown();
    drop(a);
    assert!(files_under(&pool).is_empty());
    // Opening it with a pool publishes what it has (in the background).
    let a = pooled(&sa, &pool, 3);
    for _ in 0..500 {
        if files_under(&pool).iter().filter(|f| f.1 == 2).count() == 8 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(files_under(&pool).len(), 8, "1 MB is 8 chunks");
    let b = pooled(&tmp.path().join("b"), &pool, 3);
    let ib = put_file(&b, "y", &data);
    assert!(b.compact(ib).unwrap());
    assert!(b.compress_stats().pool_chunks > 0);
    assert_eq!(read_all(&a, ia), data);
    assert_eq!(read_all(&b, ib), data);
}

#[test]
fn deleting_a_store_only_frees_what_it_alone_used() {
    let tmp = tempfile::tempdir().unwrap();
    let pool = tmp.path().join("pool");
    let shared = artifact(1_000_000);
    let mut only_a = artifact(1_000_000);
    only_a.reverse();
    let sa = tmp.path().join("a");
    let (a, b) = (pooled(&sa, &pool, 3), pooled(&tmp.path().join("b"), &pool, 3));
    let (i1, i2, ib) = (put_file(&a, "s", &shared), put_file(&a, "o", &only_a), put_file(&b, "s", &shared));
    assert!(a.compact(i1).unwrap());
    assert!(a.compact(i2).unwrap());
    assert!(b.compact(ib).unwrap());
    let before = files_under(&pool).len();
    a.shutdown();
    drop(a);
    std::fs::remove_dir_all(&sa).unwrap();
    let freed = Vfs::sweep_chunk_pool(&pool).unwrap();
    assert!(freed > 0);
    assert!(files_under(&pool).len() < before);
    assert_eq!(read_all(&b, ib), shared);
}

#[test]
fn pool_survives_losing_its_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let pool = tmp.path().join("pool");
    let data = artifact(1_000_000);
    let (a, b) = (pooled(&tmp.path().join("a"), &pool, 3), pooled(&tmp.path().join("b"), &pool, 3));
    let (ia, ib) = (put_file(&a, "x", &data), put_file(&b, "y", &data));
    assert!(a.compact(ia).unwrap());
    assert!(b.compact(ib).unwrap());
    // Stores don't depend on the pool at all.
    std::fs::remove_dir_all(&pool).unwrap();
    assert_eq!(read_all(&a, ia), data);
    assert_eq!(read_all(&b, ib), data);
    a.remove(ROOT_INO, "x").unwrap();
    assert_eq!(read_all(&b, ib), data);
    // It comes back in use: a new file compacts and reads fine.
    let i2 = put_file(&b, "z", &artifact(500_000));
    assert!(b.compact(i2).unwrap());
    assert_eq!(read_all(&b, i2), artifact(500_000));
}

#[test]
fn pool_stress_many_stores_compacting_and_deleting_the_same_chunks() {
    let tmp = tempfile::tempdir().unwrap();
    let pool = tmp.path().join("pool");
    let stores: Vec<_> = (0..4)
        .map(|i| pooled(&tmp.path().join(format!("s{i}")), &pool, 1 + i * 2))
        .collect();
    // A few distinct contents, so stores keep colliding on the same chunks.
    let contents: Vec<Vec<u8>> = (0..3usize).map(|i| artifact(400_000 + i * 150_000)).collect();
    std::thread::scope(|scope| {
        for (t, vfs) in stores.iter().enumerate() {
            let contents = &contents;
            let pool = &pool;
            scope.spawn(move || {
                let mut live: Vec<(u64, String, usize)> = Vec::new();
                for round in 0..40usize {
                    let which = (t + round) % contents.len();
                    let name = format!("f{round}");
                    let ino = put_file(vfs, &name, &contents[which]);
                    // A compaction may abort on a race; retrying must work.
                    let mut done = false;
                    for _ in 0..50 {
                        if vfs.compact(ino).unwrap() {
                            done = true;
                            break;
                        }
                    }
                    assert!(done, "compaction never succeeded");
                    live.push((ino, name, which));
                    if round % 3 == 2 {
                        let (i, n, w) = live.remove(round % live.len());
                        assert_eq!(read_all(vfs, i), contents[w]);
                        vfs.remove(ROOT_INO, &n).unwrap();
                    }
                    if round % 7 == 0 {
                        let _ = Vfs::sweep_chunk_pool(pool).unwrap();
                    }
                    for (i, _, w) in &live {
                        assert_eq!(&read_all(vfs, *i), &contents[*w]);
                    }
                }
                for (i, n, w) in live {
                    assert_eq!(read_all(vfs, i), contents[w]);
                    vfs.remove(ROOT_INO, &n).unwrap();
                }
            });
        }
    });
    // Everything deleted: no chunk file left anywhere.
    let _ = Vfs::sweep_chunk_pool(&pool).unwrap();
    assert_eq!(files_under(&pool).len(), 0);
    for i in 0..4 {
        assert_eq!(files_under(&tmp.path().join(format!("s{i}/chunks"))).len(), 0);
    }
}

#[test]
fn crash_between_adopting_and_committing_leaves_nothing_behind() {
    use std::os::unix::fs::MetadataExt;
    let tmp = tempfile::tempdir().unwrap();
    let pool = tmp.path().join("pool");
    let data = artifact(1_000_000);
    let (sa, sb) = (tmp.path().join("a"), tmp.path().join("b"));
    let a = pooled(&sa, &pool, 3);
    let b = pooled(&sb, &pool, 3);
    let ia = put_file(&a, "x", &data);
    assert!(a.compact(ia).unwrap());
    b.shutdown();
    drop(b);
    // What a crash mid-adoption leaves in store b: links to the chunks with
    // no reference to them in b's metadata.
    let first_link = |dir: &std::path::Path| -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for shard in std::fs::read_dir(dir).unwrap() {
            let shard = shard.unwrap().path();
            if shard.is_dir() {
                out.extend(std::fs::read_dir(shard).unwrap().map(|e| e.unwrap().path()));
            }
        }
        out
    };
    let chunks = first_link(&pool);
    assert_eq!(chunks.len(), 8);
    for c in &chunks {
        let name = c.file_name().unwrap().to_str().unwrap();
        let dst = sb.join("chunks").join(&name[..2]);
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::hard_link(c, dst.join(name)).unwrap();
    }
    assert!(chunks.iter().all(|c| std::fs::metadata(c).unwrap().nlink() == 3));

    // Reopening removes b's stray links and nothing else.
    let b = pooled(&sb, &pool, 3);
    assert_eq!(files_under(&sb.join("chunks")).len(), 0);
    assert!(chunks.iter().all(|c| std::fs::metadata(c).unwrap().nlink() == 2));
    assert_eq!(read_all(&a, ia), data);
    // And it can adopt them properly afterwards.
    let ib = put_file(&b, "y", &data);
    assert!(b.compact(ib).unwrap());
    assert_eq!(b.compress_stats().pool_chunks, 8);
    assert_eq!(read_all(&b, ib), data);
}
