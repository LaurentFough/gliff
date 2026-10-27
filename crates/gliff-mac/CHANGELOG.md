---
title: "gliff-mac changelog"
description: "Version history of the gliff macOS client."
slug: gliff-mac-changelog
author: El (Capisterre Group)
type: changelog
date-created: 2026-10-27
date-modified: 2026-10-27
date-published:
source: https://github.com/omacom/gliff
domain: remote-desktop
url: https://github.com/omacom/gliff/blob/main/crates/gliff-mac/CHANGELOG.md
draft: true
tags: [gliff, macos, changelog]
keywords: [gliff-mac, versions]
---

# gliff-mac changelog

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions
follow [SemVer](https://semver.org/). Module versions (`*_VERSION` constants)
are logged at `--debug`.

## [0.1.0] — 2026-10-27

### Added
- AppKit window (objc2 0.6 / objc2-app-kit 0.3) showing a remote Hyprland
  screen; zero-copy `CGImage` frames on a `CALayer`, nearest-neighbour at
  integer scales.
- Session worker on the gliff protocol v1: ssh or TCP, CPU decode with
  OpenH264, 4:4:4 (Dual420) by default, reconnect with backoff.
- Keyboard: positional `kVK_*` → evdev table, left/right modifiers, Caps
  Lock tap, Command as Super/Ctrl/Alt, ISO swap, release hotkey
  (`shift+escape`, chords, `double-<key>`, `none`), `--keymap` XKB file.
- Pointer: motion, five+ buttons, precise and wheel scrolling with
  `axis_stop`, remote cursor shapes, hidden cursor on headless screens.
- Resize requests that follow the window (200 ms settle).
- Host dialog when launched without arguments; remembers the last host.
- `--dump DIR` headless PNG mode (any OS).
- Logging to stderr, `--log-file`, `--syslog`.
- `scripts/macos/bundle.sh` to build `Gliff.app` (universal, signing, icon).

### Not yet
- VideoToolbox decode, clipboard, live keymap from the input source,
  system-shortcut capture, tabs.
