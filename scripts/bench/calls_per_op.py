"""Module calls FSKit makes per syscall, one operation type at a time.

    mount -F -t buildfs -o owners,nobrowse,fresh buildfs://calls /tmp/fskit-mnt
    python3 -I scripts/bench/calls_per_op.py /tmp/fskit-mnt

Reads the extension's `stats` log lines, which flush on the first call after
3 s of quiet, so each phase is followed by a pause and a flushing lookup.
Counts include that lookup and anything background daemons do meanwhile, so
read them to about +-0.05 per op.
"""
import datetime, json, os, re, subprocess, sys, time
from collections import Counter

root = sys.argv[1]
N = 200
d = os.path.join(root, "calls")
os.mkdir(d)
ST = os.path.join(root, "flush-marker")
marks = []
flushes = 0


def flush():
    # A name never looked up before, so the module is sure to be called.
    global flushes
    flushes += 1
    time.sleep(3.3)
    try:
        os.stat(f"{ST}-{flushes}")
    except OSError:
        pass
    time.sleep(0.2)


def phase(label, fn):
    fn()
    flush_start = time.time()
    flush()
    marks.append((label, flush_start))


def p(i, name="f"):
    return os.path.join(d, f"{name}{i}")


def mk(name):
    def go():
        for i in range(N):
            os.close(os.open(p(i, name), os.O_CREAT | os.O_WRONLY, 0o644))
    return go


flush()
start = time.time()
phase("create+close new file", mk("f"))
phase("stat (attrs cached)", lambda: [os.stat(p(i)) for i in range(N)])
phase("open+close existing", lambda: [os.close(os.open(p(i), os.O_RDONLY)) for i in range(N)])
phase("open+close again", lambda: [os.close(os.open(p(i), os.O_RDONLY)) for i in range(N)])
phase("stat missing name", lambda: [os.path.exists(p(i, "missing")) for i in range(N)])
phase("stat same missing name again", lambda: [os.path.exists(p(i, "missing")) for i in range(N)])
phase("hard link", lambda: [os.link(p(i), p(i, "h")) for i in range(N)])
phase("rename to new name", lambda: [os.rename(p(i), p(i, "r")) for i in range(N)])
phase("unlink, other links remain", lambda: [os.unlink(p(i, "h")) for i in range(N)])
phase("unlink, last link", lambda: [os.unlink(p(i, "r")) for i in range(N)])
time.sleep(1)

since = datetime.datetime.fromtimestamp(start - 1).strftime("%Y-%m-%d %H:%M:%S")
out = subprocess.run(
    ["/usr/bin/log", "show", "--start", since, "--style", "ndjson", "--predicate",
     'subsystem == "com.pawlowskialex.buildfs.fsmodule" AND eventMessage BEGINSWITH "stats "'],
    capture_output=True, text=True).stdout
lines = []
for l in out.splitlines():
    if not l.startswith("{"):
        continue
    e = json.loads(l)
    if "eventMessage" in e:
        ts = datetime.datetime.strptime(e["timestamp"][:26], "%Y-%m-%d %H:%M:%S.%f").timestamp()
        lines.append((ts, e["eventMessage"]))
prev = start
print(f"{'operation':32s} {'calls/op':>8s}  breakdown per op")
for label, end in marks:
    c = Counter()
    # A phase's calls are dumped by the flushing lookup that follows it.
    for ts, msg in lines:
        if end < ts <= end + 3.6:
            for name, n, _ in re.findall(r"(\w[\w.]*)=(\d+)/(\d+)us", msg):
                c[name] += int(n)
    total = sum(c.values()) / N
    parts = " ".join(f"{k}={v / N:.2f}" for k, v in c.most_common() if v / N >= 0.05)
    print(f"{label:32s} {total:8.2f}  {parts}")
