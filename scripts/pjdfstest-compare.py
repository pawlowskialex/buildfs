import re, sys
def parse(path):
    txt = open(path, errors="replace").read()
    summ = txt[txt.find("Test Summary Report"):]
    res, cur = {}, None
    for line in summ.splitlines():
        m = re.match(r"(\S+\.t)\s+\(Wstat: (\d+).*Tests: (\d+) Failed: (\d+)\)", line)
        if m:
            cur = m.group(1).split("/tests/")[-1]; res[cur] = set(); continue
        m = re.search(r"Failed tests?:\s*(.*)", line)
        if m and cur is not None:
            spec = m.group(1)
        elif cur is not None and re.match(r"^\s+[\d, -]+$", line):
            spec = line
        else:
            if re.match(r"^\s+(Parse errors|Non-zero)", line) and cur: res[cur].add(-1)
            continue
        for part in spec.split(","):
            part = part.strip()
            if not part: continue
            if "-" in part:
                a, b = part.split("-"); res[cur].update(range(int(a), int(b) + 1))
            else: res[cur].add(int(part))
    return res
base, test = parse(sys.argv[1]), parse(sys.argv[2])
regress = {f: sorted(test.get(f, set()) - base.get(f, set())) for f in set(base) | set(test)}
regress = {f: v for f, v in regress.items() if v}
better = {f: sorted(base.get(f, set()) - test.get(f, set())) for f in set(base) | set(test)}
better = {f: v for f, v in better.items() if v}
print("failing assertions: apfs=%d buildfs=%d" % (sum(map(len, base.values())), sum(map(len, test.values()))))
print("REGRESSIONS (fail on buildfs, pass on APFS):", sum(map(len, regress.values())))
for f in sorted(regress): print("  ", f, regress[f][:30])
print("pass on buildfs but fail on APFS:", sum(map(len, better.values())))
for f in sorted(better): print("  ", f, better[f][:15])
