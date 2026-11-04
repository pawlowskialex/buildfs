"""Summarize an xctrace Time Profiler export (all processes).

    xcrun xctrace export --input t.trace \
      --xpath '/trace-toc/run[@number="1"]/data/table[@schema="time-profile"]' > t.xml
    python3 -I scripts/bench/xtprof.py t.xml [process-substring ...]

Prints CPU per process, then for each matching process: CPU per thread,
self time by binary, top self frames, and inclusive time of frames.
Weights are in ms of CPU sampled (1 sample ~ 1 ms per running thread).
CALLERS=<frame substring> also prints the most common call chains (DEPTH
frames, default 8) leading to matching frames.
"""
import os
import sys
import xml.etree.ElementTree as ET
from collections import Counter, defaultdict

path, wanted = sys.argv[1], [w.lower() for w in sys.argv[2:]]
byid = {}


def res(e):
    """Resolve id/ref deduplication."""
    if e is None:
        return None
    r = e.get("ref")
    return byid.get(r, e) if r else e


def index(e):
    i = e.get("id")
    if i:
        byid[i] = e
    for c in e:
        index(c)


rows = []
for ev, e in ET.iterparse(path, events=("end",)):
    if e.tag == "row":
        index(e)
        rows.append(e)

proc_w = Counter()
per = defaultdict(lambda: {"thr": Counter(), "tid": Counter(), "role": defaultdict(Counter), "bin": Counter(), "self": Counter(), "incl": Counter(), "total": 0})
# Frames that identify what a thread is doing, checked in order.
ROLES = ["dispatch_mig_server", "_xpc_connection_call_reply_async", "_xpc_connection_mach_event",
         "_dispatch_mach_msg_invoke", "_dispatch_source_invoke", "_dispatch_lane_serial_drain",
         "_dispatch_root_queue_drain", "mach_msg2_trap"]
span = [None, None]

for r in rows:
    proc = res(r.find("process"))
    thr = res(r.find("thread"))
    w = res(r.find("weight"))
    pname = proc.get("fmt") if proc is not None else "?"
    weight = int(w.text) / 1e6 if w is not None and w.text else 1.0  # ns -> ms
    proc_w[pname] += weight
    if wanted and not any(x in pname.lower() for x in wanted):
        continue
    d = per[pname]
    d["total"] += weight
    tname = thr.get("fmt") if thr is not None else "?"
    # Strip the tid so threads of the same queue aggregate.
    d["thr"][tname.split(" (0x")[0]] += weight
    tid = res(thr.find("tid")) if thr is not None else None
    tidv = tid.text if tid is not None else "?"
    d["tid"][tidv] += weight
    st = res(r.find("sample-time"))
    if st is not None:
        t_ns = int(st.text)
        span[0] = t_ns if span[0] is None else min(span[0], t_ns)
        span[1] = t_ns if span[1] is None else max(span[1], t_ns)
    bt = res(r.find("tagged-backtrace"))
    if bt is None:
        bt = res(r.find("backtrace"))
    frames = []
    if bt is not None:
        for f in bt.findall("frame"):
            f = res(f)
            binary = res(f.find("binary"))
            frames.append((f.get("name") or f.get("addr") or "?", binary.get("name") if binary is not None else "?"))
    names = [n for n, _ in frames]
    role = next((x for x in ROLES if any(x in n for n in names)), names[-1] if names else "?")
    d["role"][tidv][role] += weight
    callers = os.environ.get("CALLERS")
    if callers:
        depth = int(os.environ.get("DEPTH", "8"))
        for i, (n, _) in enumerate(frames):
            if callers in n:
                d.setdefault("chains", Counter())[" <- ".join(x for x, _ in frames[i:i + depth])] += weight
                break
    if frames:
        leaf, lbin = frames[0]
        d["self"][f"{leaf}  [{lbin}]"] += weight
        d["bin"][lbin] += weight
        for name in {f"{n}  [{b}]" for n, b in frames}:
            d["incl"][name] += weight

total = sum(proc_w.values())
print(f"total sampled CPU: {total:.0f} ms")
for p, w in proc_w.most_common(15):
    print(f"  {w:8.0f} ms  {100*w/total:5.1f}%  {p}")

for p, d in sorted(per.items(), key=lambda kv: -kv[1]["total"]):
    t = d["total"]
    print(f"\n===== {p}: {t:.0f} ms")
    print("-- threads")
    for k, w in d["thr"].most_common(8):
        print(f"  {w:8.0f} ms  {100*w/t:5.1f}%  {k}")
    secs = (span[1] - span[0]) / 1e9 if span[0] is not None else 0
    print(f"-- per thread (sampled span {secs:.1f}s; % = share of one core)")
    for k, w in d["tid"].most_common(10):
        roles = ", ".join(f"{r} {100*x/w:.0f}%" for r, x in d["role"][k].most_common(3))
        print(f"  tid {k:>9s} {w:8.0f} ms  {100*w/(secs*1000) if secs else 0:5.1f}% core   {roles}")
    print("-- self time by binary")
    for k, w in d["bin"].most_common(12):
        print(f"  {w:8.0f} ms  {100*w/t:5.1f}%  {k}")
    print("-- top self frames")
    for k, w in d["self"].most_common(25):
        print(f"  {w:8.0f} ms  {100*w/t:5.1f}%  {k}")
    if d.get("chains"):
        print(f"-- call chains into {os.environ['CALLERS']}")
        for k, w in d["chains"].most_common(8):
            print(f"  {w:8.0f} ms  {100*w/t:5.1f}%  {k}")
    print("-- top inclusive frames")
    for k, w in d["incl"].most_common(45):
        print(f"  {w:8.0f} ms  {100*w/t:5.1f}%  {k}")
