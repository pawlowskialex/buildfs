"""Open+close throughput against a mount whose replies are delayed.

    mount -F -t buildfs -o delay=1000 buildfs://x /tmp/fskit-mnt
    python3 -I scripts/bench/inflight.py /tmp/fskit-mnt 1000 1 2 4 8 16

With every reply delayed by `delay` µs, throughput * delay = the number of
requests in flight. If it stops growing with more processes, FSKit (or the
kernel) caps concurrency there. An open+close is 3 module calls
(getAttributes, open, close).
"""
import os, sys, time, multiprocessing as mp

CALLS_PER_ITER = 3


def work(path, n):
    for _ in range(n):
        fd = os.open(path, os.O_RDONLY)
        os.close(fd)


if __name__ == "__main__":
    root, delay_us, procs = sys.argv[1], int(sys.argv[2]), [int(x) for x in sys.argv[3:]]
    secs = float(os.environ.get("SECS", "3"))
    for P in procs:
        # Size the run for ~secs assuming no cap; a cap makes it longer.
        n = max(10, int(secs / (delay_us / 1e6 * CALLS_PER_ITER) / max(1, P // 4)))
        paths = [os.path.join(root, f"if{i}") for i in range(P)]
        for p in paths:
            open(p, "w").write("x")
        t = time.perf_counter()
        ps = [mp.Process(target=work, args=(p, n)) for p in paths]
        [p.start() for p in ps]
        [p.join() for p in ps]
        dt = time.perf_counter() - t
        rate = P * n * CALLS_PER_ITER / dt
        print(f"delay={delay_us}us P={P:2d}: {rate:8.0f} module calls/s, in flight {rate * delay_us / 1e6:5.2f}", flush=True)
