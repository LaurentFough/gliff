#!/bin/sh
#= linux-root-check.sh v1.0.0 — keep every root step of the Linux build and
#= install scripts going through gliff_root (bin/lib/linux-install.sh), so
#= doas, sudo and su all work (twice a doas user hit "doas: Authentication
#= required": makepkg -i on 2026-10-31, makepkg -s on 2026-11-04).
#=
#= 1. Rules over bin/ and the Makefile: no makepkg -s/-i/-r (or the long
#=    forms), no sudo/doas/su/pkexec, and pacman -S/-U/-R only after
#=    gliff_root. Messages (gliff_log, need_tool, echo) and comments are
#=    skipped.
#= 2. gliff_auth_tool's order: GLIFF_AUTH, makepkg.conf's PACMAN_AUTH (the
#=    user's, then the system's), sudo, doas, su.
#= 3. bin/package-linux with stand-ins for uname, pacman, makepkg and bsdtar:
#=    a missing build dependency is installed through GLIFF_AUTH, and makepkg
#=    is never asked to install anything.
#= POSIX sh; runs anywhere (scripts/check.sh). Usage: scripts/linux-root-check.sh [-v]
#= Logging: one line per check to stdout; -v also prints the commands seen.
set -eu
ROOT="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
VERBOSE=0; [ "${1:-}" = -v ] && VERBOSE=1
FAILED=0
ok() { printf 'ok   %s\n' "$*"; }
bad() { printf 'FAIL %s\n' "$*"; FAILED=1; }
WORK=$(mktemp -d "${TMPDIR:-/tmp}/gliff-root-check.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
trap 'exit 1' HUP INT TERM

#= --- 1. rules ------------------------------------------------------------
#= Shell lines that run something: drop comments and message lines.
code_lines() {
    grep -n '' "$1" | grep -v -E '^[0-9]+:[[:space:]]*#' \
        | grep -v -E 'gliff_log|need_tool|echo |printf |log '"'"'|log "|usage:' || true
}
#= check_file FILE NAME: report each rule FILE breaks (as NAME).
check_file() {
    f=$1 rel=$2
    lines=$(code_lines "$f")
    [ -n "$lines" ] || return 0
    hits=$(printf '%s\n' "$lines" | grep -E 'makepkg([^|;&#]*[[:space:]])(-[a-zA-Z]*[sir][a-zA-Z]*|--(syncdeps|install|rmdeps))([[:space:]]|$|\))' || true)
    [ -z "$hits" ] || bad "$rel: makepkg must not install or remove (use gliff_root pacman): $hits"
    #= gliff_auth_tool itself names the tools; it is the one place allowed.
    if [ "$rel" != bin/lib/linux-install.sh ]; then
        hits=$(printf '%s\n' "$lines" | grep -E '(^|[^A-Za-z_-])(sudo|doas|pkexec)([[:space:]]|$)|(^|[^A-Za-z_-])su[[:space:]]+(-c|root)' || true)
        [ -z "$hits" ] || bad "$rel: root through sudo/doas/su directly (use gliff_root): $hits"
    fi
    hits=$(printf '%s\n' "$lines" | grep -E '(^|[^A-Za-z_-])pacman[[:space:]]+-[A-Za-z]*[SUR]' | grep -v 'gliff_root pacman' || true)
    [ -z "$hits" ] || bad "$rel: pacman -S/-U/-R without gliff_root: $hits"
}
#= The rules catch what they are for (each sample must fail), before
#= they are trusted on the real scripts.
for sample in 'makepkg -si' '(cd x && makepkg -fs --noconfirm)' 'makepkg --syncdeps' \
              'sudo pacman -U x' 'doas pacman -S y' 'su -c "pacman -U x"' 'pacman -U "$pkg"'; do
    printf '%s\n' "$sample" > "$WORK/sample"
    before=$FAILED; FAILED=0
    check_file "$WORK/sample" sample > /dev/null
    if [ "$FAILED" = 1 ]; then FAILED=$before; else FAILED=$before; bad "rules missed: $sample"; fi
done
for sample in 'makepkg -f --noconfirm' 'makepkg --printsrcinfo' 'gliff_root pacman -U "$pkg"' 'pacman -T cmake' 'pacman -Q gliff'; do
    printf '%s\n' "$sample" > "$WORK/sample"
    before=$FAILED; FAILED=0
    check_file "$WORK/sample" sample > /dev/null
    if [ "$FAILED" = 0 ]; then FAILED=$before; else FAILED=$before; bad "rules refused a safe line: $sample"; fi
done
[ "$FAILED" = 0 ] && ok "rules: catch makepkg -s/-i/-r, sudo/doas/su and bare pacman -S/-U; allow the safe forms"
for f in $(find "$ROOT/bin" -type f ! -name '*.md' | sort; echo "$ROOT/Makefile"); do
    check_file "$f" "${f#"$ROOT"/}"
done
[ "$FAILED" = 0 ] && ok "rules: root steps in bin/ and the Makefile go through gliff_root"

#= --- 2. gliff_auth_tool order -------------------------------------------------
STUBS="$WORK/stubs"; mkdir -p "$STUBS" "$WORK/home/.config/pacman"
for t in sudo doas; do printf '#!/bin/sh\nexit 0\n' > "$STUBS/$t"; chmod +x "$STUBS/$t"; done
printf '#!/bin/sh\necho 1000\n' > "$STUBS/id"; chmod +x "$STUBS/id"
#= auth_case EXPECTED TOOLS PACMAN_AUTH_USER PACMAN_AUTH_ETC [GLIFF_AUTH]
auth_case() {
    want=$1 tools=$2 user=$3 etc=$4 gliff=${5:-}
    bin="$WORK/bin-$$-$want-$tools"; rm -rf "$bin"; mkdir -p "$bin"
    cp "$STUBS/id" "$bin/"
    for t in $tools; do cp "$STUBS/$t" "$bin/"; done
    : > "$WORK/home/.makepkg.conf"; : > "$WORK/etc.conf"
    [ -z "$user" ] || printf 'PACMAN_AUTH=(%s)\n' "$user" > "$WORK/home/.makepkg.conf"
    [ -z "$etc" ] || printf 'PACMAN_AUTH=(%s)\n' "$etc" > "$WORK/etc.conf"
    got=$(env -i PATH="$bin:/usr/bin:/bin" HOME="$WORK/home" XDG_CONFIG_HOME="$WORK/home/.config" \
        GLIFF_ETC_MAKEPKG_CONF="$WORK/etc.conf" GLIFF_AUTH="$gliff" ROOT="$ROOT" \
        sh -c '. "$ROOT/bin/lib/linux-install.sh"; gliff_auth_tool')
    #= /usr/bin may have a real sudo or doas: only judge cases it cannot change.
    if [ "$got" = "$want" ]; then ok "auth: $want (tools: ${tools:-none}, user: ${user:--}, etc: ${etc:--}, GLIFF_AUTH: ${gliff:--})"
    else bad "auth: wanted $want, got ${got:-root} (tools: ${tools:-none}, user: ${user:--}, etc: ${etc:--}, GLIFF_AUTH: ${gliff:--})"; fi
}
auth_case doas "sudo doas" "" "" doas
auth_case doas "sudo doas" "doas" ""
auth_case doas "sudo doas" "" "doas"
auth_case sudo "sudo doas" "sudo -k" "doas"
auth_case sudo "sudo doas" "" ""
if ! command -v sudo >/dev/null 2>&1 || [ "$(command -v sudo)" = "$STUBS/sudo" ]; then
    auth_case doas "doas" "" ""
fi

#= --- 3. bin/package-linux with stand-ins -------------------------------------
FAKE="$WORK/fake"; LOG="$WORK/calls.log"; mkdir -p "$FAKE"; : > "$LOG"
cat > "$FAKE/uname" <<'S'
#!/bin/sh
echo Linux
S
cat > "$FAKE/pacman" <<S
#!/bin/sh
echo "pacman \$*" >> "$LOG"
case "\$1" in
    -T) echo cmake; exit 127 ;;   # one build dependency missing
    -Q) exit 1 ;;
    *) exit 0 ;;
