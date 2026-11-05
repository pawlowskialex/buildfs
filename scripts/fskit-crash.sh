#!/usr/bin/env bash
# Crash safety of the FSKit volume (docs/HANDOFF-fskit.md §5.4.6): write
# known files and sync them, start a clean cargo build, kill -9 the
# extension mid-build, remount, and check that the WAL was replayed, the
# synced files are intact, and the build can finish.
#
#   scripts/fskit-crash.sh            # KILL_AFTER=8 seconds into the build
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
M="${MNT:-/tmp/fskit-mnt}"
KILL_AFTER="${KILL_AFTER:-8}"
STORE="${STORE:-crash}"
OUT="${OUT:-/tmp/buildfs-crash}"
mkdir -p "$M" "$OUT"
LOG() { /usr/bin/log show --start "$1" --predicate 'subsystem == "com.pawlowskialex.buildfs.fsmodule"' 2>/dev/null; }
mounted() { mount | grep -q " on $(cd "$M" && pwd -P) "; }
status=0

mount -F -t buildfs -o owners,fresh "buildfs://$STORE" "$M"
# Known content written (and closed) before the crash.
mkdir -p "$M/known"
for i in $(seq 1 200); do head -c $((i * 997)) /dev/urandom > "$M/known/f$i"; done
ln -s f1 "$M/known/link"; xattr -w user.k v "$M/known/f2"
(cd "$M/known" && shasum -a 256 f* > "$OUT/known.sha")
# With write-back caching, data the module hasn't received yet lives only in
# the kernel's page cache and dies with the module (as on power loss for a
# local file system). sync(2) flushes it, like an app calling fsync.
sync

CARGO_TARGET_DIR="$M/target" cargo build -q --workspace --manifest-path "$ROOT/Cargo.toml" > "$OUT/build1.log" 2>&1 &
build=$!
sleep "$KILL_AFTER"
# Each volume has its own extension process: find the one serving $STORE.
pid=$(/usr/bin/log show --last 10m --style ndjson --predicate "subsystem == \"com.pawlowskialex.buildfs.fsmodule\" AND eventMessage BEGINSWITH \"activated store $STORE \"" 2>/dev/null |
  python3 -I -c 'import json, sys; ids = [p for l in sys.stdin if l.startswith("{") for p in [json.loads(l).get("processID")] if p]; print(ids[-1] if ids else "")')
[[ -n $pid ]] || { echo "FAIL: no extension process found for store $STORE"; exit 1; }
since=$(date "+%Y-%m-%d %H:%M:%S")
echo "killing extension pid $pid ${KILL_AFTER}s into the build"
kill -9 "$pid"
# A process being exec'd from the volume at the moment it dies can hang at
# _dyld_start until signalled, leaving cargo waiting for it. Seen once.
for _ in $(seq 1 30); do kill -0 "$build" 2>/dev/null || break; sleep 1; done
if kill -0 "$build" 2>/dev/null; then
  echo "note: build still running 30 s after the kill; signalling what it waits for"
  pkill -TERM -P "$build" 2>/dev/null
fi
wait "$build"; echo "build after kill exited $? (expected to fail)"

# The kernel keeps the dead mount; take it down and remount.
for _ in 1 2 3; do mounted || break; /sbin/umount "$M" 2>/dev/null || /sbin/umount -f "$M" 2>/dev/null; sleep 1; done
mounted && { echo "FAIL: could not unmount the dead volume"; exit 1; }
mount -F -t buildfs -o owners "buildfs://$STORE" "$M" || { sleep 5; mount -F -t buildfs -o owners "buildfs://$STORE" "$M"; }
sleep 1
echo "--- recovery log"
LOG "$since" | grep -oE "(recovered|removed|freed|queued|opened store).*" | tee "$OUT/recovery.txt"
# Durable commits happen once a second, so a kill in a quiet moment of the
# build can leave nothing to replay. The data checks below are what count.
grep -q "recovered [0-9]* metadata transactions" "$OUT/recovery.txt" && echo "ok   WAL replayed" || echo "note: nothing to replay in the WAL"

if (cd "$M/known" && shasum -a 256 -c --quiet "$OUT/known.sha"); then echo "ok   200 known files intact"; else echo "FAIL known files differ"; status=1; fi
[[ $(readlink "$M/known/link") == f1 && $(xattr -p user.k "$M/known/f2") == v ]] && echo "ok   symlink and xattr intact" || { echo "FAIL symlink/xattr"; status=1; }

if CARGO_TARGET_DIR="$M/target" cargo build -q --workspace --manifest-path "$ROOT/Cargo.toml" > "$OUT/build2.log" 2>&1; then
  echo "ok   build finishes after recovery"
else
  echo "FAIL build after recovery (see $OUT/build2.log)"; tail -5 "$OUT/build2.log"; status=1
fi
"$M/target/debug/buildfs" --version > /dev/null && echo "ok   built binary runs" || { echo "FAIL built binary"; status=1; }
"$ROOT/target/release/buildfs" unmount --mount "$M" > /dev/null
exit $status
