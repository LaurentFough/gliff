#!/usr/bin/env python3
"""Bounded memory check of the actual Mac viewer's native window.

Run on a logged-in Mac. This opens a temporary window and feeds BGRA frames,
without SSH, screen capture, or remote input. The child is always cleaned up.
"""
import argparse
import signal
import struct
import subprocess
import sys
import time
from pathlib import Path


def timeout(_signal, _frame):
    raise TimeoutError("native viewer check timed out")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--native", type=Path, default=Path.home() / "Applications/Gliff.app/Contents/MacOS/gliff-native")
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("Run this check in a macOS desktop session.")
    signal.signal(signal.SIGALRM, timeout)
    # A Retina-size window reproduces the expensive Data -> CFData bridge.
    width, height = 2200, 1440
    pixels = bytes([24, 16, 8, 255]) * (width * height)
    data = struct.pack("<III", width, height, 2000) + pixels
    packet = bytes([1]) + struct.pack("<I", len(data)) + data
    child = subprocess.Popen([str(args.native), "app", "Gliff viewer memory check"],
                             stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    try:
        signal.alarm(35)
        samples = []
        for frame in range(240):
            child.stdin.write(packet)
            child.stdin.flush()
            time.sleep(0.05)
            if frame % 8 == 7:
                rss = int(subprocess.check_output(["/bin/ps", "-o", "rss=", "-p", str(child.pid)], timeout=3).strip())
                samples.append(rss)
                assert rss < 512 * 1024, f"native viewer exceeded 512 MiB: {samples}"
        # EOF uses the production shutdown path; it also releases held input.
        child.stdin.close()
        assert child.wait(timeout=5) == 0
        assert b"Gliff displayed first frame" in child.stderr.read(), "window did not display a frame"
        growth = max(samples[5:]) - min(samples[5:])
        assert growth < 128 * 1024, f"native viewer retains frame memory after warmup: {samples}"
        print(f"VIEWER MEMORY PASS: 240 Retina frames; peak {max(samples)//1024} MiB; settled spread {growth//1024} MiB")
    finally:
        signal.alarm(0)
        if child.poll() is None:
            child.kill()
            child.wait()


if __name__ == "__main__":
    main()
