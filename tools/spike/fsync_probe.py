"""fsync latency probe: a stand-in for a Raft log writer.

Appends 4 KiB records to a file in DIR and calls fdatasync after each, with a 2 ms gap
between appends, for SECONDS. Prints one line:
"n=<appends> p50_ms=<..> p99_ms=<..> max_ms=<..>".

Usage: python3 fsync_probe.py DIR SECONDS
"""

import os
import sys
import time


def main() -> None:
    directory, seconds = sys.argv[1], float(sys.argv[2])
    path = os.path.join(directory, "raft-log-probe")
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND | os.O_TRUNC, 0o644)
    record = b"r" * 4096
    lat = []
    end = time.monotonic() + seconds
    written = 0
    while time.monotonic() < end:
        t0 = time.monotonic()
        os.write(fd, record)
        os.fdatasync(fd)
        lat.append((time.monotonic() - t0) * 1000.0)
        written += len(record)
        if written >= 64 << 20:
            os.ftruncate(fd, 0)
            written = 0
        time.sleep(0.002)
    os.close(fd)
    lat.sort()

    def pct(p: float) -> float:
        return lat[min(len(lat) - 1, int(p / 100.0 * len(lat)))]

    print(f"n={len(lat)} p50_ms={pct(50):.2f} p99_ms={pct(99):.2f} max_ms={lat[-1]:.2f}")


if __name__ == "__main__":
    main()
