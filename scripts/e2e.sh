#!/usr/bin/env bash
# End-to-end test for gliff. Must run inside a Hyprland session (it starts a
# nested Hyprland as the device under test). GPU (Vulkan + VA-API) checks run
# when the machine has it; the CPU (OpenH264) checks always run.
#
# It exercises, and asserts PASS on:
#   1. gliff-probe protocols / gpu / surfaces / encode-decode round-trips (GPU + CPU)
#   2. gliff-probe pipeline: capture one frame and run the whole 4:4:4 path
#   3. server --listen --headless  + serve-test client  (Dual420 4:4:4, GPU)
#   4. server --listen --headless --low-bandwidth + serve-test (Single420, GPU)
#   5. the CPU tier matrix: cpu<->cpu, gpu server -> cpu client, cpu server ->
#      gpu client
#   6. text clipboard in both directions
#   7. mirrored output resize
#   8. a 1 MiB binary clipboard item in both directions (chunked)
#   9. copied files (a directory tree) in both directions, via the spool
#  10. a keymap sent mid-session is the one the compositor serves its clients
#  11. a keymap change mid-session reaches clients behind an input method
#      (fcitx5), when fcitx5 and foot are installed
#
# Exits non-zero on the first failure.
set -uo pipefail
cd "$(dirname "$0")/.."

fail() { echo "E2E FAIL: $*" >&2; cleanup; exit 1; }
PIDS=()
NEST_SIG=""
cleanup() {
    for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
    [ -n "$NEST_SIG" ] && pkill -9 -f "Hyprland .*e2e-hypr.lua" 2>/dev/null || true
}
trap cleanup EXIT

command -v Hyprland >/dev/null || fail "Hyprland not found"
[ -n "${WAYLAND_DISPLAY:-}" ] || fail "run inside a Wayland (Hyprland) session"

echo "== building (release) =="
cargo build --release --workspace >/dev/null 2>&1 || fail "build failed"
PROBE=target/release/gliff-probe
SERVER=target/release/gliff-server

echo "== starting nested Hyprland =="
CONF=$(mktemp --suffix=-e2e-hypr.lua)
# The detailed default wallpaper gives every run the same demanding frame.
cat > "$CONF" <<HYPR
hl.monitor({ output = "", mode = "1280x800", position = "auto", scale = 1 })
hl.config({
    misc = {
        force_default_wallpaper = 2,
        disable_splash_rendering = true,
    },
    ecosystem = {
        no_update_news = true,
        no_donation_nag = true,
    },
})
HYPR
before=$(ls "$XDG_RUNTIME_DIR/hypr" 2>/dev/null)
WAYLAND_DISPLAY="$WAYLAND_DISPLAY" HYPRLAND_INSTANCE_SIGNATURE= setsid Hyprland -c "$CONF" >/tmp/gliff-e2e-hypr.log 2>&1 &
sleep 6
# The nested instance is the directory that was not there before we started it.
NEST_SIG=$(comm -13 <(echo "$before" | sort) <(ls "$XDG_RUNTIME_DIR/hypr" | sort) | head -1)
[ -n "$NEST_SIG" ] || fail "nested Hyprland did not start"
export HYPRLAND_INSTANCE_SIGNATURE="$NEST_SIG"
export WAYLAND_DISPLAY=$(sed -n 2p "$XDG_RUNTIME_DIR/hypr/$NEST_SIG/hyprland.lock")
echo "   nested sig $NEST_SIG on $WAYLAND_DISPLAY"

damage() { for i in $(seq 1 80); do hyprctl notify 1 200 0 "e2e $i" >/dev/null 2>&1; sleep 0.1; done; }

