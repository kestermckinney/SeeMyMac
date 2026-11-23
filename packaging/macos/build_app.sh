#!/usr/bin/env bash
# Builds SeeMyMac.app (universal: Apple silicon and Intel, macOS 13 or later) and, optionally,
# signs it with a Developer ID, has Apple notarize it, and packages the results:
#
#   target/macos/SeeMyMac-<version>-macOS.zip  the notarized app, which the in-app updater
#                                              downloads (its name must contain "macOS" and
#                                              end in .zip; see src/update.rs)
#   target/macos/SeeMyMac-<version>-macOS.pkg  the installer for first installs
#
# The app lives in the user's profile (~/Applications). The installer only offers "Install for
# me only", so it puts the app there without an administrator password, after showing the
# licence for the user to agree to. Its artwork is packaging/macos/background.png (regenerate
# with packaging/art/render.sh). The updater also installs into ~/Applications when the running
# copy's folder is not writable.
#
# Usage: packaging/macos/build_app.sh [--sign] [--notarize] [--pkg] [--all] [--skip-build]
#   --sign        Sign with SIGN_IDENTITY (and the installer with INSTALLER_IDENTITY)
#   --notarize    Submit to Apple, wait, and staple the ticket (implies --sign)
#   --pkg         Also build the installer package
#   --all         --sign --notarize --pkg
#   --skip-build  Reuse the binaries already in target/
#
# Settings (environment variables; the defaults are the same account as ProjectNotes):
#   SIGN_IDENTITY              Developer ID Application certificate name
#   INSTALLER_IDENTITY         Developer ID Installer certificate name (signs the .pkg)
#   NOTARIZE_KEYCHAIN_PROFILE  created once with: xcrun notarytool store-credentials <profile>
#   NOTARY_API_KEY_PATH        App Store Connect API key (.p8) to notarize with instead of the profile;
#   NOTARY_API_KEY_ID          used by the GitHub Actions release workflow
#   NOTARY_API_ISSUER_ID
#   TARGETS                    Rust targets to build (default: both Mac architectures); set one
#                              (e.g. aarch64-apple-darwin) for a quick local build

set -euo pipefail

TEAM_ID="${TEAM_ID:-2PY624KHXH}"
SIGN_IDENTITY="${SIGN_IDENTITY:-Developer ID Application: Paul McKinney (2PY624KHXH)}"
INSTALLER_IDENTITY="${INSTALLER_IDENTITY:-Developer ID Installer: Paul McKinney (2PY624KHXH)}"
NOTARIZE_KEYCHAIN_PROFILE="${NOTARIZE_KEYCHAIN_PROFILE:-ProjectNotes-Notarize}"
TARGETS="${TARGETS:-aarch64-apple-darwin x86_64-apple-darwin}"
MIN_MACOS="13.0"
BUNDLE_ID="io.github.kestermckinney.SeeMyMac"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
OUT="${ROOT}/target/macos"
APP="${OUT}/SeeMyMac.app"

DO_SIGN=false; DO_NOTARIZE=false; DO_PKG=false; DO_BUILD=true
for arg in "$@"; do
    case "$arg" in
        --sign) DO_SIGN=true ;;
        --notarize) DO_SIGN=true; DO_NOTARIZE=true ;;
        --pkg) DO_PKG=true ;;
        --all) DO_SIGN=true; DO_NOTARIZE=true; DO_PKG=true ;;
        --skip-build) DO_BUILD=false ;;
        *) echo "Unknown option: $arg" >&2; exit 1 ;;
    esac
done

log() { printf '\n==> %s\n' "$*"; }
die() { echo "error: $*" >&2; exit 1; }

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "${ROOT}/Cargo.toml" | head -n 1)"
[[ -n "${VERSION}" ]] || die "could not read the version from Cargo.toml"
ZIP="${OUT}/SeeMyMac-${VERSION}-macOS.zip"
PKG="${OUT}/SeeMyMac-${VERSION}-macOS.pkg"

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
    if [[ -n "${NOTARY_API_KEY_PATH:-}" ]]; then
        # CI: an App Store Connect API key (no keychain profile needed).
        xcrun notarytool submit "$1" --team-id "${TEAM_ID}" --key "${NOTARY_API_KEY_PATH}" \
            --key-id "${NOTARY_API_KEY_ID:?set NOTARY_API_KEY_ID}" --issuer "${NOTARY_API_ISSUER_ID:?set NOTARY_API_ISSUER_ID}" --wait
    else
        xcrun notarytool submit "$1" --team-id "${TEAM_ID}" --keychain-profile "${NOTARIZE_KEYCHAIN_PROFILE}" --wait
    fi
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

