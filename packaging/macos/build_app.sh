#!/usr/bin/env bash
# Builds SeeMyMac.app (universal: Apple silicon and Intel, macOS 13 or later) and, optionally,
# signs it with a Developer ID, has Apple notarize it, and packages the results:
#
#   target/macos/SeeMyMac-<version>-macOS.zip  the notarized app, which the in-app updater
#                                              downloads (its name must contain "macOS" and
#                                              end in .zip; see src/update.rs)
#   target/macos/SeeMyMac-<version>-macOS.dmg  the same app in a disk image for first installs
#
# The app lives in the user's profile (~/Applications). The disk image holds just the app:
# drag it into ~/Applications (create that folder if it is not there). The updater also installs
# there when the running copy's folder is not writable.
#
# Usage: packaging/macos/build_app.sh [--sign] [--notarize] [--dmg] [--all] [--skip-build]
#   --sign        Sign with SIGN_IDENTITY (hardened runtime, secure timestamp)
#   --notarize    Submit to Apple, wait, and staple the ticket (implies --sign)
#   --dmg         Also build the disk image
#   --all         --sign --notarize --dmg
#   --skip-build  Reuse the binaries already in target/
#
# Settings (environment variables; the defaults are the same account as ProjectNotes):
#   SIGN_IDENTITY              Developer ID Application certificate name
#   NOTARIZE_KEYCHAIN_PROFILE  created once with: xcrun notarytool store-credentials <profile>
#   TARGETS                    Rust targets to build (default: both Mac architectures); set one
#                              (e.g. aarch64-apple-darwin) for a quick local build

set -euo pipefail

TEAM_ID="${TEAM_ID:-2PY624KHXH}"
SIGN_IDENTITY="${SIGN_IDENTITY:-Developer ID Application: Paul McKinney (2PY624KHXH)}"
NOTARIZE_KEYCHAIN_PROFILE="${NOTARIZE_KEYCHAIN_PROFILE:-ProjectNotes-Notarize}"
TARGETS="${TARGETS:-aarch64-apple-darwin x86_64-apple-darwin}"
MIN_MACOS="13.0"
BUNDLE_ID="io.gitlab.kestermckinney.SeeMyMac"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
OUT="${ROOT}/target/macos"
APP="${OUT}/SeeMyMac.app"

DO_SIGN=false; DO_NOTARIZE=false; DO_DMG=false; DO_BUILD=true
for arg in "$@"; do
    case "$arg" in
        --sign) DO_SIGN=true ;;
        --notarize) DO_SIGN=true; DO_NOTARIZE=true ;;
        --dmg) DO_DMG=true ;;
        --all) DO_SIGN=true; DO_NOTARIZE=true; DO_DMG=true ;;
        --skip-build) DO_BUILD=false ;;
        *) echo "Unknown option: $arg" >&2; exit 1 ;;
    esac
done

log() { printf '\n==> %s\n' "$*"; }
die() { echo "error: $*" >&2; exit 1; }

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "${ROOT}/Cargo.toml" | head -n 1)"
[[ -n "${VERSION}" ]] || die "could not read the version from Cargo.toml"
ZIP="${OUT}/SeeMyMac-${VERSION}-macOS.zip"
DMG="${OUT}/SeeMyMac-${VERSION}-macOS.dmg"

# ── Build ─────────────────────────────────────────────────────────────────────
export MACOSX_DEPLOYMENT_TARGET="${MIN_MACOS}"
BINARIES=()
for t in ${TARGETS}; do
    if [[ "${DO_BUILD}" == true ]]; then
        log "Building ${t}"
        rustup target add "${t}" >/dev/null
        (cd "${ROOT}" && cargo build --release --locked --bin see_my_mac --target "${t}")
    fi
    bin="${ROOT}/target/${t}/release/see_my_mac"
    [[ -x "${bin}" ]] || die "missing ${bin}"
    BINARIES+=("${bin}")