echo "== 1. probe checks =="
$PROBE --instance "$NEST_SIG" protocols 2>/dev/null | grep -q "^PASS" || fail "protocols"
PROBE_GPU=$($PROBE gpu 2>/dev/null || true)
# The GPU checks below (surfaces, round trip) go through the VA-API codec, so
# they key on a VA-API encoder, not on the GPU tier as such: a machine that
# encodes through Vulkan Video runs the server's GPU tier but not these.
if echo "$PROBE_GPU" | grep -q "^PASS VA-API H.264 encode"; then
    HAS_GPU=1
    CLIENT_VIDEO=gpu
else
    HAS_GPU=0
    CLIENT_VIDEO=cpu
    if echo "$PROBE_GPU" | grep -q "^PASS Vulkan Video H.264 encode"; then
        echo "   Vulkan Video encodes here, VA-API does not; the VA-API GPU checks are skipped"
    else
        echo "   no GPU tier on this machine; GPU checks are skipped"
    fi
fi
if [ "$HAS_GPU" = 1 ]; then
    $PROBE surfaces 2>/dev/null | grep -q "^PASS Vulkan split" || fail "VA-API surface hand-off"
    $PROBE roundtrip 2>/dev/null | grep -q "^PASS min RGB PSNR" || fail "GPU codec round-trip"
fi
$PROBE --video cpu roundtrip 2>/dev/null | grep -q "^PASS min RGB PSNR" || fail "CPU codec round-trip"
echo "   probe checks PASS"

echo "== 2. capture->4:4:4 pipeline =="
# A fresh output per run: an idle headless output does not render again, so
# a second capture on the same output can wait past the probe's deadline.
run_pipeline() {
    local video=$1
    hyprctl output create headless e2ecap >/dev/null 2>&1
    sleep 1
    local HN
    HN=$(hyprctl monitors -j | python3 -c "import sys,json;print(next((m['name'] for m in json.load(sys.stdin) if 'e2ecap' in m['name'] or m['name']=='e2ecap'),''))")
    local pipe
    pipe=$($PROBE --instance "$NEST_SIG" --video "$video" pipeline --output "${HN:-e2ecap}" 2>/dev/null)
    hyprctl output remove "${HN:-e2ecap}" >/dev/null 2>&1
    echo "$pipe" | grep -q "^PASS" || fail "4:4:4 pipeline ($video)"
    echo "$pipe" | grep -q "^FAIL" && fail "4:4:4 pipeline ($video) ($(echo "$pipe" | grep '^FAIL' | head -1))"
}
[ "$HAS_GPU" = 1 ] && run_pipeline gpu
run_pipeline cpu
echo "   pipeline PASS"

run_server_test() {
    local port=$1; shift
    local label=$1; shift
    local client_video=$1; shift
    "$SERVER" --listen "127.0.0.1:$port" --headless --instance "$NEST_SIG" "$@" >/tmp/gliff-e2e-server.log 2>&1 &
    local sp=$!; PIDS+=("$sp")
    sleep 2
    damage & local dp=$!; PIDS+=("$dp")
    local out
    out=$(timeout 30 $PROBE --video "$client_video" serve-test --connect "127.0.0.1:$port" --frames 8 2>&1)
    kill "$dp" 2>/dev/null; kill "$sp" 2>/dev/null; sleep 1
    echo "$out" | grep -q "^PASS" || fail "$label ($(echo "$out" | tail -1))"
    echo "$out" | grep -q "^FAIL" && fail "$label ($(echo "$out" | grep '^FAIL' | head -1))"
    echo "   $label PASS"
}

if [ "$HAS_GPU" = 1 ]; then
    echo "== 3. server + client, Dual420 =="
    run_server_test 9040 "Dual420 stream" gpu

    echo "== 4. server + client, Single420 (--low-bandwidth) =="
    run_server_test 9041 "Single420 stream" gpu --low-bandwidth
else
    echo "== 3./4. GPU server tests skipped (no GPU tier) =="
fi

echo "== 4b. CPU tier matrix =="
run_server_test 9044 "cpu server -> cpu client" cpu --video cpu
if [ "$HAS_GPU" = 1 ]; then
    run_server_test 9045 "gpu server -> cpu client" cpu
    run_server_test 9046 "cpu server -> gpu client" gpu --video cpu
