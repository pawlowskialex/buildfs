"""Time per thread state from an xctrace System Trace export (streaming).

    xcrun xctrace export --input t.trace \
      --xpath '/trace-toc/run[@number="1"]/data/table[@schema="thread-state"]' > s.xml
    python3 -I scripts/bench/xtstate.py s.xml fskitd BuildFSExt python3

For each matching process prints, per thread, how long it was Running,
Runnable (ready but waiting for a CPU), Blocked, etc. Busy threads with
much Runnable time are starved for CPU; Blocked time is waiting on
something (mach_msg, locks, I/O).
"""
import sys
import xml.etree.ElementTree as ET
from collections import defaultdict

path, wanted = sys.argv[1], [w.lower() for w in sys.argv[2:]]
vals = {}  # id -> (text, fmt), for id/ref deduplication


def val(e):
    if e is None:
        return None, None
    r = e.get("ref")
    if r:
        return vals.get(r, (None, None))
    v = (e.text, e.get("fmt"))
    if e.get("id"):
        vals[e.get("id")] = v
    return v


def walk(e):
    """Register every id'd element (nested ones too, e.g. process in thread)."""
    for c in e.iter():
        if c.get("id") and c.get("id") not in vals:
            vals[c.get("id")] = (c.text, c.get("fmt"))


acc = defaultdict(lambda: defaultdict(float))  # (proc, thread) -> state -> ns
span = [None, None]
ctx = ET.iterparse(path, events=("end",))
for ev, e in ctx:
    if e.tag != "row":
        continue
    walk(e)
    _, proc = val(e.find("process"))
    _, thread = val(e.find("thread"))
    state, _ = val(e.find("thread-state"))
    dur, _ = val(e.find("duration"))
    start, _ = val(e.find("start-time"))
    if start:
        s = int(start)
        d = int(dur or 0)
        span[0] = s if span[0] is None else min(span[0], s)
        span[1] = s + d if span[1] is None else max(span[1], s + d)
    if proc and (not wanted or any(w in proc.lower() for w in wanted)) and dur:
        acc[(proc, thread)][state] += int(dur)
    e.clear()

secs = (span[1] - span[0]) / 1e9
print(f"trace span {secs:.2f}s; values are % of wall time")
states = ["Running", "Runnable", "Preempted", "Blocked", "Waiting", "Interrupted"]
byproc = defaultdict(list)
for (p, t), st in acc.items():
    byproc[p].append((t, st))
for p, threads in sorted(byproc.items()):
    tot = defaultdict(float)
    for _, st in threads:
        for k, v in st.items():
            tot[k] += v
    if tot["Running"] / 1e9 / secs < 0.02:
        continue  # skip idle processes
    print(f"\n== {p}: running {100*tot['Running']/1e9/secs:.0f}% of a core, "
          f"runnable {100*(tot['Runnable']+tot['Preempted'])/1e9/secs:.0f}%")
    other = sorted({k for _, st in threads for k in st} - set(states))
    cols = [s for s in states if tot[s]] + other
    print("  " + f"{'thread':44s}" + "".join(f"{c[:10]:>11s}" for c in cols))
    for t, st in sorted(threads, key=lambda x: -x[1]["Running"])[:8]:
        print("  " + f"{(t or '?')[:44]:44s}" + "".join(f"{100*st[c]/1e9/secs:10.1f}%" for c in cols))
