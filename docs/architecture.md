# gliff architecture and design

gliff remote-desktops a Hyprland or macOS session to a Linux or macOS client over SSH.
This document covers what is built, the design choices behind it, and what is
still pending. Building, running a development setup and the testing tools
are in `docs/development.md`.

## Data flow

```
        server (remote host)                         client (local)
  ┌─────────────────────────────┐              ┌──────────────────────────────┐
  Hyprland output                │              │  GTK4 window (gtk::Picture)
    │ ext-image-copy-capture     │              │        ▲ GdkDmabufTexture
    ▼                            │              │        │
  GBM dmabuf ─ Vulkan import ─►  │              │   BGRX dmabuf (Vulkan export)
    │ VkImage (sampled)          │              │        ▲
    ▼ split.comp (BT.709 + AVC444)│             │   recombine.comp
  main NV12 + aux NV12           │   protocol   │        ▲
    │ (VA-API surfaces, imported)│  over TCP    │   two NV12 ◄ two VA-API decoders
    ▼ two VA-API H.264 encoders  │   or ssh     │        ▲ (surfaces, imported)
  VideoFrame{main,aux} ──────────┼──────────────┼────────┘ payloads
                                 │              │
  hypr-input ◄ Key/Pointer ◄─────┼──────────────┼──◄ GTK event controllers
```

Nothing touches pixels on the CPU. On the server the captured dmabuf is
imported once per ring buffer, the split shader writes the encoders' input
surfaces (VA-API allocations, exported as dmabufs and imported into Vulkan
once), and the VA-API encoders read them in place; only the coded bytes come
back to the CPU. On the client the two VA-API decoders write their surfaces,
the recombine shader samples them through the same kind of import and
writes a linear BGRX image, and GTK imports that image as a dmabuf texture.

Without a VA-API H.264 codec, either end falls back to the CPU tier (`gliff-sw`,
`--video cpu`): the server maps the captured buffer and converts BGRA to
I420 in one fused fixed-point pass (AVX2 row kernels chosen at run time,
rows spread over rayon), OpenH264 encodes it on up to four threads, and the
client converts the decoded I420 straight back to BGRA the same way. The
CPU tier prefers one 4:2:0 stream, since the second stream doubles the
encode work; `--full-chroma` on a CPU server keeps Dual420 for a GPU client.

Keyboard, pointer, resize, frame acks and keyframe requests flow client→server;
video, cursor and pongs flow server→client.

## macOS path

The macOS client uses an AppKit window; the host captures a physical display
through ScreenCaptureKit and injects input through CoreGraphics. A small Swift
bridge talks to `gliff-macos` over bounded local packets. Rust retains the
existing CBOR protocol and SSH transport, so either platform can connect to
the other without a protocol version change. Mac hosts prefer VideoToolbox
hardware H.264 for Single420. The Swift encoder requires hardware and confirms
the selected session's hardware flag. It converts length-prefixed NAL units
to Annex B and includes SPS/PPS on keyframes. One encode request at a time and
per-packet autorelease pools and a three-surface pixel-buffer pool limit bound
retained buffers. Startup/runtime failures
fall back to OpenH264 with a keyframe and updated pipeline metadata. Explicit
CPU mode and optional Dual420 hosting use OpenH264; Mac viewers still decode
through OpenH264. The GPU wire tier identifies hardware encoding here; on
Apple Silicon this uses the media encoder rather than a 3D shader pipeline.

The native connection picker browses `_gliff._tcp` through Bonjour and resolves
up to 128 services. Linux's desktop-session discovery service execs an Avahi
publisher advertising the user's SSH endpoint; it never initializes capture.
Selection passes a hostname and port to Rust, where network-provided targets
are validated before invoking SSH. Native askpass handles SSH host confirmation
and authentication. Browsing and cancelling the picker do not start video.

Capture scale accounts for macOS logical display coordinates, including Retina
and downscaled streams. Both sides translate hardware key positions to and from
Linux evdev; Command maps to Super. macOS pasteboard providers request text only
on paste and use the existing chunked clipboard transfer engine.

Linux software capture uses `wl_shm`; this avoids dmabuf mapping failures on
NVIDIA and removes the software capture path's need for a DRM device. The GPU
path continues to capture dmabufs.