fi

echo "== 5b. sensitive clipboard selections are withheld =="
cargo test --locked -p gliff clipboard_ui::tests::password_manager_selection_cannot_be_offered_or_read -- --ignored >/tmp/gliff-e2e-sensitive.log 2>&1 || fail "sensitive clipboard selection"
echo "   sensitive clipboard selection PASS"

echo "== 6. text clipboard both directions =="
command -v wl-copy >/dev/null && command -v wl-paste >/dev/null || fail "wl-clipboard not installed"
# The probe offers its item after a few frames and requests the compositor's
# item on its own. Wait for both before pasting and killing anything. The
# copy each step starts with already offers the client's mime types, so wait
# for the client's content itself, not for a type.
wait_for() { for i in $(seq 1 60); do "$@" && return 0; sleep 0.25; done; return 1; }
pasted_is() { [ "$(wl-paste -n 2>/dev/null)" = "$1" ]; }
paste_matches() { wl-paste --type "$1" 2>/dev/null | cmp -s - "$2"; }
wait_recv() { for i in $(seq 1 60); do grep -q "$1" "$2" && return 0; sleep 0.25; done; return 1; }
wl-copy "e2e-clip-in" 2>/dev/null
"$SERVER" --listen 127.0.0.1:9042 --headless --instance "$NEST_SIG" >/tmp/gliff-e2e-server.log 2>&1 &
csp=$!; PIDS+=("$csp"); sleep 2
damage & cdp=$!; PIDS+=("$cdp")
clipf=$(mktemp)
GLIFF_SEND_CLIP="e2e-clip-out" timeout 20 $PROBE --video "$CLIENT_VIDEO" serve-test --connect 127.0.0.1:9042 --frames 200 >"$clipf" 2>&1 &
clipc=$!; PIDS+=("$clipc")
wait_for pasted_is "e2e-clip-out" || fail "server never offered the client's text (got: $(wl-paste -n 2>/dev/null))"
wait_recv "CLIP-RECV:" "$clipf" || fail "client never received the compositor's text"
pasted=$(wl-paste -n 2>/dev/null)
kill "$clipc" "$cdp" "$csp" 2>/dev/null
grep -q "CLIP-RECV: e2e-clip-in" "$clipf" || fail "compositor->client clipboard (got: $(grep CLIP-RECV "$clipf"))"
[ "$pasted" = "e2e-clip-out" ] || fail "client->compositor clipboard (got: $pasted)"
rm -f "$clipf"
echo "   clipboard both directions PASS"

echo "== 7. mirrored output resize and mid-session CPU fallback =="
resize_cases=(dmabuf)
if [ "$HAS_GPU" = 1 ]; then
    fallback_shim=$(mktemp --suffix=.so)
    cc -shared -fPIC -Wall -Wextra -Werror scripts/test-fixtures/fail-gbm-allocation.c \
        -o "$fallback_shim" $(pkg-config --cflags gbm) -ldl || fail "build GBM failure fixture"
    resize_cases+=(shm-fallback)