esac
S
cat > "$FAKE/makepkg" <<S
#!/bin/sh
echo "makepkg \$*" >> "$LOG"
case "\$*" in
    *--printsrcinfo*) printf 'pkgbase = gliff\n\tmakedepends = cargo\n\tmakedepends = cmake\n\tdepends = opus\n' ;;
    *) : > "\$PKGDEST/gliff-0.4.1-1-x86_64.pkg.tar.zst" ;;
esac
S
cat > "$FAKE/bsdtar" <<'S'
#!/bin/sh
case "$*" in
    *.PKGINFO*) printf 'pkgver = 0.4.1-1\narch = x86_64\n' ;;
    *build-id*) echo "$GLIFF_BUILD_ID" ;;
esac
S
#= Not root, so gliff_root uses the chosen tool (CI and containers run as root).
cp "$STUBS/id" "$FAKE/id"
#= The root tool the user chose: records the command, then runs it.
cat > "$FAKE/rootme" <<S
#!/bin/sh
echo "rootme \$*" >> "$LOG"
exec "\$@"
S
command -v sha256sum >/dev/null 2>&1 || printf '#!/bin/sh\nshasum -a 256 "$@"\n' > "$FAKE/sha256sum"
chmod +x "$FAKE"/*
if PATH="$FAKE:$PATH" GLIFF_AUTH=rootme "$ROOT/bin/package-linux" "$WORK/out" > "$WORK/package.out" 2>&1; then
    ok "package-linux ran with stand-ins"
else
    bad "package-linux failed with stand-ins: $(tail -n 5 "$WORK/package.out")"
fi
[ "$VERBOSE" = 1 ] && sed 's/^/     /' "$LOG"
if grep -q '^rootme pacman -S --needed --asdeps --noconfirm cmake$' "$LOG"; then
    ok "package-linux: the missing build dependency went through the chosen root tool"
else
    bad "package-linux: no 'rootme pacman -S … cmake' (calls: $(tr '\n' ';' < "$LOG"))"
fi
builds=$(grep '^makepkg ' "$LOG" | grep -v -- '--printsrcinfo' || true)
if [ -n "$builds" ] && ! printf '%s\n' "$builds" | grep -q -E '[[:space:]](-[a-zA-Z]*[sir][a-zA-Z]*|--(syncdeps|install|rmdeps))([[:space:]]|$)'; then
    ok "package-linux: makepkg only builds ($builds)"
else
    bad "package-linux: makepkg asked to install or remove, or did not build: ${builds:-none}"
fi

if [ "$FAILED" = 0 ]; then echo "LINUX ROOT CHECK PASS"; else echo "LINUX ROOT CHECK FAILED"; exit 1; fi
