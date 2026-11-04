# buildfs

A userspace filesystem for build artifacts that compresses everything with
zstd. It is served to the macOS kernel over **NFSv4.1 on localhost**, so no
kernel extension, FUSE or FSKit is required.

```sh
cargo build --release
mkdir -p ~/mnt/build
./target/release/buildfs mount --store ~/.buildfs-store ~/mnt/build
CARGO_TARGET_DIR=~/mnt/build/myproject cargo build
```

`mount` serves and mounts in one process and unmounts on Ctrl-C/SIGTERM.
No root needed. `serve` runs only the server (mount it yourself with
`mount_nfs -o vers=4.1,tcp,port=20490,namedattr 127.0.0.1:/ <dir>`).

| Option | Default | |
|---|---|---|
| `--compress-after <s>` | 15 | idle time before a file is compressed (0 = never) |
| `--level <n>` | 3 | zstd level |
| `--workers <n>` | cores/4 | background compression threads (utility QoS) |
| `--sync relaxed\|strict` | relaxed | see Durability |
| `-o <opts>` | | extra `mount_nfs` options |

## Results

Clean debug build of this workspace (M-series Mac, APFS):

| | APFS | buildfs |
|---|---|---|
| target size on disk | 513 MB | **170 MB** (3.0x; 675 of 1590 files inlined) |
| clean build | 10.6 s | 15.5 s |
| incremental (leaf crate) | 0.8 s | 2.9 s (3.7 s on compressed artifacts) |
| no-op | 0.12 s | 0.6 s |

All 2279 artifacts verified byte-identical after compression and remount.

## Supported

Files, directories, hard links, symlinks, FIFOs, sockets, device nodes,
xattrs (stored natively via NFSv4 named attributes, including
`com.apple.provenance`, no `._` files), chmod/chown/utimes/birthtime,
hidden/archive/system flags, byte-range locks (advisory), sparse
truncation, rename-over, filehandles that survive server restarts.

## Design

```
macOS NFS client ── NFSv4.1/TCP ── vendor/embednfs (patched) ── BuildFs adapter
                                                                    │
                     Vfs: POSIX semantics, one redb txn per op ─────┤
                     meta.redb  inodes · dirents · cookies · xattrs · inline
                     wal/       redo log (plain write(2), no fsync)
                     hot/       uncompressed files being written
                     cold/      zstd images: 128 KiB chunks + index
```

- **Hot → cold.** Writes go to a plain per-inode file. Once a file has been
  idle for `--compress-after` seconds, a worker compresses it into an image
  of independently compressed 128 KiB chunks (stored raw if incompressible),
  so random reads only decompress what they touch. Files that compress to
  3 KiB or less are stored in redb, saving a 4 KiB block each. Writing to a
  cold file rehydrates it first; truncating it to zero (`O_TRUNC`) just drops
  the image. The NFS change attribute doesn't change, so client caches stay
  valid. Writes that race with a compaction are detected and redone.
- **Metadata** lives in redb. Inode numbers and readdir cookies come from
  monotonic counters and are never reused, which lets filehandles embed the
  inode number and lets directory listings be resumed while the directory
  is being modified.
- **Durability.** `relaxed` mode: every transaction is appended to the WAL
  before it commits, and file data is `pwrite`n before replying. Anything
  acknowledged therefore survives a crash of buildfs itself, though not a
  kernel panic or power loss. Durable redb commits happen once a second in
  the background. Writes reply `FILE_SYNC`, so the client skips COMMIT.
  `strict` mode: COMMIT fsyncs the file and waits for a group-committed
  durable metadata commit.
- **Recovery.** At startup the WAL is replayed, orphaned hot/cold files are
  removed, and files left uncompressed are queued for compression.

## embednfs changes

The base is [bloom-directory/embednfs](https://github.com/bloom-directory/embednfs),
an NFSv4.1 server that targets the macOS client. The patches are listed in
[vendor/VENDORED.md](vendor/VENDORED.md); each is general enough to go
upstream. The ones that matter most:

- **READDIR cookie verifier** was the directory's change attribute, so every
  mutation of a busy directory invalidated listings in progress. The client
  then saw partial `deps/` listings, and rustc failed with "can't find
  crate" in about one of every 3–6 clean builds.
- **Mandatory lock checks on READ/WRITE** broke `cp -p`, `ditto` and
  `fs::copy` (macOS locks a file and then reads it through its open stateid).
- Stable filehandles, special files, a persistent server identity,
  non-atomic `change_info`, and the `xattr_exists` hook.

## macOS NFS client notes

- `vers=4.1` must be given explicitly (`vers=4` means 4.0). Mounting as a
  regular user on a directory you own works.
- Named attributes are off unless you pass `namedattr`. Without it, the
  client stores xattrs in AppleDouble `._` files.
- Builds started from a terminal get `com.apple.provenance` on every file,
  and the kernel re-reads it on link/open/exec. Over NFS that costs a few
  round trips per operation. Refusing the attribute would be faster, but it
  breaks `cp -p`, `ditto` and Rust's `fs::copy`, so it is stored like any
  other xattr.
- Hiding `._` files from READDIR breaks xattrs with `rdirplus`, because the
  client answers lookups from cached listings.
- After a server restart, the request in flight during reconnect can fail
  with EIO. This happens inside the client's session recovery ("freeing free
  slot", SEQ_MISORDERED); later requests and data are fine.

## Roadmap

- NFSv4.1 delegations, so the client can handle opens, locks and attributes
  locally. Remaining overhead is per-RPC client latency (about 20k RPCs per
  incremental build).
- Content-addressed chunks for dedup across projects and builds (identical
  dependency builds in different `target/` directories).
- Trained zstd dictionaries per artifact type (`.rmeta`, `.rlib`, `.o`).
- pjdfstest in CI; a launchd agent; `buildfs stats`.

## Development

```sh
cargo test --workspace            # embednfs protocol suite + buildfs VFS tests
cargo run --release -p buildfs --example vfs_bench
```