fi
for resize_case in "${resize_cases[@]}"; do
    set_monitor() { hyprctl eval "hl.monitor({ output = \"$1\", mode = \"$2\", position = \"auto\", scale = 1 })" >/dev/null 2>&1; }
    hyprctl output create headless e2emirror >/dev/null 2>&1
    sleep 1
    mirror=$(hyprctl monitors -j | python3 -c "import sys,json; print(next(m['name'] for m in json.load(sys.stdin) if 'e2emirror' in m['name']))")
    set_monitor "$mirror" 1280x800@60
    sleep 1
    server_env=()
    if [ "$resize_case" = shm-fallback ]; then
        server_env=("LD_PRELOAD=$fallback_shim")
    fi
    env RUST_LOG=info "${server_env[@]}" "$SERVER" --listen 127.0.0.1:9043 --output "$mirror" --instance "$NEST_SIG" >/tmp/gliff-e2e-mirror-server.log 2>&1 &
    msp=$!; PIDS+=("$msp"); sleep 1
    damage & mdp=$!; PIDS+=("$mdp")
    mirror_log=$(mktemp)
    timeout 30 "$PROBE" --video "$CLIENT_VIDEO" serve-test --connect 127.0.0.1:9043 --frames 30 >"$mirror_log" 2>&1 &
    mcp=$!; PIDS+=("$mcp")
    wait_for_mirror() {
        local pattern=$1
        for ((attempt = 0; attempt < 100; attempt++)); do
            grep -q "$pattern" "$mirror_log" && return 0
            kill -0 "$mcp" 2>/dev/null || break
            sleep 0.1
        done
        fail "mirror resize: missing '$pattern' (log: $mirror_log)"
    }
    wait_for_mirror 'first decoded frame ok'
    set_monitor "$mirror" 640x480@60
    wait_for_mirror 'reconfig to 640x480'
    set_monitor "$mirror" 1024x768@60
    wait_for_mirror 'reconfig to 800x600'
    wait "$mcp" || fail "mirror resize: probe failed (log: $mirror_log)"
    grep -q '^PASS decoded 30 frames' "$mirror_log" || fail "mirror resize: frames stopped (log: $mirror_log)"
    grep -q '^FAIL' "$mirror_log" && fail "mirror resize: failed assertion (log: $mirror_log)"
    kill "$mdp" "$msp" 2>/dev/null
    hyprctl output remove "$mirror" >/dev/null 2>&1
    if [ "$resize_case" = shm-fallback ]; then
        grep -q "using the GPU video pipeline" /tmp/gliff-e2e-mirror-server.log || fail "fallback test did not start on GPU"
        grep -q "capture fell back to wl_shm; falling back to the CPU pipeline" /tmp/gliff-e2e-mirror-server.log || fail "resize did not switch to CPU"
        rm -f "$fallback_shim"
    fi
    echo "   mirrored output resize ($resize_case) PASS"
done

echo "== 7b. rotated output capture and encoding =="
hyprctl output create headless e2erotate >/dev/null 2>&1
sleep 1
rotated=$(hyprctl monitors -j | python3 -c "import sys,json; print(next(m['name'] for m in json.load(sys.stdin) if 'e2erotate' in m['name']))")
hyprctl eval "hl.monitor({ output = \"$rotated\", mode = \"1280x800@60\", position = \"auto\", scale = 1, transform = 1 })" >/dev/null
sleep 1
$PROBE --video cpu --capture shm --instance "$NEST_SIG" pipeline --output "$rotated" >/tmp/gliff-e2e-rotate-cpu.log 2>&1 || fail "rotated CPU pipeline"
if [ "$HAS_GPU" = 1 ]; then
    $PROBE --instance "$NEST_SIG" pipeline --output "$rotated" >/tmp/gliff-e2e-rotate-gpu.log 2>&1 || fail "rotated GPU pipeline"
fi
hyprctl output remove "$rotated" >/dev/null 2>&1
echo "   rotated output CPU/GPU pipelines PASS"

echo "== 7c. odd-sized output capture and encoding =="
# The stream is the output cropped to even dimensions; the GPU split must
# drop the extra column and row rather than resample the whole picture.
hyprctl output create headless e2eodd >/dev/null 2>&1
sleep 1
odd=$(hyprctl monitors -j | python3 -c "import sys,json; print(next(m['name'] for m in json.load(sys.stdin) if 'e2eodd' in m['name']))")
hyprctl eval "hl.monitor({ output = \"$odd\", mode = \"1281x801@60\", position = \"auto\", scale = 1 })" >/dev/null
sleep 1
$PROBE --video cpu --capture shm --instance "$NEST_SIG" pipeline --output "$odd" >/tmp/gliff-e2e-odd-cpu.log 2>&1 || fail "odd-sized CPU pipeline"
if [ "$HAS_GPU" = 1 ]; then
    $PROBE --instance "$NEST_SIG" pipeline --output "$odd" >/tmp/gliff-e2e-odd-gpu.log 2>&1 || fail "odd-sized GPU pipeline"
