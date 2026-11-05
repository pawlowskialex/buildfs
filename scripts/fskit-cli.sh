#!/usr/bin/env bash
# End-to-end test of the CLI: choosing a backend, mount, unmount, integrate,
# attach, detach, and the launchd agent. Uses throwaway stores, directories
# under /tmp, a temporary CARGO_HOME, and temporary agents. The FSKit module
# must be installed and enabled.
#
# Not covered, because it means switching the module off by hand: a disabled
# module (mount says how to turn it on, and doesn't use NFS instead). Other
# Macs are simulated: BUILDFS_TEST_MACOS_VERSION, BUILDFS_TEST_FSKIT_MODULE.
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
NS="$STORE-nfs"; NSTORE="$HOME/.buildfs/stores/$NS"
cleanup() {
  "$B" uninstall-agent --name "$STORE-agent" > /dev/null 2>&1
  "$B" uninstall-agent --name "$STORE-auto" > /dev/null 2>&1
  "$B" uninstall-agent --name "$NS" > /dev/null 2>&1
  for d in "$T"/*/ "$T"/*/*/; do mounted "${d%/}" && /sbin/umount "${d%/}"; done
  pkill -f -- "--store $NSTORE" 2> /dev/null
  rm -rf "$T" "$NSTORE" "$NSTORE-b" "$NSTORE-p1" "$NSTORE-p2"
}
trap cleanup EXIT

# Mount
check "mount --fskit" "$B" mount --fskit --name "$STORE" -o fresh "$M"
check "volume is mounted with owners,nobrowse" bash -c "/sbin/mount | grep ' on $M (' | grep nobrowse | grep -qv noowners"
check "mount again is a no-op" "$B" mount --fskit --name "$STORE" "$M"
check "other store on the same mountpoint is refused" bash -c "! '$B' mount --fskit --name other '$M' 2>/dev/null"
check "reserved store name is refused" bash -c "! '$B' mount --fskit --name att-x '$T/other' 2>/dev/null"
check "bad store name is refused" bash -c "! '$B' mount --fskit --name 'A_b' '$T/other' 2>/dev/null"
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

# Choosing a backend: what this Mac gets
N="$T/nfs"
from() { /sbin/mount | sed -nE "s|^(.*) on $1 \\(.*|\\1|p"; }
export -f from
nfs_server() { pgrep -f -- "--store $NSTORE" > /dev/null; }
export -f nfs_server
check "mount without flags uses FSKit here" bash -c "'$B' mount --name '$STORE' '$M' && [[ \$(from '$M') == buildfs://$STORE ]]"
check "and repeating it is a no-op" bash -c "'$B' mount --name '$STORE' '$M' && [[ \$(from '$M') == buildfs://$STORE ]]"
check "--foreground is refused with FSKit" bash -c "! '$B' mount --foreground --backend fskit --name '$STORE' '$T/other' 2>/dev/null"
"$B" unmount --mount "$M" > /dev/null

