---
title: "gliff for macOS: ToDone (running report)"
description: "Running state of LaurentFough's gliff fork: upstream PRs, features added, pending work, and a log entry for every commit or merge."
slug: gliff-macos-todone
author: LaurentFough
type: report
date-created: 2026-10-29
date-modified: 2026-10-31
date-published:
source: https://github.com/LaurentFough/gliff
domain: remote-desktop
url: https://github.com/LaurentFough/gliff/blob/acrogenesis/macos-client-host/ToDone.md
draft: true
tags: [gliff, macgliff, macos, report, changelog]
keywords: [upstream PRs, VideoToolbox, Metal, settings, signing, Makefile, PR 80 sound, CoreAudio, wpctl]
---

# gliff for macOS: ToDone

A running report for the fork `LaurentFough/gliff`.

**Updating it:** with every commit or merge to the fork:

- add a dated entry at the top of the [Log](#log);
- bump `date-modified` in the frontmatter;
- update whichever sections (state, PRs, features, pending) the change affects.
- when a feature lands or its status changes, update [ForkNews.md](ForkNews.md) and the same list under "New in this fork" in the README.

The log is newest first, and each entry names the commit.

## State

| | |
|---|---|
| Working branch | `acrogenesis/macos-client-host` (see Log for the latest commit) |
| Pushed to | `LaurentFough/gliff` only (never acrogenesis or omacom) |
| Ahead of acrogenesis' branch | see `git rev-list --count origin/acrogenesis/macos-client-host..HEAD` |
| Last updated | 2026-10-31 |

### Branches on the fork

| Branch | Contents | Status |
|---|---|---|
| `acrogenesis/macos-client-host` | #47 base, signing and install fixes, Makefile, #98, Mac settings, GPU video, #80 sound, Mac sound, host mute, Mac reconnect, Mac tabs, known machines, auto-reconnect, one app (panels), quit on last window, About, screenshots, plan docs, ToDone.md | working branch |
| `main` | upstream, untouched | — |

The working branch is the fork's default branch (2026-10-31). The ten feature branches merged into it were deleted from the fork on 2026-10-31. Also kept: `archive/gliff-mac-objc2` (the old objc2 prototype).

## Upstream PRs

| PR | What it is | Status |
|---|---|---|
| #47 | acrogenesis' macOS client and host | **Base** of the fork, not merged in |
| #98 | Settings window, per-machine tab menu | **Merged** (`9bd7870`), then brought to the Mac client |
| #5 | dhh's native macOS client (VideoToolbox + Metal) | **Ported, not merged**: only its GPU video path. **Merged** (`354e80d`) |
| #80 | Sound from Linux hosts | **Merged** (with the Mac sound work), tested Linux host → Mac |
| #26 | — | **Deferred**; not looked at yet |

## Features added

### Building, signing and installing on a Mac

- Signing without a paid Apple Developer account, using a self-signed `gliff-local` identity created by `bin/setup-macos-signing`.
- Install fixes:
  - the saved signing identity is read with `plutil`;
  - binary paths come from cargo, so a custom `target-dir` works.
- A `Makefile` over the `bin/` and `scripts/` tools:
  - `help`, `doctor`, `build`, `check`, `fix`, `test`, `e2e`, `clean`, `distclean`;
  - `macos`, the full pipeline (sign, build, install, verify), plus `verify`, `signing`, `app`, `install`, `package`, `package-release`;
  - `video-check`, and the `dev` and `discovery` targets.
- Every make step is logged to `target/logs/`.
- Test fix: the ssh login-shell test no longer depends on the developer's shell setup.

### Settings (#98 on both clients)

- A new `gliff-settings` crate shared by the Linux and Mac clients. It saves to `~/.config/gliff/config.toml` and keeps keys it doesn't own.
- Layering: built-in defaults < `[defaults]` < command-line options < `[overrides."user@host"]`. Old `recent_machines` entries are migrated.
- Mac picker: a Settings button, per-machine screen choice, and in-window choices remembered per host and port.
- Mac viewer: a Machine menu with mirror, private screen, clipboard and Reconnect. Changes apply live, or reconnect when they have to.
- A Mac settings window. The picker check also covers settings and migration.

### GPU video (#5 port)

- VideoToolbox hardware H.264 decode of both streams, a Metal 4:4:4 (AVC444) recombine, and presentation through a `CAMetalLayer` with no frame copies.
- The client asks hosts for 4:4:4 first when the Mac has hardware decode.
- Automatic CPU (OpenH264) fallback when there is no hardware decoder or after repeated decode errors. To force it: `--video cpu`, or `decode = "cpu"` in the config.
- The pipe carries coded bytes (kilobytes per frame) instead of decoded pixels (megabytes).
- `make video-check` passes on all four cases:

  | Case | GPU avg (ms) | CPU avg (ms) | Speed-up |
  |---|---:|---:|---:|
  | 1080p 4:2:0 | 3.73 | 5.92 | 1.6× |
  | 1080p 4:4:4 | 4.44 | 5.62 | 1.3× |
  | 4K 4:2:0 | 6.68 | 12.08 | 1.8× |
  | 4K 4:4:4 | 6.90 | 18.81 | 2.7× |

### Sound (#80, plus the Mac and the host mute)

- #80: Linux hosts send what applications play as Opus (PipeWire `pw-record` + graph linker, or `parec`); Linux viewers play it through ALSA. Echo-free when two machines show each other.
- The Mac viewer plays it through #80's `gliff-audio` (Opus, jitter buffer, cpal on CoreAudio). **Machine ▸ Play Sound** is the same top-level `muted` key as the Linux speaker button; `--no-audio` starts muted. Oversized packets are refused.
- `gliff-server --mute-local-audio`: the host's speakers are muted while the client plays its sound, following a new default output, and restored on sound off, session end, or a crash (watchdog). PipeWire path only. Set it as the machine's server command.

### Mac tabs and reconnect (merged 2026-10-29; tested 2026-10-30)

- **Tabs:** one viewer process hosts every connection as a native macOS tab. The tab bar's + or File ▸ New Tab opens the connection window; Window menu tab commands; every tab stays connected; only visible tabs play sound; the clipboard and Machine menu follow the active tab; open tabs reopen at launch (`[[open_tabs]]`), closing a tab forgets it.
- **Reconnect:** a failed or dropped session shows its error with a Reconnect button (and Machine ▸ Reconnect). Sessions that had connected retry automatically, 5 × 1.5 s as on Linux; a failed first connect waits, so SSH prompts do not repeat; incompatible servers are never retried.
- Bridge: packet kind 6 (TARGET window), events carry `"window"`; the Rust `Hub` routes them.

### Known machines in the connection window (merged 2026-10-30; tested)

- A sidebar in the connection window (at launch and for File ▸ New Tab) lists the config's `machines`, newest first, with a search field; choosing one fills the address and its screen; double-click connects.
- The SSH Address field completes inline from the same list while typing forward; deleting keeps the typed part.
- The client sends `machines` with `machine_settings` (test extended); the picker check covers the list, search, choosing and completion.
- **+** adds the typed address to `machines`; **−** forgets the selected machine (asks first): `Store::forget` removes it from `machines`, `[overrides]` and `[[open_tabs]]`, as the Linux client's Forget.

### Per-machine auto-reconnect (merged 2026-10-30; tested on the Mac)

- New shared setting `auto_reconnect` (default on) in `gliff-settings`, per machine or in `[defaults]`.
- Off: not reopened at launch, and a dropped session waits for Reconnect (Mac: the error says so; Linux: "click the tab to reconnect"). Changing it never reconnects a session, on either client.
- Mac: Machine ▸ Reconnect Automatically (checkbox) and a Reconnect row in Settings. Linux honours it; its settings window does not show it yet.

### One app on the Mac (merged 2026-10-31; tested)

- Settings and File ▸ New Tab open inside the viewer (`crate::panel`, `open_panel` / `close_panel`, routed by window id like tabs): one Dock icon, ⌘-Tab entry and menu bar. The connection window at launch (no viewer yet) is still its own process.
- New Tab: a sheet over a tab without a live session; its own window over a connected tab or with no tab. ⌘, brings an open Settings forward; the viewer has an Edit menu.
- Settings ▸ **Connect** (or double-click a machine) opens it in a new tab (`connect_machine`).
- The connection window's nearby list takes a taller window's height.
- Settings ▸ General ▸ **Quit Gliff when its last window closes** (`quit_on_last_window`, default off); off, Gliff stays open and ⌘T or the Dock icon opens the connection window.
- Gliff ▸ **About Gliff**: version and build ID (commit, `-dirty`, arch; `GLIFF_BUILD_ID` overrides).

### Discovery, checks and screenshots

- Discovery needs `gliff-discovery` on each host: the Arch package says so after install; `docs/macos.md` leads with it.
- `scripts/check.sh` stops early when Rust is older than `rust-version` (1.87), with how to update.
- README / ForkNews screenshots (`docs/images/`): connection window, tabs, Settings ▸ Reconnect, Machine menu mockup (`docs/images/src/`), features outlined in red.

### Build once, install on other Macs (merged 2026-10-31; tested)

- `GliffBuildID` in the app's `Info.plist` (commit, `-dirty`, architecture). `make install` skips the build when the installed app is the same clean build with the same signing identity and a valid signature; `FORCE=1` rebuilds.
- `make install-package ZIP=…`: checksum, architecture and macOS checks, quarantine removed, re-signed with the receiving Mac's identity, installed, verified.
- `make release-upload` / `make install-release`: the ZIP as a pre-release `macos-COMMIT` on GitHub (gh), installed through `install-package`.
- Shared `bin/lib/macos-install.sh` (identity choice, signing, safe swap) used by all three installers. Makefile 0.4.0. Apple Silicon only for now.

### Docs

- `ForkNews.md` (the short list of what the fork adds, also near the top of the README), README fork notes, `docs/macos.md`, `docs/settings.md`, and the crate list in `docs/architecture.md`.
- Future-feature plans in `docs/feature-future/`: the iOS/iPadOS client and several clients on one desktop.
- In the macGliff project: ADR 0001, the GPU video decision with its results.

## Pending

### Sound

Support today:

| Host → client | Sound |
|---|---|
| Linux → Linux | Yes (#80) |
| Linux → Mac | Yes (tested 2026-10-29), including `--mute-local-audio` |
| Mac → any | No: the Mac host does not send sound yet |

- [ ] Linux host → Linux client with sound, on the merged working branch.

### Deferred testing

- [ ] #98's settings on the Linux client.
- [x] Mac tabs: several machines connected at once, each in its own tab (tested 2026-10-30).
- [x] Mac tabs (tested 2026-10-30): sound follows the visible tab, and stays off for a hidden tab across a reconnect and while minimised; drag a tab out; the Machine menu follows the active tab; clipboard to the active tab's machine; quit with two tabs and relaunch (both reopen); close one, quit, relaunch (only the other).
- [x] Mac tabs: Window ▸ Merge All Windows (T5) works since the Window menu lists the tab commands (retested 2026-10-30).
- [x] Mac reconnect (tested 2026-10-30): a dropped session retries and recovers; after 5 attempts the error and Reconnect button, then Reconnect works; a failed first connect waits with no retries or repeated prompts; fixing the server command in Settings then Reconnect connects; other tabs keep streaming meanwhile.

### Follow-ups

- [ ] A decode (GPU/CPU) option in the Mac settings window (config-only for now).
- [ ] A "Host speakers: keep / mute" choice in both clients' settings (today: the server command).
- [ ] Sound from a Mac host (ScreenCaptureKit audio capture + #80's Opus encoder).
- [ ] A warning when "Private screen" is chosen with a Mac host.
- [x] Tabs on the Mac: merged 2026-10-29 (one viewer process for every connection); several connections in tabs tested 2026-10-30; the rest under Deferred testing.
- [x] Reconnect for dead sessions on the Mac: merged 2026-10-29; testing tabled, see Deferred testing.
- [ ] Viewer and client CPU use and frame rate in a live GPU session (Activity Monitor).
- [x] Connection window's known machines: list, search, choosing, double-click, autocomplete, File ▸ New Tab (tested 2026-10-30).
- [x] + (adds the typed address) and − (asks, then forgets the machine and its settings) (tested 2026-10-30).
- [x] Auto-reconnect on the Mac (tested 2026-10-30): Machine ▸ Reconnect Automatically off skips the launch reopen and drop retries; Settings ▸ Reconnect matches.
- [ ] Auto-reconnect on Linux: a machine with `auto_reconnect = false` is not reopened and not retried.
- [x] `feature/macos-panels-in-viewer`: the connection window's nearby list takes the extra height when the window is made taller (reported and tested 2026-10-30).
- [x] `feature/macos-panels-in-viewer`: Settings and File ▸ New Tab open inside the viewer, not as separate apps (reported 2026-10-30; tested 2026-10-30): one Dock icon and ⌘-Tab entry; New Tab's connection window is a sheet on the front tab; Settings is a window of the viewer and ⌘, brings an open one forward; Edit ▸ Paste works in both. The connection window at launch (no viewer yet) is still its own process.
- [x] `feature/macos-panels-in-viewer`: File ▸ New Tab is a sheet over a tab with no live session (connecting, failed or dropped), else its own window (connected tab, or no tab) — corrected 2026-10-31.
- [x] `feature/macos-panels-in-viewer`: Settings ▸ Connect, or double-clicking a machine, opens it in a new tab (viewer only).
- [x] `feature/macos-panels-in-viewer`: Gliff ▸ About Gliff shows the version and build (commit, `-dirty`, arch).
- [x] `feature/macos-panels-in-viewer`: Settings ▸ General ▸ Quit Gliff when its last window closes (`quit_on_last_window`, default off). Off: last tab closed, Gliff stays open; ⌘T or the Dock icon opens the connection window; the Machine menu is greyed out. On: closing the last tab quits and the machine reopens at the next launch.
- [ ] Linux settings window: show `auto_reconnect`.
- [x] Discovery on the Mac (resolved 2026-10-30): the hosts were not running the `gliff-discovery` user service. `gliff-enable-discovery` on each host fixes it; the package now says so after install, and docs/macos.md leads with it.
- [ ] An iOS/iPadOS client: plan, effort and open decisions in [docs/feature-future/ios-ipados-plan.md](docs/feature-future/ios-ipados-plan.md) (Phase 0 spike first).
- [ ] Several clients on one desktop: plan, levels and open decisions in [docs/feature-future/multi-client-plan.md](docs/feature-future/multi-client-plan.md) (Level 0 test first; for later).

### Build and release

- [x] #3 (tested and merged 2026-10-31): build-once installs on `feature/macos-install-reuse` (`make install` twice, the second skips; a change then `make install` rebuilds; About Gliff shows the same build ID as `GliffBuildID`; `make package` → `make install-package` on a second Mac; `make release-upload` → `make install-release`), then merge.
- [ ] Intel Macs: a universal (arm64 + x86_64) package, or per-architecture releases. Deferred (2026-10-31): no Intel Mac on macOS to test with (the Intel Mac runs Omarchy, so it is covered by the Linux packages).
- [x] Linux prebuilt packages (tested and merged 2026-10-31): `make package` on an Arch/Omarchy machine; `make install` twice (the second skips); `make install-package PKG=…` on a second machine; `make release-upload` → `make install-release`.
- [ ] Releases from CI: attach the macOS runners' packages to `macos-*` releases instead of building locally.

### Ideas (not decided)

- [ ] Tailnet machines in the connection window (Tailscale peers, for hosts mDNS cannot reach).
- [ ] Settings ▸ Connect: switch to the machine's open tab instead of opening a second one.
- [ ] Connect in the stand-alone Settings window at launch (opened from the launch connection window; today only inside the viewer).
- [ ] Open the launch connection window inside the viewer too, so Gliff is one process from the start.
- [ ] Screenshots still missing: the address autocomplete, the Reconnect button on a dropped session, the File and Window menus' tab commands.
- [ ] Screenshots: a second (lighter or dashed) outline for older fork features (the Settings window itself, Mirror/Private Screen, Share Clipboard).
- [ ] Build ID: `-dirty` can lag until the next commit or Swift change (build.rs watches HEAD, not the index).
- [ ] Several clients on one desktop, Level 0 test and then Levels 1–2 (see the plan above).

### Repo housekeeping

- [x] `acrogenesis/macos-client-host` is the fork's default branch (2026-10-31).
- [x] The ten merged branches are deleted from the fork (2026-10-31).
- [x] `rust-toolchain.toml` pins Rust 1.99.0 with rustfmt, clippy, rust-src and rust-analyzer (`adf8813`, 2026-10-31); `rust-version` (1.87) stays the minimum.
- [x] Rename remotes in Claude's clone (`origin` = the fork, `acrogenesis` = acrogenesis); do the same in your clones if you like.

### PR #26

- [ ] Look at it (deferred).

## Log

Newest first. Format: `### YYYY-MM-DD — summary (commit)`, then a line or two.

### 2026-10-31 — Merge `feature/linux-packages` (build once, install on other Arch/Omarchy machines)
Tested on Arch/Omarchy: `make install` (and the skip on a second run, through doas), `make package`, `make install-package`, releases. No-fast-forward merge; README / ForkNews marked tested.

### 2026-10-31 — Linux packages: install through doas (`feature/linux-packages`)
First test: the build worked, `make install` failed at makepkg's install step with "doas: Authentication required" (makepkg exit 14). `bin/install` now builds with `bin/package-linux` and installs with `bin/install-linux-package`, whose root step (`gliff_root`) picks `GLIFF_AUTH`, else `makepkg.conf`'s `PACMAN_AUTH`, else sudo, doas or su, and runs it on the terminal. README says so.

### 2026-10-31 — Build once, install on other Arch/Omarchy machines (`feature/linux-packages`)
Intel Mac builds deferred (that Mac runs Omarchy). New: `bin/package-linux` (pacman package with `/usr/share/gliff/build-id`, renamed `gliff-VER-linux-ARCH-COMMIT.pkg.tar.zst`, plus `.sha256`), `bin/install-linux-package` (checksum, name, arch, skip same build, `sudo pacman -U`), `bin/release-linux` / `bin/install-linux-release` (`linux-COMMIT` pre-releases via gh, curl fallback); `bin/install` skips an unchanged clean build on Arch; `gliff_build_id` moved to `bin/lib/build-id.sh` (shared), Linux helpers in `bin/lib/linux-install.sh`; PKGBUILD writes the build ID; Makefile 0.5.0 dispatches `package` / `install-package` / `release-upload` / `install-release` by OS (`PKG=` on Arch). README procedure and targets updated. Shell syntax checked here; not run on Arch yet.

### 2026-10-31 — Merge `feature/macos-install-reuse` (build once, install on other Macs)
#3 tested on the Mac: `make install` skips an unchanged build, `make package` / `make install-package`, `make release-upload` / `make install-release`. README: an "Other Macs: build once" procedure under Install and a fork note; docs/macos.md: notes (the other Mac needs a clone, checking the build ID, removing a release). No-fast-forward merge into the working branch; delete the branch on the fork when convenient.

### 2026-10-31 — `feature/macos-install-reuse` brought up to date; one build ID
Merged the working branch into it (conflicts in README, ForkNews, ToDone resolved). `bin/build-macos-app` now computes the build ID before `cargo build` and exports it as `GLIFF_BUILD_ID`, so About Gliff and `Info.plist`'s `GliffBuildID` are the same string; build.rs's own fallback uses the same form (`COMMIT[-dirty]+ARCH`, 12-digit commit, `arm64`).

### 2026-10-31 — Fork housekeeping done; Rust toolchain pinned (`adf8813`)
Default branch is now `acrogenesis/macos-client-host`; the ten merged branches are gone from the fork (left: the working branch, `main`, `feature/macos-install-reuse`, `archive/gliff-mac-objc2`). `rust-toolchain.toml` (LaurentFough) pins 1.99.0 for rustup users. Upstream: omacom/main has nothing the fork lacks (last: 0.4.1, 2026-10-24); acrogenesis' branch has nothing new.

### 2026-10-31 — ToDone audit
State and branch tables brought up to date (open `feature/macos-install-reuse`; ten merged branches to delete). Features added gains one app on the Mac, quit on last window, About, discovery hint, the Rust version check and screenshots. Pending gains Build and release (#3, Intel, Linux packages, CI releases, previously only on the install branch) and Ideas (Tailnet list, Connect to an open tab, Connect at launch, launch window in the viewer, missing screenshots, secondary outlines, build ID lag, multi-client levels).

### 2026-10-31 — Plan docs moved to `docs/feature-future/`
`ios-ipados-plan.md` and `multi-client-plan.md` now live in `docs/feature-future/`; links in the README, ForkNews, the backlog and their frontmatter `url` updated (older log entries keep the paths of their day).

### 2026-10-31 — Plan doc: several clients on one desktop
`docs/multi-client-plan.md`: what already works (one server per connection), what breaks (`--mute-local-audio` restore, pointer contention, no viewer notice), levels 0–3 with estimates, a Level 0 checklist and the decisions. Linked from the README, ForkNews and the backlog; for later.

### 2026-10-31 — Merge `feature/macos-panels-in-viewer` into the working branch
All hand tests passed (New Tab sheet/window, About, Quit on last window on and off, Connect from Settings). No-fast-forward merge; README fork notes, the shared list and status table (README and ForkNews.md) updated.

### 2026-10-31 — `feature/macos-panels-in-viewer`: automated checks pass on the Mac
`scripts/check.sh` (fmt, clippy, tests, native checks) and `scripts/macos-picker-check.py` pass (after `91d146b`, clippy `while_let_loop`). Hand tests: all passed (2026-10-31).

### 2026-10-31 — New Tab rule corrected; Connect from Settings (`feature/macos-panels-in-viewer`)
New Tab's sheet/window rule inverted as requested: a sheet over a tab without a live session; a window over a connected tab or with no tab. Settings gains Connect (and double-click on a machine), sent as the app event `connect_machine`; the client opens a new tab with the machine's settings.

### 2026-10-31 — New Tab sheet or window; About Gliff; Quit on last window (`feature/macos-panels-in-viewer`)
Panels and list resizing tested on the Mac. New Tab: a sheet only over a tab showing a connected machine, else its own window. About Gliff in every Gliff menu (version from the client; build ID from build.rs: commit, `-dirty`, arch, or `GLIFF_BUILD_ID`). New app-wide setting `quit_on_last_window` (gliff-settings `Store`, default off) in Settings ▸ General; the client sends it to the viewer (`app_settings`); off keeps Gliff open with no windows (Dock icon or ⌘T reopens the connection window). `check.sh` now stops early on a Rust older than `rust-version`.

### 2026-10-30 — Connection and settings windows inside the viewer; list resizing (`feature/macos-panels-in-viewer`)
New `panel.rs`: the connection ("choose") and settings windows open in the running viewer (`open_panel`, routed by window id like tabs) and fall back to their own helper process when no viewer runs. Swift: `PanelWindow`; the picker and settings window split into build/handle so both modes share them; New Tab's picker is a sheet; viewer gains an Edit menu. Picker layout: the nearby list hugs least and its height is a minimum, so it takes a taller window's height. `scripts/macos-picker-check.py` checks both (LAYOUT, PANELS). Not compiled here (no Swift, no crates.io): run `scripts/check.sh` and the picker check.

### 2026-10-30 — Tabs caption clarified
The screenshot shows two tabs; the caption now says any number of tabs (connections) is supported.

### 2026-10-30 — Discovery resolved: hosts were not advertising
Cause: `gliff-discovery` not running on the Linux hosts (no code fault on the Mac). The Arch package's install hook now tells you to run `gliff-enable-discovery`; docs/macos.md "When nothing is found" leads with it; status rows updated.

### 2026-10-30 — Screenshot: connection window with known machines
The sidebar (search, list, + / −) and the autocompleting SSH Address field outlined; host name replaced with a placeholder.

### 2026-10-30 — Screenshots: red outlines, one per tab
Outlines are now macOS systemRed (#FF3B30); the tabs shot has one box per tab (per connection).

### 2026-10-30 — Fix: screenshots not shown
`.gitignore` ignores `*.png`, so the three images were never committed; `docs/images/*.png` is now an exception. Screenshots captions shortened.

### 2026-10-30 — Screenshots in the README and ForkNews
Tabs and Settings ▸ Reconnect screenshots (host names, IPs and the home path replaced with placeholders, EXIF dropped), and a Machine menu mockup drawn to match `Gliff.swift` (`docs/images/src/`). New features outlined in 2 pt blue. Same Screenshots section in both files.

### 2026-10-30 — README and ForkNews brought up to date
README fork notes gain known machines, per-machine auto-reconnect, the Window menu's tab commands, `make distclean`, the project `.gitignore` and a link to the iOS plan. The shared list and status table (README and ForkNews.md) add the same. Discovery pinned.

### 2026-10-30 — Merge known machines and per-machine auto-reconnect into the working branch
No-fast-forward merge of `feature/machines-and-auto-reconnect` (which holds `feature/macos-picker-machines`): the connection window's known-machines sidebar with + / − and autocomplete, and the shared `auto_reconnect` setting. Both tested on the Mac. Discovery: `dns-sd` on the Mac sees no `_gliff._tcp` either, so the advertisements never reach it.

### 2026-10-30 — T5 retested: Merge All Windows works
Every tabs (T1–T9) and reconnect (R1–R5) check now passes.

### 2026-10-30 — Mac tabs and reconnect tested; Window menu gains the tab commands
Tabs T1–T4 and T6–T9 and reconnect R1–R5 passed. T5 failed: no Window ▸ Merge All Windows, because AppKit does not add its tab commands to a menu built in code. The Window menu now lists Show Previous/Next Tab (⌘{ ⌘}), Move Tab to New Window, Merge All Windows and Show Tab Bar; retest pending.

### 2026-10-30 — Add/forget machines; per-machine auto-reconnect (`feature/machines-and-auto-reconnect`)
The known-machines list and autocomplete tested OK. Added + / − to the picker sidebar (`Store::forget`, as Linux's Forget) and a shared `auto_reconnect` setting: Machine ▸ Reconnect Automatically and a Settings row on the Mac; off skips launch reopen and drop retries, on both clients; changing it never reconnects. Tests: model, store forget, menu setting, picker rules.

### 2026-10-30 — Fix: the connection window's sidebar did not compile
`make check` on `feature/macos-picker-machines`: `#selector(connect)` for the sidebar's double-click came after the local `connect` button, which Swift refuses ("cannot refer to variable 'connect'"). Now `#selector(ComputerPicker.connect(_:))`.

### 2026-10-30 — Connection window: known machines and autocomplete (`feature/macos-picker-machines`)
Sidebar of past machines (search, choose, double-click to connect) and inline autocomplete of the SSH address from the config's `machines`, also in File ▸ New Tab. Discovery not finding hosts: the browse code matches what Linux advertises; docs/macos.md gains "When nothing is found" (dns-sd, Local Network permission incl. terminal launches, Avahi, LAN-only mDNS).

### 2026-10-30 — Build once, install on other Macs (`feature/macos-install-reuse`)
Build ID in the app; `make install` skips unchanged builds; `make install-package` (checksum, quarantine, local re-sign); `make release-upload` / `make install-release` via GitHub Releases; shared `bin/lib/macos-install.sh`; Makefile 0.4.0; docs/macos.md "Build once, install on other Macs". Shell syntax checked here; nothing run on a Mac yet.

### 2026-10-30 — Mac tabs: several connections at once tested
Several machines connected at the same time, each in its own tab, work as implemented. ForkNews.md and the README's fork features and status now say so; the remaining tab checks (sound following the visible tab, drag and merge, Machine menu, clipboard, restore at launch) and Mac reconnect stay under Deferred testing.

### 2026-10-29 — `make distclean` (Makefile 0.3.0)
Removes everything `.gitignore` ignores (`git clean -Xd`: `target/`, stray logs, `.DS_Store`, `patch/`), nothing tracked. Lists it first and asks; `FORCE=1` skips the question and is required without a terminal. README make table updated.

### 2026-10-29 — Project `.gitignore`; iOS plan in docs (`907e2ed`)
`.gitignore` grouped for this workspace: Rust/cargo (Cargo.lock stays tracked), Python, Swift/Xcode, logs, macOS (`.DS_Store` and friends), editors, packaging, and local scratch (`/patch/`, `*.patch`). The old `patch/` files are not committed: each is already in history as a commit (signing, `plutil`, cargo paths, the ssh test), or belongs to the abandoned objc2 `gliff-mac` prototype. Added `docs/ios-ipados-plan.md` (the iOS/iPadOS plan doc, with frontmatter) and a follow-up for it.

### 2026-10-29 — ForkNews.md, and the README's "New in this fork"
The short list of what the fork adds (upstream PRs, its own features, status), now including Mac tabs and reconnect. The README carries the same list near the top, links ForkNews.md and ToDone.md, and its fork notes gain Tabs and Reconnect; "one connection per window" is gone.

### 2026-10-29 — Merge Mac tabs and reconnect into the working branch; testing tabled
No-fast-forward merge of `feature/macos-tabs` (which holds `feature/macos-reconnect`). `make check` passed the Rust tests and the helper compiled; the File menu and New Tab connection window appear. Hands-on testing of tabs and reconnect is tabled (Deferred testing).

### 2026-10-29 — Input check follows tabs (`feature/macos-tabs`)
`make check` on the tabs branch: all Rust tests passed (42 in gliff-macos), the helper compiled, but `scripts/macos-input-check.py` built its viewer from `App.view`/`App.window`, which moved into `Tab`. The harness now builds a `Tab` and registers it with `App.addTab`, and also checks resize events carry the window id.

### 2026-10-29 — Mac: native tabs (`feature/macos-tabs`, on `feature/macos-reconnect`)
One viewer process hosts every connection so macOS groups them as tabs: the bridge gains packet kind 6 (TARGET window) and events carry `"window"`; the Rust `Hub` routes them, and each tab runs the existing session code (`visit`). Tab bar + / File ▸ New Tab opens the connection window; tabs stay connected; only visible tabs play sound; the clipboard follows the active tab; open tabs reopen at launch (`[[open_tabs]]`, closing a tab forgets it). Tests: event routing, open-tabs store, background-tab sound. Not built here.

### 2026-10-29 — Mac: reconnect dead sessions (`feature/macos-reconnect`)
A failed or dropped session shows its error with a Reconnect button. Sessions that had connected retry automatically (5 × 1.5 s, as on Linux); a failed first connect waits, so SSH prompts do not repeat; incompatible servers are never retried (typed `Incompatible` error). Settings and the Machine menu work while waiting. `Retry` rule with unit tests; docs/macos.md.

### 2026-10-29 — Follow-ups: Mac tabs, reconnect for dead sessions
Added to Follow-ups. Mac tabs need one process for all connections, or PR #5's in-process core.

### 2026-10-29 — Merge sound into the working branch; clean up
No-fast-forward merge of `feature/macos-audio` (which holds `merge/pr-80`): #80, Mac sound and `--mute-local-audio`, tested Linux host → Mac (muted on the host, playing on the Mac). Merged branches deleted locally; deleting them on the fork is left to you.

### 2026-10-29 — Host `--mute-local-audio` (`c0fd85a`)
Sound played on both the host's speakers and the client (#80's design). New `gliff-server --mute-local-audio`: mutes the default output via `wpctl` while the client plays sound, only when the recorder links applications directly (PipeWire), follows a new default output every 2 s, restores what it muted on sound off, session end or a fallback recorder, and a detached watchdog unmutes if the server dies. `local_mute.rs` with unit tests on a fake backend. Enable per machine with the server command. README, docs/macos.md.

### 2026-10-29 — Mac viewer plays Linux hosts' sound (`bdc80da`)
Branched from `merge/pr-80`. The Mac client offers `audio`, asks a server that offers it, and plays through #80's `gliff-audio` `Sink` (Opus, jitter buffer, cpal on CoreAudio). New `sound.rs` with tests; Machine ▸ Play Sound; `--no-audio`; the switch is the top-level `muted` key shared with the Linux client (`Store::muted`/`set_muted`, with a test); audio packets over 4000 bytes are refused. Docs: `docs/macos.md`, `docs/settings.md`, README. Not built here.

### 2026-10-29 — Working branch merged into `merge/pr-80` (`5185c1b`)
Clean merge, so the #80 test build includes GPU video and settings. The Mac client now reads past `AudioFrame` payloads (`server_payload`), so a host that sends sound anyway cannot break the stream; with a test. README: #5 listed as ported, GPU video and `make video-check` notes, ToDone.md.

### 2026-10-29 — ToDone.md added
This running report.

### 2026-10-29 — Merge GPU video into the working branch (`354e80d`)
No-fast-forward merge of `feature/macos-gpu-video`, with no conflicts.

### 2026-10-29 — GPU video check and benchmark (`f4803db`)
`scripts/macos-video-check.py`, `examples/video_fixture.rs`, `make video-check`. PASS on all four cases.

### 2026-10-28 — GPU video decode in the Mac viewer (`9ebc641`)
VideoToolbox + Metal, ported from #5, with the CPU fallback and `--video cpu`. ADR 0001 written.

### 2026-10-28 — PR #80 merged on a test branch (`11d1587`, `b1ab0d8`)
Sound from Linux hosts; the Mac client and host turn audio off. README fork notes grouped by topic.

### Earlier — settings, Makefile, signing (`35fde42` … `7d15374`)
- `35fde42`, `b9e2bdb`, `e259d5d`: local signing and install fixes.
- `328badd`: ssh test isolation.
- `6ba1339`, `2bfb7b8`: the Makefile and `make macos`.
- `436ab7b`: README.
- `9bd7870`: merge of #98.
- `f41e583`, `3e19383`, `7d15374`: per-machine settings on the Mac.

## References

```bibtex
@misc{gliff_upstream,
  title        = {omacom/gliff: Hyprland remote desktop over ssh},
  author       = {{omacom}},
  howpublished = {\url{https://github.com/omacom/gliff}}
}
@misc{gliff_fork,
  title        = {LaurentFough/gliff: macOS fork},
  author       = {LaurentFough},
  howpublished = {\url{https://github.com/LaurentFough/gliff}}
}
@misc{gliff_pr47,
  title        = {omacom/gliff pull request 47: macOS client and host},
  author       = {{acrogenesis}},
  howpublished = {\url{https://github.com/omacom/gliff/pull/47}}
}
@misc{gliff_pr5,
  title        = {omacom/gliff pull request 5: native macOS client},
  author       = {Heinemeier Hansson, David},
  howpublished = {\url{https://github.com/omacom/gliff/pull/5}}
}
@misc{gliff_pr98,
  title        = {omacom/gliff pull request 98: settings window and per-machine tab menu},
  author       = {{omacom}},
  howpublished = {\url{https://github.com/omacom/gliff/pull/98}}
}
@misc{gliff_pr80,
  title        = {omacom/gliff pull request 80: sound from Linux hosts},
  author       = {{omacom}},
  howpublished = {\url{https://github.com/omacom/gliff/pull/80}}
}
```
