# macOS client and host

Gliff can connect from macOS to Hyprland, from Hyprland to macOS, or between
Macs. Both ends speak the same protocol over SSH. macOS needs version 13
Ventura or later, Rust, and Xcode Command Line Tools for a source build.

## Install on a Mac

Run `bin/install-macos` in the checkout. It builds the native client and host,
installs `~/Applications/Gliff.app`, and creates `~/.local/bin/gliff` and
`~/.local/bin/gliff-server`. Homebrew and rustup locations are included in the
build PATH, so this also works from an SSH session.

To retain permission grants across source updates, sign with a development
certificate from your keychain. Find its SHA-1 fingerprint with
`security find-identity -v -p codesigning`, then install with
`GLIFF_CODESIGN_IDENTITY=<fingerprint> bin/install-macos`. The installer saves
that choice in the app and reuses it on future updates. Without a certificate
it uses ad-hoc signing, which can require renewed grants after each rebuild.

Open Gliff to show **Nearby Computers**. Select a computer to fill in its
SSH username, hostname and port, then choose **Connect**. You can edit these
fields or enter a manual address. Cancel closes the picker without connecting.
Choose **Mirror screen** to see the host's existing desktop, or **Private
screen** for a separate desktop on a Linux host that follows the Mac window.
Mirror is the initial default; Gliff remembers the choice for each SSH address
and port. Mirroring keeps the host screen's proportions, so screens with a
different aspect ratio can have empty bands. Private mode follows the window
and fullscreen aspect ratio, within the viewer's 3840×2160 video limit.
The standalone connection window follows the Mac's appearance and accent color. Tab moves
between fields, Return connects, and Escape cancels; changing focus does not
start a connection.
It opens at the minimum size that fits its controls. The computer list grows
to show up to four rows, then scrolls; discovery preserves a window you enlarge.
The app asks for Local Network access on macOS versions that require it;
enable Gliff under System Settings → Privacy & Security → Local Network.

Or run:

```
~/.local/bin/gliff user@linux-host
~/.local/bin/gliff --headless user@linux-host
```

The Linux host needs Gliff installed and a running Hyprland session. The
`--headless` option creates a private screen there and follows the Mac window.

## Mac packages and release signing

`bin/package-macos --development` builds an ad-hoc signed ZIP and checksum in
`target/macos`, without changing an installed app. Development archives are
for testing and require macOS permission grants for that build. CI builds and
packages Apple Silicon and Intel independently.

Public releases use the maintainer's **Developer ID Application** certificate,
hardened runtime, notarization, and a stapled ticket. They include the app icon
and license notices. Configure the CI secrets `GLIFF_MACOS_CERTIFICATE_BASE64`
(exported certificate and private key as a base64-encoded P12),
`GLIFF_MACOS_CERTIFICATE_PASSWORD`, `GLIFF_MACOS_SIGNING_IDENTITY` (SHA-1),
`GLIFF_APPLE_ID`, `GLIFF_APPLE_TEAM_ID`, and `GLIFF_APPLE_APP_PASSWORD`
(notarization app-specific password). These credentials belong to the project
maintainer; do not commit them. Tag builds fail if signing is not configured,
and publish both Mac architectures and their SHA-256 checksums after validation.