fi
hyprctl output remove "$odd" >/dev/null 2>&1
echo "   odd-sized output CPU/GPU pipelines PASS"

echo "== 8. binary clipboard both directions (1 MiB, chunked) =="
# Different bytes each way, so a paste of our own copy cannot pass for the
# client's item.
blob=$(mktemp); head -c 1048576 /dev/urandom >"$blob"
cblob=$(mktemp); head -c 1048576 /dev/urandom >"$cblob"
recvf=$(mktemp); outf=$(mktemp)
wl-copy --type application/octet-stream <"$blob" 2>/dev/null
"$SERVER" --listen 127.0.0.1:9047 --headless --instance "$NEST_SIG" >/tmp/gliff-e2e-server.log 2>&1 &
bsp=$!; PIDS+=("$bsp"); sleep 2
damage & bdp=$!; PIDS+=("$bdp")
clipf=$(mktemp)
GLIFF_SEND_CLIP_FILE="$cblob" GLIFF_RECV_CLIP_FILE="$recvf" timeout 20 $PROBE --video "$CLIENT_VIDEO" serve-test --connect 127.0.0.1:9047 --frames 200 >"$clipf" 2>&1 &
bclipc=$!; PIDS+=("$bclipc")
wait_for paste_matches application/octet-stream "$cblob" || fail "server never offered the client's binary item"
wait_recv "CLIP-RECV-FILE:" "$clipf" || fail "client never received the compositor's binary item"
wl-paste --type application/octet-stream >"$outf" 2>/dev/null
kill "$bclipc" "$bdp" "$bsp" 2>/dev/null
grep -q "CLIP-RECV-FILE: 1048576 bytes" "$clipf" || fail "compositor->client binary clipboard (got: $(grep CLIP-RECV "$clipf"))"
cmp -s "$blob" "$recvf" || fail "compositor->client binary clipboard differs"
cmp -s "$cblob" "$outf" || fail "client->compositor binary clipboard differs ($(stat -c %s "$outf") bytes)"
rm -f "$clipf" "$blob" "$cblob" "$recvf" "$outf"
echo "   binary clipboard both directions PASS"

echo "== 9. copied files both directions (spooled) =="
srcd=$(mktemp -d); mkdir -p "$srcd/photos/sub"
head -c 300000 /dev/urandom >"$srcd/photos/a.bin"; echo "hello" >"$srcd/photos/sub/b.txt"; echo "solo" >"$srcd/solo.txt"
recvd=$(mktemp -d)
printf 'file://%s\r\nfile://%s\r\n' "$srcd/photos" "$srcd/solo.txt" | wl-copy --type text/uri-list 2>/dev/null
"$SERVER" --listen 127.0.0.1:9048 --headless --instance "$NEST_SIG" >/tmp/gliff-e2e-server.log 2>&1 &
fsp=$!; PIDS+=("$fsp"); sleep 2
damage & fdp=$!; PIDS+=("$fdp")
clipf=$(mktemp)
GLIFF_SEND_CLIP_FILES="$srcd/photos:$srcd/solo.txt" GLIFF_RECV_CLIP_DIR="$recvd" timeout 20 $PROBE --video "$CLIENT_VIDEO" serve-test --connect 127.0.0.1:9048 --frames 200 >"$clipf" 2>&1 &
fclipc=$!; PIDS+=("$fclipc")
# The client's files arrive as a URI list into the spool, not our sources.
spooled_uris() { local u; u=$(wl-paste --type text/uri-list 2>/dev/null); [ -n "$u" ] && ! grep -qF "file://$srcd/" <<<"$u"; }
wait_for spooled_uris || fail "server never offered the client's files"
wait_recv "CLIP-RECV-FILES:" "$clipf" || fail "client never received the compositor's files"
pasted_uris=$(wl-paste --type text/uri-list 2>/dev/null | tr -d '\r')
kill "$fclipc" "$fdp" "$fsp" 2>/dev/null
grep -q "CLIP-RECV-FILES: 5 entries" "$clipf" || fail "compositor->client files (probe said: $(grep -iE 'clip|offer|serv|error|panick' "$clipf" | tail -8 | tr '\n' ' '); server said: $(grep -iE 'clip|warn' /tmp/gliff-e2e-server.log | tail -5 | tr '\n' ' '))"
cmp -s "$srcd/photos/a.bin" "$recvd/photos/a.bin" && cmp -s "$srcd/photos/sub/b.txt" "$recvd/photos/sub/b.txt" && cmp -s "$srcd/solo.txt" "$recvd/solo.txt" || fail "compositor->client files differ"
spool=$(echo "$pasted_uris" | head -1 | sed 's|^file://||; s|/photos$||')
[ -n "$spool" ] && [ -d "$spool/photos" ] || fail "client->compositor files: no spool in URI list (got: $pasted_uris)"
cmp -s "$srcd/photos/a.bin" "$spool/photos/a.bin" && cmp -s "$srcd/photos/sub/b.txt" "$spool/photos/sub/b.txt" && cmp -s "$srcd/solo.txt" "$spool/solo.txt" || fail "client->compositor files differ"
rm -rf "$spool"
rm -rf "$clipf" "$srcd" "$recvd"
echo "   copied files both directions PASS"

