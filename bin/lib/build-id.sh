# shellcheck shell=sh
#= The build ID of a checkout, shared by the macOS and Linux build, install
#= and release scripts: COMMIT[-dirty]+ARCH, e.g. 0707147a1b2c+arm64 or
#= 0707147a1b2c+x86_64; "unknown+ARCH" outside git. crates/gliff-macos's
#= build.rs uses the same form, so About Gliff matches.
#= Source it; it defines one function and sets nothing else.
#= Version: 1.0.0

#= gliff_build_id ROOT: what a build of the checkout at ROOT would be.
#= Untracked files are ignored: cargo builds only what tracked code uses.
gliff_build_id() {
    _arch=$(uname -m)
    _commit=$(git -C "$1" rev-parse --short=12 HEAD 2>/dev/null) || { printf 'unknown+%s\n' "$_arch"; return; }
    if [ -n "$(git -C "$1" status --porcelain --untracked-files=no 2>/dev/null)" ]; then
        _commit="$_commit-dirty"
    fi
    printf '%s+%s\n' "$_commit" "$_arch"
}