# NFS, chosen by flag: the same commands as FSKit
check "--backend nfs returns once mounted" bash -c "'$B' mount --backend nfs --name '$NS' '$N' && [[ \$(from '$N') == 127.0.0.1:/ ]]"
check "the store is in the default place" test -f "$NSTORE/meta.redb"
check "the server is running, detached" nfs_server
check "NFS files work" bash -c "echo nfs > '$N/f' && [[ \$(cat '$N/f') == nfs ]]"
check "NFS mount again is a no-op" bash -c "'$B' mount --backend nfs --name '$NS' '$N' && [[ \$(pgrep -f -- '--store $NSTORE' | wc -l) -eq 1 ]]"
check "without flags, a mount stays on its backend" bash -c "'$B' mount --name '$NS' '$N' && [[ \$(from '$N') == 127.0.0.1:/ && \$(pgrep -f -- '--store $NSTORE' | wc -l) -eq 1 ]]"
check "another store on an NFS mountpoint is refused" bash -c "! '$B' mount --backend nfs --name '$NS-b' '$N' 2>/dev/null && [[ ! -e '$NSTORE-b' ]]"
check "FSKit on an NFS mountpoint is refused" bash -c "! '$B' mount --backend fskit --name '$STORE' '$N' 2>/dev/null"
check "a second NFS mount gets its own port" bash -c "'$B' mount --backend nfs --name '$NS-b' '$T/nfs2' && [[ \$(from '$T/nfs2') == 127.0.0.1:/ ]] && '$B' unmount --mount '$T/nfs2' > /dev/null && rm -rf '$NSTORE-b'"
check "delete-store refuses a store in use" bash -c "! '$B' delete-store '$NS' 2>/dev/null && [[ \$(cat '$N/f') == nfs ]]"
NP="$T/nfsproj"; mkdir -p "$NP/dir"
check "attach on NFS" bash -c "'$B' attach --mount '$N' '$NP/dir' && echo att > '$NP/dir/a'"
check "integrate shows the NFS mount" bash -c "'$B' integrate --mount '$N' | grep -q 'buildfs mount: $N'"
check "unmount takes NFS attachments and the server along" bash -c "'$B' unmount --mount '$N' && ! /sbin/mount | grep -qE ' on ($N|$NP/dir) \\(' && ! nfs_server"
check "mounting again restores data and attachments" bash -c "'$B' mount --backend nfs --name '$NS' '$N' && [[ \$(cat '$N/f') == nfs && \$(cat '$NP/dir/a') == att ]]"
check "plain umount also stops the server" bash -c "/sbin/umount '$NP/dir' && /sbin/umount '$N' && for i in {1..100}; do nfs_server || exit 0; python3 -I -c 'import time; time.sleep(0.1)'; done; exit 1"
"$B" mount --backend nfs --name "$NS" "$N" > /dev/null
pkill -9 -f -- "--store $NSTORE"
check "mounting recovers from a killed server" bash -c "'$B' mount --backend nfs --name '$NS' '$N' && [[ \$(cat '$N/f') == nfs && \$(cat '$NP/dir/a') == att ]]"
check "SIGTERM unmounts" bash -c "pkill -f -- '--store $NSTORE'; for i in {1..100}; do /sbin/mount | grep -qE ' on ($N|$NP/dir) \\(' || exit 0; python3 -I -c 'import time; time.sleep(0.1)'; done; exit 1"

# Other Macs, simulated
check "macOS 26 uses NFS, and says why" bash -c "BUILDFS_TEST_MACOS_VERSION=26.4 '$B' mount --name '$NS' '$N' 2>&1 | grep -q 'FSKit needs macOS 27' && [[ \$(from '$N') == 127.0.0.1:/ ]]"
"$B" unmount --mount "$N" > /dev/null
check "no module uses NFS, and says why" bash -c "BUILDFS_TEST_FSKIT_MODULE=missing '$B' mount --name '$NS' '$N' 2>&1 | grep -q \"module isn't installed\" && [[ \$(from '$N') == 127.0.0.1:/ ]]"
"$B" unmount --mount "$N" > /dev/null
check "--backend fskit is honored on macOS 26" bash -c "BUILDFS_TEST_MACOS_VERSION=26.4 '$B' mount --backend fskit --name '$STORE' '$M' && [[ \$(from '$M') == buildfs://$STORE ]] && '$B' unmount --mount '$M' > /dev/null"

# One store, both backends
check "a store written over NFS opens on FSKit" bash -c "'$B' mount --backend fskit --name '$NS' '$T/up' && [[ \$(from '$T/up') == buildfs://$NS && \$(cat '$T/up/f') == nfs ]]"
check "the other backend is refused while it's open" bash -c "'$B' mount --backend nfs --name '$NS' '$N' 2>&1 | grep -q 'in use by another mount' && ! /sbin/mount | grep -q ' on $N ('"
check "and writing on FSKit shows up on NFS" bash -c "echo fskit > '$T/up/g' && xattr -w user.x y '$T/up/g' && '$B' unmount --mount '$T/up' > /dev/null && '$B' mount --backend nfs --name '$NS' '$N' && [[ \$(cat '$N/g') == fskit && \$(xattr -p user.x '$N/g') == y ]] && '$B' unmount --mount '$N' > /dev/null"
check "du can see the store" bash -c "[[ \$(/usr/bin/du -sk '$NSTORE' | cut -f1) -gt 0 ]]"

