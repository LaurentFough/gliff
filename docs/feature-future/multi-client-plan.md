---
title: "gliff with several clients on one desktop: plan and effort"
description: "Whether several gliff clients can share one host desktop, what already works because each connection runs its own server, what breaks, and the work by level: test, harden, view-only and joining, one shared stream."
slug: gliff-multi-client-plan
author: LaurentFough
type: plan
date-created: 2026-10-31
date-modified: 2026-10-31
date-published:
source: https://github.com/LaurentFough/gliff
domain: remote-desktop
url: https://github.com/LaurentFough/gliff/blob/acrogenesis/macos-client-host/docs/feature-future/multi-client-plan.md
draft: true
tags: [gliff, multi-client, shared-session, plan, hyprland, view-only]
keywords: [several clients, shared desktop, view-only, local mute, private screen, RFB shared flag, session broker, fan-out encoding]
---

# gliff with several clients on one desktop: plan and effort

Oct 31, 2026 · @Ł0ƒƑ

Several gliff clients can very likely share one desktop already. Each connection runs its own `gliff-server` over SSH, and nothing on the host allows only one. It is untested, though, and a few host-side pieces assume they are alone. A short test, then 1 to 2 days of hardening, covers the usual case: the same person on two of their own machines. View-only viewers and joining someone's private screen add 2 to 4 days. A single shared stream for many viewers is a 2 to 4 week project, worth it only for larger audiences.

Nothing here is started; this records the October 30 discussion for later.

## How it works today

Every client connection starts its own server process; sessions only meet on the desktop itself.

| Piece | Today | With two clients |
| --- | --- | --- |
| Server | One `gliff-server` per SSH connection | Two independent processes |
| Mirror screen | Each server captures the chosen output | Both see the same desktop; capture and encoding happen twice |
| Private screen | Each server creates its own headless output, named `gliff-<pid>` (`session.rs`), and never adopts another's | Two separate desktops, nothing shared |
| Keyboard and pointer | Each server adds its own virtual keyboard and pointer (`hypr-input`) | Both can type and move the one pointer; they fight over it |
| Clipboard | Each session syncs with its client (`clipboard.rs`) | A copy on the host reaches every client |
| Sound | Each session records and sends its own stream (`audio.rs`) | Both clients hear the host |
| Keep-awake | Per headless output (`KeepAwake`) | No conflict: each private screen has its own |

The Mac viewer can also open two tabs to the same machine; that is the same case, two servers on one host.

## What breaks or needs care

Most of it works; three things do not behave well with two sessions.

1. **Host speaker mute (`--mute-local-audio`).** Each server mutes the default output and restores *only what it muted* (`local_mute.rs`). The second session finds the speakers already muted, so it changes nothing. When the first session ends, it unmutes while the second is still playing, and the host's speakers come back on. Needs a count shared by the sessions on the host.
2. **Pointer and keyboard contention.** Two people on one mirrored desktop both drive the single pointer and focus. That is inherent to sharing; the fix is a choice, not a bug: one viewer can be view-only (Level 2).
3. **No sign of a second viewer.** Nothing tells the host user, or the first viewer, that someone else connected. The server already posts desktop notifications for slow pastes (`notify.rs`), so a "2 viewers connected" notice reuses that.
4. **Cost.** Each session captures and encodes separately. VA-API handles several encode sessions; integrated GPUs and the CPU fallback may struggle with two 4K streams.

## Levels of work

Each level builds on the one before; stop at whichever meets the need.

| Level | What you get | Work | Estimate |
| --- | --- | --- | --- |
| 0. Test | Confirmation that two clients share a desktop today | The checklist below | About an hour of testing |
| 1. Harden | Two clients behave correctly together | Shared mute count on the host (a small state file under `$XDG_RUNTIME_DIR`, with a lock); "N viewers connected" notification through `notify.rs`; a test in `local_mute.rs` for two sessions | 1–2 days |
| 2. View-only and joining | A second person can watch without control, or join a private screen | `--view-only` on the server (ignore keyboard, pointer and clipboard input), a per-machine `view_only` setting and a Machine menu item; list running private screens (`gliff-<pid>`) so the connection window can join one through the existing `--output` | 2–4 days |
| 3. One shared stream | One capture and encode, sent to every viewer, like VNC's shared mode | A per-desktop broker on the host that the first server starts and later servers join over a Unix socket; a keyframe when someone joins; one bitrate for viewers on different networks (the slowest, or a second encode); who decides a private screen's size; input rules (first come, or a control hand-off); protocol additions on both clients | 2–4 weeks |

Estimates assume Claude writes the code and you test on the Hyprland hosts and the Mac. Claude cannot run a Hyprland session, so every hardware step happens on your machines, as with the GPU video work.

## Level 0 checklist

Run with the current working branch; two clients can be a Mac and a Linux client, or two tabs on one Mac.

- [ ] Mirror: both clients show the same desktop and both can type
- [ ] Private: each client gets its own screen; closing one leaves the other
- [ ] Sound plays on both clients
- [ ] Clipboard: a copy on the host reaches both clients
- [ ] One client disconnects; the other keeps working
- [ ] With `--mute-local-audio`: note whether the host speakers come back while the other client still plays (expected: yes, the Level 1 bug)
- [ ] Host load with two 4K streams (`top`, `intel_gpu_top` or `radeontop`)

## Decisions

Three choices shape the work; the recommended answer is listed first in each.

| Decision | Recommended | Alternatives |
| --- | --- | --- |
| Scope | Levels 0 to 2: the same user on a few machines, plus an occasional watcher | Level 3 for many viewers or a weak host |
| Who controls input | Every viewer, with view-only as an opt-in per machine | First viewer only; an explicit "take control" hand-off |
| Shared state on the host | A small file and lock under `$XDG_RUNTIME_DIR/gliff/` | A broker process (needed anyway at Level 3) |

## References

```bibtex
@techreport{rfc6143,
  author      = {Richardson, Tristan and Levine, John},
  title       = {The Remote Framebuffer Protocol},
  institution = {IETF},
  type        = {RFC},
  number      = {6143},
  year        = {2011},
  url         = {https://www.rfc-editor.org/rfc/rfc6143},
  note        = {Section 7.3.1, ClientInit shared-flag: several viewers on one desktop}
}
@misc{xdg_basedir,
  title        = {XDG Base Directory Specification},
  author       = {{freedesktop.org}},
  howpublished = {\url{https://specifications.freedesktop.org/basedir-spec/latest/}},
  note         = {XDG_RUNTIME_DIR for per-user runtime state}
}
```

- [ ] Run the Level 0 checklist
- [ ] Choose the scope (Levels 0–2 or 3)
- [ ] Record the plan as an ADR in the macGliff project
