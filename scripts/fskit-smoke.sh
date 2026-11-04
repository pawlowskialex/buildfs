#!/usr/bin/env bash
# Functional smoke test of a mounted buildfs volume (or APFS, as a control):
#   FSKIT=1 scripts/fskit-smoke.sh /tmp/fskit-mnt
# Prints one line per check and exits non-zero if any failed.
set -uo pipefail
M="${1:?mount point}"
T="$M/smoke.$$"
fails=0
ok() { printf 'ok   %s\n' "$1"; }
bad() { printf 'FAIL %s%s\n' "$1" "${2:+: $2}"; fails=$((fails + 1)); }
check() { local name=$1; shift; if out=$("$@" 2>&1); then ok "$name"; else bad "$name" "$out"; fi; }
st() { /usr/bin/stat -f "$1" "$2"; }
export -f st

mkdir -p "$T" || { echo "cannot create $T"; exit 1; }
cd "$T" || exit 1

# Files and directories
check "write+read" bash -c 'printf "hello\n" > f && [[ $(cat f) == hello ]]'
check "append" bash -c 'printf "x" >> f && [[ $(st %z f) == 7 ]]'
check "mkdir -p + rmdir" bash -c 'mkdir -p a/b/c && rmdir a/b/c a/b a'
check "rmdir non-empty fails" bash -c 'mkdir d && touch d/x && ! rmdir d 2>/dev/null && rm d/x && rmdir d'
check "large file roundtrip" bash -c 'head -c 3000000 /dev/urandom > big && cp big big2 && cmp big big2'
check "sparse + truncate" bash -c 'truncate -s 10000000 sp && [[ $(st %z sp) == 10000000 ]] && truncate -s 5 sp && [[ $(st %z sp) == 5 ]] && [[ $(od -An -tx1 sp | tr -d " \n") == 0000000000 ]]'
check "truncate shrinks data" bash -c 'printf 0123456789 > tr && truncate -s 4 tr && [[ $(cat tr) == 0123 ]]'
check "overwrite middle" bash -c 'printf AAAAAAAAAA > ow && printf BB | dd of=ow bs=1 seek=4 conv=notrunc 2>/dev/null && [[ $(cat ow) == AAAABBAAAA ]]'

# Links
check "hard link" bash -c 'printf data > h1 && ln h1 h2 && [[ $(st %l h1) == 2 && $(st %i h1) == $(st %i h2) ]] && rm h1 && [[ $(cat h2) == data && $(st %l h2) == 1 ]]'
check "symlink" bash -c 'ln -s ../some/target sl && [[ $(readlink sl) == ../some/target ]] && ln -s f sl2 && [[ $(head -c5 sl2) == hello ]]'
check "rename" bash -c 'printf r > r1 && mv r1 r2 && [[ ! -e r1 && $(cat r2) == r ]]'
check "rename over existing" bash -c 'printf new > n1 && printf old > n2 && mv n1 n2 && [[ $(cat n2) == new && ! -e n1 ]]'
check "rename dir across dirs" bash -c 'mkdir -p x/y z && touch x/y/f && mv x/y z/ && [[ -e z/y/f && ! -e x/y ]]'
check "rename dir into itself fails" bash -c 'mkdir -p q/r && ! mv q q/r/ 2>/dev/null'

# Open-unlink: data stays readable through an open fd after unlink.
check "open-unlink" python3 -I -c '
import os
fd = os.open("ou", os.O_CREAT | os.O_RDWR, 0o644)
os.write(fd, b"still here")
os.unlink("ou")
os.lseek(fd, 0, 0)
assert os.read(fd, 100) == b"still here"
os.close(fd)
assert not os.path.exists("ou")'

# Delete entries while iterating a 300-entry directory (Rust remove_dir_all).
check "unlink while iterating" python3 -I -c '
import os
os.mkdir("it")
for i in range(300): open(f"it/f{i}", "w").close()
n = 0
with os.scandir("it") as d:
    for e in d: os.unlink(e.path); n += 1
left = os.listdir("it")
assert n == 300 and not left, (n, len(left))
os.rmdir("it")'

# Metadata
check "chmod" bash -c 'touch m && chmod 640 m && [[ $(st %Lp m) == 640 ]]'
check "utimes" bash -c 'touch -t 202001020304.05 ut && [[ $(st %Sm ut) == "Jan  2 03:04:05 2020" ]]'
mygroups=$(id -G)
other=$(for g in $mygroups; do [[ $g != $(id -g) ]] && echo "$g" && break; done)
if [[ -n $other ]]; then
  check "chown :supplementary-group" bash -c "touch cg && chown :$other cg && [[ \$(st %g cg) == $other ]]"
else
  ok "chown :group (skipped: no supplementary group)"
fi
check "chown to other user fails" bash -c 'touch co && ! chown 0 co 2>/dev/null'
check "chflags hidden" bash -c 'touch hf && chflags hidden hf && [[ $(st %Sf hf) == hidden ]] && chflags nohidden hf'
if [[ "${FSKIT:-0}" == 1 ]]; then
  # FSKit rejects mknod/mkfifo before calling the module (macOS 27).
  check "mkfifo (FSKit: unsupported)" bash -c '! mkfifo ff 2>/dev/null'
else
  check "mkfifo" bash -c 'mkfifo ff && [[ -p ff ]]'
fi

# xattrs
check "xattr -w/-p/-l/-d" bash -c 'touch xa && xattr -w user.k v1 xa && [[ $(xattr -p user.k xa) == v1 ]] && xattr -w com.apple.provenance x xa && [[ $(xattr xa | sort | tr "\n" " ") == "com.apple.provenance user.k " ]] && xattr -d user.k xa && [[ $(xattr xa) == com.apple.provenance ]]'
check "xattr big value" bash -c 'touch xb && v=$(head -c 30000 /dev/zero | tr "\0" a) && xattr -w user.big "$v" xb && [[ $(xattr -p user.big xb | wc -c | tr -d " ") == 30001 ]]'

# Copies that carry metadata
src="$HOME/.buildfs-smoke-src.$$"
mkdir -p "$src/sub" && printf 'payload' > "$src/sub/file" && xattr -w user.copy yes "$src/sub/file" && chmod 750 "$src/sub/file" && ln -s file "$src/sub/link"
check "cp -p" bash -c "/bin/cp -p '$src/sub/file' cpp && [[ \$(cat cpp) == payload && \$(xattr -p user.copy cpp) == yes && \$(st %Lp cpp) == 750 ]]"
check "cp -R" bash -c "/bin/cp -R '$src' cpr && cmp '$src/sub/file' cpr/sub/file && [[ -L cpr/sub/link ]]"
check "ditto" bash -c "ditto '$src' dit && [[ \$(xattr -p user.copy dit/sub/file) == yes && \$(readlink dit/sub/link) == file ]]"
check "cp from mount out" bash -c "/bin/cp -p cpp '$src/back' && cmp cpp '$src/back' && [[ \$(xattr -p user.copy '$src/back') == yes ]]"
rm -rf "$src"

# Large directory listing with attributes (getattrlistbulk via ls -l)
check "ls -l 2000 entries" bash -c 'mkdir bigdir && (cd bigdir && for i in $(seq 1 2000); do : > "e$i"; done) && [[ $(/bin/ls -l bigdir | tail -n +2 | wc -l | tr -d " ") == 2000 ]]'

check "rm -rf" bash -c "cd '$M' && rm -rf '$T' && [[ ! -e '$T' ]]"
cd "$M" && rm -rf "$T" 2>/dev/null
echo "failures: $fails"
exit $((fails > 0))
