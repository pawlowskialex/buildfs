#!/usr/bin/env bash
# Runs the pjdfstest POSIX suite on APFS and on a fresh buildfs mount and
# reports assertions that fail only on buildfs. Runs unprivileged, so
# root-only cases fail on both sides and cancel out.
#
# MOUNT_KIND=nfs (default) or fskit (the FSKit module must be installed
# and enabled; see fskit/try.sh). CLASSIFY=1 also attributes each
# regression to a cause (scripts/pjdfstest-classify.py).
#
# Known buildfs-only failures on FSKit (macOS 27), all FSKit limitations:
#   mkfifo/mknod/bind      FSKit doesn't pass FIFO/socket creation to modules
#                          (ENOTSUP), and every later assertion on the object
#                          fails with ENOENT
#   */03.t, rename/02.t    pathconf(_PC_PATH_MAX) is EINVAL, as on NFS
#
# Known buildfs-only failures on NFS (macOS NFS client limitations, no
# request reaches the server):
#   */03.t, rename/02.t  pathconf(_PC_PATH_MAX) is EINVAL on NFS mounts
#   open/17.t #2         the client refuses to open FIFOs (EACCES)
set -euo pipefail
KIND="${MOUNT_KIND:-nfs}"
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

if [[ ! -s "$WORK/apfs.txt" ]]; then
  rm -rf "$WORK/apfs" && mkdir -p "$WORK/apfs"
  (cd "$WORK/apfs" && prove -r "$TESTS" > "$WORK/apfs.txt" 2>&1) || true
fi

mkdir -p "$WORK/mnt"
if [[ $KIND == fskit ]]; then
  # owners: without it XNU silently ignores chown/chgrp on the volume.
  mount -F -t buildfs -o owners,fresh buildfs://pjdfstest "$WORK/mnt"
  trap '/sbin/umount "$WORK/mnt" 2>/dev/null || true' EXIT
else
  cargo build --release -q -p buildfs --manifest-path "$ROOT/Cargo.toml"
  rm -rf "$WORK/store"
  "$ROOT/target/release/buildfs" mount --store "$WORK/store" --port "$PORT" "$WORK/mnt" > "$WORK/server.log" 2>&1 &
  SERVER=$!
  trap 'kill -INT $SERVER 2>/dev/null; wait $SERVER 2>/dev/null || true' EXIT
  for _ in $(seq 50); do mount | grep -q " on $WORK/mnt " && break; sleep 0.2; done
fi
mkdir -p "$WORK/mnt/run"
(cd "$WORK/mnt/run" && prove -r "$TESTS" > "$WORK/buildfs-$KIND.txt" 2>&1) || true
python3 -I "$ROOT/scripts/pjdfstest-compare.py" "$WORK/apfs.txt" "$WORK/buildfs-$KIND.txt"
if [[ "${CLASSIFY:-0}" == 1 ]]; then
  # Rerun regressed files verbosely and attribute every failing assertion.
  V="$WORK/verbose-$KIND"; rm -rf "$V"; mkdir -p "$V"
  files=$(python3 -I "$ROOT/scripts/pjdfstest-compare.py" "$WORK/apfs.txt" "$WORK/buildfs-$KIND.txt" |
    awk '/^REGRESSIONS/{f=1;next} /^pass on/{f=0} f{print $1}')
  for f in $files; do (cd "$WORK/mnt/run" && prove -v "$TESTS/$f" > "$V/$(echo "$f" | tr / _).txt" 2>&1) || true; done
  python3 -I "$ROOT/scripts/pjdfstest-classify.py" "$WORK/apfs.txt" "$WORK/buildfs-$KIND.txt" "$V"
fi
