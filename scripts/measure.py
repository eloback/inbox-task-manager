"""Measurements run inside the sandbox by measure.sh.

    python3 measure.py runs N COMMAND...     median wall time of N runs, in ms
    python3 measure.py tty-runs N COMMAND... the same, drawing on this terminal (the switcher)
    python3 measure.py daemon SECONDS CMD... CPU (own + children) and RSS of a daemon
"""

import os
import statistics
import subprocess
import sys
import time


def runs(n: int, command: list[str], tty: bool = False) -> None:
    times = []
    for _ in range(n):
        start = time.perf_counter()
        subprocess.run(command, stdout=None if tty else subprocess.DEVNULL, check=True)
        times.append((time.perf_counter() - start) * 1000)
    summary = f"median {statistics.median(times):.2f} ms (min {min(times):.2f}, max {max(times):.2f}, n={n})"
    print(summary, file=sys.stderr if tty else sys.stdout)  # a switcher owns stdout


def ticks(pid: int) -> int:
    with open(f"/proc/{pid}/stat") as f:
        fields = f.read().rsplit(")", 1)[1].split()
    # utime, stime, cutime, cstime (fields 14-17; index 11-14 after the name)
    return sum(int(x) for x in fields[11:15])


def rss_kb(pid: int) -> int:
    with open(f"/proc/{pid}/status") as f:
        return next(int(line.split()[1]) for line in f if line.startswith("VmRSS:"))


def daemon(seconds: float, command: list[str]) -> None:
    proc = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(3)  # past start-up and the first pass
    start, t0, peak = ticks(proc.pid), time.monotonic(), 0
    while time.monotonic() - t0 < seconds:
        time.sleep(1)
        peak = max(peak, rss_kb(proc.pid))
    used = (ticks(proc.pid) - start) / os.sysconf("SC_CLK_TCK")
    elapsed = time.monotonic() - t0
    proc.terminate()
    proc.wait()
    print(f"cpu {100 * used / elapsed:.3f}% of a core ({used:.2f} s in {elapsed:.0f} s), peak rss {peak / 1024:.1f} MB")


if __name__ == "__main__":
    match sys.argv[1]:
        case "runs" | "tty-runs":
            runs(int(sys.argv[2]), sys.argv[3:], tty=sys.argv[1] == "tty-runs")
        case "daemon":
            daemon(float(sys.argv[2]), sys.argv[3:])