For a local release build, set `GLIFF_CODESIGN_IDENTITY` to the Developer ID
fingerprint and `GLIFF_NOTARY_PROFILE` to a profile created with
`xcrun notarytool store-credentials`, then run `bin/package-macos`.
The packaging command does not accept a development certificate for a public
release and verifies Gatekeeper acceptance after notarization. See Apple's
[notarization workflow](https://developer.apple.com/documentation/security/customizing-the-notarization-workflow).

The binaries target macOS13. Live capture/input testing has currently been on
an M1 Max running macOS26.6.2; the Intel CI job checks builds and unit behavior,
not Screen Recording, Accessibility, or older operating-system runtime behavior.

## Discover nearby Omarchy desktops

The Mac browses `_gliff._tcp` Bonjour services on the local network. Linux
packages include a desktop-session service that advertises the SSH endpoint
through Avahi, including the logged-in username and SSH port. It starts with
the desktop and stops on logout; it does not capture video or open a Gliff TCP
listener. SSH and Avahi must be running on the host. Discovery requires both
computers on a network that permits mDNS multicast.

For a source installation, run `bin/enable-discovery` as the Linux desktop
user after building. Installed packages provide `gliff-enable-discovery`.
Use `--ssh-port 2222` for nonstandard SSH ports and `--disable` to stop
advertising. To run the publisher manually:

```
gliff-server --advertise
gliff-server --advertise --ssh-port 2222
```

Check discovery from the Mac without opening a viewer:

```
gliff --discover
```

This prints nearby computers as JSON after a bounded scan. A service's
advertised user is a suggestion; you can edit the SSH address. Host identity
and authentication are checked by SSH. First-time host confirmation and key
passphrase/password prompts appear as native dialogs. For a manual connection
with a nonstandard port, use `gliff --port 2222 user@host`.

Bonjour discovery is local to the LAN. Enter an SSH address manually for
routed networks, jump hosts and VPN-only peers.

## Host the Mac

On the Mac, choose **Gliff → Enable Remote Control…**, or run:

```
~/.local/bin/gliff-server --permissions --request
```

In System Settings → Privacy & Security, enable Gliff for Screen Recording
(Screen & System Audio Recording in recent releases) and Accessibility.
These grants allow capture and remote input; SSH access alone does not grant
screen capture. Reconnect after changing them. The Mac must have a logged-in
GUI session. Changing from ad-hoc signing to a certificate requires a new grant
once; subsequent builds with that certificate keep the same signing identity.

The SSH host launches a separate instance of Gliff through LaunchServices.
Capture and input run in that app's privacy context, and SSH relays the
connection through a private Unix socket. Permission checks also use this
path, so they check Gliff rather than the SSH daemon. The bridge uses bounded
buffers and closes the host instance when the connection ends.

From Linux:

```
gliff user@mac-host
```

The SSH launcher checks the app bundle when the SSH PATH lacks `gliff-server`.
An explicit `--server-bin` still overrides that discovery.

The main display is selected by default. List displays on the Mac with
`gliff-server --outputs`, then select one with:

```
gliff --output display-1 user@mac-host
```

Display IDs come from the Mac; use the ID listed by `--outputs`. A mirrored
stream fits the client's window while pointer coordinates remain in the Mac's
logical display coordinates. macOS hosts mirror physical screens and reject
`--headless` with a clear error.

## Current differences from Linux

- Native AppKit window with one connection per window. The Linux client's
  recent-host tabs, Omarchy theme and automatic reconnection are not yet in
  the macOS client.
- The Mac viewer requests cursor pixels inside the video. Updated Linux
  hosts use this mode without opening a separate cursor capture session,
  avoiding Hyprland 0.56's cursor-session crash on display disconnection.
- Mac hosts prefer VideoToolbox hardware H.264 encoding for the default
  single 4:2:0 stream, and verify that the session actually uses hardware.
  OpenH264 is the fallback if hardware creation or encoding fails. Host
  `--video cpu` forces software; `--full-chroma` uses the software dual-stream
  4:4:4 path. Capture is capped at 1920×1200 and 30 fps. The Mac viewer still
  decodes with OpenH264 on the CPU. A Linux viewer's `--video cpu` setting
  controls its own decoding, and does not disable hardware on the Mac host.
- Command maps to Super; Option maps to Alt. Hardware key positions are
  translated between macOS and evdev. Mac clients export their active Unicode
  keyboard layout to Hyprland, including Shift, Caps Lock, Option symbols and
  common dead keys, and update it when the input source changes. Mac hosts
  use their active local layout. Input-method composition and custom keys
  that produce multiple Unicode characters are not forwarded as text.
- Keyboard input is captured over the picture. Shift+Esc releases it;
  clicking the picture captures it again. Held keys and buttons are released
  when the window loses focus or disconnects.
- Shared text clipboard uses lazy paste requests, chunking and a 1 MiB cap.
  Image and copied-file clipboard transfers are not yet supported on macOS.

## Verify

Run `scripts/check.sh` on both platforms. The macOS checks include a capped
native pipe-reader memory regression,
without opening a window or capturing a screen. It verifies that temporary
Foundation buffers are released after each packet and that Retina-size
packets use bounded read buffers rather than churning large allocations.
Native keyboard checks read real German and US layouts without selecting them;
Linux tests compile the captured German symbols with the host's XKB library.
Picker checks cover screen preferences independently for each host and port.

The opt-in viewer regression feeds240 Retina-size frames to a temporary
window, bounds memory and cleans up without SSH or capture:

```
python3 scripts/macos-viewer-check.py
```

With a nearby host advertising Gliff, the optional integration check exercises
the actual picker, SSH-field prefill, deduplication and cancellation without
opening an SSH or video session:

```
python3 scripts/macos-discovery-check.py --expect-host computer.local --expect-user user
```

On a Mac:

```
gliff-server --permissions
gliff-server --outputs
gliff --check-frames 8 user@linux-host
```

From Linux, check a Mac host with:

```
gliff-probe --video cpu serve-test --ssh user@mac-host --frames 8
```

This also verifies a mirrored resize and keyframe decoding, using the same
SSH discovery as the GUI client.

`--check-frames` connects over SSH and decodes real frames without opening a
window. To exercise the UI, run the same connection without that flag and
check pointer motion, typing, resize, release/focus behavior and a text paste
in both directions. On Linux, `scripts/e2e.sh` verifies the video and input
paths in a nested Hyprland session, including the shared-memory software path.
