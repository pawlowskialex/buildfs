use buildfs::meta::{Kind, ROOT_INO};
use buildfs::vfs::{Caller, NewNode, Vfs};
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
