#!/usr/bin/env bash
# Profiles the whole FSKit call path (kernel, fskitd, our extension) under
# two workloads. xctrace's Time Profiler samples all processes, root daemons
# such as fskitd included, without sudo.
#
#   fskit/profile-fskitd.sh            # results in /tmp/fskitd-prof
#   python3 -I scripts/bench/xtprof.py /tmp/fskitd-prof/par8.xml fskitd BuildFSExt kernel
#
# Workloads (fresh in-memory mount, inline replies):
#   par1: open+close loop, 1 process   (latency path)
#   par8: open+close loop, 8 processes (throughput ceiling, ~41k module calls/s)
# Per workload: xctrace Time Profiler over all processes (exported to XML),
# calls/s during the recording, and per-process CPU from top.
set -euo pipefail
cd "$(dirname "$0")/.."
OUT=/tmp/fskitd-prof
MNT="${MNT:-/tmp/fskit-mnt}"
SECS="${SECS:-8}"
rm -rf "$OUT"; mkdir -p "$OUT" "$MNT"

mount -F -t buildfs buildfs://prof "$MNT"
cleanup() {
  touch "$OUT/stop"; wait 2>/dev/null || true
  /sbin/umount "$MNT" 2>/dev/null || true
}
trap cleanup EXIT

FSKITD=$(pgrep -x fskitd)
uptime > "$OUT/load.txt"
sw_vers >> "$OUT/load.txt"

for w in 1 8; do
  name=par$w
  rm -f "$OUT/stop"
  # Workload: repeat par_oc until told to stop; log calls/s per iteration.
  ( while [[ ! -e "$OUT/stop" ]]; do python3 -I scripts/bench/par_oc.py "$MNT" "$w"; done ) > "$OUT/$name-calls.txt" 2>&1 &
  wl=$!
  sleep 2
  EXT=$(pgrep -f BuildFSExtension.appex | head -1)
  echo "== $name fskitd=$FSKITD ext=$EXT"

  top -l 3 -s 2 -stats pid,command,cpu,threads 2>&1 | grep -E "^(PID|$FSKITD |$EXT |0 )|^Processes|^CPU usage" > "$OUT/$name-top.txt" || true

  echo "  xctrace Time Profiler (${SECS}s, all processes)"
  xcrun xctrace record --template 'Time Profiler' --all-processes \
      --time-limit "${SECS}s" --output "$OUT/$name.trace" --no-prompt > "$OUT/$name-xctrace.log" 2>&1 || echo "  xctrace failed (see $name-xctrace.log)"


  touch "$OUT/stop"; wait "$wl" || true
done

for w in 1 8; do
  [[ -d "$OUT/par$w.trace" ]] || continue
  xcrun xctrace export --input "$OUT/par$w.trace" \
    --xpath '/trace-toc/run[@number="1"]/data/table[@schema="time-profile"]' > "$OUT/par$w.xml"
done
echo "done: results in $OUT"
/bin/ls -la "$OUT"
