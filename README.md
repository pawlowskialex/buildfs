# buildfs

A userspace filesystem for build artifacts that compresses everything with
zstd and stores identical content once. It is served to the macOS kernel
over **NFSv4.1 on localhost**, so no kernel extension, FUSE or FSKit is
required.

```sh
cargo build --release
mkdir -p ~/mnt/build
./target/release/buildfs mount --store ~/.buildfs-store ~/mnt/build
CARGO_TARGET_DIR=~/mnt/build/myproject cargo build
```

`mount` serves and mounts in one process and unmounts on Ctrl-C/SIGTERM.
No root needed. Put all projects' target directories under one mount so
shared dependencies are stored once. `serve` runs only the server (mount it
yourself with `mount_nfs -o vers=4.1,tcp,port=20490,namedattr 127.0.0.1:/ <dir>`).

To keep a mount running across logins (restarted automatically if it
crashes; logs in `~/Library/Logs/buildfs-<name>.log`):

```sh
buildfs install-agent --store ~/.buildfs-store ~/mnt/build
buildfs uninstall-agent          # unmounts and removes the agent
```

| Option | Default | |
|---|---|---|
| `--compress-after <s>` | 15 | idle time before a file is compressed (0 = never) |
| `--level <n>` | 3 | zstd level (see Compression settings) |
| `--workers <n>` | cores/4 | background compression threads (utility QoS) |
| `--sync relaxed\|strict` | relaxed | see Durability |
| `-o <opts>` | | extra `mount_nfs` options |

## Integrating with toolchains

**Global settings, no per-project changes.** `buildfs integrate` shows the
status and what it would change; `buildfs integrate rust xcode go uv` (or
`all`) applies, and `--revert` undoes. It never overwrites a value you set
yourself.

| Tool | Setting | Moves |
|---|---|---|
| Rust | `~/.cargo/config.toml` `[build] build-dir = "<mnt>/cargo/{workspace-path-hash}"` | deps, incremental, build scripts of every workspace; final binaries stay in `target/`; `cargo clean` still works |
| Xcode | `defaults write com.apple.dt.Xcode IDECustomDerivedDataLocation <mnt>/xcode/DerivedData` | DerivedData (Xcode and xcodebuild) |
| Go | `go env -w GOCACHE=<mnt>/go/cache` | build cache |
| uv | `~/.config/uv/uv.toml` `cache-dir` | Python package cache |
| Zig, Gradle, Python bytecode | `ZIG_GLOBAL_CACHE_DIR`, `GRADLE_USER_HOME`, `PYTHONPYCACHEPREFIX` | printed for your shell profile (env-only tools) |

These go through each tool's own configuration, so IDEs (rust-analyzer,
Xcode, gopls) and scripts pick them up too.

**Per-project directories.** `node_modules`, `.next`, SwiftPM `.build`,
Gradle/CMake `build/` and `.venv` have no global setting. Symlinks aren't
reliable (npm replaces a symlinked `node_modules` with a real directory),
so `buildfs attach <dir>` mounts a store subtree at that path instead.
Tools see an ordinary directory, and `.gitignore` keeps working.
Attachments are recorded in the store and restored when buildfs starts;
`buildfs attachments` lists them, and `buildfs detach [--keep] <dir>` undoes
one.

**What fits.** Compiler outputs (Rust, Xcode, Go, Swift, C++) are few,
large, write-once files: ideal. `node_modules` shrinks well too (a Next.js
app: 336 MB to 116 MB), but creating or reading thousands of tiny files
costs about 2.5 ms each over NFS, so `npm install` took 22 s instead of 4 s.
Attach it when disk space matters more than install time.

## Results

Debug builds of this workspace (M-series Mac, APFS):

| | APFS | buildfs |
|---|---|---|
| one target dir | 513 MB | **170 MB** (3.0x) |
| two target dirs, same deps | 1.1 GB | **184 MB** (6.1x: 3.06x zstd, 2x dedup) |
| clean build | 10.6 s | 15.5 s |
| incremental (leaf crate) | 0.8 s | 2.9–4 s |
| no-op | 0.12 s | 0.6 s |

All 4680 artifacts of the two-target run were byte-identical after
compression and remount. Timings vary ±20% with machine load.

**Conformance.** `scripts/pjdfstest.sh` runs the pjdfstest POSIX suite
(8686 assertions, unprivileged) on APFS and on buildfs. Every assertion
matches APFS except 69, all caused by the macOS NFS client: it reports no
`_PC_PATH_MAX` (the `*/03.t` path-length tests) and refuses to open FIFOs
itself (EACCES; no request reaches the server).

## Supported

