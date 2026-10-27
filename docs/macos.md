---
title: "gliff on macOS (gliff-mac)"
description: "Build, run, and understand gliff-mac, the native macOS client that views and drives a remote Hyprland session over ssh."
slug: gliff-macos
author: El (Capisterre Group)
type: technical-guide
date-created: 2026-10-27
date-modified: 2026-10-27
date-published:
source: https://github.com/omacom/gliff
domain: remote-desktop
url: https://github.com/omacom/gliff/blob/main/docs/macos.md
draft: true
tags: [gliff, macos, hyprland, remote-desktop, rust, appkit, objc2, openh264]
keywords: [gliff-mac, AVC444, 4:4:4 chroma, ssh, NSEvent, CALayer, evdev keycodes, XKB]
---

# gliff on macOS

`gliff-mac` is a native macOS **client** for gliff: it shows a remote
Hyprland session in a Mac window and sends your keyboard and pointer back,
using the same protocol and the same `gliff-server` as the Linux client.
Nothing changes on the remote machine.

```
gliff-mac user@host
```

| | Linux client (`gliff`) | macOS client (`gliff-mac`) |
|---|---|---|
| UI | GTK4 / libadwaita | AppKit via `objc2` |
| Video decode | VA-API + Vulkan, CPU fallback | CPU (OpenH264), 4:4:4 by default; VideoToolbox planned |
| Scaling | integer decoder zoom + GTK | Core Animation layer (nearest at integer scale) |
| Keymap sent | your live XKB keymap | `--keymap FILE` (XKB text), else the server's US |
| Clipboard | text, images, files | not yet |
| Tabs / several machines | yes | one machine per window (run it again for more) |
| Reconnect | yes | yes (backoff 1 s → 30 s) |

## Build

Requirements: macOS 11 or newer, Xcode command line tools
(`xcode-select --install`), and Rust 1.87+ (`rustup`).

```sh
git clone https://github.com/omacom/gliff.git
cd gliff
cargo build --release -p gliff-mac        # just the macOS client
./target/release/gliff-mac user@host
```

Build only this package (`-p gliff-mac`): the rest of the workspace is
Linux-only (Wayland, VA-API, Vulkan, GBM) and does not compile on macOS.

The first macOS build adds the `objc2` crates to `Cargo.lock`; commit the
updated lock file so the Linux CI, which builds with `--locked`, still
passes.

### App bundle

```sh
scripts/macos/bundle.sh                  # target/macos/Gliff.app, ad-hoc signed
scripts/macos/bundle.sh --universal      # arm64 + x86_64
scripts/macos/bundle.sh --sign "Developer ID Application: …"
open -n target/macos/Gliff.app --args user@host
```

Opened from Finder without arguments, Gliff asks for `user@host` and
remembers the last answer in
`~/Library/Application Support/gliff-mac/last-host`.

## Use

```
gliff-mac user@host                    # mirror the focused remote screen
gliff-mac --output DP-1 user@host      # mirror a named screen
gliff-mac --headless user@host         # a private screen sized to the window
gliff-mac --server-bin 'gliff-server --low-bandwidth' user@host
gliff-mac --ssh-opt=-J --ssh-opt=jump user@host
```

**Keyboard.** While the pointer is over the picture, every key goes to the
remote, Command shortcuts included. Command is sent as Super (Hyprland's
usual modifier); change it with `--command-as ctrl|alt|super`.
`Shift+Esc` hands the keyboard back without moving the pointer
(`--release-hotkey ctrl+alt+q`, `double-escape`, or `none`). Moving the
pointer off the picture, or clicking another app, also releases it.

macOS reserves some shortcuts before any app sees them: Command-Tab,
Command-Space (Spotlight), Mission Control and the Control-arrow Spaces
keys. Rebind them locally in System Settings › Keyboard › Keyboard
Shortcuts if you need them on the remote.

**Layout.** gliff sends keys by position and the remote's virtual keyboard
turns them into characters with the keymap the client sent. Without one,
the remote types US. To type another layout, make an XKB keymap once, on
any Linux machine with `xkbcli`:

```sh
xkbcli compile-keymap --layout de --variant mac > de-mac.xkb
gliff-mac --keymap de-mac.xkb user@host     # or: export GLIFF_KEYMAP=…
```

Apple ISO keyboards swap the `§` and `` ` `` keys; `--iso-swap` swaps them
back.

**Colour.** `--chroma full` (default) asks for 4:4:4, which keeps coloured
text sharp; it decodes two streams on the CPU. `--chroma reduced` halves the
decode work on slow Macs or very large windows.

**Logs.** stderr always; `--debug` (or `RUST_LOG=debug`) for detail;
`--log-file PATH` appends to a file; `--syslog` sends to the unified log:

```sh
log stream --predicate 'process == "gliff-mac"'
```

**Headless test.** `--dump DIR --frames N` connects, decodes N frames, writes
them as PNG and exits. It needs no window and runs on Linux too, which makes
it handy in CI and for checking a server from a terminal.

## How it works

```
 gliff-server (Hyprland host)                 gliff-mac (this Mac)
 ─────────────────────────────                ─────────────────────────────
 capture → split → H.264 ×2  ── ssh stdio ──► session worker (tokio thread)
                                               │ OpenH264 decode ×2
                                               │ AVC444 recombine → BGRA
                                               ▼
                                             Picture (pooled buffer)
                                               │ sync_channel(2), latest wins
                                               ▼
 virtual keyboard / pointer ◄── ClientMsg ── main thread (AppKit)
                                               NSTimer 120 Hz (common modes)
                                               CGImage (zero-copy) → CALayer
                                               NSEvent monitors → evdev codes