done

# ── Bundle ────────────────────────────────────────────────────────────────────
log "Creating SeeMyMac.app ${VERSION}"
rm -rf "${OUT}"; mkdir -p "${APP}/Contents/MacOS" "${APP}/Contents/Resources"
lipo -create "${BINARIES[@]}" -output "${APP}/Contents/MacOS/see_my_mac"

ICONSET="${OUT}/seemymac.iconset"; mkdir -p "${ICONSET}"
for s in 16 32 128 256 512; do
    sips -z "$s" "$s" "${ROOT}/assets/icons/seemac.png" --out "${ICONSET}/icon_${s}x${s}.png" >/dev/null
    sips -z "$((s * 2))" "$((s * 2))" "${ROOT}/assets/icons/seemac.png" --out "${ICONSET}/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "${ICONSET}" -o "${APP}/Contents/Resources/seemymac.icns"
rm -rf "${ICONSET}"

cat > "${APP}/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleExecutable</key><string>see_my_mac</string>
	<key>CFBundleIconFile</key><string>seemymac</string>
	<key>CFBundleIdentifier</key><string>${BUNDLE_ID}</string>
	<key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
	<key>CFBundleName</key><string>SeeMyMac</string>
	<key>CFBundleDisplayName</key><string>SeeMyMac</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>${VERSION}</string>
	<key>CFBundleVersion</key><string>${VERSION}</string>
	<key>LSMinimumSystemVersion</key><string>${MIN_MACOS}</string>
	<key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
	<key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
plutil -lint "${APP}/Contents/Info.plist" >/dev/null

# ── Sign ──────────────────────────────────────────────────────────────────────
if [[ "${DO_SIGN}" == true ]]; then
    log "Signing with ${SIGN_IDENTITY}"
    codesign --force --sign "${SIGN_IDENTITY}" --options runtime --timestamp "${APP}"
    codesign --verify --deep --strict --verbose=2 "${APP}"
else
    echo "Not signing (pass --sign). The updater and Gatekeeper will treat this as untrusted."
fi

zip_app() { rm -f "${ZIP}"; ditto -c -k --keepParent "${APP}" "${ZIP}"; }
notarize() {
    xcrun notarytool submit "$1" --team-id "${TEAM_ID}" --keychain-profile "${NOTARIZE_KEYCHAIN_PROFILE}" --wait
}

# ── Notarize and staple, then zip ─────────────────────────────────────────────
zip_app
if [[ "${DO_NOTARIZE}" == true ]]; then
    log "Notarizing the app"
    notarize "${ZIP}"
    xcrun stapler staple "${APP}"
    xcrun stapler validate "${APP}"
    zip_app   # the shipped zip carries the stapled ticket
fi
log "Built ${ZIP}"

# ── Disk image ────────────────────────────────────────────────────────────────
if [[ "${DO_DMG}" == true ]]; then
    log "Creating the disk image"
    STAGE="${OUT}/dmg"; mkdir -p "${STAGE}"
    ditto "${APP}" "${STAGE}/SeeMyMac.app"
    cat > "${STAGE}/Install.txt" <<TXT
Drag SeeMyMac into the Applications folder inside your own home folder (~/Applications;
create it if it is not there). SeeMyMac then updates itself without asking for a password.
TXT
    rm -f "${DMG}"
    hdiutil create -volname "SeeMyMac ${VERSION}" -srcfolder "${STAGE}" -ov -format UDZO "${DMG}" >/dev/null
    rm -rf "${STAGE}"
    if [[ "${DO_SIGN}" == true ]]; then codesign --sign "${SIGN_IDENTITY}" --timestamp "${DMG}"; fi
    if [[ "${DO_NOTARIZE}" == true ]]; then
        notarize "${DMG}"
        xcrun stapler staple "${DMG}"
    fi
    log "Built ${DMG}"
fi

ls -lh "${OUT}"
