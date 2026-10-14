# Developing gliff

How to build gliff from source, run it on one machine, and test it. The
design is in `docs/architecture.md`; driver-specific behaviour is in
`docs/hardware-quirks.md`.

## Build

```
bin/build
```

`bin/build` runs `cargo build --release --locked`, and the binaries land in
`target/release/`. For a launcher entry that rebuilds this checkout when
opened, run `bin/install-dev` and choose **Gliff (Development)**; build
failures raise a notification and are logged to `target/dev-build.log`. The
remote end of an SSH session still runs the `gliff-server` installed there.

Needs Rust, a C++ toolchain (the vendored OpenH264 build; nasm speeds it up),
and the runtime libraries in the `pkgbuild/PKGBUILD` `depends`. The GPU tier
needs a Vulkan driver for compute (Mesa RADV or ANV) and a VA-API H.264
driver on the same GPU (`mesa` on AMD, `intel-media-driver` on Intel);
without both, gliff uses the CPU tier. The compute shaders are committed as SPIR-V
(`crates/gliff-vk/shaders/build.sh` rebuilds them with `glslc`), and the
libva bindings as bindgen output (`crates/gliff-va/bindings/gen.sh`).

Verify the machine first:

```
gliff-probe all                  # protocols, outputs, GPU tier, GPU + CPU round-trips
gliff-probe pipeline             # capture one frame and run the whole 4:4:4 path
gliff-probe --video cpu roundtrip  # the CPU (OpenH264) tier alone
```

To run under the Khronos validation layer:

```
VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation gliff-probe roundtrip
```

## Run on one machine

Start a nested Hyprland, then inside it:

```
gliff-server --listen 127.0.0.1:9000 --headless
gliff --connect 127.0.0.1:9000
```

`--listen` and `--connect` have no authentication and are for development
only. `--video cpu` (or `GLIFF_VIDEO=cpu`) on either end forces the CPU tier,
and `--full-chroma` on a CPU server keeps 4:4:4. Server and client pick their
tiers independently, so every pairing can be tested on one machine.

## Testing

- **`scripts/check.sh`** runs what CI runs (`cargo fmt --check`, `cargo
  clippy --all-targets -- -D warnings`, `cargo test --workspace`); run it
  before every push, and `scripts/check.sh --fix` to apply the rustfmt and
  clippy fixes first. Clippy lints gated on the MSRV (`rust-version` in
  `Cargo.toml`) switch on across the whole workspace when it is raised.
- **Unit tests** cover the pure logic: AVC444 split/recombine losslessness,
  single-stream subsample/upsample, BGRA↔YUV444 colour round-trip, the H.264
  header parser against an x264 stream, framing with payloads and partial
  writes, the rate controller and ladder against a simulated link (a fake
  clock drives satellite, LAN and collapse scenarios), keymap building,
  Hyprland instance discovery, and the clipboard rules and engine (mime
  filtering, URI lists, safe paths, the send window and assembler, chunked
  transfers between two engines, the size cap, and a spooled directory
  tree).
- **`gliff-probe`** is the hardware integration harness: `protocols`,
  `outputs`, `permissions` (Hyprland settings that can block capture),
  `gpu` (the Vulkan device and the VA-API codec capabilities),
  `surfaces` (a VA-API surface written by the split shader and read back
  through the driver), `roundtrip` (synthetic BGRA → encode → decode → PSNR
  against the CPU reference), `capture`, `input`, `pipeline` (a captured
  dmabuf through the exact server and client pipelines), `serve-test` (a
  headless protocol client), `stream-bench` (startup milestones, fps,
  latency, interval and size statistics, `--timeline`, `--csv`), `clipboard`
  and `keymap` (watch what the compositor serves), `bench` (the CPU
  reference conversion), and `all` (every non-interactive check). Run it under
  `VK_LAYER_KHRONOS_validation` after touching `gliff-vk`, and with
  `RUST_LOG=libva=debug` to see the driver's own messages.
- **`scripts/e2e.sh`** boots a nested Hyprland and asserts PASS across the probe
  checks, the capture pipeline, both Dual420 and Single420 server-plus-client
  streams, the CPU tier matrix (cpu<->cpu and each mixed pairing), the
  clipboard in both directions (as text, as a 1 MiB binary item and as a
  copied directory tree), a mirrored-output resize, a keymap sent
  mid-session, and the GTK client's shortcut inhibit across repeated focus
  changes. It needs a Hyprland session, so it is not a CI unit test; run
  it on a target machine. Without the GPU tier it skips the GPU cases and
  still runs the CPU cases.
- **`scripts/bench.sh`** runs a server and `stream-bench` in a nested
  Hyprland over a shaped link: `BENCH_PRESET=lan|dsl|satellite`, a
  token-bucket TCP proxy (`scripts/throttle-proxy.py`) or `tc netem` in an
  unprivileged network namespace (`scripts/netem.sh`, loss and delay in both
  directions). `BENCH_STATIC=1` drops the damage loop.

## Packaging

`pkgbuild/` holds the launcher entry, the icon, and a `PKGBUILD` that builds
the checkout with `bin/build`; `bin/install` builds and installs that
package. The package in the [Omarchy Package
Repository](https://github.com/omacom/omarchy-pkgs) has its own recipe,
`pkgbuilds/gliff`, which builds a release tag and installs the launcher
entry and the icon from `pkgbuild/`.

## Release

The version is set in one place, `[workspace.package]` in `Cargo.toml`;
`pkgbuild/PKGBUILD` reads it from there.

1. Set the version in `Cargo.toml`, then run `cargo build` to update
   `Cargo.lock`.
2. Commit the two files as "Release gliff 0.2.0".
3. Tag the commit `v0.2.0` and push the commit and the tag.

CI checks that the tag matches the version, runs the checks, and publishes
the GitHub release. The Omarchy Package Repository watches the tags and
opens the pull request that updates its recipe.
