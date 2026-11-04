#!/usr/bin/env bash
# A/B benchmark of the FSKit module's reply modes: "serial" (handlers run
# inline on FSKit's NSXPC queue) vs "concurrent" (`-o concurrent`, handlers
# and replies on a concurrent dispatch queue). Each run mounts a fresh
# volume; the order alternates per round so drift in machine load hits both
# modes. Assumes the extension is built and registered (see try.sh).
#
#   ROUNDS=3 CARGO_BENCH=1 fskit/ab.sh | tee /tmp/ab.txt
set -euo pipefail
cd "$(dirname "$0")/.."
MNT="${MNT:-/tmp/fskit-mnt}"
ROUNDS="${ROUNDS:-3}"
mkdir -p "$MNT"

now() { python3 -I -c 'import time; print(time.time())'; }
since() { python3 -I -c "import sys, time; print('%.2f' % (time.time() - float(sys.argv[1])))" "$1"; }

run() {
  local mode=$1 round=$2 opt=()
  [[ $mode == concurrent ]] && opt=(-o concurrent)
  mount -F -t buildfs "${opt[@]}" "buildfs://ab-$mode-$round" "$MNT"
  echo "== round=$round mode=$mode load=[$(sysctl -n vm.loadavg)]"
  for p in 1 4 8; do python3 -I scripts/bench/par_oc.py "$MNT" "$p"; done
  python3 -I scripts/bench/micro.py "$MNT"
  python3 -I scripts/bench/links.py "$MNT"
  if [[ "${CARGO_BENCH:-1}" == 1 ]]; then
    local T="$MNT/target" s
    s=$(now); CARGO_TARGET_DIR="$T" cargo build -q -p buildfs; echo "cargo clean: $(since "$s") s"
    for i in 1 2 3; do
      touch crates/buildfs/src/vfs/mod.rs
      s=$(now); CARGO_TARGET_DIR="$T" cargo build -q -p buildfs; echo "cargo incremental: $(since "$s") s"
    done
    s=$(now); CARGO_TARGET_DIR="$T" cargo build -q -p buildfs; echo "cargo noop: $(since "$s") s"
    "$T/debug/buildfs" --version >/dev/null && echo "binary runs: ok"
  fi
  /sbin/umount "$MNT"
}

for r in $(seq 1 "$ROUNDS"); do
  if (( r % 2 )); then run serial "$r"; run concurrent "$r"
  else run concurrent "$r"; run serial "$r"; fi
done
