# buildfs

A userspace filesystem for build artifacts that compresses everything with
zstd and stores identical content once. No kernel extension or root needed.
There are two front ends over the same core:

| | **FSKit** (macOS 27+) | **NFSv4.1 on localhost** |
|---|---|---|
| Incremental cargo build (APFS: 0.9 s) | **2.0 s** | 6.4 s |
| Setup | build and enable an app extension | none |
| Server process | none (`fskitd` runs the module) | yes |
| FIFOs, sockets, device nodes | no | yes |
| `--sync strict` | no | yes |
| Attachments share dedup with the root | large chunks only (shared pool) | yes |

## Quick start: FSKit

One-time setup: build and register the module, then enable it. This needs
Xcode, and an Apple developer team of your own in
`fskit/BuildFS.xcodeproj` (`DEVELOPMENT_TEAM`), because FSKit modules must be
signed.

```sh
cargo build --release  # the CLI: target/release/buildfs
fskit/install.sh       # builds ~/Applications/BuildFS.app and registers it
# System Settings > General > Login Items & Extensions >
#   File System Extensions: enable "buildfs"
```

Then, with `buildfs` on your `PATH`:

```sh
buildfs mount --fskit ~/mnt/build          # store "default"; returns at once
CARGO_TARGET_DIR=~/mnt/build/myproject cargo build
buildfs unmount                            # with its attachments
```

To keep it mounted across logins and bring it back after a crash:

```sh
buildfs install-agent --fskit ~/mnt/build  # logs: ~/Library/Logs/buildfs-default.log
buildfs uninstall-agent                    # unmounts and removes the agent
```

`--name <store>` selects another store. Stores live in the module's sandbox
container, which nothing else can read: `buildfs delete-store <name>` deletes
one (freeing what no other store shares), and its size and the total for all
stores are logged whenever it closes
(`log show --predicate 'subsystem == "com.pawlowskialex.buildfs.fsmodule"' | grep "closed store"`).

Mount with `mount -F -t buildfs -o owners,nobrowse buildfs://<store> <dir>` if
you do it by hand. **`owners` matters:** without it macOS marks the volume as
ignoring ownership, and `chown`/`chgrp` report success but change nothing.

