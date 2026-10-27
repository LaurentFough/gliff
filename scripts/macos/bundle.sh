#!/bin/sh
#= bundle.sh — build gliff-mac and wrap it in Gliff.app.
#= Version: 0.1.0 (see crates/gliff-mac/CHANGELOG.md)
#=
#= Usage: scripts/macos/bundle.sh [--universal] [--sign IDENTITY] [--out DIR]
#=                                [--log FILE] [--syslog] [--debug] [--help]
#=
#=   --universal    build for arm64 and x86_64 and join them with lipo
#=   --sign ID      codesign with this identity (default: ad-hoc "-")
#=   --out DIR      where to put Gliff.app (default: target/macos)
#=   --log FILE     also append log lines to FILE
#=   --syslog       also send log lines to syslog via logger(1)
#=   --debug        trace every command (set -x)
#=
#= POSIX sh; needs Xcode command line tools (cc, codesign, lipo) and cargo.
#= The .app runs the client with no host; pass one through `open`:
#=   open -n target/macos/Gliff.app --args user@host
#= or run the binary inside it directly:
#=   target/macos/Gliff.app/Contents/MacOS/gliff-mac user@host

set -eu

SCRIPT_VERSION="0.1.0"
ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)"
OUT="$ROOT/target/macos"
SIGN="-"
UNIVERSAL=0
LOG_FILE=""
USE_SYSLOG=0

#= ---- logging -------------------------------------------------------------
log() {
    #= log LEVEL MESSAGE... → stderr, optional file, optional syslog.
    level="$1"; shift
    line="$(date '+%Y-%m-%dT%H:%M:%S') bundle.sh[$level] $*"
    printf '%s\n' "$line" >&2
    if [ -n "$LOG_FILE" ]; then
        printf '%s\n' "$line" >>"$LOG_FILE" || true
    fi
    if [ "$USE_SYSLOG" -eq 1 ] && command -v logger >/dev/null 2>&1; then
        logger -t gliff-bundle "[$level] $*" || true
    fi
}
die() { log ERROR "$*"; exit 1; }

usage() { sed -n 's/^#= \{0,1\}//p' "$0" | sed -n '1,22p'; }

#= ---- arguments -----------------------------------------------------------
while [ $# -gt 0 ]; do
    case "$1" in
        --universal) UNIVERSAL=1 ;;
        --sign) [ $# -ge 2 ] || die "--sign needs an identity"; SIGN="$2"; shift ;;
        --out) [ $# -ge 2 ] || die "--out needs a directory"; OUT="$2"; shift ;;
        --log) [ $# -ge 2 ] || die "--log needs a file"; LOG_FILE="$2"; shift ;;
        --syslog) USE_SYSLOG=1 ;;
        --debug) set -x ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown argument: $1 (try --help)" ;;
    esac
    shift
done

#= ---- checks --------------------------------------------------------------
[ "$(uname -s)" = "Darwin" ] || die "this script builds a macOS app; run it on macOS"
for tool in cargo codesign plutil; do
    command -v "$tool" >/dev/null 2>&1 || die "missing tool: $tool"
done
if [ "$UNIVERSAL" -eq 1 ]; then
    command -v lipo >/dev/null 2>&1 || die "missing tool: lipo"
    command -v rustup >/dev/null 2>&1 || die "--universal needs rustup to add targets"
fi

#= Version from the crate manifest, so the bundle matches the binary.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/crates/gliff-mac/Cargo.toml" | head -1)"
[ -n "$VERSION" ] || die "cannot read the gliff-mac version"
log INFO "bundle.sh $SCRIPT_VERSION: building gliff-mac $VERSION"

#= ---- build ---------------------------------------------------------------
cd "$ROOT"
BIN=""
if [ "$UNIVERSAL" -eq 1 ]; then
    for t in aarch64-apple-darwin x86_64-apple-darwin; do
        rustup target add "$t" >/dev/null || die "rustup target add $t failed"
        log INFO "cargo build --release -p gliff-mac --target $t"
        cargo build --release -p gliff-mac --target "$t" || die "build for $t failed"
    done
    BIN="$ROOT/target/gliff-mac-universal"
    lipo -create -output "$BIN" \
        "$ROOT/target/aarch64-apple-darwin/release/gliff-mac" \
        "$ROOT/target/x86_64-apple-darwin/release/gliff-mac" || die "lipo failed"
else
    log INFO "cargo build --release -p gliff-mac"
    cargo build --release -p gliff-mac || die "build failed"
    BIN="$ROOT/target/release/gliff-mac"
fi
[ -x "$BIN" ] || die "no binary at $BIN"

#= ---- bundle --------------------------------------------------------------
APP="$OUT/Gliff.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" || die "cannot create $APP"
cp "$BIN" "$APP/Contents/MacOS/gliff-mac" || die "cannot copy the binary"

cat >"$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Gliff</string>
    <key>CFBundleDisplayName</key><string>Gliff</string>
    <key>CFBundleIdentifier</key><string>com.omacom.gliff-mac</string>
    <key>CFBundleExecutable</key><string>gliff-mac</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleIconFile</key><string>gliff</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
</dict>
</plist>
PLIST
plutil -lint "$APP/Contents/Info.plist" >/dev/null || die "Info.plist does not validate"

#= ---- icon (best effort) --------------------------------------------------
#= SVG → 1024 px PNG with rsvg-convert, else Quick Look; then iconutil.
SVG="$ROOT/pkgbuild/gliff.svg"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM
PNG=""
if command -v rsvg-convert >/dev/null 2>&1; then
    rsvg-convert -w 1024 -h 1024 "$SVG" -o "$TMP/icon.png" && PNG="$TMP/icon.png"
elif command -v qlmanage >/dev/null 2>&1; then
    qlmanage -t -s 1024 -o "$TMP" "$SVG" >/dev/null 2>&1 && PNG="$TMP/gliff.svg.png"
fi
if [ -n "$PNG" ] && [ -f "$PNG" ] && command -v iconutil >/dev/null 2>&1; then
    SET="$TMP/gliff.iconset"
    mkdir -p "$SET"
    for s in 16 32 128 256 512; do
        sips -z "$s" "$s" "$PNG" --out "$SET/icon_${s}x${s}.png" >/dev/null
        d=$((s * 2))
        sips -z "$d" "$d" "$PNG" --out "$SET/icon_${s}x${s}@2x.png" >/dev/null
    done
    if iconutil -c icns "$SET" -o "$APP/Contents/Resources/gliff.icns"; then
        log INFO "icon added"
    else
        log WARN "iconutil failed; the app keeps the generic icon"
    fi
else
    log WARN "no SVG rasteriser (rsvg-convert or qlmanage); the app keeps the generic icon"
fi

#= ---- sign ----------------------------------------------------------------
log INFO "codesign (identity: $SIGN)"
codesign --force --deep --options runtime --sign "$SIGN" "$APP" || die "codesign failed"
codesign --verify --strict "$APP" || die "signature does not verify"

log INFO "done: $APP"
printf '%s\n' "$APP"
