# shellcheck shell=sh
#= Shared by bin/install-macos, bin/install-macos-package and
#= bin/install-macos-release: the build ID, the signing identity choice,
#= signing an app bundle, and the safe swap into ~/Applications.
#= Source it after setting ROOT; it defines functions and sets nothing else.
#= Version: 1.0.0

GLIFF_DEST_APP="${GLIFF_DEST_APP:-$HOME/Applications/Gliff.app}"

#= gliff_build_id: shared with the Linux scripts (bin/lib/build-id.sh).
. "$ROOT/bin/lib/build-id.sh"

#= gliff_plist_value APP KEY: a string from APP's Info.plist, or nothing.
gliff_plist_value() {
    [ -f "$1/Contents/Info.plist" ] || return 0
    plutil -extract "$2" raw -o - "$1/Contents/Info.plist" 2>/dev/null || true
}

#= gliff_valid_fingerprint VALUE: a 40-hex-digit SHA-1 certificate fingerprint.
gliff_valid_fingerprint() {
    case "$1" in *[!a-fA-F0-9]*|'') return 1 ;; esac
    [ "${#1}" -eq 40 ]
}

#= gliff_pick_identity: set GLIFF_SIGNING_IDENTITY. Order: GLIFF_CODESIGN_IDENTITY,
#= then the identity the installed app was signed with (GliffSigningIdentity),
#= then this Mac's self-signed "gliff-local", then a free Apple ID's "Apple
#= Development" certificate, else ad-hoc ("-"). A stable identity keeps
#= Screen Recording and Accessibility grants across updates.
gliff_pick_identity() {
    _saved=$(gliff_plist_value "$GLIFF_DEST_APP" GliffSigningIdentity)
    gliff_valid_fingerprint "$_saved" || _saved=-
    GLIFF_SIGNING_IDENTITY="${GLIFF_CODESIGN_IDENTITY:-$_saved}"
    if [ "$GLIFF_SIGNING_IDENTITY" = - ] && [ -z "${GLIFF_CODESIGN_IDENTITY:-}" ]; then
        _found=$(security find-identity -v -p codesigning 2>/dev/null | awk '
            $3 == "\"gliff-local\"" { own = $2 }
            /"Apple Development:/ && dev == "" { dev = $2 }
            END { if (own != "") print own; else if (dev != "") print dev }
        ')
        if [ -n "$_found" ]; then
            GLIFF_SIGNING_IDENTITY="$_found"
            printf 'Signing with local identity %s\n' "$GLIFF_SIGNING_IDENTITY"
        fi
    fi
    if [ "$GLIFF_SIGNING_IDENTITY" != - ] && ! gliff_valid_fingerprint "$GLIFF_SIGNING_IDENTITY"; then
        printf 'GLIFF_CODESIGN_IDENTITY must be a certificate SHA-1 fingerprint (40 hex digits), not "%s".\n' "$GLIFF_SIGNING_IDENTITY" >&2
        return 1
    fi
}

#= gliff_sign_app APP IDENTITY: (re)sign a built bundle as bin/build-macos-app
#= does (development mode), recording IDENTITY in its Info.plist first so
#= later installs reuse it.
gliff_sign_app() {
    plutil -replace GliffSigningIdentity -string "$2" "$1/Contents/Info.plist"
    codesign --force --sign "$2" --identifier org.omacom.gliff.native "$1/Contents/MacOS/gliff-native"
    codesign --force --sign "$2" --identifier org.omacom.gliff.server "$1/Contents/MacOS/gliff-server"
    codesign --force --sign "$2" "$1"
    codesign --verify --deep --strict "$1"
}

#= gliff_place_app STAGED_APP STAGE_DIR: move a finished bundle to
#= $GLIFF_DEST_APP, keeping the old one in STAGE_DIR until the move is done
#= (gliff_restore_previous puts it back on failure), then link the
#= command-line tools into ~/.local/bin.
gliff_place_app() {
    mkdir -p "$(dirname -- "$GLIFF_DEST_APP")" "$HOME/.local/bin"
    if [ -e "$GLIFF_DEST_APP" ]; then
        mv "$GLIFF_DEST_APP" "$2/previous.app"
    fi
    mv "$1" "$GLIFF_DEST_APP"
    for _name in gliff gliff-server; do
        ln -sfn "$GLIFF_DEST_APP/Contents/MacOS/$_name" "$HOME/.local/bin/$_name"
    done
    printf 'Installed %s (%s)\n' "$GLIFF_DEST_APP" "$(gliff_plist_value "$GLIFF_DEST_APP" GliffBuildID)"
}

#= gliff_restore_previous STAGE_DIR: an EXIT trap helper; undoes a half swap.
gliff_restore_previous() {
    if [ -d "$1/previous.app" ] && [ ! -e "$GLIFF_DEST_APP" ]; then
        mv "$1/previous.app" "$GLIFF_DEST_APP"
    fi
    rm -rf "$1"
}

#= gliff_installed_is ID IDENTITY: the installed app is build ID, signed with
#= IDENTITY, and its signature still verifies. A dirty or unknown ID never
#= matches: its contents cannot be compared.
gliff_installed_is() {
    case "$1" in *-dirty+*|unknown+*) return 1 ;; esac
    [ "$(gliff_plist_value "$GLIFF_DEST_APP" GliffBuildID)" = "$1" ] || return 1
    [ "$(gliff_plist_value "$GLIFF_DEST_APP" GliffSigningIdentity)" = "$2" ] || return 1
    codesign --verify --deep --strict "$GLIFF_DEST_APP" 2>/dev/null
}

#= gliff_after_install IDENTITY: the closing hints every installer prints.
gliff_after_install() {
    printf 'Enable hosting with: %s --permissions --request\n' "$GLIFF_DEST_APP/Contents/MacOS/gliff-server"
    if [ "$1" = - ]; then
        printf 'Ad-hoc signing changes the permission identity on every install. Run bin/setup-macos-signing once (no Apple account needed), then reinstall to keep grants across updates.\n'
    fi
}