See [FSKit notes](#fskit-notes) for its limits.

## Quick start: NFS

```sh
cargo build --release
mkdir -p ~/mnt/build
./target/release/buildfs mount --store ~/.buildfs-store ~/mnt/build
CARGO_TARGET_DIR=~/mnt/build/myproject cargo build
```

`mount` serves and mounts in one process and unmounts on Ctrl-C/SIGTERM.
Put all projects' target directories under one mount so shared dependencies
are stored once. `serve` runs only the server (mount it yourself with
`mount_nfs -o vers=4.1,tcp,port=20490,namedattr 127.0.0.1:/ <dir>`).

To keep a mount running across logins (restarted automatically if it
crashes; logs in `~/Library/Logs/buildfs-<name>.log`):

```sh
buildfs install-agent --store ~/.buildfs-store ~/mnt/build
buildfs uninstall-agent          # unmounts and removes the agent
```

## Options

| Option | Default | |
|---|---|---|
| `--compress-after <s>` | 15 | idle time before a file is compressed (0 = never) |
| `--level <n>` | 3 | zstd level (see Compression settings) |
| `--workers <n>` | cores/4 | background compression threads (utility QoS) |
| `--sync relaxed\|strict` | relaxed | see Durability (NFS only) |
| `-o <opts>` | | extra mount options |

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
so `buildfs attach <dir>` mounts buildfs at that path instead. Tools see an
ordinary directory, and `.gitignore` keeps working. Attachments are recorded
and restored when buildfs mounts; `buildfs attachments` lists them, and
`buildfs detach [--keep] <dir>` undoes one.

On NFS an attachment is a subtree of the store. FSKit can't mount a subtree
(each volume is a process with exclusive access to its store), so there each
attachment is a store of its own, with its own extension process (about 11 MB
when idle). All FSKit stores share their compressed chunks through a pool (see
Design), which pays off when projects have the same dependencies. For
`node_modules`:

| | separate | shared pool |
|---|---|---|
| same dependencies, 3 projects, plus a Vite app | 370 MB | **201 MB** |
| unrelated stacks (estimate from one store holding them) | 144–173 MB | 145–163 MB |

Files that compress to 3 KiB or less and chunks of 2 KiB or less live in each
store's own database, and aren't shared.

**What fits.** Compiler outputs (Rust, Xcode, Go, Swift, C++) are few,
large, write-once files: ideal. `node_modules` shrinks well too (a Next.js
app: 338 MB to 113 MB). Creating thousands of tiny files is where the front
ends differ: `npm install` took 8.5 s on an FSKit attachment and 22 s over NFS
(an older measurement), against 4–5 s on APFS.

## Results

Debug builds of the `buildfs` crate, M1 Max, macOS 27.2, interleaved on fresh
stores (`scripts/bench/compare.sh`, medians of 3 rounds, load average 2–12):

| | APFS | FSKit | NFS |
|---|---|---|---|
| clean build | 10.7 s | 12.2 s | 21.2 s |
| incremental (leaf crate) | 0.9 s | 2.0 s | 6.4 s |
| no-op | 0.1 s | 0.3 s | 1.3 s |
| stat | 2.9 µs | 3.0 µs | 68 µs |
| create + write 4K + close | 77 µs | 1,062 µs | 13,896 µs |
| open + read + close | 21 µs | 159 µs | 451 µs |
| rename | 57 µs | 801 µs | 5,412 µs |
| hard link | 397 µs | 1,923 µs | 5,366 µs |
| unlink | 42 µs | 407 µs | 437 µs |

Space (the core is the same on both front ends):

| | APFS | buildfs |
|---|---|---|
| this workspace, one target dir | 931 MB | **259 MB** (3.60x, FSKit) |
| two target dirs, same deps | 1.1 GB | **184 MB** (6.1x: 3.06x zstd, 2x dedup; NFS, older build) |

All 2964 artifacts of the workspace build were byte- and metadata-identical
after compression and remount on FSKit
(`scripts/fskit-integrity.sh`), as were all 4680 of the two-target run on NFS.
Timings vary ±20% with machine load.

**Conformance.** `scripts/pjdfstest.sh` runs the pjdfstest POSIX suite
(8686 assertions, unprivileged) on APFS and on buildfs.

- **NFS:** every assertion matches APFS except 69, all caused by the macOS
  NFS client: it reports no `_PC_PATH_MAX` (the `*/03.t` path-length tests) and
  refuses to open FIFOs itself (EACCES; no request reaches the server).
- **FSKit** (`MOUNT_KIND=fskit CLASSIFY=1`): 702 assertions fail on buildfs
  only, all from FSKit's limits: 200 FIFO and socket creations that never reach
  the module, 434 later assertions on those objects, and 68 path-length tests
  (`_PC_PATH_MAX` again). The classifier attributes every assertion after a
  refused creation to it, so it could hide a real failure inside such a loop
  iteration; the other iterations of those loops all pass.

## Supported

Both front ends: files, directories, hard links, symlinks, xattrs (stored
natively, including `com.apple.provenance`, no `._` files), chmod/chown/chgrp
by name or id, utimes/birthtime, hidden/archive flags, sparse truncation,
rename-over, unlinked-but-open files, `mmap`, `flock` and byte-range locks
(advisory).

NFS also: sockets, device nodes, FIFOs (created and listed; the client won't
open them), the system flag, filehandles that survive server restarts.

Checked on an FSKit volume (`scripts/fskit-smoke.sh`, plus by hand):
`/bin/cp -p`, `ditto`, `git clone` and `fsck`, cargo with `target-dir` and with
`build-dir`, `xcodebuild` and `swift build` of a small package, a Python venv
and an npm install in attached directories.

## Design

```
macOS NFS client ── NFSv4.1/TCP ── vendor/embednfs (patched) ── BuildFs adapter ──┐
kernel ── fskitd ── NSXPC ── BuildFSExtension.swift ── crates/buildfs-ffi (C ABI) ┤
                                                                                  │
                     Vfs: POSIX semantics, one redb txn per op ───────────────────┘
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
- **Shared pool (FSKit).** Stores can't share a database, but they can share
  files. A store publishes each chunk file as a hard link in a pool; another
  store needing that chunk verifies the pool's copy against the bytes it has,
  and links it into its own `chunks/` instead of compressing it. The file's
  link count is the reference count across stores, so they need no
  coordination. A store only ever uses its own link, so any crash or race ends
  in a duplicate or an orphan, never lost data. Orphans are swept in the
  background at startup, and when a store is deleted. `-o nopool` opts out.
  Sharing is forward-looking: chunks two stores already hold separately stay
  separate. Damage to a shared chunk file (say, after power loss, since chunk
  files aren't fsynced) affects every store using it, as it already did for
  every file using the chunk within a store.
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

## FSKit notes

- **Speed.** The kernel caches file data (the module grants write-back
  caching), attributes and names, so stat and cached reads don't reach
  buildfs. What's left is about 83 µs per call into the module, nearly all of
  it in `fskitd` and NSXPC (`docs/HANDOFF-fskit.md` has the profile). A hard
  link takes 12–13 such calls, 9 of them lookups, which is why build tools
  that hard-link their outputs (cargo) are its slowest case. The raw figures
  and drafts of the reports for Apple are in `docs/apple-feedback.md`.
- **Crashes.** If the extension dies, the kernel unmounts the volume within
  about a second. Metadata recovers from the WAL at the next mount. In tests
  every closed file and every fsynced file survived a `kill -9`, even a kill
  microseconds after `close`; a file still open and never fsynced lost its
  data. The mountpoint is then an ordinary empty directory until something
  mounts it again, so a tool that keeps writing by path silently writes to
  your local disk. A process being started from the volume at that moment
  can hang until it is killed (seen once; Ctrl-C clears it).
- **Privacy permissions.** macOS treats these volumes as network volumes. A
  terminal needs that permission to use one, and a GUI app started from Finder
  blocked on it in our test, presumably at a prompt, so expect one from Xcode
  or your editor. Jobs started by launchd, with no UI to ask, get `EPERM` on the
  volume: they can mount it but neither read nor unmount it. That's why the
  agent only ever mounts, and `uninstall-agent` unmounts from your shell. CI
  runners or other daemons started by launchd probably can't use the mount
  without being granted that permission (not tested).
- **No FIFOs, sockets or device nodes.** FSKit refuses them before the module
  is called: `mkfifo` and `bind` fail with `ENOTSUP`.
- **Fixed behaviour.** `noatime` is always on. `_PC_PATH_MAX` and
  `_PC_PIPE_BUF` return `EINVAL`, as on NFS.
- **`--sync strict`** isn't available: the kernel's write-back cache decides
  when data reaches buildfs. Use `fsync`, which reaches it.
- **Not tested yet:** the agent across a real reboot or logout, sleep and
  wake, and Xcode.app itself (only `xcodebuild`).

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
cargo test --workspace            # embednfs protocol suite, VFS and C API tests
scripts/pjdfstest.sh              # POSIX suite, diffed against APFS (~8 min)
MOUNT_KIND=fskit CLASSIFY=1 scripts/pjdfstest.sh   # the same on FSKit (~4 min)
fskit/install.sh                  # build and register the module
scripts/fskit-cli.sh              # CLI, attachments and agent, end to end
scripts/fskit-smoke.sh <mount>    # functional checks (FSKIT=1 on FSKit)
scripts/fskit-integrity.sh        # build, compress, remount, compare
scripts/fskit-crash.sh            # kill -9 mid-build, recover
scripts/bench/compare.sh          # APFS vs FSKit vs NFS, interleaved
scripts/bench/calls_per_op.py <mount>   # module calls per syscall
fskit/profile-fskitd.sh           # profile the whole call path (no sudo)
cargo run --release -p buildfs --example vfs_bench
cargo run --release -p buildfs --example compress_eval -- <dirs>
```
