#!/usr/bin/env python3
"""Exercise the production native pipe reader without opening a GUI or capture.

Foundation's autoreleased pipe buffers must be released between packets. This
small test reproduced the original leak: 64 MiB of input grew RSS by 64 MiB.
Measure after a following empty packet, when the preceding payload is released,
and distinguish allocator warm-up from continued growth without raising the cap.
"""

import signal
import struct
import subprocess
import sys
import tempfile
from pathlib import Path


def timeout(_signal, _frame):
    raise TimeoutError("native bridge memory check timed out")


def main():
    if sys.platform != "darwin":
        sys.exit("Run this check on macOS.")
    source = Path(__file__).resolve().parents[1] / "crates/gliff-macos/native/Gliff.swift"
    # Compile the actual bridge functions; the callback just acknowledges each
    # packet, avoiding the UI, SSH, capture and permissions during this test.
    parts = source.read_text().split("// NSPasteboard requests", 1)
    assert len(parts) == 2, "cannot locate native bridge functions"
    harness = """
readCommands(onClose: { exit(0) }) { kind, data in
    guard (kind == 1 && (data.count == 1024 * 1024 || data.count == 12 * 1024 * 1024)) || (kind == 0 && data.isEmpty) else { exit(2) }
    FileHandle.standardOutput.write(Data([1]))
}
dispatchMain()
"""
    signal.signal(signal.SIGALRM, timeout)
    with tempfile.TemporaryDirectory(prefix="gliff-memory-check-") as folder:
        swift = Path(folder, "main.swift")
        binary = Path(folder, "bridge")
        swift.write_text(parts[0] + harness)
        subprocess.run(
            ["xcrun", "swiftc", "-O", "-swift-version", "5", str(swift), "-o", str(binary)],
            check=True,
            timeout=45,
        )
        child = subprocess.Popen([str(binary)], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        try:
            signal.alarm(30)
            def exchange(packet):
                child.stdin.write(packet)
                child.stdin.flush()
                assert child.stdout.read(1) == bytes([1]), "bridge stopped before acknowledging packet"

            for size in (1024 * 1024, 12 * 1024 * 1024):
                payload = bytes(size)
                packet = bytes([1]) + struct.pack("<I", len(payload)) + payload
                samples = []
                for frame in range(1, 65):
                    exchange(packet)
                    if frame in (1, 8, 16, 32, 48, 64):
                        # A payload acknowledgement happens inside its callback.
                        # This barrier proves the packet's autorelease pool has
                        # drained before we sample memory, with no sleep/race.
                        exchange(bytes(5))
                        rss = int(subprocess.check_output(
                            ["/bin/ps", "-o", "rss=", "-p", str(child.pid)], timeout=3
                        ).strip())
                        samples.append(rss)
                        assert rss < 128 * 1024, f"bridge exceeded 128 MiB RSS: {samples}"
                # macOS allocators populate caches over the first few packets.
                # Keep the absolute cap on every sample and check for growth
                # after warm-up; the original per-packet leak still fails both.
                settled = samples[2:]
                growth = max(settled) - settled[0]
                assert growth < 16 * 1024, f"bridge retains packet memory (KiB): {samples}"
                print(f"MEMORY PASS: 64 x {size//(1024*1024)} MiB packets, RSS KiB {samples}, settled growth {growth} KiB")
            child.stdin.close()
            assert child.wait(timeout=3) == 0
        finally:
            signal.alarm(0)
            if child.poll() is None:
                child.kill()
                child.wait()


if __name__ == "__main__":
    main()
