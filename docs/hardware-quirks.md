# Hardware and driver quirks

gliff runs its pixel work in Vulkan compute and its H.264 codec through
VA-API, with a CPU fallback (OpenH264 in `gliff-sw`) for machines without
the codec: in the default `--video gpu` mode a failed device open, a VA-API
driver without H.264 High encode (or decode, on the client), a failure to
create the encoder, or a failed first encode drops to the CPU tier with a
log line, and `--video cpu` / `GLIFF_VIDEO=cpu` forces it. Driver behaviour
differs, so this file records what we have found and where more testing is
needed. The detailed findings come from AMD:

- GPU: AMD Radeon RX 7600 (Navi 33) and Granite Ridge iGPU (Ryzen 9 9955HX).
  Drivers: Mesa RADV 26.2 for compute, Mesa `radeonsi` VA-API 26.2 (libva
  2.24) for the codec, kernel 7.2.
  Compositor: Hyprland 0.56.2.

The same path also runs on Intel Panther Lake (see the Intel section).

`gliff-probe gpu` prints the Vulkan device, the VA-API driver and its H.264
capabilities; `gliff-probe surfaces` proves the hand-off between the two.

## Confirmed on AMD (RADV + radeonsi)

### Codec surfaces must be VA-API allocations
The `radeonsi` encoder refuses an external linear dmabuf as its input (found
on the project's first day with GBM buffers). A surface the driver allocated,
exported with `vaExportSurfaceHandle` (`DRM_PRIME_2`, separate layers, read
and write) and written through its dmabuf encodes fine. Every codec surface
is therefore VA-owned and imported into Vulkan, never the reverse. The
export gives one object holding both NV12 planes with a tiled modifier
(`0x200000018601b04` on Navi 33); Vulkan imports it as one
`G8_B8R8_2PLANE_420_UNORM` image with two explicit plane layouts.

### The imported surface takes STORAGE usage
RADV accepts `STORAGE | SAMPLED | TRANSFER` with `MUTABLE_FORMAT |
EXTENDED_USAGE` on the imported NV12 image, so the split shader writes the
encoder input in place through R8 / R8G8 plane views, and the readback
through `vaGetImage` matches the CPU split exactly. When a driver refuses
STORAGE the pipeline writes a Vulkan-owned NV12 image and copies it into
the surface plane by plane (`Image::copy_nv12_from`); `gliff-probe surfaces`
prints which path was taken.

### Packed headers are accepted
`VAConfigAttribEncPackedHeaders` reports SEQUENCE | PICTURE | SLICE | MISC |
RAW (0x1f), so gliff writes the SPS, PPS and every slice header itself
(`gliff-va/src/h264/writer.rs`), unescaped with `has_emulation_bytes = 0`,
and the driver inserts them into the coded buffer: an IDR access unit comes
back with the SPS and PPS in front. The encoder still prepends its own copy
when a driver leaves them out.

### Rate control
`VAConfigAttribRateControl` reports CQP | CBR | VBR | QVBR; gliff uses CBR
with `disable_frame_skip` and `disable_bit_stuffing` set, a `window_size`
of the rate controller's VBV and an HRD buffer of the same size. A rate
change re-sends the three misc buffers with the next frame and takes effect
at once (`gliff-probe roundtrip --adapt`).

### Encode entrypoint and limits
`VAEntrypointEncSlice` (the full-featured one), maximum 4096x4096 for both
encode and decode. Larger outputs are scaled to fit.

### Decode
`VAEntrypointVLD` with one surface per DPB slot plus a spare; the decoder
hands VA-API the whole slice NAL and `slice_data_bit_offset = 8 +` the
parsed header length. `radeonsi` parses the headers itself and ignores the
offset; Intel uses it.

### Cross-API synchronisation
Vulkan and VA-API share no fences. The split submission blocks on its fence
before `vaBeginPicture`, and `vaSyncSurface` runs before the recombine
reads a decoded surface. Ownership crosses with `VK_QUEUE_FAMILY_FOREIGN_EXT`
barriers (`EXTERNAL` when the extension is missing), contents preserved.

### Host memory for readback should be cached
Reading 8 MB of BGRA back through write-combined host memory took ~25 ms;
through `HOST_CACHED` memory it takes ~1 ms. `HostBuffer` prefers cached
memory and falls back to write-combined.

## Intel (ANV + iHD)

The Intel path uses Mesa ANV for the Vulkan compute stages and
`intel-media-driver` (iHD) for the codec, which covers every generation from
Skylake on, including Lunar Lake, Battlemage and Panther Lake. Vulkan Video
is not used, so no `ANV_DEBUG` flags are needed.

It has run on Panther Lake (Xe3): `gliff-probe gpu`, `surfaces`,
`roundtrip` and `pipeline` pass, and so does `scripts/e2e.sh`. Earlier
generations are not yet run. The items below are what the code expects of
iHD, from the Mesa and libva sources; they are the first things to check, in
that probe order, when a new generation fails:

- **Entrypoint.** On Gen12 and later iHD offers `VAEntrypointEncSliceLP`
  (VDEnc) for H.264, which gliff takes when `EncSlice` is absent. CBR on it
  needs the HuC firmware; a FAIL with "no CBR" means `dmesg | grep -i huc`.
- **Packed headers.** iHD requires them and gliff always supplies them.
- **The hand-off.** iHD exports NV12 with a Y-tiled or Tile4 modifier; ANV
  must import it with STORAGE for the direct path, else the copy path runs.
  `gliff-probe surfaces` reports both.
- **Slice data offset.** iHD reads `slice_data_bit_offset`; the value follows
  ffmpeg's convention (NAL header bits plus the unescaped header bits).
- **Coded buffer status.** `VA_CODED_BUF_STATUS_*` bits are logged; a
  `BAD_BITSTREAM` status fails the encode and the server falls back to CPU.

## Not yet tested anywhere
- Intel generations before Panther Lake.
- NVIDIA has no VA-API encoder; both ends take the CPU tier there. Decode
  through `nvidia-vaapi-driver` is untried.
- Baseline-profile streams (the CPU tier's output) through the VA-API High
  decode config: radeonsi accepts them, and so does iHD on Panther Lake
  (the e2e cpu-server -> gpu-client case); other drivers are unverified.
- Native 4:4:4 encode (HEVC 4:4:4 on Intel) to retire the dual-stream split.
- Tiled capture buffers. The capture ring prefers linear modifiers and the
  dmabuf import passes the modifier through, but only linear has been run.
- Multiple GPUs / non-renderD128 nodes: `--render-node` matches the DRM
  device number to the Vulkan physical device and opens the VA display on
  the same node, untested with two GPUs.

## OpenH264 (the CPU tier)

- **The decoder holds one picture for non-Baseline streams.** OpenH264
  skips its reorder buffer only for Baseline-profile streams, so the CPU
  encoder emits Baseline and its streams display with no delay. A
  GPU-encoded (High-profile) stream comes out one access unit late; the
  client drains the held picture after 150 ms of stream silence
  (`Decoder::flush`), which does not disturb later decoding.
- **Per-decode flushing breaks the reference chain.** The `openh264`
  crate's default `Flush::Flush` ejects the reference picture of a
  low-delay stream (dsOutOfMemory on the fourth frame of a GPU-encoded
  stream); the decoder runs with `Flush::NoFlush`.
- **CBR is soft without frame skipping.** The encoder disables
  `skip_frames` so every capture yields a frame (the AVC444 pair must stay
  in step), which OpenH264 says weakens its bitrate cap. The server's own
  `RateController` adapts the target from ack timing on top.

## Hyprland cursor capture (0.56.2, and upstream main as of 2026-09-02)

The server captures the remote cursor with an `ext-image-copy-capture-v1`
cursor session. Three Hyprland behaviours shape how `hypr-capture` drives it:

- **Every shared cursor image is fully transparent.**
  `CCursorshareSession::render()` draws the cursor texture only when the
  pointer image has both a buffer and a surface set, and the pointer manager
  never sets both. The buffer is cleared to `{0,0,0,0}` instead, or to opaque
  black when the pointer is on another output. The client therefore treats an
  image with no visible shape as "no remote image" and shows the default
  pointer. Upstream fix: in `render()`, take
  `Pointer::mgr()->getCurrentCursorTexture()` and draw it when non-null.
- **Surface cursors have no constraints.** `calculateConstraints` records the
  format and size only for shm buffer cursors (hyprcursor shapes). If the
  pointer shows a client-provided cursor surface (a terminal's I-beam, say)
  when the capture session is created, the format is invalid and Hyprland
  drops the capture session without sending `stopped`. The capture thread
  detects this with a `wl_display.sync` after creation and recreates the
  cursor session on the next cursor change. If the surface cursor appears
  later, the size stays at the previous value and `capture` fails with
  `stopped`, which is not a real stop: constraints arrive again on the next
  cursor change, so the thread waits for `done`.
- **Constraints arrive before the in-flight frame completes.** On a cursor
  change Hyprland sends `buffer_size` + `done`, then `ready` for the pending
  frame; the next frame only completes on the following change. The capture
  thread must not destroy the in-flight frame on `done`, or every shape after
  the first is lost.

Hyprland also sends the hotspot in logical units while the image is in
physical pixels, so on a scaled output the hotspot will be off once real
images arrive.

- **Removing a monitor under a live cursor session aborts Hyprland.**
  `output remove` on a headless output unmaps its layer surfaces, which
  refocuses and re-renders the cursor; `CCursorshareSession::copy()` then
  renders into the vanishing monitor and `beginRender` aborts (SIGABRT, seen
  three times on 0.56.2; Hyprland restarts in safe mode). The server ends the
  capture thread and only then removes the headless output. Ending the
  thread with a plain flush is not enough: the destroy requests sit in
  Hyprland's queue while `output remove` arrives on the hyprctl socket, and
  Hyprland can handle the remove first. The capture thread therefore ends
  with a roundtrip, which blocks until Hyprland has processed the destroys.

## Hyprland with the Lua config (Omarchy)

`hyprctl keyword` is rejected (`keyword can't work with non-legacy parsers`).
`hypr-ipc` falls back to `eval hl.monitor({ output = ..., mode = ...,
position = "auto", scale = ... })`, which applies at runtime. `output create
headless` and `output remove` are hyprctl commands and work with both config
types. The server reads back the mode Hyprland applied instead of assuming
the request took effect.

## Hyper-V guests (no render node)

A Hyper-V guest has only the `hyperv_drm` display node (`/dev/dri/card1`)
and no `/dev/dri/renderD*`. Hyprland renders in software there and does not
offer `zwp_linux_dmabuf_v1`, so there is nothing to allocate a dmabuf on or
hand one to. The capture thread then logs one warning naming the reason (no
dmabuf global, or the GBM device could not be opened on the render node) and
allocates its ring as wl_shm buffers over memfds; the compositor copies each
frame into them. The server sees the missing render node before opening
Vulkan or VA-API and takes the CPU (OpenH264) tier with its own warning.
Without wl_shm either, capture fails with an error naming both. Observed on
gliff 0.3.1 guests, where the session previously ended with "compositor does
not offer zwp_linux_dmabuf_v1".

QEMU guests with `virtio-vga` and no 3D (virgl off) are a variant: they have
a render node and Hyprland offers dmabuf, but GBM cannot allocate on it
(`DRM_IOCTL_MODE_CREATE_DUMB failed: Permission denied`). A GBM allocation
failure drops the ring to wl_shm the same way, with one warning.

## Known limitations recorded from code review (not yet fixed)

- **Instance discovery tie-break.** `hypr-ipc` picks the newest instance
  whose socket answers, so dead leftovers (a killed nested Hyprland) are
  skipped; two live instances started within the same coarse filesystem
  timestamp are still tie-broken by name, which could pick the older one.
- **`wl_output` bound at version 4.** A compositor offering an older `wl_output`
  would fail to bind. Hyprland always offers v4.
- **Capture dmabuf uses one buffer-object fd for all planes.** Correct for the
  single-plane XRGB/ARGB formats we select; wrong if a multi-fd planar format is
  ever chosen.