echo "== 10. client keymap reaches compositor clients =="
"$SERVER" --listen 127.0.0.1:9049 --headless --instance "$NEST_SIG" >/tmp/gliff-e2e-server.log 2>&1 &
ksp=$!; PIDS+=("$ksp"); sleep 2
damage & kdp=$!; PIDS+=("$kdp")
keyf=$(mktemp); kout=$(mktemp)
$PROBE --instance "$NEST_SIG" keymap --secs 12 --caps Control_L >"$kout" 2>&1 &
kwatch=$!; PIDS+=("$kwatch"); sleep 1
GLIFF_SEND_KEYMAP_OPTIONS=ctrl:nocaps timeout 10 $PROBE --video "$CLIENT_VIDEO" serve-test --connect 127.0.0.1:9049 --frames 60 >"$keyf" 2>&1 \
    || fail "keymap client ($(tail -3 "$keyf" | tr '\n' ' '))"
wait "$kwatch"
kill "$kdp" "$ksp" 2>/dev/null
grep -q "^PASS" "$kout" || fail "client keymap not served ($(tr '\n' ' ' <"$kout"); client said: $(tail -3 "$keyf" | tr '\n' ' '))"
rm -f "$keyf" "$kout"
echo "   client keymap PASS"

echo "== 11. keymap change mid-session reaches clients behind an input method =="
# Omarchy runs fcitx5: with an input method active, Hyprland routes keys
# through its keyboard grab, which kept the first keymap of a virtual
# keyboard. The probe types evdev 16 under us ("q") then under fr ("a").
if command -v fcitx5 >/dev/null && command -v foot >/dev/null && command -v dbus-run-session >/dev/null; then
    typed=$(mktemp); klog=$(mktemp); imhome=$(mktemp -d)
    XDG_CONFIG_HOME="$imhome/config" XDG_CACHE_HOME="$imhome/cache" XDG_DATA_HOME="$imhome/data" setsid dbus-run-session -- sh -c "fcitx5 --disable notificationitem >/tmp/gliff-e2e-fcitx.log 2>&1 & sleep 3; foot -T e2e-typed sh -c 'cat > $typed'" >/dev/null 2>&1 &
    imp=$!; PIDS+=("$imp"); sleep 5
    # Earlier steps leave the pointer elsewhere: focus the terminal, put the
    # pointer on it (focus follows the pointer) and mirror its monitor.
    read -r taddr tx ty tw th tmon < <(hyprctl clients -j | python3 -c "import sys,json; c=next(c for c in json.load(sys.stdin) if c['title']=='e2e-typed'); print(c['address'], *c['at'], *c['size'], c['monitor'])")
    tout=$(hyprctl monitors -j | python3 -c "import sys,json; print(next(m['name'] for m in json.load(sys.stdin) if m['id']==$tmon))")
    hyprctl dispatch "hl.dsp.focus({ window = \"address:$taddr\" })" >/dev/null 2>&1
    hyprctl dispatch "hl.dsp.cursor.move({ x = $((tx + tw / 2)), y = $((ty + th / 2)) })" >/dev/null 2>&1
    "$SERVER" --listen 127.0.0.1:9050 --output "$tout" --instance "$NEST_SIG" >/tmp/gliff-e2e-server.log 2>&1 &
    isp=$!; PIDS+=("$isp"); sleep 2
    damage & idp=$!; PIDS+=("$idp")
    GLIFF_SEND_KEYMAP_SWITCH=1 timeout 20 $PROBE --video "$CLIENT_VIDEO" serve-test --connect 127.0.0.1:9050 --frames 12 >"$klog" 2>&1 \
        || fail "keymap switch client ($(tail -n 4 "$klog" | tr '\n' ' '))"
    sleep 1
    kill "$idp" "$isp" 2>/dev/null; kill -- -"$imp" 2>/dev/null
    got=$(tr '\n' ' ' <"$typed"); rm -f "$typed" "$klog"; rm -rf "$imhome"
    [ "$got" = "q a " ] || fail "keymap switch behind an input method: typed '$got', want 'q a '"
    echo "   keymap switch behind an input method PASS"
