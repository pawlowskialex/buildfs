//! Evaluates compression settings on real build artifacts.
//! Usage: compress_eval <dir>...
use std::collections::HashSet;
use std::time::Instant;

const CHUNK: usize = 128 * 1024;

fn main() -> anyhow::Result<()> {
    let mut chunks = Vec::new();
    let mut small_files = Vec::new();
    let mut seen = HashSet::new();
    for dir in std::env::args().skip(1) {
        for entry in walk(std::path::Path::new(&dir)) {
            let data = std::fs::read(&entry)?;
            if data.len() <= 64 * 1024 {
                small_files.push(data.clone());
            }
            for c in data.chunks(CHUNK) {
                if seen.insert(*blake3::hash(c).as_bytes()) {
                    chunks.push(c.to_vec());
                }
            }
        }
    }
    let total: usize = chunks.iter().map(Vec::len).sum();
    println!("{} unique chunks, {:.1} MiB; {} small files", chunks.len(), total as f64 / 1048576.0, small_files.len());
    for level in [1, 3, 6, 9, 15, 19] {
        let t = Instant::now();
        let mut c = zstd::bulk::Compressor::new(level)?;
        let out: usize = chunks.iter().map(|d| c.compress(d).map_or(d.len(), |v| v.len().min(d.len()))).sum();
        let secs = t.elapsed().as_secs_f64();
        println!("level {level:2}: ratio {:.2}x  {:6.0} MiB/s", total as f64 / out as f64, total as f64 / 1048576.0 / secs);
    }
    // Dictionary trained on every 4th chunk, evaluated on the others.
    let train: Vec<&[u8]> = chunks.iter().step_by(4).map(|c| &c[..c.len().min(16 * 1024)]).collect();
    let dict = zstd::dict::from_samples(&train, 112 * 1024)?;
    let test: Vec<&Vec<u8>> = chunks.iter().enumerate().filter(|(i, _)| i % 4 != 0).map(|(_, c)| c).collect();
    let ttotal: usize = test.iter().map(|c| c.len()).sum();
    let mut plain = zstd::bulk::Compressor::new(3)?;
    let mut withd = zstd::bulk::Compressor::with_dictionary(3, &dict)?;
    let p: usize = test.iter().map(|d| plain.compress(d).map_or(d.len(), |v| v.len())).sum();
    let w: usize = test.iter().map(|d| withd.compress(d).map_or(d.len(), |v| v.len())).sum();
    println!("chunks, level 3: no dict {:.2}x, dict {:.2}x (+{:.1}%)", ttotal as f64 / p as f64, ttotal as f64 / w as f64, (p as f64 / w as f64 - 1.0) * 100.0);
    let sf: Vec<&Vec<u8>> = small_files.iter().enumerate().filter(|(i, _)| i % 4 != 0).map(|(_, c)| c).collect();
    let strain: Vec<&[u8]> = small_files.iter().step_by(4).map(|c| c.as_slice()).collect();
    let sdict = zstd::dict::from_samples(&strain, 112 * 1024)?;
    let mut sd = zstd::bulk::Compressor::with_dictionary(3, &sdict)?;
    let stotal: usize = sf.iter().map(|c| c.len()).sum();
    let sp: usize = sf.iter().map(|d| plain.compress(d).map_or(d.len(), |v| v.len())).sum();
    let sw: usize = sf.iter().map(|d| sd.compress(d).map_or(d.len(), |v| v.len())).sum();
    println!("small files ({:.1} MiB): no dict {:.2}x, dict {:.2}x", stotal as f64 / 1048576.0, stotal as f64 / sp as f64, stotal as f64 / sw as f64);
    Ok(())
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => out.extend(walk(&p)),
            Ok(t) if t.is_file() => out.push(p),
            _ => {}
        }
    }
    out
}
