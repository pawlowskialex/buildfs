#!/usr/bin/env bash
# Runs the pjdfstest POSIX suite on APFS and on a fresh buildfs mount and
# reports assertions that fail only on buildfs. Runs unprivileged, so
# root-only cases fail on both sides and cancel out.
#
# Known buildfs-only failures (macOS NFS client limitations, no request
# reaches the server):
#   */03.t, rename/02.t  pathconf(_PC_PATH_MAX) is EINVAL on NFS mounts
#   open/17.t #2         the client refuses to open FIFOs (EACCES)
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${WORK:-/tmp/buildfs-pjdfstest}"
PORT="${PORT:-20499}"
mkdir -p "$WORK"
if [[ ! -x "$WORK/pjdfstest/pjdfstest" ]]; then
  git clone -q --depth 1 https://github.com/pjd/pjdfstest "$WORK/pjdfstest"
  cat > "$WORK/pjdfstest/config.h" <<'H'
#define HAVE_OPENAT 1
#define HAVE_UNLINKAT 1
#define HAVE_MKDIRAT 1
#define HAVE_LINKAT 1
#define HAVE_SYMLINKAT 1
#define HAVE_RENAMEAT 1
#define HAVE_MKFIFOAT 1
#define HAVE_MKNODAT 1
#define HAVE_FCHMODAT 1
#define HAVE_FCHOWNAT 1
#define HAVE_FSTATAT 1
#define HAVE_CHFLAGS 1
#define HAVE_FCHFLAGS 1
#define HAVE_LCHFLAGS 1
#define HAVE_LCHMOD 1
#define HAVE_UTIMENSAT 1
#define HAVE_STRUCT_STAT_ST_ATIMESPEC 1
#define HAVE_STRUCT_STAT_ST_MTIMESPEC 1
#define HAVE_STRUCT_STAT_ST_CTIMESPEC 1
#define HAVE_STRUCT_STAT_ST_BIRTHTIMESPEC 1
H
  cc -O2 -Wno-deprecated-declarations -o "$WORK/pjdfstest/pjdfstest" "$WORK/pjdfstest/pjdfstest.c"
fi
TESTS="$WORK/pjdfstest/tests"
cargo build --release -q -p buildfs --manifest-path "$ROOT/Cargo.toml"

if [[ ! -s "$WORK/apfs.txt" ]]; then
  rm -rf "$WORK/apfs" && mkdir -p "$WORK/apfs"
  (cd "$WORK/apfs" && prove -r "$TESTS" > "$WORK/apfs.txt" 2>&1) || true
fi

rm -rf "$WORK/store" && mkdir -p "$WORK/mnt"
"$ROOT/target/release/buildfs" mount --store "$WORK/store" --port "$PORT" "$WORK/mnt" > "$WORK/server.log" 2>&1 &
SERVER=$!
trap 'kill -INT $SERVER 2>/dev/null; wait $SERVER 2>/dev/null || true' EXIT
for _ in $(seq 50); do mount | grep -q " on $WORK/mnt " && break; sleep 0.2; done
mkdir -p "$WORK/mnt/run"
(cd "$WORK/mnt/run" && prove -r "$TESTS" > "$WORK/buildfs.txt" 2>&1) || true
python3 -I "$ROOT/scripts/pjdfstest-compare.py" "$WORK/apfs.txt" "$WORK/buildfs.txt"