```

- **Reused unchanged:** `gliff-proto` (messages), `gliff-transport`
  (framing, ssh), `gliff-sw` (OpenH264, AVC444 recombine). The client speaks
  protocol version 1 and advertises no optional features.
- **`session.rs`:** the GTK client's `net.rs` without the GPU tier: Hello,
  Pong, StreamConfig checks, decode, frame acks, keyframe requests on
  error, idle drain of OpenH264's held picture, cursor shapes, reconnect.
- **`keys.rs`:** `kVK_*` → evdev table, left/right modifiers from the
  device-dependent `modifierFlags` bits, Caps Lock as a tap, buttons,
  scroll (sign flipped; momentum dropped because the remote toolkit scrolls
  kinetically after `axis_stop`). Keys released while Command is down get no
  key-up from macOS, so they are released with Command.
- **`geometry.rs`:** the same fit rule as the GTK client (prefer an integer
  scale when the slack is within Hyprland's alignment step), and the same
  pointer mapping into the remote's logical space.
- **`mac/`:** AppKit through `objc2` 0.6 / `objc2-app-kit` 0.3, mostly with
  `msg_send!` so it does not depend on the generated method names of a
  particular binding version. Core Graphics is declared by hand
  (`mac/cg.rs`): each decoded frame becomes a `CGImage` over the pooled
  buffer with no copy, and goes back to the pool when Core Animation
  releases it.

## Roadmap

1. **VideoToolbox decode** (`VTDecompressionSession`, two sessions for
   AVC444, Metal recombine): the GPU tier, as on Linux. Needs the
   SPS/PPS from `StreamConfig.extradata` turned into a
   `CMVideoFormatDescription`.
2. **Clipboard** through `NSPasteboard` with lazy providers, reusing
   `gliff-transport::clipboard`.
3. **Live keymap**: translate the active input source (`TISCopyCurrentKeyboardLayoutInputSource`)
   into XKB text, so `--keymap` becomes optional.
4. **System shortcuts** via a `CGEventTap` (needs the Accessibility
   permission), off by default.
5. **Tabs** for several machines, as in the GTK client.
6. **`gliff-server` on macOS** (ScreenCaptureKit + VideoToolbox encode +
   `CGEvent` input): a separate, larger project.

## Troubleshooting

- *Black window, "Connecting…" forever:* try `gliff-mac --dump /tmp/f user@host`
  in a terminal; it prints the same errors without the UI. Run
  `gliff-probe all` on the remote.
- *`ssh` cannot find `gliff-server`:* `--server-bin /path/to/gliff-server`.
- *Wrong characters:* the remote is using US; pass `--keymap`.
- *High CPU at 4K:* `--chroma reduced`, or a smaller window with `--headless`.

## References

```bibtex
@misc{gliff_repo,
  title        = {gliff: a remote desktop for Hyprland},
  author       = {{omacom}},
  howpublished = {\url{https://github.com/omacom/gliff}},
  note         = {Upstream project; protocol in crates/gliff-proto, design in docs/architecture.md}
}
@misc{objc2,
  title        = {objc2: Objective-C interface and bindings to Apple frameworks in Rust},
  author       = {Mads Marquart and contributors},
  howpublished = {\url{https://github.com/madsmtm/objc2}},
  note         = {define\_class!, msg\_send!, objc2-app-kit 0.3}
}
@misc{apple_nsevent_monitor,
  title        = {addLocalMonitorForEvents(matching:handler:)},
  author       = {{Apple Inc.}},
  howpublished = {\url{https://developer.apple.com/documentation/appkit/nsevent/addlocalmonitorforevents(matching:handler:)}},
  note         = {AppKit documentation}
}
@misc{apple_cgimage,
  title        = {CGImage and CGDataProvider},
  author       = {{Apple Inc.}},
  howpublished = {\url{https://developer.apple.com/documentation/coregraphics/cgimage}},
  note         = {Core Graphics documentation}
}
@misc{apple_calayer,
  title        = {CALayer: contents, magnificationFilter},
  author       = {{Apple Inc.}},
  howpublished = {\url{https://developer.apple.com/documentation/quartzcore/calayer}},
  note         = {Core Animation documentation}
}
@misc{apple_videotoolbox,
  title        = {VTDecompressionSession},
  author       = {{Apple Inc.}},
  howpublished = {\url{https://developer.apple.com/documentation/videotoolbox/vtdecompressionsession}},
  note         = {Planned GPU decode tier}
}
@misc{linux_input_codes,
  title        = {input-event-codes.h},
  author       = {{The Linux kernel developers}},
  howpublished = {\url{https://github.com/torvalds/linux/blob/master/include/uapi/linux/input-event-codes.h}},
  note         = {evdev KEY\_* and BTN\_* values}
}
@misc{wayland_pointer_axis,
  title        = {wl\_pointer: axis, axis\_stop, axis\_discrete},
  author       = {{Wayland project}},
  howpublished = {\url{https://wayland.app/protocols/wayland#wl_pointer}},
  note         = {Scroll semantics the server forwards}
}
@misc{xkbcommon_xkbcli,
  title        = {xkbcli compile-keymap},
  author       = {{xkbcommon project}},
  howpublished = {\url{https://xkbcommon.org/doc/current/}},
  note         = {Producing XKB keymap text for --keymap}
}
@misc{openh264,
  title        = {OpenH264},
  author       = {{Cisco Systems}},
  howpublished = {\url{https://github.com/cisco/openh264}},
  note         = {CPU H.264 codec used by gliff-sw}
}
```