## Crates

| Crate | Role | `unsafe` |
|---|---|---|
| `gliff-proto` | wire messages, framing header, CPU reference for colour and the AVC444 split/recombine | none |
| `gliff-transport` | `Framed` length-prefixed IO with out-of-band payloads (`write_vectored`), the clipboard transfer engine and file spool, ssh spawn | none |
| `hypr-ipc` | Hyprland control socket (instance discovery, outputs, options) | none |
| `hypr-wl` | shared Wayland plumbing (connect, globals, output/seat tracking, calloop runner) | none |
| `hypr-capture` | output + cursor capture into GBM dmabufs on a calloop thread | none |
| `hypr-input` | virtual keyboard (xkb state) + virtual pointer + clipboard bridge (mime types and pipes) on calloop threads | none |
| `gliff-va` | libva display, H.264 encode/decode contexts on driver-owned surfaces, dmabuf export, the H.264 header parser and writer | yes, libva calls through committed bindgen output |
| `gliff-vk` | Vulkan device, dmabuf import/export, split/recombine compute, the pipeline that hands surfaces to `gliff-va` | yes, Vulkan API calls |
| `gliff-macos` | native AppKit client, ScreenCaptureKit host and CoreGraphics input bridge | no Rust unsafe; native APIs live in Swift |
| `gliff-sw` | CPU fallback with OpenH264 encode/decode, fused BGRA<->I420 conversion | the encoder trace level through the raw API, and the AVX2 row kernels (pointer loads and stores) |
| `gliff-server` | ties capture+input+encoder to the protocol; `--stdio`/`--listen` | none |
| `gliff` | GTK4/libadwaita UI, decode worker | one block: hands GTK a dmabuf fd |
| `gliff-probe` | environment checks and the headless test client | none |

`gliff-vk` wraps `ash` and `gliff-va` wraps libva; both APIs are `unsafe`
because they cannot check lifetimes or synchronisation. The crates expose
plain Rust types (`Gpu`, `Encoder`, `Decoder`, `DisplayFrame`, `Surface`).
Every crate with `unsafe` documents the safety requirements at each block.

## Key design choices

- **SSH is the only transport.** The server opens no port in production; the
  client spawns `ssh -T host gliff-server --stdio` and the protocol runs over
  that pipe. `--listen`/`--connect` exist for local development only and have no
  auth. Each frame is a header (body and payload lengths), a CBOR message with
  numeric keys, then any payload; large video and cursor payloads ride outside
  the CBOR body so the encoder output is sent with `write_vectored` and read
  straight into the decoder.

- **The protocol evolves without version bumps.** Peers ignore fields they do
  not know and skip whole messages they do not know, since the frame header
  covers the payload too. Anything new is sent only to a peer that listed the
  matching feature in the handshake. `PROTOCOL_VERSION` changes only for a
  break those rules cannot absorb; the rules are at the top of
  `crates/gliff-proto/src/msg.rs`.

- **Why not UDP.** Measured on 2026-09-14 between two Wi-Fi machines over
  Tailscale's direct path (MTU 1280, 330-380 Mbit/s of SSH throughput, RTT
  3-12 ms with contention spikes): a 1856x1238 Dual420 mirror ran at 58 fps
  with a worst frame-arrival gap of 71 ms while TCP retransmitted 0.7-6.5% of
  segments. A frame is ~115 packets, so raw UDP would damage most frames at
  those loss rates, and a working UDP path needs NACK or FEC, a jitter buffer
  and codec resilience, whose recovery also costs one RTT. TCP in SSH stays
  until a high-RTT lossy path becomes a primary use; then the candidate is
  video datagrams over UDP, AEAD-encrypted with a session key handed over
  SSH, while control, input and clipboard stay on the SSH channel. QUIC was
  considered and rejected: its TLS handshake duplicates the trust SSH
  already gives us, its per-stream retransmission is unneeded when the
  encoder can answer a lost keyframe with a fresh one, and its congestion
  controller would sit under ours. The UDP path still owes packetization to
  the path MTU, a nonce counter with a replay window, a NACK-or-re-keyframe
  policy, pacing, and one open UDP port on the server that silently drops
  unauthenticated packets. To measure
  again: client `RUST_LOG=info,gliff_vk=debug`, server
  `--server-bin 'env RUST_LOG=info,gliff_server=debug gliff-server'`, compare
  the `queued frame`, `output write completed`, and `decode + recombine`
  timestamps, and sample
  `ss -tin` on the server for retransmits.
  Measured again on 2026-09-22 under the `satellite` bench preset
  (2.5 Mbit/s shaped, 550 ms one-way, 10% random loss both ways, 45 s run):
  TCP holds the stream to 0.3 Mbit/s and 4.3 fps with a 2.3 s median frame
  latency (p95 4.8 s) and 34 arrival gaps over 100 ms — retransmission
  timeouts with 1.1 s RTT stall delivery for seconds while later frames wait
  in order. The ladder correctly sits on its lowest rung, so on such links
  the transport, not the adaptation, is the ceiling; that is the case a UDP
  media path addresses (drop or supersede late frames, recover only
  keyframes, delay-based control that ignores random loss).