# The existing NFS form: foreground, explicit store
"$B" mount --store "$NSTORE" --port 20555 "$N" > "$T/fg.log" 2>&1 &
fg=$!
if wait_for 15 mounted "$N"; then ok "--store stays in the foreground, mounted"; else bad "--store stays in the foreground, mounted" "$(tail -3 "$T/fg.log")"; fi
if kill -0 $fg 2> /dev/null; then ok "the foreground server is still running"; else bad "the foreground server is still running"; fi
check "unmount ends a foreground server" bash -c "'$B' unmount --mount '$N' && for i in {1..100}; do kill -0 $fg 2>/dev/null || exit 0; python3 -I -c 'import time; time.sleep(0.1)'; done; exit 1"
wait $fg; rc=$?
if [[ $rc == 0 ]]; then ok "and it exits cleanly"; else bad "and it exits cleanly" "status $rc"; fi

# Agents: the backend is chosen once, at install
AU="$STORE-auto"; AUM="$T/agentauto"
check "install-agent without flags uses FSKit here" bash -c "'$B' install-agent --name '$AU' '$AUM' && grep -q -- '<string>--fskit</string>' '$HOME/Library/LaunchAgents/dev.buildfs.$AU.plist'"
if wait_for 45 mounted "$AUM"; then ok "that agent mounted the volume"; else bad "that agent mounted the volume"; fi
check "uninstall it" "$B" uninstall-agent --name "$AU"
NA="$T/agentnfs"
check "install-agent on macOS 26 uses NFS" bash -c "BUILDFS_TEST_MACOS_VERSION=26.4 '$B' install-agent --name '$NS' '$NA' 2>&1 | grep -q 'FSKit needs macOS 27'"
check "its plist records the store and port" bash -c "grep -q '<string>$NSTORE</string>' '$HOME/Library/LaunchAgents/dev.buildfs.$NS.plist' && grep -q '<string>--port</string>' '$HOME/Library/LaunchAgents/dev.buildfs.$NS.plist'"
if wait_for 30 mounted "$NA"; then ok "launchd keeps it on NFS on macOS 27 (as after an upgrade)"; else bad "launchd keeps it on NFS on macOS 27 (as after an upgrade)" "$(tail -3 "$HOME/Library/Logs/buildfs-$NS.log")"; fi
check "still NFS" bash -c "[[ \$(from '$NA') == 127.0.0.1:/ && \$(cat '$NA/f') == nfs ]]"
if wait_for 15 mounted "$NP/dir"; then ok "the NFS agent restores attachments under launchd"; else bad "the NFS agent restores attachments under launchd" "$(tail -3 "$HOME/Library/Logs/buildfs-$NS.log")"; fi
check "and reaches its main loop" bash -c "grep -q 'mounted 127.0.0.1:/ at $NA' '$HOME/Library/Logs/buildfs-$NS.log'"
served() { grep -c 'serving existing mount' "$HOME/Library/Logs/buildfs-$NS.log"; }
before=$(served)
pkill -9 -f -- "--keep-mounted --store $NSTORE"
restarted() { [[ $(served) -gt $before ]]; }
if wait_for 30 restarted; then ok "launchd restarts a killed NFS agent, which adopts the mount"; else bad "launchd restarts a killed NFS agent, which adopts the mount"; fi
check "reads work at once under launchd, on the root" bash -c "[[ \$(cat '$NA/f') == nfs ]]"
check "and on the attachment" bash -c "[[ \$(cat '$NP/dir/a') == att ]]"
check "unmount points at uninstall-agent for an NFS agent" bash -c "'$B' unmount --mount '$NA' 2>&1 | grep -q 'uninstall-agent' && [[ \$(from '$NA') == 127.0.0.1:/ ]]"
check "uninstall the NFS agent" bash -c "'$B' uninstall-agent --name '$NS' && ! /sbin/mount | grep -qE ' on ($NA|$NP/dir) \\('"
wait_for 10 bash -c "! nfs_server" > /dev/null
check "delete-store" bash -c "'$B' delete-store '$NS' && [[ ! -e '$NSTORE' ]]"
check "delete-store of no store fails" bash -c "! '$B' delete-store '$NS' 2>/dev/null"

