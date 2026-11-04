# Vendored crates

| Crate | Upstream | Commit |
|---|---|---|
| embednfs, embednfs-proto | https://github.com/bloom-directory/embednfs (fork of https://github.com/PeronGH/embednfs) | 09660bef6e41385f73120efe0083bcd0351aa921 |

License: MIT (see upstream).

## Local modifications

Tracked in git history of this repo. Summary kept below.
- **Stable filehandles** (`StableHandles` trait): backends with persistent ids
  get stateless, restart-safe filehandles; no unbounded handle tables.
  PUTFH accepts the tagged format.
- **Special files**: `ObjectType`/`CreateKind` gain FIFO, socket, char and
  block devices; CREATE forwards NF4FIFO/NF4SOCK/NF4CHR/NF4BLK; rawdev exported.
- **Persistent server identity** (`NfsServerBuilder::server_identity`): stable
  `server_owner`/`server_scope` so clients see a restarted server as the same.
- **Non-atomic change_info**: before/after come from separate reads while
  other slots run concurrently, so `atomic` is now reported as false.
- **Stable readdir cookies** (`FsCapabilities::stable_readdir_cookies`): constant
  cookie verifier instead of NOT_SAME on every directory change. Without this,
  macOS sees truncated listings of busy directories (rustc "can't find crate").
