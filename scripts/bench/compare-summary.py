"""Medians per kind from scripts/bench/compare.sh output.

    python3 -I scripts/bench/compare-summary.py /tmp/compare.txt
"""
import re, statistics as st, sys
cur = None
d = {}
kinds = []
for line in open(sys.argv[1]):
    m = re.match(r"== round=\d+ kind=(\w+)", line)
    if m:
        cur = m.group(1)
        if cur not in kinds:
            kinds.append(cur)
        continue
    if cur is None:
        continue
    m = re.search(r"(\d+) parallel:\s+(\d+) module calls/s", line)
    if m:
        d.setdefault(f"par_oc {m.group(1)}-way (opens/s x2)", {}).setdefault(cur, []).append(float(m.group(2)))
        continue
    m = re.match(r"(\S.*?)\s{2,}([\d.]+) us/op", line)
    if m:
        d.setdefault(m.group(1).strip() + " (us)", {}).setdefault(cur, []).append(float(m.group(2)))
        continue
    m = re.search(r"link:\s+([\d.]+)", line)
    if m:
        d.setdefault("hard link (us)", {}).setdefault(cur, []).append(float(m.group(1)))
        continue
    m = re.match(r"cargo (\w+): ([\d.]+) s", line)
    if m:
        d.setdefault(f"cargo {m.group(1)} (s)", {}).setdefault(cur, []).append(float(m.group(2)))
print("| metric | " + " | ".join(kinds) + " |")
print("|---|" + "---|" * len(kinds))
for k, v in d.items():
    cells = []
    for kind in kinds:
        xs = v.get(kind, [])
        cells.append(f"{st.median(xs):,.1f}" if xs else "-")
    print(f"| {k} | " + " | ".join(cells) + " |")
