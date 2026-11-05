# buildfs

A userspace filesystem for macOS that compresses build artifacts with zstd and
stores identical content once: a Rust workspace's `target` directory takes about
3.5x less space. Tools see an ordinary directory, with hard links, xattrs and
`mmap` working as usual. No kernel extension or root needed.

**Status: preview.** It passes the test suites below, but hasn't been used on real
projects for any length of time, so don't keep anything there that you can't
rebuild. The on-disk format may change. Not tested yet: FSKit across a real reboot,
logout, sleep and wake, Xcode.app itself, any real system older than macOS 27, and
real power loss.

## Quick start

Needs a Rust toolchain (1.88 or later) and macOS. It works right away over NFS; see
[FSKit](#fskit) for the much faster backend.

```sh
cargo build --release          # the CLI: put target/release/buildfs on your PATH
buildfs mount ~/mnt/build      # store "default"; returns once mounted
CARGO_TARGET_DIR=~/mnt/build/myproject cargo build
buildfs unmount                # with its attachments
```

To keep it mounted across logins and bring it back after a crash:

```sh
buildfs install-agent ~/mnt/build  # logs: ~/Library/Logs/buildfs-default.log
buildfs uninstall-agent            # unmounts and removes the agent
```

Put all projects' target directories under one mount, so that shared
dependencies are stored once. `--name <store>` selects another store. Stores
live in `~/.buildfs/stores/<name>`, where `du` works on them;
`buildfs delete-store <name>` deletes one, freeing what no other store shares.

## Backends

The same commands and the same stores work on two front ends. `buildfs mount`
uses FSKit on macOS 27 and later if its module is installed, and NFS otherwise,
saying so when it falls back. `--backend fskit|nfs` overrides that.

| | **FSKit** (macOS 27+) | **NFSv4.1 on localhost** |
|---|---|---|
| Incremental cargo build (APFS: 0.9 s) | **2.2 s** | 7.0 s |
| Setup | build and enable an app extension | none |
| Server process | none (`fskitd` runs the module) | yes, in the background |
| FIFOs, sockets, device nodes | no | yes |
| `--sync strict`, clean build | 2.5x slower | 1.4x slower |
| Attachments share dedup with the root | large chunks only (shared pool) | yes |

Things to know about the choice:

- **A module that's installed but disabled is an error**, with instructions, not
  a reason to use NFS: you would silently lose most of the speed. Use
  `--backend nfs` if that's what you want.
- **An agent keeps the backend it was installed with**, so upgrading macOS
  doesn't change what it does. To move an agent to FSKit after upgrading, install
  it again; the store comes along.
- **A store can move between backends, but its attachments can't**, because they
  work differently on each (see below). `detach` them first, and attach them
  again afterwards.
- A store can only be open once, so mounting it twice is refused.
- The check for macOS versions before 27 is unit-tested and simulated
  (`BUILDFS_TEST_MACOS_VERSION`), but hasn't run on a real older system.

### FSKit

One-time setup: build and register the module, then enable it. This needs
Xcode, and an Apple developer team of your own in
`fskit/BuildFS.xcodeproj` (`DEVELOPMENT_TEAM`), because FSKit modules must be
signed. Bundle identifiers belong to one team, so you'll also need to replace
`com.pawlowskialex.buildfs` there, in `crates/buildfs/src/backend.rs` and in
`fskit/install.sh`. FSKit's speed depends on `DataCacheHandler`, which is new in
macOS 27, so earlier versions use NFS.

```sh
fskit/install.sh       # builds ~/Applications/BuildFS.app and registers it
# System Settings > General > Login Items & Extensions >
#   File System Extensions: enable "buildfs"
```

The module reaches `~/.buildfs` through a sandbox temporary-exception
entitlement. That may rule out the Mac App Store, and notarizing with it hasn't
been tried. Stores on other paths can't be used with FSKit. A store's size, and
the total for all stores, are also logged when it closes
(`log show --predicate 'subsystem == "com.pawlowskialex.buildfs.fsmodule"' | grep "closed store"`).

Mount with `mount -F -t buildfs -o owners,nobrowse buildfs://<store> <dir>` if
you do it by hand. **`owners` matters:** without it macOS marks the volume as
ignoring ownership, and `chown`/`chgrp` report success but change nothing.

See [FSKit notes](#fskit-notes) for its limits.

### NFS

Nothing to set up. `buildfs mount` runs the server in the background (log:
`~/Library/Logs/buildfs-<store>.log`) on the first free port from 20490. The
server exits when its mount goes away, so `buildfs unmount` or plain `umount`
stops it.

For debugging, `--foreground` keeps the server attached to the terminal, where it
unmounts on Ctrl-C or SIGTERM. So does `--store <dir>`, which keeps a store
anywhere you like. Those stores can't join the chunk pool, which has to be on the
same file system as the stores. `serve` runs only the server (mount it yourself
with `mount_nfs -o vers=4.1,tcp,port=20490,namedattr 127.0.0.1:/ <dir>`).

## Nix

The flake provides the CLI (`packages.aarch64-darwin.default`) and modules for
[nix-darwin](https://github.com/nix-darwin/nix-darwin)
(`darwinModules.default`) and
[home-manager](https://github.com/nix-community/home-manager)
(`homeModules.default`). They take the same options, so use whichever manages your
setup, but not both for the same store:

```nix
{
  inputs.buildfs.url = "github:pawlowskialex/buildfs";

  # Add buildfs.darwinModules.default or buildfs.homeModules.default to your modules, then:
  services.buildfs = {
    enable = true;
    mounts.default = {
      mountPoint = "/Users/me/mnt/build";
      backend = "fskit"; # or "nfs"
    };
    integrate.tools = [ "rust" "xcode" "go" "uv" ];
  };
}
```

Each key under `mounts` is a store name, kept running by a launchd agent. Also
available per mount: `port` (each NFS mount needs its own), `sync`, `level`,
`compressAfter` and `workers`.

`integrate` is the declarative version of `buildfs integrate`, and also takes `zig`,
`gradle` and `python`. Use `integrate.mount` if you have several mounts. Xcode is set
through its defaults, and the rest through environment variables, which override the
tools' config files, so your `~/.cargo/config.toml` and the like stay yours. Shells get
them from your configuration, and GUI apps through `launchctl setenv`, once restarted.
`buildfs integrate` shows these as `integrated through $VARIABLE`, and leaves them
alone.

- **`backend` is required.** The choice has to stay fixed once the agent is installed,
  and a Nix build can't tell whether the FSKit module is present.
- **The FSKit module isn't included**, because it has to be signed with your own
  developer team. Install it as described under [FSKit](#fskit).
- **To stop a mount**, remove it from `mounts`; `uninstall-agent` refuses agents managed
  this way. Unmount an NFS mount first: removing it stops its server, and the mount left
  behind hangs anything that touches it until `umount -f`.
- **To stop integrating a tool**, remove it from `integrate.tools`. New shells stop
  seeing the variable, but GUI apps keep it until you log out or run
  `launchctl unsetenv`.
- **With nix-darwin**, agents are installed for every user who logs in but use one
  user's home, so that module suits a single-user Mac. The home-manager module doesn't
  have this limit.

Both modules build against current nix-darwin and home-manager, and home-manager's
agents and variables have been run for real: mounts on both backends, and a cargo build
landing on the mount. Neither module has been through a real `darwin-rebuild switch` or
`home-manager switch`, so the agents' restart and removal, the `launchctl setenv` agent
and the Xcode setting haven't been seen in action.

## Options

| Option | Default | |
|---|---|---|
| `--compress-after <s>` | 15 | idle time before a file is compressed (0 = never) |
| `--level <n>` | 3 | zstd level (see Compression settings) |
| `--workers <n>` | cores/4 | background compression threads (utility QoS) |
| `--sync relaxed\|strict` | relaxed | see Durability |
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

With Nix, use the [`integrate` option](#nix) instead: this command won't write to a
config file that Nix manages.

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
when idle). All stores in `~/.buildfs/stores` share their compressed chunks
through a pool (see Design), which pays off when projects have the same
dependencies. For `node_modules` on FSKit:

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
stores (`scripts/bench/compare.sh`, medians of 3 rounds, load average 2–6):

| | APFS | FSKit | NFS |
|---|---|---|---|
| clean build | 9.9 s | 11.1 s | 20.5 s |
| incremental (the storage engine, then the CLI) | 0.9 s | 2.2 s | 7.0 s |
| no-op | 0.1 s | 0.3 s | 1.4 s |
| stat | 2.8 µs | 3.1 µs | 67 µs |
| create + write 4K + close | 77 µs | 1,031 µs | 14,427 µs |
| open + read + close | 24 µs | 154 µs | 437 µs |
| rename | 59 µs | 820 µs | 5,710 µs |
| hard link | 378 µs | 1,707 µs | 5,689 µs |
| unlink | 42 µs | 405 µs | 439 µs |

Space (the core is the same on both front ends):

| | APFS | buildfs |
|---|---|---|
| this workspace, one target dir | 881 MB | **249 MB** (3.53x, FSKit) |
| two target dirs, same deps | 1.1 GB | **184 MB** (6.1x: 3.06x zstd, 2x dedup; NFS, older build) |

All 3082 artifacts of the workspace build were byte- and metadata-identical
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
- **Shared pool.** Stores can't share a database, but they can share
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
  durable metadata commit. On FSKit, which never reports an `fsync`, every write
  does the same before it replies, since dirty data only arrives on `fsync`,
  close or kernel writeback. Both use `F_FULLFSYNC`. That costs about 19 ms per
  file written and closed on FSKit (1 ms in relaxed mode) and 30 ms on NFS; a
  clean `cargo build` took 28 s against 11 s in relaxed mode, and 29 s on NFS.

  Strict mode covers file data and the metadata committed with it. A change with
  no file data (create, rename, chmod, xattrs) waits for the next barrier, which
  is at most about a second away, in both front ends: renaming a file and then
  losing power can lose the rename. On FSKit, `fsync` itself never reaches
  buildfs, so it can't shorten that wait. Strict mode has been measured but not
  tested against a real power failure.
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
  it in `fskitd` and NSXPC. A hard
  link takes 12–13 such calls, 9 of them lookups, which is why build tools
  that hard-link their outputs (cargo) are its slowest case.
- **Crashes.** If the extension dies, the kernel unmounts the volume within
  about a second. Metadata recovers from the WAL at the next mount. In tests
  every closed file and every fsynced file survived a `kill -9`, even a kill
  microseconds after `close`; a file still open and never fsynced lost its
  data. The mountpoint is then an ordinary empty directory until something
  mounts it again, so a tool that keeps writing by path silently writes to
  your local disk. A process being started from the volume at that moment
  can hang until it is killed (seen once; Ctrl-C clears it).
- **Privacy permissions** (NFS mounts too). macOS treats these volumes as
  network volumes, and each app that uses one needs that permission. A terminal
  needs it, and so will Xcode or your editor. See
  [If a volume seems to hang](#if-a-volume-seems-to-hang). Jobs started by launchd
  can mount a volume, but reading or unmounting it fails with `EPERM` or blocks
  forever at a prompt no job can answer. That's why neither kind of agent reads
  its volume, and `uninstall-agent` unmounts from your shell. CI runners or other
  daemons started by launchd probably can't use the mount without being granted
  that permission (not tested).
- **No FIFOs, sockets or device nodes.** FSKit refuses them before the module
  is called: `mkfifo` and `bind` fail with `ENOTSUP`.
- **Fixed behaviour.** `noatime` is always on. `_PC_PATH_MAX` and
  `_PC_PIPE_BUF` return `EINVAL`, as on NFS.
- **`fsync` isn't visible to the module.** FSKit doesn't call `synchronize` on
  `fsync` or `F_FULLFSYNC`; dirty data just arrives as writes. So `--sync strict`
  makes every write durable before replying, whether or not `fsync` follows.
  See Durability.
- **Not tested yet:** the agent across a real reboot or logout, sleep and
  wake, and Xcode.app itself (only `xcodebuild`).

## If a volume seems to hang

Usually it's waiting for a privacy decision, not a problem with buildfs, on
either backend. The signs: `open`, `mkdir`, `link` and `ls` block indefinitely,
while `stat` stays instant and the buildfs server or module is idle, because the
call never reaches it. A blocked call also wedges `umount`, even `umount -f`, until
it ends.

1. **Look for a prompt on your screen**, possibly behind other windows, and answer
   it.
2. Run the same `ls` from Terminal. If that's instant, the permission of the app
   that started the blocked process is what's in question, and it's often an IDE or
   other GUI app, not the terminal you think of.
3. To confirm, kill a blocked command, then run
   `log show --last 2m --predicate 'process == "kernel" AND eventMessage CONTAINS "approval"'`.
   `approval_response_wait: sleep interrupted` means that the call was waiting
   for this.

This has stalled builds for hours under a GUI app, with an allow-or-deny prompt on
screen. Whether that prompt was the whole cause wasn't established: the stall
cleared at about the time another app that was making many privacy requests exited.

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
- After a server restart, the request that triggers the client's session
  recovery fails with EIO, though the server answers everything ("freeing free
  slot", SEQ_MISORDERED). buildfs triggers recovery itself with `statfs` on each
  mount before serving users, so this normally isn't seen; a request already in
  flight can still get it. Later requests and data are fine.
- Read delegations don't help: the client returns them almost immediately
  (about one DELEGRETURN per OPEN), which adds round trips. Most remaining
  RPCs are lookups of path components in busy directories, which only
  directory delegations could avoid.
- Unmounting from a process started by launchd hangs in the kernel or fails
  with EPERM, and so does reading the volume (see
  [If a volume seems to hang](#if-a-volume-seems-to-hang)). This is why the agent
  leaves its mount in place, reads its registry from the store, and
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
scripts/fskit-cli.sh              # CLI, both backends, attachments and agents, end to end
scripts/fskit-smoke.sh <mount>    # functional checks (FSKIT=1 on FSKit)
scripts/fskit-integrity.sh        # build, compress, remount, compare
scripts/fskit-crash.sh            # kill -9 mid-build, recover
scripts/bench/compare.sh          # APFS vs FSKit vs NFS, interleaved
scripts/bench/calls_per_op.py <mount>   # module calls per syscall
fskit/profile-fskitd.sh           # profile the whole call path (no sudo)
cargo run --release -p buildfs-core --example vfs_bench
cargo run --release -p buildfs-core --example compress_eval -- <dirs>
```

## License

MIT; see [LICENSE](LICENSE). The vendored embednfs crates are also MIT
([vendor/LICENSE-embednfs](vendor/LICENSE-embednfs)).
