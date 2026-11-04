import os, sys, time, multiprocessing as mp
N = 5000
def work(path):
    for _ in range(N):
        fd = os.open(path, os.O_RDONLY); os.close(fd)
if __name__ == "__main__":
    root, P = sys.argv[1], int(sys.argv[2])
    paths = [os.path.join(root, f"oc{i+1}") for i in range(P)]
    for p in paths:
        open(p, "w").write("x")
    t = time.perf_counter()
    ps = [mp.Process(target=work, args=(p,)) for p in paths]
    [p.start() for p in ps]; [p.join() for p in ps]
    assert all(p.exitcode == 0 for p in ps), [p.exitcode for p in ps]
    dt = time.perf_counter() - t
    print(f"{root:16s} {P} parallel: {P*N*2/dt:8.0f} module calls/s, {dt/N*1e6:6.1f} us per open+close")
