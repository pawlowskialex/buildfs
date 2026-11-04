import os, sys, time
d = os.path.join(sys.argv[1], "lnk"); os.makedirs(d, exist_ok=True)
N = 300
for i in range(N):
    with open(f"{d}/s{i}", "wb") as f: f.write(b"y" * 100)
time.sleep(0.5)
s = time.perf_counter()
for i in range(N): os.link(f"{d}/s{i}", f"{d}/h{i}")
print(f"{sys.argv[1]:20s} link: {(time.perf_counter()-s)/N*1e6:8.1f} us/op")
for i in range(N): os.unlink(f"{d}/s{i}"); os.unlink(f"{d}/h{i}")
