"""Per code path, how many threads are inside it at the same sampling tick.

    python3 -I scripts/bench/xtoverlap.py t.xml   # Time Profiler export (see xtprof.py)

A strictly serial path (one dispatch lane) never shows more than one thread
per tick, however many threads take turns running it. The "(control)" rows
show the method detects overlap where it exists.
"""
import sys, xml.etree.ElementTree as ET
from collections import defaultdict, Counter
byid = {}
def res(e):
    r = e.get("ref") if e is not None else None
    return byid.get(r, e) if r else e
ROLES = {
  "fskitd": [("(control) any fskitd code", ""),("kernel request (dispatch_mig_server)", "dispatch_mig_server"),
             ("reply receive (_xpc_connection_call_reply_async)", "_xpc_connection_call_reply_async")],
  "ext":    [("(control) any extension code", ""),("message receive (_xpc_connection_mach_event)", "_xpc_connection_mach_event"),
             ("handler on concurrent queue (buildfs code)", "BuildFSVolume")],
}
ticks = defaultdict(lambda: defaultdict(set))  # (proc, role) -> tick -> threads
for ev, e in ET.iterparse(sys.argv[1], events=("end",)):
    if e.tag != "row": continue
    for c in e.iter():
        if c.get("id"): byid[c.get("id")] = c
    p = res(e.find("process")); pn = p.get("fmt") if p is not None else ""
    key = "ext" if "BuildFSExtension" in pn else "fskitd" if pn.startswith("fskitd") else None
    if not key: continue
    thr = res(e.find("thread")); tid = res(thr.find("tid")).text
    t = int(res(e.find("sample-time")).text)
    bt = res(e.find("tagged-backtrace"))
    names = [res(f).get("name") or "" for f in bt.findall("frame")] if bt is not None else []
    for label, needle in ROLES[key]:
        if any(needle in n for n in names):
            ticks[(key, label)][t // 250_000].add(tid)   # 250 us buckets: one tick across cores
for (key, label), tk in sorted(ticks.items()):
    dist = Counter(len(v) for v in tk.values())
    n = sum(dist.values())
    multi = sum(c for k, c in dist.items() if k > 1)
    print(f"{key:6s} {label:52s} ticks={n:6d}  >1 thread at once: {100*multi/n:5.1f}%  dist={dict(sorted(dist.items()))}")
