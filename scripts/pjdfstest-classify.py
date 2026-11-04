"""Attributes each pjdfstest regression (vs APFS) to a cause.

    python3 -I scripts/pjdfstest-classify.py <apfs.txt> <buildfs.txt> <verbose-dir>

<verbose-dir> holds `prove -v` output per regressed file, named after the
test path with "/" replaced by "_" (pjdfstest.sh CLASSIFY=1 creates it).
Causes recognized: FIFO/socket creation refused (FSKit doesn't pass mknod
to modules) and everything that then fails on the missing object, and the
path-length tests that can't run without _PC_PATH_MAX. Anything else is
listed individually.
"""
import re, sys, os, importlib.util, collections
spec = importlib.util.spec_from_file_location("cmp", os.path.join(os.path.dirname(os.path.abspath(__file__)), "pjdfstest-compare.py"))
src = open(spec.origin).read().split("base, test = parse")[0]
ns = {}; exec(src, ns)
base, test = ns["parse"](sys.argv[1]), ns["parse"](sys.argv[2])
vdir = sys.argv[3]
cats = collections.Counter(); other = []
for f in sorted(set(test) | set(base)):
    reg = set(test.get(f, set())) - set(base.get(f, set()))
    if not reg: continue
    path = os.path.join(vdir, f.replace("/", "_") + ".txt")
    lines = open(path, errors="replace").read().splitlines() if os.path.exists(path) else []
    special = set()   # names whose special-file creation was refused
    for line in lines:
        m = re.match(r"(not )?ok (\d+)(?: - tried '([^']*)', expected (\S*), got (.*))?", line)
        if not m: continue
        failed, n, cmd, exp, got = m.group(1), int(m.group(2)), m.group(3) or "", m.group(4), (m.group(5) or "").strip()
        words = cmd.split()
        op = words[0] if words else ""
        name = words[1] if len(words) > 1 else ""
        if op in ("mkfifo", "bind", "mknod") and failed and got in ("45", "EOPNOTSUPP", "EPERM"):
            special.add(name)
        elif op in ("create", "mkdir", "symlink", "open", "mkfifo", "bind") and not failed:
            special.discard(name)
        if not failed or n not in reg: continue
        if op in ("mkfifo", "bind", "mknod") and got in ("45", "EOPNOTSUPP"):
            cats["special file creation refused by FSKit"] += 1
        elif special:
            # Within a loop iteration whose FIFO/socket couldn't be created:
            # symlinks to it, renames onto its name, etc. all go wrong too.
            # (Could hide a genuine failure in such an iteration; the
            # non-special iterations of the same loops still count.)
            cats["follow-on: operation on a FIFO/socket that couldn't be created"] += 1
        elif got == "" or exp is None or re.search(r"/0[3]\.t$", f) or f == "rename/02.t":
            cats["path-length tests: no _PC_PATH_MAX on FSKit (same as NFS)"] += 1
        else:
            cats["other"] += 1; other.append(f"{f} #{n}: {cmd} expected {exp} got {got}")
    for n in reg:
        if not any(re.match(rf"not ok {n}\b", l) for l in lines):
            cats["not reproduced in verbose rerun"] += 1
print("total regressions:", sum(cats.values()))
for k, v in cats.most_common(): print(f"  {v:5d}  {k}")
print("--- other:"); print("\n".join(other))