# NFS stores share the chunk pool too
P1="$HOME/.buildfs/stores/$NS-p1"; P2="$HOME/.buildfs/stores/$NS-p2"
"$B" mount --backend nfs --name "$NS-p1" --compress-after 1 "$T/p1" > /dev/null 2>&1
"$B" mount --backend nfs --name "$NS-p2" --compress-after 1 "$T/p2" > /dev/null 2>&1
cp "$T/payload" "$T/p1/data"; cp "$T/payload" "$T/p2/data"
python3 -I -c 'import time; time.sleep(8)'
"$B" unmount --mount "$T/p1" > /dev/null; "$B" unmount --mount "$T/p2" > /dev/null
if grep -qE '[1-9][0-9]* chunks came from the shared pool' "$HOME/Library/Logs/buildfs-$NS-p1.log" "$HOME/Library/Logs/buildfs-$NS-p2.log"; then ok "NFS stores share chunks"; else bad "NFS stores share chunks"; fi
check "their data reads back" bash -c "'$B' mount --backend nfs --name '$NS-p1' '$T/p1' > /dev/null && cmp '$T/payload' '$T/p1/data' && '$B' unmount --mount '$T/p1' > /dev/null"
check "deleting one leaves the other intact" bash -c "'$B' delete-store '$NS-p1' && '$B' mount --backend nfs --name '$NS-p2' '$T/p2' > /dev/null && cmp '$T/payload' '$T/p2/data' && '$B' unmount --mount '$T/p2' > /dev/null && '$B' delete-store '$NS-p2'"

# --sync strict on FSKit
since=$(date "+%Y-%m-%d %H:%M:%S")
check "--sync strict mounts" bash -c "'$B' mount --fskit --sync strict --name '$STORE' '$M' && echo strict > '$M/s' && [[ \$(cat '$M/s') == strict ]]"
"$B" unmount --mount "$M" > /dev/null
python3 -I -c 'import time; time.sleep(1)'
if /usr/bin/log show --start "$since" --predicate "subsystem == \"com.pawlowskialex.buildfs.fsmodule\" AND eventMessage BEGINSWITH \"activated store $STORE \"" 2>/dev/null | grep -q 'sync=strict'; then ok "the module got it"; else bad "the module got it"; fi

# Throwaway stores and registry
"$B" delete-store "$AU" > /dev/null
"$B" delete-store "$STORE" > /dev/null
"$B" delete-store "$AS" > /dev/null
"$B" delete-store "$(python3 -I -c "import json,os; print(json.load(open(os.path.expanduser('~/Library/Application Support/buildfs/$AS-attachments.json')))[0]['export'])")" > /dev/null
rm -f "$HOME/Library/Logs/buildfs-$NS.log" "$HOME/Library/Logs/buildfs-$NS-b.log" "$HOME/Library/Logs/buildfs-$NS-p1.log" "$HOME/Library/Logs/buildfs-$NS-p2.log" "$HOME/Library/Logs/buildfs-$AU.log" "$HOME/Library/Logs/buildfs-$AS.log" "$HOME/Library/Application Support/buildfs/$STORE-attachments.json" "$HOME/Library/Application Support/buildfs/$AS-attachments.json"
echo "failures: $fails"
exit $((fails > 0))
