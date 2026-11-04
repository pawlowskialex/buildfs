import os, sys, time
root = sys.argv[1]
d = os.path.join(root, "micro"); os.makedirs(d, exist_ok=True)
N = 2000
def t(label, fn):
    s = time.perf_counter(); fn(); e = time.perf_counter()
    print(f"{label:28s} {(e-s)/N*1e6:8.1f} us/op")
def create():
    for i in range(N):
        with open(os.path.join(d, f"f{i}"), "wb") as f: f.write(b"x" * 4096)
def stat():
    for i in range(N): os.stat(os.path.join(d, f"f{i}"))
def openread():
    for i in range(N):
        with open(os.path.join(d, f"f{i}"), "rb") as f: f.read()
def rename():
    for i in range(N): os.rename(os.path.join(d, f"f{i}"), os.path.join(d, f"g{i}"))
def listdir():
    for i in range(N // 100): os.listdir(d)
def unlink():
    for i in range(N): os.unlink(os.path.join(d, f"g{i}"))
t("create+write4k+close", create); t("stat", stat); t("open+read+close", openread)
t("rename", rename); t("listdir(2000) x1/100", listdir); t("unlink", unlink)
