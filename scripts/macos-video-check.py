#!/usr/bin/env python3
"""Check and time the Mac viewer's hardware video path (VideoToolbox + Metal).

Run on a logged-in Mac with Gliff installed (`make macos`). For each case it:

1. encodes a moving test pattern with gliff's CPU encoder and times the CPU
   decoder on it (`examples/video_fixture.rs`);
2. opens the real viewer helper with GLIFF_VIDEO_PROBE=1, sends the coded
   frames the way the client does (`video_config`, then native kind 5), and
   waits for each `decoded` event;
3. checks that every frame decoded on the GPU, that the colours match the
   pattern (channel order) and the CPU decoder (the Metal recombine against
   the Rust reference), and, for 4:4:4, that one-pixel red/blue columns stay
   apart (4:2:0 would blur them to purple);
4. prints GPU and CPU decode times side by side.

The GPU time covers decode and recombine of both streams, waiting on the
GPU; the CPU time covers OpenH264 decode and conversion to BGRA. Neither
includes getting the picture on screen, where the CPU path also copies each
frame (8 MB at 1080p, 33 MB at 4K) through a pipe and draws it on the CPU.

Exit status: 0 pass (or skip on a Mac without hardware H.264 decode), 1 fail.
Version: 0.1.0
"""
import argparse
import json
import os
import queue
import struct
import subprocess
import sys
import tempfile
import threading
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CASES = {
    "1080p-420": (1920, 1080, False),
    "1080p-444": (1920, 1080, True),
    "4k-420": (3840, 2160, False),
    "4k-444": (3840, 2160, True),
}
#= Tolerances, in 8-bit levels. GPU against CPU decode of the same stream:
#= the Metal kernel follows the Rust reference, so this stays tight. Against
#= the source pattern: H.264 is lossy, so allow for coding error.
TOL_CPU = 4
TOL_SOURCE = 40
KIND_EVENT, KIND_VIDEO = 2, 5


def log(msg):
    print(msg, file=sys.stderr, flush=True)


def fixture(case, out, frames):
    """Encode one case; returns the example's JSON summary."""
    width, height, dual = CASES[case]
    cmd = ["cargo", "run", "--quiet", "--release", "--locked", "-p", "gliff-macos",
           "--example", "video_fixture", "--", "--out", str(out),
           "--width", str(width), "--height", str(height), "--frames", str(frames)]
    if dual:
        cmd.append("--dual")
    log(f"== {case}: encoding {frames} frames ({width}x{height}{' 4:4:4' if dual else ' 4:2:0'})")
    result = subprocess.run(cmd, cwd=ROOT, check=True, capture_output=True, text=True, timeout=900)
    return json.loads(result.stdout.strip().splitlines()[-1])


