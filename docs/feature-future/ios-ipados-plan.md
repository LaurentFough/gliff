---
title: "gliff for iOS and iPadOS: plan and effort"
description: "Whether and how to build an iOS/iPadOS gliff client: what the fork already provides, what is new (in-app SSH, touch input), effort by phase, distribution, and the decisions to make."
slug: gliff-ios-ipados-plan
author: LaurentFough
type: plan
date-created: 2026-10-29
date-modified: 2026-10-31
date-published:
source: https://claude.ai/artifact/GxWqoHcvapjDQ8UN68VGkd
domain: remote-desktop
url: https://github.com/LaurentFough/gliff/blob/acrogenesis/macos-client-host/docs/feature-future/ios-ipados-plan.md
draft: true
tags: [gliff, ios, ipados, plan, russh, videotoolbox, metal]
keywords: [iOS client, iPadOS, in-app SSH, russh, touch input, TestFlight, SideStore, gliff-client, gliff-ffi]
---

# gliff for iOS and iPadOS: plan and effort

Oct 29, 2026 · @Ł0ƒƑ

An iOS/iPadOS gliff client is feasible in about 6 to 10 weeks of focused work, with roughly 70% of the code already in the fork and in upstream PR #5. The two genuinely new pieces are SSH inside the app and touch input. The main constraint is distribution: without a paid Apple Developer account, installs expire every 7 days.

The Linux hosts need no changes: they already serve video, sound and `--mute-local-audio` to any client.

## What we can reuse

Most of the client exists already; iOS mainly changes the view layer and where the code runs (in-process instead of a helper process).

| Piece | Source | iOS fit |
| --- | --- | --- |
| Protocol (messages, framing) | `gliff-proto`, `gliff-transport` | Pure Rust; cross-compiles to iOS unchanged |
| Session logic (Hello, frame acks, pacing, keyframe requests) | PR #5's `gliff-client` crate | Platform-neutral and in-process, as iOS requires |
| Rust to Swift bridge | PR #5's `gliff-ffi` (about 15 C functions: session start/stop, keys, pointer, scroll, resize, clipboard) | A ready template |
| GPU video (VideoToolbox decode, Metal 4:4:4 recombine) | `Gliff.swift` in the fork | Both frameworks exist on iOS; only `NSView` becomes `UIView` |
| Sound | `gliff-audio` | cpal has an iOS backend; add an `AVAudioSession` playback category |
| Settings | `gliff-settings` | Works as-is, stored in the app container |
| Discovery | Mac picker logic | Bonjour via `NWBrowser`, plus the Local Network permission prompt |

**Architecture:** a Rust core compiled into the app as a static library, with a native Swift UI on top, as PR #5 does for the Mac. A Swift re-implementation of the protocol would mean two copies to keep in sync.

## What's new

In-app SSH and touch input are most of the new work; the rest is app lifecycle and small platform details.

1. **SSH inside the app (largest new piece).** iOS apps cannot start an `ssh` process, which is how every gliff client connects today. Options:
   - **`russh`** (recommended): pure-Rust async SSH that fits gliff's tokio code.
   - **libssh2**: a C library, more work to cross-compile.
   - **A Swift library** such as Citadel: puts the transport on the Swift side.

   Around it: private keys in the Keychain, created on the device (Secure Enclave or ed25519, with a "copy public key" button); a host-key prompt with a saved known-hosts list; password login. There is no `~/.ssh/config` or ssh-agent on iOS. Tailscale's iOS app works alongside, so hosts are reachable anywhere.
2. **Touch and pointer input (largest UX job).**
   - Trackpad mode by default: relative pointer, tap to click, two-finger tap to right-click, two-finger scroll, pinch to zoom on the device. Direct-touch mode optional.
   - Magic Keyboard trackpads and mice: native, with pointer lock in full screen.
   - Hardware keyboards: `pressesBegan` gives key positions that map to Linux keycodes, much like the Mac mapping.
   - On-screen keyboard: iOS exposes no layout tables, so the app sends a chosen layout to Hyprland (US by default) and turns typed characters into key presses, plus a bar for Super, Ctrl, Alt, Esc, Tab and the arrows.
3. **App lifecycle.** iOS suspends an app soon after it leaves the screen, which drops the connection, so the app reconnects when it comes back. A Private (headless) screen fits well: Hyprland sizes it to the iPad, including Split View and Stage Manager, through the existing resize message.
4. **Clipboard.** Text through `UIPasteboard`. iOS asks permission on paste, so sync on demand rather than continuously.

**Host side:** nothing needed. An iPad Pro's 2732×2048 screen fits within the client's 3840×2160 limit.

## Effort

About 6 to 10 weeks to a client you would use every day, in four phases; Phase 0 answers the risky questions first and cheaply.

| Phase | Scope | Effort |
| --- | --- | --- |
| 0. Spike | Rust core builds for iOS and the simulator; `russh` runs `gliff-server --stdio`; frames arrive | 3–5 days |
| 1. MVP viewer | Connect screen, GPU video, trackpad mode, hardware keyboard, key login | 2–3 weeks |
| 2. Daily use | On-screen keyboard and key bar, clipboard, sound, discovery, settings, reconnect, Stage Manager | 2–4 weeks |
| 3. Hardening | Host-key and key-management UX, tests, iPhone layout, App Store or TestFlight packaging | 1–2 weeks |

Estimates assume Claude writes the code and you build and test in Xcode. Claude cannot run Xcode, the simulator or a device, so every build-and-test step happens on your Mac, as with the GPU video work.

## Distribution

A free Apple ID plus SideStore works for personal use; sharing the app with others eventually needs the paid program.

| Option | Cost | Catch |
| --- | --- | --- |
| Free Apple ID, installed from Xcode | $0 | The app expires every 7 days and must be reinstalled; at most 3 sideloaded apps |
| SideStore or AltStore | $0 | Refreshes the 7-day signature for you; some setup |
| Apple Developer Program | $99/year | 1-year installs, TestFlight for others, App Store |

Prices and limits are as Claude knows them; check Apple's current terms before deciding.

## Decisions

Four choices shape the work; the recommended answer is listed first in each.

| Decision | Recommended | Alternatives |
| --- | --- | --- |
| Devices | iPad first: the screen suits remote desktop, and keyboards and trackpads make it practical | iPhone too, as a compact layout later |
| SSH library | `russh` (pure Rust, tokio) | libssh2; a Swift library such as Citadel |
| Where it lives | `crates/gliff-ios` plus an Xcode project on a `feature/ios-client` branch, with PR #5's `gliff-client` and `gliff-ffi` as the shared core | A separate repository |
| Distribution | Free Apple ID with 7-day installs to start | Paid program from the start |

Bringing in PR #5's shared core would also let the Mac viewer move in-process later.

- [ ] Confirm the four decisions
- [ ] Phase 0 spike on a feature branch
- [ ] Record the plan as ADR 0002 in the macGliff project
