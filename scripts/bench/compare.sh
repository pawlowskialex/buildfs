#!/usr/bin/env bash
# Interleaved benchmark of APFS, the FSKit volume and the NFS server, all on
# fresh stores, with the load average printed for every run.
#
#   ROUNDS=3 KINDS="apfs fskit nfs" scripts/bench/compare.sh | tee /tmp/compare.txt
#   python3 -I scripts/bench/compare-summary.py /tmp/compare.txt
#
# SYNC=strict mounts both front ends with --sync strict.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ROUNDS="${ROUNDS:-3}"
KINDS="${KINDS:-apfs fskit nfs}"
W="${WORK:-/tmp/buildfs-compare}"
PORT="${PORT:-20599}"
SYNC="${SYNC:-relaxed}"
FSKIT_OPTS="owners,fresh"; [[ $SYNC == strict ]] && FSKIT_OPTS+=",strict"
mkdir -p "$W"
cargo build --release -q -p buildfs --manifest-path "$ROOT/Cargo.toml"

now() { python3 -I -c 'import time; print(time.time())'; }
since() { python3 -I -c "import sys, time; print('%.2f' % (time.time() - float(sys.argv[1])))" "$1"; }

setup() { # kind -> prints the directory to use
  local m="$W/mnt-$1"
  case $1 in
    apfs) rm -rf "$W/apfs"; mkdir -p "$W/apfs"; echo "$W/apfs" ;;
    fskit) mkdir -p "$m"; mount -F -t buildfs -o "$FSKIT_OPTS" buildfs://compare "$m"; echo "$m" ;;
    nfs)
      mkdir -p "$m"; rm -rf "$W/nfs-store"
      "$ROOT/target/release/buildfs" mount --store "$W/nfs-store" --port "$PORT" --sync "$SYNC" "$m" > "$W/nfs.log" 2>&1 &
      echo $! > "$W/nfs.pid"
      for _ in $(seq 100); do mount | grep -q " on $(cd "$m" && pwd -P) " && break; sleep 0.2; done
      echo "$m" ;;
  esac
}

teardown() {
  case $1 in
    fskit) /sbin/umount "$W/mnt-fskit" ;;
    nfs) kill -INT "$(cat "$W/nfs.pid")"; while kill -0 "$(cat "$W/nfs.pid")" 2>/dev/null; do sleep 0.2; done ;;
    *) ;;
  esac
}

run() {
  local kind=$1 round=$2 d
  d=$(setup "$kind")
  echo "== round=$round kind=$kind load=[$(sysctl -n vm.loadavg)]"
  python3 -I "$ROOT/scripts/bench/micro.py" "$d"
  python3 -I "$ROOT/scripts/bench/links.py" "$d"
  for p in 1 4 8; do python3 -I "$ROOT/scripts/bench/par_oc.py" "$d" "$p"; done
  local T="$d/target" s
  s=$(now); CARGO_TARGET_DIR="$T" cargo build -q -p buildfs --manifest-path "$ROOT/Cargo.toml"; echo "cargo clean: $(since "$s") s"
  for i in 1 2 3; do
    touch "$ROOT/crates/buildfs-core/src/vfs/mod.rs"
    s=$(now); CARGO_TARGET_DIR="$T" cargo build -q -p buildfs --manifest-path "$ROOT/Cargo.toml"; echo "cargo incremental: $(since "$s") s"
  done
  s=$(now); CARGO_TARGET_DIR="$T" cargo build -q -p buildfs --manifest-path "$ROOT/Cargo.toml"; echo "cargo noop: $(since "$s") s"
  teardown "$kind"
}

kinds=($KINDS)
for r in $(seq 1 "$ROUNDS"); do
  # Rotate the order each round so load drift hits every kind.
  for i in "${!kinds[@]}"; do run "${kinds[$(( (i + r - 1) % ${#kinds[@]} ))]}" "$r"; done
done
