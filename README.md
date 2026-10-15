# gliff

[![CI](https://github.com/omacom/gliff/actions/workflows/ci.yml/badge.svg)](https://github.com/omacom/gliff/actions/workflows/ci.yml)

gliff is a remote desktop for Hyprland and macOS. It shows the desktop of
another machine in a window on yours, and all it needs between the two is
ssh: no open ports, no accounts, no relay.

On Linux, full-resolution colour (4:4:4) keeps coloured text sharp, with
GPU video where supported and a CPU fallback. Mac hosts prefer hardware
H.264 through VideoToolbox; full-colour mode uses the CPU. Mac viewers
currently decode on the CPU.

![A gliff window showing a terminal on a remote machine](.github/assets/screenshot.png)

## What you get

- Mirror a physical screen on either platform. Hyprland hosts also provide
  private screens that follow the size of your window.
- Remote keyboard, mouse and scrolling. Linux clients send their keyboard
  layout; Mac clients use physical key positions and the host layout.
- Shared text clipboard in both directions. Linux-to-Linux sessions also
  transfer images and copied files.
- Linux hosts adapt quality to the link, giving up frame rate before sharpness.
- Linux viewers provide tabs, reconnection, and your Omarchy theme. Mac
  viewers use a native window with one connection.

## Requirements

- Linux with Hyprland, or macOS 13 Ventura or later, on either end.
- A logged-in desktop session on the remote machine. On a Mac host, grant
  Screen Recording and Accessibility to Gliff.
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
Xcode Command Line Tools are required):

```
bin/install-macos
```

This installs `~/Applications/Gliff.app`, with `gliff` and `gliff-server`
links in `~/.local/bin`. Open the app to select a nearby computer or enter an
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

**Clipboard.** On Linux, copy on one machine and paste on the other. Nothing is sent
until you paste. A paste that takes more than a second shows its progress
with a cancel button. Pasted files are kept in `~/.cache/gliff/clipboard`.

Clipboard sharing trusts every connected host: a host can request the
advertised clipboard contents without a local paste. Use `gliff --no-clipboard`
to disable sharing in both directions when connecting to an untrusted host.
Local selections marked `x-kde-passwordManagerHint` are not shared.

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

## More

- [docs/architecture.md](docs/architecture.md): how it works, the design
  choices, measurements, and what is not built yet.
- [docs/hardware-quirks.md](docs/hardware-quirks.md): what each GPU driver
  and Hyprland do, and what is tested where.
- [docs/development.md](docs/development.md): building, running on one
  machine, and the tests.

gliff is released under the MIT license.