class Viewer:
    """The viewer helper over its packet bridge (kind:u8, len:u32 LE, payload)."""

    def __init__(self, native):
        env = dict(os.environ, GLIFF_VIDEO_PROBE="1")
        self.child = subprocess.Popen([str(native), "app", "Gliff video check"], env=env,
                                      stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.events = queue.Queue()
        self.stderr = []
        threading.Thread(target=self._read, daemon=True).start()
        self.drainer = threading.Thread(target=self._drain, daemon=True)
        self.drainer.start()

    def _read(self):
        out = self.child.stdout
        while True:
            header = out.read(5)
            if len(header) < 5:
                self.events.put(None)
                return
            kind, length = header[0], struct.unpack("<I", header[1:])[0]
            body = out.read(length)
            if kind == KIND_EVENT:
                try:
                    self.events.put(json.loads(body))
                except json.JSONDecodeError:
                    pass

    def _drain(self):
        for line in self.child.stderr:
            self.stderr.append(line.decode(errors="replace"))

    def send(self, kind, payload):
        self.child.stdin.write(bytes([kind]) + struct.pack("<I", len(payload)) + payload)
        self.child.stdin.flush()

    def command(self, value):
        self.send(KIND_EVENT, json.dumps(value).encode())

    def wait_for(self, types, timeout=15):
        """The next event whose type is in `types` (others are skipped)."""
        while True:
            ev = self.events.get(timeout=timeout)
            if ev is None:
                raise RuntimeError("viewer exited: " + "".join(self.stderr[-5:]))
            if ev.get("type") in types:
                return ev

    def close(self):
        try:
            self.child.stdin.close()
            self.child.wait(timeout=10)
        except Exception:
            self.child.kill()
            self.child.wait()
        #= Let the last stderr lines (the "(GPU)" first-frame line) arrive.
        self.drainer.join(timeout=5)


def stats(values):
    values = sorted(values)
    if not values:
        return 0.0, 0.0
    p95 = values[max(0, -(-len(values) * 95 // 100) - 1)]
    return sum(values) / len(values), p95


def near(a, b, tol):
    return all(abs(x - y) <= tol for x, y in zip(a, b))


def run_case(native, case, frames, workdir):
    width, height, dual = CASES[case]
    out = Path(workdir, case)
    summary = fixture(case, out, frames)
    viewer = Viewer(native)
    failures = []
    try:
        caps = viewer.wait_for({"video_caps"})
        if caps.get("hardware") is not True:
            return {"case": case, "skipped": "this Mac reports no hardware H.264 decode"}
        viewer.command({"type": "video_config", "width": width, "height": height,
                        "dual": dual, "scale_milli": 2000})
        times, probe = [], None
        for k in range(frames):
            main = Path(out, f"main-{k:04d}.h264").read_bytes()
            aux = Path(out, f"aux-{k:04d}.h264").read_bytes() if dual else b""
            packet = struct.pack("<QII", k, len(main), len(aux)) + main + aux
            viewer.send(KIND_VIDEO, packet)
            ev = viewer.wait_for({"decoded", "decode_error", "gpu_failed"})
            if ev["type"] != "decoded":
                failures.append(f"frame {k}: {ev['type']}: {ev.get('message')}")
                break
            if ev.get("frame_id") != k:
                failures.append(f"frame {k}: answered for frame {ev.get('frame_id')}")
                break
            times.append(float(ev.get("ms", 0)))
            if "probe" in ev:
                probe = ev["probe"]
    finally:
        viewer.close()
    if not failures:
        if not any("(GPU)" in line for line in viewer.stderr):
            failures.append("the viewer never reported a GPU frame on screen")
        if probe is None:
            failures.append("no pixels were read back (GLIFF_VIDEO_PROBE)")
        else:
            expected, cpu = summary["expected"], summary["cpu"]
            names = ["red", "green", "blue", "white", "column red", "column blue"]
            for i, name in enumerate(names):
                if not near(probe[i], cpu[i], TOL_CPU):
                    failures.append(f"{name}: GPU {probe[i]} vs CPU {cpu[i]} (±{TOL_CPU})")
                #= 4:2:0 blurs the one-pixel columns by design: only check them for 4:4:4.
                if (i < 4 or dual) and not near(probe[i], expected[i], TOL_SOURCE):
                    failures.append(f"{name}: GPU {probe[i]} vs pattern {expected[i]} (±{TOL_SOURCE})")
            if dual:
                red, blue = probe[4], probe[5]
                if not (red[0] > red[2] + 100 and blue[2] > blue[0] + 100):
                    failures.append(f"4:4:4 columns blurred together: red {red}, blue {blue}")
    gpu_avg, gpu_p95 = stats(times)
    return {
        "case": case, "frames": frames, "failures": failures,
        "gpu_ms": {"avg": round(gpu_avg, 2), "p95": round(gpu_p95, 2)},
        "cpu_ms": {k: round(v, 2) for k, v in summary["cpu_decode_ms"].items()},
        "probe": probe, "expected": summary["expected"], "cpu": summary["cpu"],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--native", type=Path,
                        default=Path.home() / "Applications/Gliff.app/Contents/MacOS/gliff-native",
                        help="viewer helper (default: the installed app's)")
    parser.add_argument("--cases", default=",".join(CASES), help=f"comma list from {', '.join(CASES)}")
    parser.add_argument("--frames", type=int, default=120)
    parser.add_argument("--json", type=Path, help="also write the results here")
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("run this on a Mac, in a desktop session")
    if not args.native.is_file():
        parser.error(f"no viewer helper at {args.native}; run `make macos` first")
    cases = [c.strip() for c in args.cases.split(",") if c.strip()]
    unknown = [c for c in cases if c not in CASES]
    if unknown:
        parser.error(f"unknown cases: {', '.join(unknown)}")
    if not 2 <= args.frames <= 1000:
        parser.error("--frames must be 2 to 1000")
    with tempfile.TemporaryDirectory(prefix="gliff-video-check-") as workdir:
        results = [run_case(args.native, case, args.frames, workdir) for case in cases]
    print(f"\n{'case':<11} {'GPU avg':>8} {'GPU p95':>8} {'CPU avg':>8} {'CPU p95':>8}  result")
    failed = False
    for r in results:
        if "skipped" in r:
            print(f"{r['case']:<11} {'-':>8} {'-':>8} {'-':>8} {'-':>8}  SKIP: {r['skipped']}")
            continue
        status = "PASS" if not r["failures"] else "FAIL"
        failed |= bool(r["failures"])
        print(f"{r['case']:<11} {r['gpu_ms']['avg']:>8} {r['gpu_ms']['p95']:>8} "
              f"{r['cpu_ms']['avg']:>8} {r['cpu_ms']['p95']:>8}  {status}")
        for f in r["failures"]:
            print(f"    - {f}")
    print("(ms per frame: GPU = VideoToolbox decode + Metal recombine; CPU = OpenH264 decode + BGRA conversion)")
    if args.json:
        args.json.write_text(json.dumps(results, indent=2))
    print("VIDEO CHECK FAIL" if failed else "VIDEO CHECK PASS")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