else
    echo "   skipped (needs fcitx5, foot and dbus-run-session)"
fi

echo "== 12. focus changes do not stack shortcut-inhibit handlers =="
"$SERVER" --listen 127.0.0.1:9050 --headless --instance "$NEST_SIG" >/tmp/gliff-e2e-server.log 2>&1 &
isp=$!; PIDS+=("$isp"); sleep 2
damage & idp=$!; PIDS+=("$idp")
ixdg=$(mktemp -d); txdg=$(mktemp -d); ilog=$(mktemp)
XDG_CONFIG_HOME="$ixdg" RUST_LOG=gliff=debug target/release/gliff --connect 127.0.0.1:9050 >"$ilog" 2>&1 &
icp=$!; PIDS+=("$icp")
# A second, unconnected client window to take focus away.
XDG_CONFIG_HOME="$txdg" target/release/gliff >/dev/null 2>&1 &
iop=$!; PIDS+=("$iop"); sleep 4
# The video takes focus only while the pointer is over it, so the other
# window goes to its own workspace, leaving the client under the pointer.
hyprctl dispatch "hl.dsp.window.move({ workspace = 2, window = \"pid:$iop\" })" >/dev/null; sleep 1
for _ in $(seq 1 10); do
    hyprctl dispatch "hl.dsp.focus({ window = \"pid:$iop\" })" >/dev/null; sleep 0.5
    hyprctl dispatch "hl.dsp.focus({ window = \"pid:$icp\" })" >/dev/null; sleep 0.5
done
kill "$icp" "$iop" "$idp" "$isp" 2>/dev/null; sleep 1
focused=$(grep -c "video focused" "$ilog")
inhibits=$(grep -c "compositor shortcut inhibit" "$ilog")
[ "$focused" -ge 10 ] || fail "client gained focus $focused times, expected at least 10"
# The compositor answers each inhibit and each restore once.
[ "$inhibits" -ge "$focused" ] && [ "$inhibits" -le $((2 * focused)) ] \
    || fail "$inhibits shortcut-inhibit notifications for $focused focus changes"
rm -rf "$ixdg" "$txdg" "$ilog"
echo "   shortcut inhibit PASS"

echo "E2E PASS: all checks passed"
