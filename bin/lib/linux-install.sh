# shellcheck shell=sh
#= Shared by bin/install (Arch), bin/package-linux, bin/install-linux-package,
#= bin/release-linux and bin/install-linux-release: the build ID, what is
#= installed, and what a package file contains.
#= Source it after setting ROOT; it defines functions and sets nothing else.
#= Version: 1.1.0

. "$ROOT/bin/lib/build-id.sh"

#= Where the package records its build ID (PKGBUILD writes it).
GLIFF_BUILD_ID_FILE=/usr/share/gliff/build-id

#= gliff_log LEVEL MESSAGE…: to stderr, and to syslog with SYSLOG=1.
gliff_log() {
    _level=$1; shift
    printf '%s: %s\n' "$_level" "$*" >&2
    if [ "${SYSLOG:-0}" = 1 ] && command -v logger >/dev/null 2>&1; then
        logger -t gliff-install -p "user.$_level" -- "$*" || true
    fi
}

#= gliff_installed_build: the installed package's build ID, or nothing
#= (not installed, or installed before build IDs existed).
gliff_installed_build() {
    pacman -Q gliff >/dev/null 2>&1 || return 0
    [ -r "$GLIFF_BUILD_ID_FILE" ] && head -n 1 "$GLIFF_BUILD_ID_FILE" || true
}

#= gliff_pkg_field PKG FIELD: a field from the package's .PKGINFO
#= (pkgname, pkgver, arch, …); bsdtar comes with pacman (libarchive).
gliff_pkg_field() {
    bsdtar -xOf "$1" .PKGINFO 2>/dev/null | sed -n "s/^$2 = //p" | head -n 1
}

#= gliff_pkg_build PKG: the build ID inside a package file, or nothing.
gliff_pkg_build() {
    bsdtar -xOf "$1" usr/share/gliff/build-id 2>/dev/null | head -n 1
}

#= gliff_require_arch: stop unless this is an Arch-based system with pacman.
gliff_require_arch() {
    [ "$(uname -s)" = Linux ] || { gliff_log err "this script runs on Linux (Arch/Omarchy)"; exit 1; }
    command -v pacman >/dev/null 2>&1 || { gliff_log err "needs pacman (Arch Linux or Omarchy)"; exit 1; }
}

#= gliff_auth_tool: how to run a command as root, in this order: already
#= root (nothing); GLIFF_AUTH (e.g. doas); makepkg.conf's PACMAN_AUTH (the
#= user's, then /etc); sudo; doas; else su. Prints the command, empty for root.
gliff_auth_tool() {
    [ "$(id -u)" = 0 ] && return 0
    if [ -n "${GLIFF_AUTH:-}" ]; then printf '%s\n' "$GLIFF_AUTH"; return 0; fi
    for _conf in "${XDG_CONFIG_HOME:-$HOME/.config}/pacman/makepkg.conf" "$HOME/.makepkg.conf" /etc/makepkg.conf; do
        [ -r "$_conf" ] || continue
        #= PACMAN_AUTH=(doas) or PACMAN_AUTH=(sudo -k): the first word.
        _auth=$(sed -n 's/^[[:space:]]*PACMAN_AUTH=(\([^)]*\)).*/\1/p' "$_conf" | tail -n 1 | tr -d "'\"" | awk '{print $1}')
        if [ -n "$_auth" ] && command -v "$_auth" >/dev/null 2>&1; then printf '%s\n' "$_auth"; return 0; fi
    done
    for _tool in sudo doas; do
        if command -v "$_tool" >/dev/null 2>&1; then printf '%s\n' "$_tool"; return 0; fi
    done
    printf 'su\n'
}

#= gliff_root COMMAND…: run it as root with gliff_auth_tool, asking for a
#= password on the terminal when the tool needs one.
gliff_root() {
    _tool=$(gliff_auth_tool)
    case "$_tool" in
        '') "$@" ;;
        su)
            gliff_log info "running as root with su: $*"
            #= su -c takes one string; quote each argument.
            _cmd=""
            for _a in "$@"; do _cmd="$_cmd '$(printf %s "$_a" | sed "s/'/'\\\\''/g")'"; done
            su -c "$_cmd" ;;
        *)
            [ -t 0 ] || gliff_log warning "no terminal on stdin: $_tool may not be able to ask for a password"
            gliff_log info "running as root with $_tool: $*"
            "$_tool" "$@" ;;
    esac
}
