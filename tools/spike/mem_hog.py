"""Memory hog and small action for memory_high.sh.

hog MIB SECONDS: allocate MIB MiB in 16 MiB chunks, touching every page, then keep
touching them until SECONDS have passed since the start. Prints "allocated_mib=<n>"
after every chunk and "hog_done" at the end, flushed, so the last line says how far
it got.

small: a small action: allocate 32 MiB and hash 256 MiB through it; prints
"small_ms=<wall ms>".

Usage: python3 mem_hog.py hog MIB SECONDS | python3 mem_hog.py small
"""

import hashlib
import sys
import time

CHUNK = 16 << 20
PAGE = 4096


def touch(buf: bytearray) -> None:
    buf[::PAGE] = b"\x01" * (len(buf) // PAGE)


def hog(mib: int, seconds: float) -> None:
    start = time.monotonic()
    chunks = []
    while len(chunks) * 16 < mib:
        b = bytearray(CHUNK)
        touch(b)
        chunks.append(b)
        print(f"allocated_mib={len(chunks) * 16}", flush=True)
    while time.monotonic() - start < seconds:
        for b in chunks:
            touch(b)
        time.sleep(0.5)
    print("hog_done", flush=True)


def small() -> None:
    start = time.monotonic()
    buf = bytearray(32 << 20)
    touch(buf)
    h = hashlib.sha256()
    for _ in range(8):
        h.update(buf)
    print(f"small_ms={int((time.monotonic() - start) * 1000)}", flush=True)


if __name__ == "__main__":
    if sys.argv[1] == "hog":
        hog(int(sys.argv[2]), float(sys.argv[3]))
    else:
        small()