Files, directories, hard links, symlinks, sockets, device nodes, FIFOs
(created and listed; the client won't open them), xattrs (stored natively
via NFSv4 named attributes, including `com.apple.provenance`, no `._`
files), chmod/chown/chgrp by name or id, utimes/birthtime,
hidden/archive/system flags, byte-range locks (advisory), sparse
truncation, rename-over, unlinked-but-open files (no `.nfs*` renames),
filehandles that survive server restarts.

## Design

```
macOS NFS client ── NFSv4.1/TCP ── vendor/embednfs (patched) ── BuildFs adapter
                                                                    │
                     Vfs: POSIX semantics, one redb txn per op ─────┤
                     meta.redb  inodes · dirents · cookies · xattrs · inline
                     wal/       redo log (plain write(2), no fsync)
                     hot/       uncompressed files being written
                     chunks/    content-addressed zstd chunks (> 2 KiB)
```

- **Hot → chunks.** Writes go to a plain per-inode file. Once a file has
  been idle for `--compress-after` seconds, a worker splits it into 128 KiB
  chunks identified by the BLAKE3 hash of their contents. A chunk that is
  already stored just gains a reference (no compression work); a new one is
  compressed with zstd (or stored raw if that doesn't help) and kept in redb
  if it is 2 KiB or less, otherwise as a file. Reads only decompress the
  chunks they touch, through a cache keyed by hash. Files that compress to
  3 KiB or less are stored whole in redb, which saves a 4 KiB block each.
  Writing to a compressed file rehydrates it first; truncating it to zero
  (`O_TRUNC`) just drops its references. The NFS change attribute doesn't
  change, so client caches stay valid. Writes that race with a compaction
  are detected and redone.
- **Chunk lifetime.** Reference counts change only in metadata
  transactions. A chunk file is written before its first reference commits
  and deleted after its last reference is dropped. Deletion re-checks the
  count under a lock that compaction also holds while committing, so a
  delete racing a new reference can only make that compaction retry.
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
- **Open files.** embednfs reports when open states start and end
  (including lease expiry), and buildfs keeps unlinked inodes alive until
  the last one closes. The server sets `OPEN4_RESULT_PRESERVE_UNLINKED`, so
  the client removes open files directly instead of renaming them to
  `.nfs*`.
- **Recovery.** At startup the WAL is replayed, files unlinked while open
  are freed, unreferenced hot and chunk files are removed, and files left
  uncompressed are queued for compression. If a previous instance died with
  the mount still in place, `mount` adopts it: the kernel client reconnects
  on its own, and even file descriptors opened before the crash keep
  working.

## Compression settings

Measured on the artifacts of two target directories (526 MiB of distinct
chunks, `examples/compress_eval.rs`):

| zstd level | ratio | speed (one core) |
|---|---|---|
| 1 | 2.84x | 602 MiB/s |
| **3** (default) | 2.94x | 460 MiB/s |
| 9 | 3.09x | 80 MiB/s |
| 19 | 3.41x | 4 MiB/s |

Compression runs in the background on utility-QoS threads, so a higher
`--level` costs CPU and energy, not build time. Trained dictionaries add
only 2.3% on 128 KiB chunks. They add 27% on small files, but those are
under 10% of the bytes, so they aren't used.

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
- **Owner strings.** Only numeric owners were parsed, so `chown`/`chgrp` by
  name (macOS sends `root`, `wheel@host` and `EVERYONE@`) reported success
  and changed nothing. Unknown names now fail with `NFS4ERR_BADOWNER`.
- Open-state tracking with `PRESERVE_UNLINKED`, stable filehandles,
  special files, a persistent server identity, non-atomic `change_info`,
  and the `xattr_exists` hook.

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
- Read delegations don't help: the client returns them almost immediately
  (about one DELEGRETURN per OPEN), which adds round trips. Most remaining
  RPCs are lookups of path components in busy directories, which only
  directory delegations could avoid.
- Unmounting from a process started by launchd hangs in the kernel or fails
  with EPERM. This is why the agent leaves its mount in place and
  `uninstall-agent` unmounts from your shell.

## Roadmap

- `buildfs stats` for a live mount (store usage, dedup ratio).
- More integrations: Bazel `--output_user_root`, ccache/sccache config,
  Gradle via `gradle.properties` where possible.
- Garbage collection for stale artifacts (e.g. drop chunks of files unread
  for N days), a store size limit.
- Pursue upstreaming the embednfs patches.

## Development

```sh
cargo test --workspace            # embednfs protocol suite + buildfs VFS tests
scripts/pjdfstest.sh              # POSIX suite, diffed against APFS (~8 min)
cargo run --release -p buildfs --example vfs_bench
cargo run --release -p buildfs --example compress_eval -- <dirs>
```