# ── Installer package ─────────────────────────────────────────────────────────
if [[ "${DO_PKG}" == true ]]; then
    log "Creating the installer package"
    WORK="${OUT}/pkg"; rm -rf "${WORK}"
    mkdir -p "${WORK}/root" "${WORK}/packages" "${WORK}/resources"
    ditto "${APP}" "${WORK}/root/SeeMyMac.app"

    # Install exactly where the distribution says (no hunting for a copy elsewhere on disk to
    # "upgrade"), and replace an existing copy outright.
    pkgbuild --analyze --root "${WORK}/root" "${WORK}/component.plist" >/dev/null
    plutil -replace 0.BundleIsRelocatable -bool NO "${WORK}/component.plist"
    plutil -replace 0.BundleOverwriteAction -string upgrade "${WORK}/component.plist"
    # In the "current user's home" domain the install location is relative to the home folder,
    # so /Applications here becomes ~/Applications (created if it is missing).
    pkgbuild --root "${WORK}/root" --component-plist "${WORK}/component.plist" \
        --identifier "${BUNDLE_ID}" --version "${VERSION}" --install-location /Applications \
        "${WORK}/component.pkg" >/dev/null
    # pkgbuild always marks the payload as needing root, which makes Installer ask for an
    # administrator password even for a home-folder install. It needs none.
    pkgutil --expand "${WORK}/component.pkg" "${WORK}/component"
    sed -i '' 's/auth="root"/auth="none"/' "${WORK}/component/PackageInfo"
    grep -q 'auth="none"' "${WORK}/component/PackageInfo" || die "could not drop the root requirement"
    pkgutil --flatten "${WORK}/component" "${WORK}/packages/SeeMyMac.pkg"

    cp "${ROOT}/LICENSE" "${WORK}/resources/LICENSE.txt"
    cp "${SCRIPT_DIR}/background.png" "${WORK}/resources/background.png"
    sed "s/__VERSION__/${VERSION}/g" "${SCRIPT_DIR}/welcome.html" > "${WORK}/resources/welcome.html"
    cp "${SCRIPT_DIR}/conclusion.html" "${WORK}/resources/conclusion.html"

    cat > "${WORK}/distribution.xml" <<XML
<?xml version="1.0" encoding="utf-8"?>
<installer-gui-script minSpecVersion="2">
    <title>SeeMyMac ${VERSION}</title>
    <background file="background.png" mime-type="image/png" alignment="bottomleft" scaling="none"/>
    <background-darkAqua file="background.png" mime-type="image/png" alignment="bottomleft" scaling="none"/>
    <welcome file="welcome.html" mime-type="text/html"/>
    <license file="LICENSE.txt" mime-type="text/plain"/>
    <conclusion file="conclusion.html" mime-type="text/html"/>
    <options customize="never" require-scripts="false" hostArchitectures="arm64,x86_64"/>
    <domains enable_anywhere="false" enable_currentUserHome="true" enable_localSystem="false"/>
    <volume-check>
        <allowed-os-versions><os-version min="${MIN_MACOS}"/></allowed-os-versions>
    </volume-check>
    <choices-outline>
        <line choice="${BUNDLE_ID}"/>
    </choices-outline>
    <choice id="${BUNDLE_ID}" visible="false" title="SeeMyMac">
        <pkg-ref id="${BUNDLE_ID}"/>
    </choice>
    <pkg-ref id="${BUNDLE_ID}" version="${VERSION}" auth="none" onConclusion="none">SeeMyMac.pkg</pkg-ref>
</installer-gui-script>
XML

    SIGN_ARGS=()
    if [[ "${DO_SIGN}" == true ]]; then SIGN_ARGS=(--sign "${INSTALLER_IDENTITY}" --timestamp); fi
    rm -f "${PKG}"
    productbuild --distribution "${WORK}/distribution.xml" --resources "${WORK}/resources" \
        --package-path "${WORK}/packages" ${SIGN_ARGS[@]+"${SIGN_ARGS[@]}"} "${PKG}"
    rm -rf "${WORK}"
    if [[ "${DO_SIGN}" == true ]]; then pkgutil --check-signature "${PKG}"; fi
    if [[ "${DO_NOTARIZE}" == true ]]; then
        log "Notarizing the installer"
        notarize "${PKG}"
        xcrun stapler staple "${PKG}"
        xcrun stapler validate "${PKG}"
    fi
    log "Built ${PKG}"
fi

ls -lh "${OUT}"
