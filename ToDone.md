---
title: "gliff for macOS: ToDone (running report)"
description: "Running state of LaurentFough's gliff fork: upstream PRs, features added, pending work, and a log entry for every commit or merge."
slug: gliff-macos-todone
author: LaurentFough
type: report
date-created: 2026-10-29
date-modified: 2026-10-30
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
| Last updated | 2026-10-30 |

### Branches on the fork

| Branch | Contents | Status |
|---|---|---|
| `acrogenesis/macos-client-host` | #47 base, signing and install fixes, Makefile, #98, Mac settings, GPU video, #80 sound, Mac sound, host mute, Mac reconnect, Mac tabs, ToDone.md | working branch |
| `main` | upstream, untouched | — |

Merged into the working branch on 2026-10-29, still on the fork until deleted (see Repo housekeeping): `feature/macos-gpu-video`, `macos-settings`, `merge/pr-98`, `merge/pr-80`, `feature/macos-audio`, `feature/macos-reconnect`, `feature/macos-tabs`.

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

### Mac tabs and reconnect (merged 2026-10-29; several connections in tabs tested 2026-10-30)

- **Tabs:** one viewer process hosts every connection as a native macOS tab. The tab bar's + or File ▸ New Tab opens the connection window; Window menu tab commands; every tab stays connected; only visible tabs play sound; the clipboard and Machine menu follow the active tab; open tabs reopen at launch (`[[open_tabs]]`), closing a tab forgets it.
- **Reconnect:** a failed or dropped session shows its error with a Reconnect button (and Machine ▸ Reconnect). Sessions that had connected retry automatically, 5 × 1.5 s as on Linux; a failed first connect waits, so SSH prompts do not repeat; incompatible servers are never retried.
- Bridge: packet kind 6 (TARGET window), events carry `"window"`; the Rust `Hub` routes them.

### Known machines in the connection window (on `feature/macos-picker-machines`; testing pending)

- A sidebar in the connection window (at launch and for File ▸ New Tab) lists the config's `machines`, newest first, with a search field; choosing one fills the address and its screen; double-click connects.
- The SSH Address field completes inline from the same list while typing forward; deleting keeps the typed part.
- The client sends `machines` with `machine_settings` (test extended); the picker check covers the list, search, choosing and completion.

### Docs

- `ForkNews.md` (the short list of what the fork adds, also near the top of the README), README fork notes, `docs/macos.md`, `docs/settings.md`, and the crate list in `docs/architecture.md`.
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
- [ ] Mac tabs, the rest: switching tabs moves sound to the visible one; drag a tab out and Merge All Windows; the Machine menu follows the active tab; clipboard to the active tab's machine; quit with two tabs and relaunch (both reopen); close one tab, quit, relaunch (only the other).
- [ ] Mac reconnect, tabled 2026-10-29: stop `gliff-server` mid-session (expect "Reconnecting… (attempt n of 5)", then recovery); a wrong address (error + Reconnect button, no repeated SSH prompts); Machine ▸ Reconnect while the error shows.

### Follow-ups

- [ ] A decode (GPU/CPU) option in the Mac settings window (config-only for now).
- [ ] A "Host speakers: keep / mute" choice in both clients' settings (today: the server command).
- [ ] Sound from a Mac host (ScreenCaptureKit audio capture + #80's Opus encoder).
- [ ] A warning when "Private screen" is chosen with a Mac host.
- [x] Tabs on the Mac: merged 2026-10-29 (one viewer process for every connection); several connections in tabs tested 2026-10-30; the rest under Deferred testing.
- [x] Reconnect for dead sessions on the Mac: merged 2026-10-29; testing tabled, see Deferred testing.
- [ ] Viewer and client CPU use and frame rate in a live GPU session (Activity Monitor).
- [ ] Test the connection window's known machines: the sidebar lists past machines and filters by search; choosing fills the address; double-click connects; typing completes; File ▸ New Tab shows the same.
- [ ] Discovery on the Mac shows nothing (reported 2026-10-30). Diagnose with docs/macos.md "When nothing is found": `dns-sd -B _gliff._tcp local.` on the Mac, Local Network permission (also for the terminal when started from one), `gliff-discovery` and Avahi on the host, and whether the hosts are on the same LAN (not Tailscale/VPN).
- [ ] An iOS/iPadOS client: plan, effort and open decisions in [docs/ios-ipados-plan.md](docs/ios-ipados-plan.md) (Phase 0 spike first).

### Repo housekeeping

- [ ] Make `acrogenesis/macos-client-host` the fork's default branch (only you can):
  ```sh
  gh repo edit LaurentFough/gliff --default-branch acrogenesis/macos-client-host
  ```
- [ ] Delete the merged branches on the fork (only you can; all seven are contained in the working branch):
  ```sh
  git push <fork-remote> --delete feature/macos-gpu-video macos-settings merge/pr-98 merge/pr-80 feature/macos-audio feature/macos-reconnect feature/macos-tabs
  ```
- [x] Rename remotes in Claude's clone (`origin` = the fork, `acrogenesis` = acrogenesis); do the same in your clones if you like.

### PR #26

- [ ] Look at it (deferred).

## Log

Newest first. Format: `### YYYY-MM-DD — summary (commit)`, then a line or two.

### 2026-10-30 — Fix: the connection window's sidebar did not compile
`make check` on `feature/macos-picker-machines`: `#selector(connect)` for the sidebar's double-click came after the local `connect` button, which Swift refuses ("cannot refer to variable 'connect'"). Now `#selector(ComputerPicker.connect(_:))`.

### 2026-10-30 — Connection window: known machines and autocomplete (`feature/macos-picker-machines`)
Sidebar of past machines (search, choose, double-click to connect) and inline autocomplete of the SSH address from the config's `machines`, also in File ▸ New Tab. Discovery not finding hosts: the browse code matches what Linux advertises; docs/macos.md gains "When nothing is found" (dns-sd, Local Network permission incl. terminal launches, Avahi, LAN-only mDNS).

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