- **4:4:4 by two 4:2:0 streams (AVC444).** Hardware H.264 encoders only do
  4:2:0, which blurs coloured text. gliff splits full 4:4:4 into a main stream
  (luma + even-position chroma) and an auxiliary stream (the dropped chroma), and
  recombines them bit-exactly on the client. Unlike RDP AVC444 v1 we do not
  average or filter the main chroma, because we own both ends, so the split and
  recombine are a lossless inverse pair. The CPU implementation in
  `gliff-proto::chroma` is unit-tested through 1080p and is the oracle the
  compute shaders are checked against (`gliff-probe pipeline`).
  `--low-bandwidth` drops to a single 4:2:0 stream with chroma upsampled on
  decode.

- **Vulkan for pixels, VA-API for the codec.** Import, colour conversion,
  split, recombine and export happen in Vulkan compute; the H.264 encode and
  decode happen in VA-API, which every AMD and Intel generation supports with
  driver rate control (Mesa's Intel Vulkan driver has no Vulkan Video encode
  on its newest chips and no bitrate control at all). Every codec surface is
  allocated by the VA-API driver, exported once as a dmabuf and imported into
  Vulkan, never the reverse: AMD's encoder refuses external input buffers.
  The two APIs meet at CPU waits, which the pipeline already had: the split
  submission blocks before the encoder starts, and `vaSyncSurface` precedes
  the recombine. gliff writes its own SPS, PPS and slice headers and hands
  them to the driver as packed headers, so one path serves both vendors.

- **Low-delay H.264.** The encoder emits IDR then P frames with one reference
  and no reordering (POC type 0), High profile, CABAC, CBR at the configured
  bitrate, with the SPS and PPS in front of every IDR so any keyframe is a
  random-access point. The decoder parses only what the hardware does not
  (SPS, PPS, the slice header) and manages a two-slot DPB with
  sliding-window marking.

