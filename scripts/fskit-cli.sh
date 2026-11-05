#!/usr/bin/env bash
# End-to-end test of the FSKit parts of the CLI: mount, unmount, integrate,
# attach, detach, and the launchd agent. Uses throwaway stores, directories
# under /tmp, a temporary CARGO_HOME, and a temporary agent.
#
#   cargo build --release -p buildfs && scripts/fskit-cli.sh
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
B="$ROOT/target/release/buildfs"
T="$(cd /tmp && pwd -P)/buildfs-cli.$$"
M="$T/mnt"
STORE="clitest"
fails=0
ok() { printf 'ok   %s\n' "$1"; }
bad() { printf 'FAIL %s%s\n' "$1" "${2:+: $2}"; fails=$((fails + 1)); }
check() { local name=$1; shift; if out=$("$@" 2>&1); then ok "$name"; else bad "$name" "$out"; fi; }
mounted() { /sbin/mount | grep -q " on $1 ("; }
export -f mounted
wait_for() { # seconds, command...
  local n=$1; shift
  for _ in $(seq 1 "$n"); do "$@" && return 0; python3 -I -c 'import time; time.sleep(1)'; done
  return 1
}
mkdir -p "$T"
cleanup() {
  "$B" uninstall-agent --name "$STORE-agent" > /dev/null 2>&1
  for d in "$T"/*/; do mounted "${d%/}" && /sbin/umount "${d%/}"; done
  rm -rf "$T"
}
trap cleanup EXIT

# Mount
check "mount --fskit" "$B" mount --fskit --name "$STORE" -o fresh "$M"
check "volume is mounted with owners,nobrowse" bash -c "/sbin/mount | grep ' on $M (' | grep nobrowse | grep -qv noowners"
check "mount again is a no-op" "$B" mount --fskit --name "$STORE" "$M"
check "other store on the same mountpoint is refused" bash -c "! '$B' mount --fskit --name other '$M' 2>/dev/null"
check "reserved store name is refused" bash -c "! '$B' mount --fskit --name att-x '$T/other' 2>/dev/null"
check "bad store name is refused" bash -c "! '$B' mount --fskit --name 'A_b' '$T/other' 2>/dev/null"
check "--sync strict is refused" bash -c "! '$B' mount --fskit --sync strict '$T/other' 2>/dev/null"
check "files work" bash -c "echo hi > '$M/f' && [[ \$(cat '$M/f') == hi ]]"
check "chgrp works" bash -c "g=\$(id -G | tr ' ' '\n' | grep -vx \$(id -g) | head -1); chgrp \$g '$M/f' && [[ \$(/usr/bin/stat -f %g '$M/f') == \$g ]]"

# Integrate, against a temporary CARGO_HOME
export CARGO_HOME_REAL="${CARGO_HOME:-$HOME/.cargo}"
check "integrate shows the mount" bash -c "'$B' integrate | grep -q 'buildfs mount: $M'"
check "integrate rust" bash -c "CARGO_HOME='$T/cargo' '$B' integrate rust && grep -q 'build-dir = \"$M/cargo/' '$T/cargo/config.toml'"
check "integrate --revert" bash -c "CARGO_HOME='$T/cargo' '$B' integrate rust --revert && ! grep -q build-dir '$T/cargo/config.toml'"

# Attach: empty, then with contents
P="$T/proj"; mkdir -p "$P/empty" "$P/full/sub"
printf payload > "$P/full/sub/a"; ln "$P/full/sub/a" "$P/full/sub/hard"; ln -s a "$P/full/sub/link"
xattr -w user.k v "$P/full/sub/a"; chmod 640 "$P/full/sub/a"
check "attach an empty dir" "$B" attach "$P/empty"
check "it is a mount" mounted "$P/empty"
check "writes work" bash -c "echo x > '$P/empty/x' && [[ \$(cat '$P/empty/x') == x ]]"
check "attach a non-empty dir without --move is refused" bash -c "! '$B' attach '$P/full' 2>/dev/null && [[ -f '$P/full/sub/a' ]] && ! /sbin/mount | grep -q ' on $P/full ('"
check "attach --move" "$B" attach --move "$P/full"
check "moved contents intact" bash -c "cd '$P/full/sub' && [[ \$(cat a) == payload && \$(/usr/bin/stat -f %l a) == 2 && \$(readlink link) == a && \$(xattr -p user.k a) == v && \$(/usr/bin/stat -f %Lp a) == 640 ]]"
check "nothing left aside" bash -c "[[ ! -e '$P/full.buildfs-moving' ]]"
check "attachments lists both" bash -c "[[ \$('$B' attachments | grep -c '^mounted') == 2 ]]"
check "attachments aren't roots" bash -c "'$B' attachments > /dev/null"

# Unmount and restore
check "unmount takes attachments along" bash -c "'$B' unmount --mount '$M' && ! /sbin/mount | grep -qE ' on ($M|$P/empty|$P/full) \('"
check "mount restores attachments" bash -c "'$B' mount --fskit --name '$STORE' '$M' && /sbin/mount | grep -q ' on $P/empty (' && /sbin/mount | grep -q ' on $P/full ('"
check "data survived" bash -c "[[ \$(cat '$P/empty/x') == x && \$(cat '$M/f') == hi && \$(xattr -p user.k '$P/full/sub/a') == v ]]"

# Shared chunk pool: attachments with the same content share chunk files
mkdir -p "$P/same1" "$P/same2"
"$B" attach "$P/same1" "$P/same2" > /dev/null
python3 -I -c "
import random, sys
random.seed(7)
words = [random.randbytes(random.randint(3, 12)) for _ in range(4000)]
sys.stdout.buffer.write(b' '.join(random.choice(words) for _ in range(600000)))" > "$T/payload"
for d in same1 same2; do cp "$T/payload" "$P/$d/data"; done
python3 -I -c 'import time; time.sleep(24)'   # compress-after is 15 s
since=$(date "+%Y-%m-%d %H:%M:%S")
"$B" unmount --mount "$M" > /dev/null
python3 -I -c 'import time; time.sleep(1)'
pooled=$(/usr/bin/log show --start "$since" --predicate 'subsystem == "com.pawlowskialex.buildfs.fsmodule" AND eventMessage BEGINSWITH "closed store att-proj-same"' 2>/dev/null |
  grep -oE "[0-9]+ from the shared pool" | cut -d' ' -f1 | sort -n | tail -1)
if [[ ${pooled:-0} -gt 0 ]]; then ok "one attachment took $pooled chunks from the pool"; else bad "attachments share chunks" "pooled=${pooled:-none}"; fi
"$B" mount --fskit --name "$STORE" "$M" > /dev/null
check "shared content reads back in both" bash -c "cmp '$T/payload' '$P/same1/data' && cmp '$T/payload' '$P/same2/data'"
check "detaching one leaves the other intact" bash -c "'$B' detach '$P/same1' > /dev/null && cmp '$T/payload' '$P/same2/data'"
check "detach the other" bash -c "'$B' detach '$P/same2' > /dev/null"

# Detach
check "detach --keep" "$B" detach --keep "$P/full"
check "kept contents are local" bash -c "! /sbin/mount | grep -q ' on $P/full (' && cd '$P/full/sub' && [[ \$(cat a) == payload && \$(/usr/bin/stat -f %l a) == 2 && \$(xattr -p user.k a) == v ]]"
check "detach" "$B" detach "$P/empty"
check "detached dir is empty and local" bash -c "! /sbin/mount | grep -q ' on $P/empty (' && [[ -d '$P/empty' && -z \$(/bin/ls -A '$P/empty') ]]"
check "no attachments left" bash -c "'$B' attachments | grep -q 'no attachments'"
check "re-attaching starts empty (store was discarded)" bash -c "'$B' attach '$P/empty' && [[ -z \$(/bin/ls -A '$P/empty') ]] && '$B' detach '$P/empty'"
check "unmount" "$B" unmount --mount "$M"

# Agent: mounts from launchd, restores attachments, recovers from a crash
A="$T/agent"; AS="$STORE-agent"; AP="$T/agentproj"; mkdir -p "$AP/dir"
check "install-agent --fskit" "$B" install-agent --fskit --name "$AS" "$A"
if wait_for 20 mounted "$A"; then ok "agent mounted the volume"; else bad "agent mounted the volume"; fi
check "the volume works for this shell" bash -c "echo agent > '$A/f' && [[ \$(cat '$A/f') == agent ]]"
check "attach under the agent's root" "$B" attach --mount "$A" "$AP/dir"
pid=$(/usr/bin/log show --last 5m --style ndjson --predicate "subsystem == \"com.pawlowskialex.buildfs.fsmodule\" AND eventMessage BEGINSWITH \"activated store $AS \"" 2>/dev/null |
  python3 -I -c 'import json, sys; ids = [p for l in sys.stdin if l.startswith("{") for p in [json.loads(l).get("processID")] if p]; print(ids[-1] if ids else "")')
sync
if [[ -n $pid ]] && kill -9 "$pid"; then ok "killed the extension (pid $pid)"; else bad "killed the extension"; fi
python3 -I -c 'import time; time.sleep(2)'
if ! mounted "$A"; then ok "the kernel unmounted the volume"; else bad "the kernel unmounted the volume"; fi
if wait_for 45 mounted "$A"; then ok "agent mounted it again"; else bad "agent mounted it again" "$(tail -3 "$HOME/Library/Logs/buildfs-$AS.log")"; fi
check "data survived the crash" bash -c "[[ \$(cat '$A/f') == agent ]]"
/sbin/umount "$AP/dir"
if wait_for 45 mounted "$AP/dir"; then ok "agent restores a missing attachment"; else bad "agent restores a missing attachment" "$(tail -3 "$HOME/Library/Logs/buildfs-$AS.log")"; fi
check "uninstall-agent unmounts everything" bash -c "'$B' uninstall-agent --name '$AS' && ! /sbin/mount | grep -qE ' on ($A|$AP/dir) \('"
python3 -I -c 'import time; time.sleep(35)'
if ! mounted "$A"; then ok "nothing mounts it again"; else bad "nothing mounts it again"; fi
check "agent is gone" bash -c "[[ ! -e '$HOME/Library/LaunchAgents/dev.buildfs.$AS.plist' ]]"

# delete-store: refused while mounted or owned by an agent, then really gone
"$B" mount --fskit --name "$STORE" "$M" > /dev/null
check "delete-store refuses a mounted store" bash -c "! '$B' delete-store '$STORE' 2>/dev/null && [[ \$(cat '$M/f') == hi ]]"
"$B" unmount --mount "$M" > /dev/null
check "delete-store" "$B" delete-store "$STORE"
"$B" mount --fskit --name "$STORE" "$M" > /dev/null
check "the store is empty afterwards" bash -c "[[ -z \$(/bin/ls -A '$M') ]]"
"$B" unmount --mount "$M" > /dev/null

# Throwaway stores and registry
"$B" delete-store "$STORE" > /dev/null
"$B" delete-store "$AS" > /dev/null
"$B" delete-store "$(python3 -I -c "import json,os; print(json.load(open(os.path.expanduser('~/Library/Application Support/buildfs/$AS-attachments.json')))[0]['export'])")" > /dev/null
rm -f "$HOME/Library/Logs/buildfs-$AS.log" "$HOME/Library/Application Support/buildfs/$STORE-attachments.json" "$HOME/Library/Application Support/buildfs/$AS-attachments.json"
echo "failures: $fails"
exit $((fails > 0))
