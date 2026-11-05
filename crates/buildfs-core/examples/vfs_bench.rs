//! Measures VFS core latency without NFS in the way.
use std::time::Instant;

use buildfs_core::SetAttrs;
use buildfs_core::meta::ROOT_INO;
use buildfs_core::vfs::{Caller, NewNode, Vfs};

fn main() -> anyhow::Result<()> {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "/tmp/bfs-vfs-bench".into());
    let _ = std::fs::remove_dir_all(&dir);
    let vfs = Vfs::open(std::path::Path::new(&dir))?;
    let caller = Caller { uid: 501, gid: 20, groups: vec![] };
    let n = 5000u32;
    let t = Instant::now();
    let mut inos = Vec::new();
    for i in 0..n {
        let (ino, _) = vfs
            .create(&caller, ROOT_INO, &format!("f{i}"), NewNode::File, &SetAttrs::default())
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        inos.push(ino);
    }
    println!("create   {:7.1} us/op", t.elapsed().as_micros() as f64 / f64::from(n));
    let t = Instant::now();
    for &ino in &inos {
        vfs.write_data(ino, 0, &[7u8; 4096]).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    println!("write4k  {:7.1} us/op", t.elapsed().as_micros() as f64 / f64::from(n));
    let t = Instant::now();
    for &ino in &inos {
        let _ = vfs.getattr(ino).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    println!("getattr  {:7.1} us/op", t.elapsed().as_micros() as f64 / f64::from(n));
    let t = Instant::now();
    for i in 0..n {
        vfs.rename(ROOT_INO, &format!("f{i}"), ROOT_INO, &format!("g{i}"))
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    println!("rename   {:7.1} us/op", t.elapsed().as_micros() as f64 / f64::from(n));
    vfs.shutdown();
    Ok(())
}