- **Latest-wins, rate-paced.** The server keeps only the most recent captured
  frame and encodes it on one commanded cadence: the pace timer and the
  encoder's programmed frame rate are the same number (the ladder's fps cap,
  bounded by the sustainable encode time), so bits per frame match the frames
  that really leave. Sending is bounded by bytes in flight against
  `gain x delivered rate x base RTT` (never below two frames), not by an ack
  count, so latency does not cap the frame rate and a slow client cannot
  build a backlog. Frames are captured on demand, so a static screen costs
  nothing; a request stays outstanding while sending is blocked, so the frame
  that finally goes out is current.
  A separate writer task owns the socket write half and keeps at most one
  queued encoded frame; unsent cursor and pong messages are coalesced,
  clipboard messages are never coalesced (transfers are ordered and
  reliable, bounded by the engine's ack window), and stream configuration
  stays ordered with its video frames.
  The session keeps processing input while a video write is blocked, and a
  write that outlives a frame interval is a congestion signal of its own.

- **Rate control and the quality ladder** (`gliff-server/src/rate.rs`). A
  transport-independent `LinkEstimator` pairs acks by frame id and measures
  the base RTT (10 s windowed minimum, seeded by a handshake ping before the
  first frame), RFC 6298 mdev with an adaptive queueing threshold, and a
  BBR-style max-filtered delivered rate; samples from content-limited sends
  (a quiet screen, refinement passes) may only raise the estimate. The
  `RateController` follows Google Congestion Control's shape: start at
  3 Mbit/s, double per completed flight in slow start, grow ~8%/s or jump to
  0.85x the delivered rate, and cut to 0.85x the recent delivered rate only
  after two consecutive evaluations of sustained queueing while blocked —
  one TCP loss-recovery stall never cuts. A future UDP transport feeds the
  same estimator inputs (bytes sent, acks, a blocked marker).
  Above the controller a ladder of seven levels degrades frame rate first
  (60, 30, 15 fps), then drops the auxiliary chroma stream, then falls to
  10 fps, then halves the resolution, with 5 fps as the floor; the level is
  chosen from affordable bits per pixel at the measured rate. During slow start the ladder moves
  freely (the seed jump), so a LAN reaches full quality in under a second
  and a slow link lands on its level at the first real measurement; after
  that, step-ups need 1.25x headroom and a hold that doubles on a flap. The
  client stretches a reduced-resolution stream to the full-quality view size
  (`StreamConfig.view_width/height`) before fitting it to the window, so a
  rung change never shrinks the picture on screen, and an fps-only rung change reprograms the rate without
  an encoder rebuild, a keyframe, or a client decoder reset.
  When the screen is still and the link has room, the last frame is
  re-encoded (at most 8 times, 200 ms apart, stopping once a pass codes
  below a fifth of the frame budget), so a picture that arrived soft under
  a low starting budget converges to sharp.

- **Threading.** Each pipeline lives on one thread: the server loop and the
  client decode worker are current-thread tokio runtimes that own their
  `gliff-vk` objects. Capture and input each own a Wayland connection on their
  own calloop thread and talk to the async side through channels; the capture
  thread hands the whole `CapturedFrame` (its dmabuf) to the encoder, which
  releases the ring slot when the encode has finished. On both peers, reads and
  writes run on separate tasks so an input burst cannot starve video and a
  blocked write cannot block reads.

- **Client display.** The decode worker exports each finished BGRX image as a
  linear dmabuf and the UI wraps it in a `GdkDmabufTexture` on a
  `gtk::Picture`. GTK imports the dmabuf itself (through its GL/Vulkan renderer
  or, failing that, a CPU map), so the client needs no GL code and no fallback
  path of its own. The CPU tier hands GTK a memory texture from a small pool
  of pixel buffers instead. `paintable::layout` fits the frame to the widget
  using the GDK surface's fractional device scale. The client asks the
  decoder for the integer part of any enlargement: the recombine shader,
  or a CPU row copy, replicates pixels for that zoom and GTK handles the
  remaining fractional fit. This preserves sharp integer zooms while filling
  mirrored windows whose size is not an integer multiple of the stream.
  The picture and pointer mapping share the same layout. In fullscreen the
  header leaves the layout and slides in over the picture at the top edge.

- **Theme.** `theme.rs` reads Omarchy 4's `colors.toml` with the same lenient
  line parser and fallback chain as `omarchy-theme-color`, and emits a `:root`
  block that overrides the libadwaita palette variables (`--window-bg-color`,
  `--accent-bg-color`, `--headerbar-bg-color`, ...). Adwaita derives the rest
  (standalone accent, borders, shades) from those, so every widget follows.
  The light/dark scheme is forced from the palette's `mode` rather than the
  desktop setting, so the base stylesheet always matches the colours. Omarchy
  swaps the `current/theme` directory atomically on a switch, so a
  `GFileMonitor` on `current/` watches for that child and re-applies after a
  short debounce. With no Omarchy state directory the module does nothing.

- **Cursor.** The remote cursor is shown as the video widget's own cursor, so
  the local compositor draws it at the real pointer with no added latency; it is
  never baked into the video. An image with no visible shape falls back to the
  default pointer (see `hardware-quirks.md` for why Hyprland sends those).

- **Clipboard.** Lazy, typed and chunked; `gliff_proto::clipboard` has the
  rules and `gliff_transport::clipboard` the engine both peers run. A new
  selection is announced as an `Offer` of mime types (plus a file list when it
  holds a `text/uri-list`); the peer advertises the same on its own clipboard
  and sends a `Request` only when an application there pastes. Each offer
  carries a serial that requests must echo, so a request racing a clipboard
  change is refused rather than resolved against the wrong file list; the
  client allocates odd transfer ids and the server even ones, so an `Abort`
  is never ambiguous between directions. The item then
  streams as `Data` chunks of at most 256 KiB with four chunks in flight per
  `Ack`, so a large payload cannot stall video or buffer without bound; an
  in-memory item is capped at 32 MiB, a file at its declared size, and either
  side may `Abort`. The server bridges
  `ext-data-control-v1` on its own thread, moving only mime lists and pipe fds;
  the client bridges `gdk::Clipboard` with a lazy `ContentProvider` subclass,
  chunks crossing to the network thread through bounded channels. Files are
  never sent as URIs: the pasting side streams each one into a disk-backed
  spool directory and hands its applications a URI list pointing there. A
  spool is removed five minutes after its offer is replaced, so an
  application can still open what it was just handed, or when the session
  ends. A loop guard on each side (a marker mime type on the server's
  source, a check for our own proxy provider on the client) stops a proxy we
  set from being offered back, and from being read to serve the peer. A paste is a
  job: one that outlasts a quiet second is reported every 250 ms with bytes
  done, total and rate, until it ends. The client shows a bar with a cancel
  button over the video; the server, which has no window, posts a desktop
  notification with a progress bar and a Cancel action over
  `org.freedesktop.Notifications`. Cancel drops the fetch, which sends
  `Abort`. A failed paste is always reported, however short.

## Measurements

On an AMD Radeon RX 7600 (RADV for compute, radeonsi for the codec, Mesa
26.2), release build, 1920x1080 Dual420, from `gliff-probe roundtrip`,
2026-09-28:

| Stage | Time per frame |
|---|---|
| split + two encodes | 7 to 8.5 ms |
| two decodes + recombine | 4.2 to 4.8 ms |

No pixel work happens on the CPU at any resolution; the remaining cost is the
encode hardware itself, which serialises the two streams, so a 1080p Dual420
frame costs about two encodes' worth of time.

The CPU tier on a Ryzen 7 7840U (16 threads), Single420,
from `gliff-probe --video cpu roundtrip --single` on moving synthetic
content, 2026-09-25:

| Stage | 1080p | 4K |
|---|---|---|
| BGRA -> I420 (fused, AVX2, rayon) | 0.2 ms | 1.3 ms |
| encode, conversion included (OpenH264, 4 threads) | ~10 ms | ~35-43 ms |
| I420 -> BGRA (fused, AVX2, rayon) | 0.4 ms | 1.8 ms |
| decode, conversion included | ~6 ms | ~19 ms |

Before the fused kernels and the encoder threads the same runs took
30-40 ms / 105-150 ms to encode and 9-12 ms / 28-35 ms to decode, with
the conversion alone at ~4 ms / ~18 ms per side. The multi-slice stream
is about 8% larger at the same quality. In the `lan` bench at 1080p with
both ends on the CPU tier (a nested desktop, `--video cpu`,
`GLIFF_VIDEO=cpu`), the server's per-frame time fell from p50 32.7 ms to
21 ms, of which the capture-buffer read is 1.8 ms and the rest is
OpenH264 (49.5 ms on one thread), and the client's decode from p50
12.9 ms to 4.3 ms; the bench's damage cadence caps both runs near 30 fps. The server adapts the CBR
target to the link (`gliff-server/src/rate.rs`); a slow link gives up frame
rate first, then chroma, then resolution.

Shaped-link runs (`scripts/bench.sh`, nested Hyprland, 2560x1440 headless,
2026-09-21):

| Preset | Result |
|---|---|
| `lan` (no shaping) | first frame decoded 0.6 s, 60 fps level at 1.0 s, 59.6 fps, 38 Mbit/s, 0 gaps > 100 ms |
| `dsl` (20 Mbit/s, 10 ms one-way) | 50 fps, 15.8 Mbit/s, 0 gaps, no ladder flap |
| `satellite` (2.5 Mbit/s, 550 ms one-way, 10% random loss BOTH ways) | first frame decoded 4.0 s, settles on the 10 fps single-stream rungs; 60 decoded frames in 19.5 s vs 29.7 s for the pre-adaptation server. TCP loss recovery still stalls the stream (gaps of seconds); that is the head-of-line blocking a UDP media path would remove. |

## Pending and recommended improvements

Built and validated on AMD and on Intel Panther Lake: capture, input, VA-API
H.264 encode/decode, Dual420 and Single420, the server loop, the GTK client,
keymap upload, remote cursor, shortcut inhibit, reconnect, the clipboard
bridge (any mime type and files, lazily streamed), and the ssh path between
machines. Earlier Intel generations use the same path and are not yet run.
NVIDIA has no VA-API encoder and is not a target for the GPU tier.

Not yet built, roughly in priority order:

1. **Optional AV1 for outputs above 4096 wide.** The VCN H.264 encoder
   stops at 4096x4096 (the kernel amdgpu codec table, reported through
   VA-API as the maximum picture size, which `gliff-probe gpu` prints), so
   a 5K output streams at 4096x2304 today: the
   server scales the stream to the encoder maximum and the client scales it
   back up. AV1 and HEVC on the same engine reach 8192x4352. AV1 is the
   preferred second codec: it is royalty-free, and hardware supports it for
   encode on AMD VCN 4.0 (RDNA3, Ryzen 7040) and later, Intel Arc and
   Meteor Lake and later, and NVIDIA RTX 40 and later; for decode on AMD
   VCN 3.0 and later, Intel 11th generation and later, and NVIDIA RTX 30
   and later. H.264 stays as the codec every GPU has. The server picks AV1
   only when both ends list it in `ClientCaps.codecs`, and the size clamp
   stays as the guard for whichever codec is chosen. A native 4:4:4 profile
   on some driver would also retire the split.
2. **First frame on a truly static screen.** The 700 ms `Recapture` rescue
   needs verifying against a locked, unchanging screen (the nested bench
   screens always produce damage); if the compositor still withholds the
   frame, the fallbacks are a `debug:damage_tracking` toggle or a 1 px
   virtual-pointer nudge.
3. **Optional UDP media transport for high-RTT lossy links.** TCP over SSH
   stays the default and keeps working everywhere; this is an optional
   enhancement, worth building only when such a path becomes a real use.
   The evidence and the trigger conditions are in "Why not UDP" above: on
   the `satellite` preset TCP's loss recovery is the ceiling (0.3 Mbit/s,
   4.3 fps, multi-second head-of-line stalls), while `lan` and `dsl` show
   no transport limit. The shape is encrypted video datagrams with a
   session key handed over SSH, control and input staying on the SSH
   channel, late frames dropped rather than waited for, and a lost keyframe
   answered with a fresh keyframe rather than a retransmit. `LinkEstimator`,
   `RateController` and the ladder already take transport-neutral inputs
   (bytes sent, acks, a blocked marker) and carry over unchanged.
4. **Polish**: multi-output selection UI, a `--max-fps` server flag, and a
   lazy file spool (a FUSE mount the pasting application
   reads through, so its own copy dialog shows progress, instead of
   spooling every file before the URI list is handed over).

See `docs/hardware-quirks.md` for driver-specific behaviour and the low-severity
items surfaced by code review.

## Dependencies and binaries

The binaries are dynamically linked: `gliff-server`/`gliff-probe` need
libvulkan, libva, libva-drm, libgbm, libdrm, libwayland-client, libxkbcommon
and libc; the Vulkan loader `dlopen`s the GPU's ICD and libva the GPU's VA
driver; `gliff` additionally pulls the full GTK4 runtime. A normal Hyprland
desktop already has all of these (they are the `pkgbuild/PKGBUILD` `depends`). The Rust
side is `ash` (thin generated bindings, no C build step) and committed
bindgen output for libva (`crates/gliff-va/bindings/gen.sh` regenerates it)
plus the Wayland, GTK and async crates. The compute shaders are committed as
SPIR-V, so no shader compiler is needed to build; rerun
`crates/gliff-vk/shaders/build.sh` (needs `glslc`) after editing a shader.
