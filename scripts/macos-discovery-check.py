#!/usr/bin/env python3
"""Check live Bonjour discovery and picker selection/cancellation without video.

Requires one nearby host advertising Gliff. This is an opt-in integration
check, separate from CI: no SSH authentication or screen capture is started.
"""
import argparse
import json
import signal
import struct
import subprocess
import sys
import time
from pathlib import Path


def timeout(_signal, _frame):
    raise TimeoutError("discovery picker check timed out")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--expect-host", required=True)
    parser.add_argument("--expect-user", required=True)
    parser.add_argument("--expect-port", type=int, default=22)
    parser.add_argument("--lifecycle", action="store_true", help="Wait for external publisher stop/start and verify row removal/return")
    parser.add_argument("--native", type=Path, default=Path.home() / "Applications/Gliff.app/Contents/MacOS/gliff-native")
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("Run this check on the Mac.")
    signal.signal(signal.SIGALRM, timeout)
    signal.alarm(45 if args.lifecycle else 20)
    child = subprocess.Popen([str(args.native), "choose"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    try:
        def send(value):
            data = json.dumps(value).encode()
            child.stdin.write(bytes([2]) + struct.pack("<I", len(data)) + data)
            child.stdin.flush()

        def exact(length):
            data = bytearray()
            while len(data) < length:
                chunk = child.stdout.read(length - len(data))
                assert chunk, "picker closed unexpectedly"
                data.extend(chunk)
            return bytes(data)

        def receive():
            header = exact(5)
            length = struct.unpack("<I", header[1:])[0]
            assert header[0] == 2 and length <= 1024 * 1024
            return json.loads(exact(length))

        time.sleep(6)
        send({"type": "inspect"})
        state = receive()
        assert state["type"] == "picker_state"
        assert state["rows"] == len(state["hosts"])
        matches = [i for i, h in enumerate(state["hosts"]) if h["host"] == args.expect_host and h["user"] == args.expect_user]
        assert len(matches) == 1, f"expected one deduplicated host: {state}"
        send({"type": "select", "index": matches[0]})
        send({"type": "inspect"})
        selected = receive()
        assert selected["host"] == f"{args.expect_user}@{args.expect_host}"
        assert selected["port"] == str(args.expect_port)
        if args.lifecycle:
            print("PICKER READY: waiting for publisher removal", flush=True)
            def wait_for(present):
                deadline = time.monotonic() + 14
                while time.monotonic() < deadline:
                    send({"type": "inspect"})
                    current = receive()
                    found = any(h["host"] == args.expect_host and h["user"] == args.expect_user for h in current["hosts"])
                    assert current["rows"] == len(current["hosts"])
                    if found == present:
                        return
                    time.sleep(0.4)
                raise AssertionError(f"publisher presence did not become {present}")
            wait_for(False)
            print("REMOVAL PASS: waiting for publisher return", flush=True)
            wait_for(True)
            print("RETURN PASS", flush=True)
        rss = int(subprocess.check_output(["/bin/ps", "-o", "rss=", "-p", str(child.pid)]).strip())
        assert rss < 128 * 1024, f"picker exceeds 128 MiB RSS: {rss}"
        send({"type": "cancel"})
        assert receive()["type"] == "cancel"
        assert child.wait(timeout=3) == 0
        print(f"DISCOVERY PASS: one {selected['host']}:{selected['port']} row; cancel exits; RSS {rss} KiB")
    finally:
        signal.alarm(0)
        if child.poll() is None:
            child.kill(); child.wait()


if __name__ == "__main__":
    main()
