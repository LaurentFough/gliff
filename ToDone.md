---
title: "gliff for macOS: ToDone (running report)"
description: "Running state of LaurentFough's gliff fork: upstream PRs, features added, pending work, and a log entry for every commit or merge."
slug: gliff-macos-todone
author: LaurentFough
type: report
date-created: 2026-10-29
date-modified: 2026-10-29
date-published:
source: https://github.com/LaurentFough/gliff
domain: remote-desktop
url: https://github.com/LaurentFough/gliff/blob/acrogenesis/macos-client-host/ToDone.md
draft: true
tags: [gliff, macgliff, macos, report, changelog]
keywords: [upstream PRs, VideoToolbox, Metal, settings, signing, Makefile, PR 80 sound]
---

# gliff for macOS: ToDone

A running report for the fork `LaurentFough/gliff`.

**Updating it:** with every commit or merge to the fork:

- add a dated entry at the top of the [Log](#log);
- bump `date-modified` in the frontmatter;
- update whichever sections (state, PRs, features, pending) the change affects.

The log is newest first, and each entry names the commit.

## State

| | |
|---|---|
| Working branch | `acrogenesis/macos-client-host` at `708a44a` |
| Pushed to | `LaurentFough/gliff` only (never acrogenesis or omacom) |
| Ahead of acrogenesis' branch | 21 commits |
| Last updated | 2026-10-29 |

### Branches on the fork

| Branch | Commit | Contents | Status |
|---|---|---|---|
| `acrogenesis/macos-client-host` | `708a44a` | #47 base, signing and install fixes, Makefile, #98, Mac settings, GPU video, ToDone.md | working branch |
| `merge/pr-80` | `5185c1b` | #80 (sound) + README notes, merged with the working branch | awaiting tests |
| `feature/macos-audio` | see Log | Mac viewer plays Linux hosts' sound; host `--mute-local-audio` (on `merge/pr-80`) | Mac sound tested OK; mute awaiting test |
| `feature/macos-gpu-video` | `f4803db` | GPU video port | merged; can be deleted |
| `macos-settings` | `7d15374` | Mac settings | merged; can be deleted |
| `merge/pr-98` | `9bd7870` | #98 | merged; can be deleted |
| `main` | `a2edd50` | upstream, untouched | — |

## Upstream PRs

| PR | What it is | Status |
|---|---|---|
| #47 | acrogenesis' macOS client and host | **Base** of the fork, not merged in |
| #98 | Settings window, per-machine tab menu | **Merged** (`9bd7870`), then brought to the Mac client |
| #5 | dhh's native macOS client (VideoToolbox + Metal) | **Ported, not merged**: only its GPU video path. **Merged** (`354e80d`) |
| #80 | Sound from Linux hosts | **On a test branch**, `merge/pr-80`, now including the working branch |
| #26 | — | **Deferred**; not looked at yet |

## Features added

### Building, signing and installing on a Mac

- Signing without a paid Apple Developer account, using a self-signed `gliff-local` identity created by `bin/setup-macos-signing`.
- Install fixes:
  - the saved signing identity is read with `plutil`;
  - binary paths come from cargo, so a custom `target-dir` works.
- A `Makefile` over the `bin/` and `scripts/` tools:
  - `help`, `doctor`, `build`, `check`, `fix`, `test`, `e2e`, `clean`;
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

### Docs

- README fork notes, `docs/macos.md`, `docs/settings.md`, and the crate list in `docs/architecture.md`.
- In the macGliff project: ADR 0001, the GPU video decision with its results.

## Pending

### PR #80 (sound)

- [ ] Linux test with sound (Linux host to Linux client).
- [x] Bring `merge/pr-80` up to date with the working branch (clean merge; Mac client now skips stray sound payloads).
- [ ] `make check` on the Mac, on `merge/pr-80`.
- [ ] Fast-forward the working branch to `merge/pr-80` and push.

Sound support today:

| Host → client | Sound |
|---|---|
| Linux → Linux | Yes |
| Linux → Mac | On `feature/macos-audio`: works (2026-10-29) |
| Mac → any | No: the Mac host does not send sound yet |

### Deferred testing

- [ ] #98's settings on the Linux client.

### Follow-ups

- [ ] A decode (GPU/CPU) option in the Mac settings window (config-only for now).
- [x] Sound on the Mac client (`feature/macos-audio`): plays a Linux host's sound (tested 2026-10-29).
- [ ] Test `gliff-server --mute-local-audio` on the Linux host (speakers silent, client hears; restored on stop, disconnect and `kill -9`).
- [ ] Merge `feature/macos-audio` after #80.
- [ ] Sound from a Mac host (ScreenCaptureKit audio capture + #80's Opus encoder).
- [ ] A warning when "Private screen" is chosen with a Mac host.
- [ ] Viewer and client CPU use and frame rate in a live GPU session (Activity Monitor).

### Repo housekeeping

- [ ] Make `acrogenesis/macos-client-host` the fork's default branch (only you can):
  ```sh
  gh repo edit LaurentFough/gliff --default-branch acrogenesis/macos-client-host
  ```
- [ ] Delete merged branches: `feature/macos-gpu-video`, `macos-settings`, `merge/pr-98`.
- [ ] Rename remotes (`origin` → `acrogenesis`, `LaurentFough/gliff` → `origin`) and track the fork, which also stops the "unpushed commits" warnings.

### PR #26

- [ ] Look at it (deferred).

## Log

Newest first. Format: `### YYYY-MM-DD — summary (commit)`, then a line or two.

### 2026-10-29 — Host `--mute-local-audio` (`feature/macos-audio`)
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
