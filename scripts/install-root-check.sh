#!/bin/sh
# Check bin/install's root steps with stand-ins for pacman, makepkg, id and
# the root tools, so it runs anywhere (no Arch, no root):
#   - the root tool order: GLIFF_AUTH, makepkg.conf's PACMAN_AUTH (the
#     user's, then the system's), sudo, doas, su;
#   - a missing build dependency and the package are installed through the
#     chosen tool, and makepkg is never asked to install (-s, -i, -r).
# Run by scripts/check.sh.
set -eu
ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/gliff-install-check.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
FAILED=0
ok() { printf 'ok   %s\n' "$*"; }
bad() { printf 'FAIL %s\n' "$*"; FAILED=1; }

LOG="$WORK/calls.log"
FAKE="$WORK/fake"; mkdir -p "$FAKE" "$WORK/home/.config/pacman" "$WORK/pkgdest"
cat > "$FAKE/id" <<'S'
#!/bin/sh
echo 1000
S
cat > "$FAKE/pacman" <<S
#!/bin/sh
echo "pacman \$*" >> "$LOG"
case "\$1" in
    -T) echo cmake; exit 127 ;;
    *) exit 0 ;;
esac
S
cat > "$FAKE/makepkg" <<S
#!/bin/sh
echo "makepkg \$*" >> "$LOG"
case "\$*" in
    *--printsrcinfo*) printf 'pkgbase = gliff\n\tmakedepends = cargo\n\tmakedepends = cmake\n\tdepends = gtk4\n' ;;
    *--packagelist*) echo "$WORK/pkgdest/gliff-1-1-x86_64.pkg.tar.zst"; echo "$WORK/pkgdest/gliff-debug-1-1-x86_64.pkg.tar.zst" ;;
    *) : > "$WORK/pkgdest/gliff-1-1-x86_64.pkg.tar.zst"; : > "$WORK/pkgdest/gliff-debug-1-1-x86_64.pkg.tar.zst" ;;
esac
S
# Root tools: record which one ran, then run the command.
for tool in sudo doas rootme; do
    cat > "$FAKE/$tool" <<S
#!/bin/sh
echo "$tool \$*" >> "$LOG"
exec "\$@"
S
done
chmod +x "$FAKE"/*
# The system tools bin/install needs, without any real sudo or doas.
SYS="$WORK/sys"; mkdir -p "$SYS"
for t in sh dirname sed awk tr tail; do ln -s "$(command -v "$t")" "$SYS/$t"; done

# run [ENV=VALUE…]: bin/install with only the stand-ins and core tools.
run() {
    : > "$LOG"
    env -i PATH="$FAKE:$SYS" HOME="$WORK/home" XDG_CONFIG_HOME="$WORK/home/.config" \
        MAKEPKG_CONF="$WORK/etc.conf" "$@" sh "$ROOT/bin/install" > "$WORK/out" 2>&1
}
# expect TOOL: both root steps ran through TOOL, makepkg only built.
expect() {
    tool=$1 what=$2
    if grep -q "^$tool pacman -S --needed --asdeps cmake\$" "$LOG" &&
       grep -q "^$tool pacman -U $WORK/pkgdest/gliff-1-1-x86_64.pkg.tar.zst\$" "$LOG"; then
        ok "$what: dependencies and package installed with $tool, not the debug package"
    else
        bad "$what: wanted $tool (calls: $(tr '\n' ';' < "$LOG"); output: $(tail -n 3 "$WORK/out"))"
    fi
}

: > "$WORK/etc.conf"
run GLIFF_AUTH=rootme && expect rootme "GLIFF_AUTH"
printf 'PACMAN_AUTH=(doas)\n' > "$WORK/home/.makepkg.conf"
run && expect doas "PACMAN_AUTH in ~/.makepkg.conf"
: > "$WORK/home/.makepkg.conf"; printf 'PACMAN_AUTH=(doas)\n' > "$WORK/etc.conf"
run && expect doas "PACMAN_AUTH in /etc/makepkg.conf"
: > "$WORK/etc.conf"
# With neither set, sudo comes first.
run && expect sudo "no setting: sudo"

# Only doas installed (the doas-only system that makepkg -si fails on).
mv "$FAKE/sudo" "$WORK/sudo.off"
run && expect doas "only doas installed"
mv "$WORK/sudo.off" "$FAKE/sudo"

if grep '^makepkg ' "$LOG" | grep -v -e '--printsrcinfo' -e '--packagelist' | grep -q -E '[[:space:]](-[a-zA-Z]*[sir][a-zA-Z]*|--(syncdeps|install|rmdeps))([[:space:]]|$)'; then
    bad "makepkg was asked to install or remove: $(grep '^makepkg ' "$LOG" | tr '\n' ';')"
else
    ok "makepkg only builds"
fi

if [ "$FAILED" = 0 ]; then echo "INSTALL ROOT CHECK PASS"; else echo "INSTALL ROOT CHECK FAILED"; exit 1; fi
