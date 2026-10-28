#!/usr/bin/env bash
# Audio capture/recovery tests using generated samples and a private PipeWire
# server. The policy-only session manager never enumerates hardware devices.
set -euo pipefail
cd "$(dirname "$0")/.."
for tool in pipewire wireplumber pw-dump pw-record pw-cat pw-link dbus-run-session setsid python3 cargo; do
    command -v "$tool" >/dev/null || { echo "Missing test dependency: $tool" >&2; exit 1; }
done
run_dir=$(mktemp -d)
pids=()
cleanup() {
    for pid in "${pids[@]}"; do kill -- -"$pid" 2>/dev/null || true; done
    for pid in "${pids[@]}"; do wait "$pid" 2>/dev/null || true; done
    rm -rf "$run_dir"
}
trap cleanup EXIT
mkdir -p "$run_dir/runtime" "$run_dir/config" "$run_dir/state" "$run_dir/cache" "$run_dir/data"
chmod 700 "$run_dir/runtime"
export XDG_RUNTIME_DIR="$run_dir/runtime" PIPEWIRE_RUNTIME_DIR="$run_dir/runtime" PIPEWIRE_REMOTE=pipewire-0
export PULSE_SERVER="unix:$run_dir/runtime/no-pulse"
export GLIFF_ISOLATED_PIPEWIRE=1 GLIFF_AUDIO_TEST_PCM="$run_dir/tone.f32"
setsid env XDG_CONFIG_HOME="$run_dir/config" pipewire -c /usr/share/pipewire/pipewire.conf >"$run_dir/pipewire.log" 2>&1 &
pids+=("$!")
for _ in {1..50}; do
    [ -S "$run_dir/runtime/pipewire-0" ] && break
    sleep 0.1
done
[ -S "$run_dir/runtime/pipewire-0" ] || { cat "$run_dir/pipewire.log" >&2; exit 1; }
setsid env XDG_CONFIG_HOME="$run_dir/config" XDG_STATE_HOME="$run_dir/state" XDG_CACHE_HOME="$run_dir/cache" XDG_DATA_HOME="$run_dir/data" \
    dbus-run-session -- wireplumber --profile policy --config-file /usr/share/wireplumber/wireplumber.conf >"$run_dir/policy.log" 2>&1 &
pids+=("$!")
python3 - <<'PY'
import array, math, os
samples = array.array('f')
for i in range(48000 * 10):
    value = 0.2 * math.sin(i * 2 * math.pi * 1000 / 48000)
    samples.extend([value, value])
with open(os.environ['GLIFF_AUDIO_TEST_PCM'], 'wb') as output:
    samples.tofile(output)
PY
cargo test --locked -p gliff-server raw_capture_relinks_and_recovers_from_graph_monitor_failure -- --ignored --nocapture
mkdir "$run_dir/mock"
cat > "$run_dir/mock/pw-record" <<'EOF'
#!/bin/sh
# Model a present recorder that fails immediately (daemon or --raw unavailable).
exit 1
EOF
cat > "$run_dir/mock/parec" <<'EOF'
#!/usr/bin/env python3
import os, sys, time
assert '--device=@DEFAULT_MONITOR@' in sys.argv
with open(os.environ['GLIFF_AUDIO_TEST_PCM'], 'rb') as source:
    while chunk := source.read(480 * 2 * 4):
        try:
            sys.stdout.buffer.write(chunk)
            sys.stdout.buffer.flush()
        except BrokenPipeError:
            break
        time.sleep(0.01)
EOF
chmod +x "$run_dir/mock/pw-record" "$run_dir/mock/parec"
PATH="$run_dir/mock:$PATH" GLIFF_AUDIO_MOCK_BACKEND=1 \
    cargo test --locked -p gliff-server an_installed_but_failing_pw_record_recovers_with_parec -- --ignored --nocapture
echo 'AUDIO E2E PASS: raw capture, relinking, graph recovery and monitor fallback'
