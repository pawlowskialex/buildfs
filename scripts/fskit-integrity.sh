#!/usr/bin/env bash
# Integrity check of the FSKit volume:
# clean build of this workspace on a fresh store, hash every file, let the
# background compactor run, unmount, remount, and compare hashes and
# metadata. Also builds natively on APFS to compare sizes.
#
#   scripts/fskit-integrity.sh            # WAIT=60 seconds for compaction
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
M="${MNT:-/tmp/fskit-mnt}"
NATIVE="${NATIVE:-/tmp/buildfs-native-target}"
OUT="${OUT:-/tmp/buildfs-integrity}"
WAIT="${WAIT:-60}"
STORE="${STORE:-integrity}"
SUB="$(id -G | tr ' ' '\n' | grep -vx "$(id -g)" | head -1)" # a supplementary group
mkdir -p "$M" "$OUT"
LOG() { /usr/bin/log show --start "$1" --predicate 'subsystem == "com.pawlowskialex.buildfs.fsmodule"' 2>/dev/null; }

manifest() { # path, output: sha256 + size + mode + mtime + nlink + symlink target
  (cd "$1" && find . \( -type f -o -type l \) -print0 | sort -z |
    xargs -0 /usr/bin/stat -f '%N %z %Lp %m %l %Y' > "$2.meta" &&
    find . -type f -print0 | sort -z | xargs -0 shasum -a 256 > "$2.sha")
}

echo "load: $(sysctl -n vm.loadavg)"
mount -F -t buildfs -o owners,fresh "buildfs://$STORE" "$M"
t=$(date +%s)
CARGO_TARGET_DIR="$M/target" cargo build -q --workspace --manifest-path "$ROOT/Cargo.toml"
echo "clean build on FSKit: $(( $(date +%s) - t )) s"
# A few metadata cases the build doesn't produce.
mkdir -p "$M/target/extra" && printf x > "$M/target/extra/f" && chmod 640 "$M/target/extra/f" &&
  xattr -w user.k v "$M/target/extra/f" && ln -sf f "$M/target/extra/l" && ln -f "$M/target/extra/f" "$M/target/extra/h" &&
  { [[ -n $SUB ]] && chgrp "$SUB" "$M/target/extra/f"; true; }
manifest "$M/target" "$OUT/before"
xattr -l "$M/target/extra/f" > "$OUT/before.xattr"
echo "files: $(wc -l < "$OUT/before.sha"), waiting ${WAIT}s for compaction"
sleep "$WAIT"
since=$(date "+%Y-%m-%d %H:%M:%S")
/sbin/umount "$M"
sleep 1
LOG "$since" | grep -oE "closed store $STORE.*" | tail -1 | tee "$OUT/close.txt"

mount -F -t buildfs -o owners "buildfs://$STORE" "$M"
manifest "$M/target" "$OUT/after"
xattr -l "$M/target/extra/f" > "$OUT/after.xattr"
since2=$(date "+%Y-%m-%d %H:%M:%S")
/sbin/umount "$M"
sleep 1
status=0
for k in sha meta xattr; do
  if cmp -s "$OUT/before.$k" "$OUT/after.$k"; then echo "after remount: $k identical"
  else echo "after remount: $k DIFFERS"; diff "$OUT/before.$k" "$OUT/after.$k" | head -20; status=1; fi
done
LOG "$since2" | grep -oE "closed store $STORE.*" | tail -1

rm -rf "$NATIVE"
t=$(date +%s)
CARGO_TARGET_DIR="$NATIVE" cargo build -q --workspace --manifest-path "$ROOT/Cargo.toml"
echo "clean build on APFS: $(( $(date +%s) - t )) s"
native=$(( $(/usr/bin/du -sk "$NATIVE" | cut -f1) * 1024 ))
store=$(grep -oE "[0-9]+ bytes on disk" "$OUT/close.txt" | grep -oE "^[0-9]+")
echo "native target: $native bytes; buildfs store: $store bytes; ratio $(python3 -I -c "print(f'{$native/$store:.2f}x')")"
exit $status
