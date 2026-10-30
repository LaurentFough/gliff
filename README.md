# gliff

[![CI](https://github.com/omacom/gliff/actions/workflows/ci.yml/badge.svg)](https://github.com/omacom/gliff/actions/workflows/ci.yml)

gliff is a remote desktop for Hyprland and macOS. It shows the desktop of
another machine in a window on yours, and all it needs between the two is
ssh: no open ports, no accounts, no relay.

On Linux, full-resolution colour (4:4:4) keeps coloured text sharp, with
GPU video where supported and a CPU fallback. Mac hosts prefer hardware
H.264 through VideoToolbox; full-colour mode uses the CPU. Mac viewers
decode in hardware (VideoToolbox, with the 4:4:4 recombine in Metal) and fall
back to the CPU where that is unavailable.

![A gliff window showing a terminal on a remote machine](.github/assets/screenshot.png)

## New in this fork

This fork ([LaurentFough/gliff](https://github.com/LaurentFough/gliff)) builds on acrogenesis' macOS client and host and adds the features below. See [ForkNews.md](ForkNews.md) for more details, and [ToDone.md](ToDone.md) for the full state and log; the detailed notes are under [Changes in this fork](#changes-in-this-fork).

<!-- The same list as ForkNews.md: update both together. -->

### Upstream PRs included

- **#47**: native macOS client and host, LAN discovery (acrogenesis). The base of the fork.
- **#98**: settings window and per-machine tab menu. Merged, and brought to the Mac client.
- **#80**: sound from Linux hosts (Opus over the existing connection). Merged.
- **#5**: dhh's macOS client. Only its VideoToolbox + Metal video path, ported into #47's viewer.

### Fork features

- **Tabs on the Mac**: every connection is a native macOS tab in one viewer. The tab bar's **+** or File ▸ New Tab adds a machine; tabs stay connected, only the visible one plays sound, and open tabs reopen at launch. *(Tested; Window ▸ Merge All Windows was missing, now added, retest pending.)*
- **Reconnect on the Mac**: a dropped or failed session shows a **Reconnect** button; sessions that had connected retry automatically (5 × 1.5 s, as on Linux), without repeating SSH prompts. *(Tested.)*
- **GPU video on the Mac viewer**: VideoToolbox hardware H.264 decode, Metal 4:4:4 recombine, `CAMetalLayer` presentation; asks for 4:4:4 first, falls back to OpenH264 (`--video cpu` forces it). 4K 4:4:4: about 6.9 ms per frame on the GPU vs 18.8 ms on the CPU.
- **Sound on the Mac viewer**: plays Linux hosts' sound through #80's `gliff-audio` (CoreAudio). **Machine ▸ Play Sound** shares the Linux client's mute switch; `--no-audio` starts muted.
- **Host speaker mute**: `gliff-server --mute-local-audio` mutes the Linux host's speakers while the client plays, and restores them on stop, disconnect or crash.
- **Shared settings**: a `gliff-settings` crate; both clients read and write `~/.config/gliff/config.toml`. On the Mac: a settings window (⌘,) and a Machine menu.
- **Local signing**: no paid Apple Developer account needed (a self-signed `gliff-local` identity), so privacy permissions survive rebuilds.
- **Makefile**: one front end for `bin/` and `scripts/`. `make macos` builds, signs, installs and verifies; `make install` on Arch; also `make check` and `make video-check`.
- **Checks**: `make video-check` checks GPU output against the CPU decoder and benchmarks both; the ssh login-shell test no longer depends on your shell setup.
- **Fixes**: Mac install and signing (`plutil`, a custom cargo `target-dir`); the client ignores stray sound packets from a host.

### Status

| Path | State |
|---|---|
| Linux host → Mac client | Tested: GPU video, sound, host mute |
| Mac tabs: several connections at once | Tested (2026-10-30) |
| Mac tabs: sound, clipboard, Machine menu, restore at launch | Tested (2026-10-30) |
| Mac tabs: Window ▸ Merge All Windows | Fixed; retest pending |
| Mac reconnect | Tested (2026-10-30) |
| Linux host → Linux client sound | Not retested on the merged branch |
| Sound from a Mac host | Not built yet |

## What you get

- Mirror a physical screen on either platform. Hyprland hosts also provide
  private screens that follow the size of your window.
- Remote keyboard, mouse and scrolling. Linux and Mac clients send their
  keyboard layout to Hyprland hosts. Mac hosts use their active local layout.
- Shared text clipboard in both directions. Linux-to-Linux sessions also
  transfer images and copied files.
- Sound from Linux hosts to Linux and Mac viewers, in both directions at
  once without echo, so two machines can show each other. Mac hosts do not
  send sound yet.
- Linux hosts adapt quality to the link, giving up frame rate before sharpness.
- Linux viewers provide tabs, reconnection, and your Omarchy theme. Mac
  viewers use native macOS tabs and reconnect after a dropped session.

## Requirements

- Linux with Hyprland, or macOS 13 Ventura or later, on either end.
- A logged-in desktop session on the remote machine. On a Mac host, grant
  Screen Recording and Accessibility to Gliff.
- For sound from a Linux host: PipeWire (`pw-record`, `pw-dump`, `pw-link`),
  or PulseAudio's `parec`. A Linux viewer plays through its ALSA default
  device; install `pipewire-alsa` or `pulseaudio-alsa` to route it through
  your sound server.
- ssh access from your machine to the remote one.
- For GPU video: an AMD or Intel GPU with its Vulkan and VA-API drivers
  (on Arch, `vulkan-radeon` and `mesa` for AMD, or `vulkan-intel` and
  `intel-media-driver` for Intel). Without them, and on NVIDIA, gliff uses
  the CPU and needs nothing extra.

## Install

Install gliff on both machines: your machine runs `gliff`, the remote one
runs `gliff-server`.

Gliff is currently available on the **edge** channel of the [Omarchy Package
Repository (OPR)](https://github.com/omacom/omarchy-pkgs):

```
omarchy pkg add gliff
```

Then open **Gliff** from the app launcher, or run `gliff` in a terminal.

On macOS, build and install the native app from this checkout (Rust and
Xcode Command Line Tools are required; an Apple Developer Program membership
is not):

```
make macos            # create a local signing identity once, build, sign, install, verify
make macos CHECK=1    # the same, running the CI checks first
```

`make macos` runs `bin/setup-macos-signing`, `bin/install-macos` and
`make verify` in that order. The first run creates a self-signed
`gliff-local` code-signing identity in your login keychain and asks for your
password once, to trust it for code signing; later runs reuse it. Signing
every build with the same identity keeps the Screen Recording, Accessibility
and Local Network grants across rebuilds, which ad-hoc signing does not.
`SIGNING=0` skips the identity and signs ad-hoc. Without make:

```
bin/setup-macos-signing   # once
bin/install-macos
```

The installer picks its signing identity in this order:
`GLIFF_CODESIGN_IDENTITY` (a 40-digit SHA-1 fingerprint), the identity saved
in the installed app, `gliff-local`, an "Apple Development" certificate (a
free Apple ID in Xcode › Settings › Accounts provides one), then ad-hoc. Such
builds run on Macs that trust the certificate; builds for other people need
Developer ID signing and notarization (`make package-release`, see
[macOS support](docs/macos.md)).

This installs `~/Applications/Gliff.app`, with `gliff` and `gliff-server`
links in `~/.local/bin`; `make verify` checks the signature and the links. Open the app to select a nearby computer or enter an
SSH address, or run:

```
~/.local/bin/gliff user@omarchy-host
~/.local/bin/gliff-server --permissions --request  # enable Mac hosting
```

Enable Gliff in **System Settings → Privacy & Security → Screen & System
Audio Recording** (called Screen Recording on older releases) and
**Accessibility**. Reconnect after granting permission. From Omarchy:

```
gliff user@mac-host
```

SSH finds the installed Mac app even when its non-interactive PATH does not
include `~/.local/bin`. See [macOS support](docs/macos.md) for display selection,
installation, testing, and the current feature differences.

The Mac app discovers nearby Gliff desktops through Bonjour on the same local
network. Allow Local Network access if macOS asks. Linux packages advertise
logged-in desktops through an Avahi publisher tied to the desktop session;
Avahi and SSH must be running. For a source/development installation, enable
advertisement as your desktop user:

```
bin/enable-discovery
```

The installed equivalent is `gliff-enable-discovery`. Add `--ssh-port 2222`
if SSH uses a different port, or `--disable` to stop advertising. Discovery
lists computers; selecting **Connect** starts the SSH and desktop session.
SSH host confirmation and authentication use native prompts on the Mac.

If your channel reports `target not found: gliff`, or you use Arch without
OPR, build and install the package from source:

```
git clone https://github.com/omacom/gliff.git
cd gliff
bin/install
```

This uses the current checkout, including fixes newer than the last release.

To build from source, see [docs/development.md](docs/development.md).

### Make targets

The `Makefile` is a front end for the scripts in `bin/` and `scripts/`: each
target calls the script that does the job, with the arguments it expects.
`make` lists them. Output goes to the terminal and to
`target/logs/<target>.log`; `SYSLOG=1` also logs each step's start and result
to syslog, and `V=1` shows the commands. Platform-specific targets stop with a
message on the other platform, and a failing script fails the target with its
exit status.

| Target | Runs |
|---|---|
| `make doctor` | tool check, cargo output directory, signing setup |
| `make build` | `bin/build` |
| `make check` / `make fix` | `scripts/check.sh` / `scripts/check.sh --fix` |
| `make test` | `cargo test --workspace --locked` |
| `make e2e` | `scripts/e2e.sh` (Linux, inside Hyprland) |
| `make install` | `bin/install`: the Mac app on macOS, `makepkg` on Arch |
| `make macos` | macOS: `signing`, `check` with `CHECK=1`, `install`, `verify` |
| `make signing` / `make signing-show` | `bin/setup-macos-signing` / its `--show` |
| `make app` | `bin/build-macos-app` into `target/macos/Gliff.app` (`APP=`, `SIGN_MODE=`) |
| `make verify` | checks the installed app's signature and `~/.local/bin` links |
| `make video-check` | checks the viewer's GPU video and compares GPU and CPU decode times (`ARGS="--cases 1080p-444"`) |
| `make package` | `bin/package-macos --development`: ad-hoc signed ZIP and checksum (`OUT=`) |
| `make package-release` | `bin/package-macos`: Developer ID, notarized (`GLIFF_NOTARY_PROFILE`) |
| `make icon` | `bin/export-macos-icon` |
| `make dev` / `make install-dev` | `bin/dev` / `bin/install-dev` (Linux) |
| `make discovery` / `make discovery-off` | `bin/enable-discovery` (`SSH_PORT=`) / `--disable` |
| `make clean` | `cargo clean` |
| `make distclean` | removes everything `.gitignore` ignores (`target/`, stray logs, `.DS_Store`, `patch/`): lists it, then asks (`FORCE=1` skips) |

Pass extra script or cargo arguments with `ARGS`, for example
`make build ARGS="-p gliff"`. The Makefile works with the GNU make 3.81 that
macOS ships.

## Use

```
gliff user@host
```

This mirrors the screen that has focus on the remote machine. To pick a
screen, or to get a private one:

```
gliff --output DP-1 user@host   # mirror a named remote screen
gliff --headless user@host      # a private remote screen, sized and
                                # scaled to this window
```

A mirrored screen keeps its own size and is fitted into the window. A private
screen follows the window as you resize it, and is removed when you
disconnect.

On Linux, every machine you connect to gets a tab in the title bar. Click a tab to
connect to that machine, or to switch to it: each connection keeps running
while you look at another, so switching is instant. Hover a tab for its
button: a stop square disconnects a running machine, and an X forgets one
that is not running. Drag the tabs to reorder them. The + after the tabs
opens the address bar; type `user@host` and press Enter to add a machine.
A bare `gliff` opens the window with a tab for each machine you have used,
and connects again to the ones that were connected when you closed it; with
no machines yet, it opens with the address bar ready.

**Keyboard.** While the pointer is over the picture, everything you type goes
to the remote machine, window-manager shortcuts included; move the pointer
off it to get your own back. Press `Shift+Esc` to get your shortcuts back
without moving the pointer. `--release-hotkey` changes that key (`ctrl+alt+q`,
`double-escape`, or `none`).

**Clipboard.** On Linux, copy on one machine and paste on the other. A copy announces
only its types, and the contents are sent when the other side asks for them,
normally when you paste. A paste that takes more than a second shows its
progress with a cancel button. Pasted files are kept in `~/.cache/gliff/clipboard`.

What you copy locally is offered only to the machine in the tab you are
looking at. Switching tabs withdraws it from the old machine and offers your
current clipboard to the new one.

Clipboard sharing trusts the host you are looking at: it can request the
advertised clipboard contents without a local paste. Use `gliff --no-clipboard`
to disable sharing in both directions when connecting to an untrusted host.
Local selections marked `x-kde-passwordManagerHint` are not shared.

Changing the clipboard cancels unfinished pastes from this machine to the
remote host. Wait for a file paste to finish before copying something else. Switching
tabs also cancels unfinished pastes to the tab you leave.

**Sound.** The window plays the sound of the machine it shows. The speaker
button in the header mutes it and remembers that; `gliff --no-audio` starts
muted. Only the tab you look at plays, and other tabs' hosts stop recording.
A host with PipeWire's `pw-record` and `pw-dump` sends what its applications
play except Gliff itself, so when two machines show each other, neither hears
its own sound come back. A host missing either sends its whole output, Gliff
included, and two such machines that show each other echo; mute one side to
stop it. Hosts keep playing their sound on their own speakers, unless the
host runs `gliff-server --mute-local-audio` (set it as the machine's server
command in Settings): then the host's speakers are muted while the client
plays its sound and restored when sound stops or the session ends, even if
the server is killed. That needs PipeWire with WirePlumber on the host
(`pw-dump`, `pw-record`, `wpctl`). Sound is Opus
at 128 kbit/s, and the client buffers 40 to 250 ms of it, more on a jittery
link.

**Slow links.** gliff measures the link and lowers the frame rate, then the
colour detail, then the resolution, and raises them again when there is
room. Two flags change the video, and you pass them to the server through
`--server-bin`: `--low-bandwidth` sends 4:2:0 colour from the start, and
`--full-chroma` keeps 4:4:4 on a server that encodes on the CPU.

```
gliff --server-bin 'gliff-server --low-bandwidth' user@host
```

## If something does not work

- On Linux, run `gliff-probe all`. It checks the compositor, the GPU
  drivers and both video paths, and prints PASS or FAIL for each.
- If ssh cannot find `gliff-server`, give its path:
  `gliff --server-bin /path/to/gliff-server user@host`.
- The server needs the remote user's running Hyprland session, which it
  finds under `XDG_RUNTIME_DIR`, or `/run/user/<uid>` when ssh does not set
  it. If the session is elsewhere, set it through
  `--server-bin 'env XDG_RUNTIME_DIR=/path gliff-server'`.
- To rule out a driver problem, force the CPU on your end with
  `gliff --video cpu user@host`.
- If the window connects but stays black, the remote GPU may allocate
  capture buffers it never fills (a VM's virtio-gpu with virgl, or a GPU the
  compositor does not render on). Have the compositor read the frames back
  instead: `gliff --server-bin 'gliff-server --capture shm' user@host`.
  The server then encodes on the CPU.
- Linux streams reduce screens wider than 4096 pixels. Mac hosting is capped
  at 1920×1200 and 30 fps.
- On macOS, `make doctor` shows the cargo output directory, the tools and the
  signing setup, and `make verify` checks the installed app. If macOS asks
  for Screen Recording or Accessibility again after every rebuild, the app is
  ad-hoc signed: run `make signing install`.
- A `build.target-dir` or `build.target` in `~/.cargo/config.toml`, or
  `CARGO_TARGET_DIR` / `CARGO_BUILD_TARGET`, moves cargo's output; the Mac
  app build asks cargo where the binaries are, so it works either way.

## Changes in this fork

This fork builds on
[acrogenesis/gliff `acrogenesis/macos-client-host`](https://github.com/acrogenesis/gliff/tree/acrogenesis/macos-client-host),
the macOS client and host (upstream PR #47), and adds the following.

### Upstream pull requests merged

- **#98, settings window and per-machine tab menu** (Linux client). Applied
  to this branch's Linux client, which lives in `crates/gliff/src/linux.rs`.
- **#80, sound from Linux hosts.** Opus over the existing connection, played
  through ALSA on Linux viewers. Mac hosts do not advertise sound; Mac
  viewers play it (see Sound on the Mac below).

Ported, not merged: #5 (dhh's macOS client, an alternative to #47's). Only
its VideoToolbox and Metal video path is taken; see GPU video below. Not
merged: #26 (HEVC for screens larger than H.264 allows, which predates
gliff 0.4.1).

### macOS

- **Tabs.** One viewer process hosts every connection as a native macOS
  tab (macOS only groups windows of one process); the tab bar's **+** or File
  ▸ New Tab opens the connection window for another machine. Every tab stays
  connected, only a visible tab plays sound, the clipboard and Machine menu
  follow the active tab, and open tabs reopen at launch (`[[open_tabs]]`).
  See [macOS support](docs/macos.md).
- **Reconnect.** A failed or dropped session shows its error with a
  **Reconnect** button. A session that had connected retries automatically,
  5 times 1.5 s apart, as on Linux; a failed first connection waits, so a
  wrong password does not reopen the SSH prompt; an incompatible server is
  never retried.
- **Per-machine settings.** #98's settings model is a shared
  `gliff-settings` crate, so both clients read and write the same
  `~/.config/gliff/config.toml`. The Mac has a settings window (⌘,), a
  Machine menu in the viewer, `--no-clipboard`, clipboard changes that apply
  at once, and screen or server-command changes that reconnect in the same
  window. The connection window follows each machine's saved screen. See
  [macOS support](docs/macos.md#settings) and [docs/settings.md](docs/settings.md).
- **GPU video.** The viewer decodes in hardware with VideoToolbox,
  recombines 4:4:4 in Metal and presents through a `CAMetalLayer`, and asks
  hosts for 4:4:4 first. It falls back to OpenH264 without a hardware
  decoder or after repeated errors; `--video cpu` or `decode = "cpu"` forces
  it. `make video-check` checks the output and compares decode times (4K
  4:4:4: 6.9 ms GPU against 18.8 ms CPU). See
  [macOS support](docs/macos.md).
- **Host speakers muted while streaming.** `gliff-server --mute-local-audio`
  mutes the Linux host's default output while a client plays its sound, follows
  a new default output, and restores what it changed on sound off, session end
  or a crash (a detached watchdog). Only when the host records each
  application directly (PipeWire), so the client never hears the mute.
- **Sound on the Mac.** The viewer plays a Linux host's sound with #80's
  `gliff-audio` (Opus, the jitter buffer, and a cpal player on CoreAudio).
  **Machine ▸ Play Sound** is the same `muted` switch as the Linux client's
  speaker button; `--no-audio` starts muted. See
  [macOS support](docs/macos.md#settings).
- **Signing without a paid developer account.** `bin/setup-macos-signing`
  creates, trusts and tests a self-signed `gliff-local` identity once, and
  `bin/install-macos` uses it (or an Apple Development certificate) so
  Screen Recording and Accessibility grants survive rebuilds.
- **Install fixes.** The installer reads the saved signing identity with
  `plutil` (a first install used to fail with `GLIFF_CODESIGN_IDENTITY must
  be a certificate SHA-1 fingerprint`), and the app build asks cargo where
  the binaries are (a moved target directory used to fail with
  `cp: target/release/gliff: No such file or directory`).

### Build and checks

- **Makefile**: one front end for `bin/` and `scripts/`, including the
  `make macos` pipeline, `make verify` and `make video-check`; see
  [Make targets](#make-targets).
- **ToDone.md**: a running report of this fork's state, pending work and a
  log of each commit or merge. **ForkNews.md**: the short list of what the
  fork adds, also at the top of this README.
- **Hermetic tests**: the ssh login-shell test runs each shell with an empty
  environment, so a developer's `ZDOTDIR`, `BASH_ENV` or `XDG_CONFIG_HOME`
  no longer breaks `scripts/check.sh`. The picker check also covers saved
  per-machine screens and the move of old picker preferences into the config.

## More

- [docs/architecture.md](docs/architecture.md): how it works, the design
  choices, measurements, and what is not built yet.
- [docs/hardware-quirks.md](docs/hardware-quirks.md): what each GPU driver
  and Hyprland do, and what is tested where.
- [docs/development.md](docs/development.md): building, running on one
  machine, and the tests.

gliff is released under the MIT license.
