#!/usr/bin/env bash
# Builds, signs and bundles the Flatpak in the GitHub Actions release workflow
# (.github/workflows/release.yml). Output: packaging/linux/SeeMyMac.flatpak
set -euo pipefail
: "${FLATPAK_GPG_PRIVATE_KEY:?Set to the path of the ASCII-armored signing key}"
: "${FLATPAK_GPG_KEY_ID:?Set the signing key fingerprint}"
: "${GITHUB_WORKSPACE:?}"
: "${GITHUB_SHA:?}"
[ "${GITHUB_REF_TYPE:-}" = tag ] || { echo 'The Flatpak release build needs a vX.Y.Z tag'; exit 1; }

# Signing material lives only in an ephemeral keyring, never in artifacts.
export GNUPGHOME
GNUPGHOME="$(mktemp -d)"
chmod 700 "$GNUPGHOME"
trap 'gpgconf --kill gpg-agent || true; rm -rf "$GNUPGHOME"' EXIT
printf '%s\n' allow-loopback-pinentry allow-preset-passphrase \
  'default-cache-ttl 21600' 'max-cache-ttl 21600' > "$GNUPGHOME/gpg-agent.conf"
# Some gpg builds exit non-zero after a successful import (warnings), so report the
# status but judge success by whether the key can actually sign (checked below).
import_status=0
gpg --batch --pinentry-mode loopback --passphrase "${FLATPAK_GPG_PASSPHRASE:-}" --import "$FLATPAK_GPG_PRIVATE_KEY" || import_status=$?
echo "gpg --import exit status: $import_status"
FLATPAK_GPG_KEY_ID="$(printf '%s' "$FLATPAK_GPG_KEY_ID" | tr -d '[:space:]')"
echo "Imported secret keys:"
gpg --batch --list-secret-keys --with-colons | awk -F: '$1 == "fpr" { print "  " $10 }'
gpg --batch --list-secret-keys "$FLATPAK_GPG_KEY_ID" >/dev/null 2>&1 \
  || { echo "FLATPAK_GPG_KEY_ID ($FLATPAK_GPG_KEY_ID) does not match an imported secret key"; exit 1; }
if [ -n "${FLATPAK_GPG_PASSPHRASE:-}" ]; then
  preset=""
  for c in "$(gpgconf --list-dirs libexecdir)/gpg-preset-passphrase" /usr/libexec/gpg-preset-passphrase /usr/lib/gnupg/gpg-preset-passphrase /usr/lib/gnupg2/gpg-preset-passphrase; do
    [ -x "$c" ] && { preset="$c"; break; }
  done
  [ -n "$preset" ] || { echo "gpg-preset-passphrase not found in this image"; exit 1; }
  gpg-connect-agent reloadagent /bye >/dev/null
  grips="$(gpg --batch --with-colons --with-keygrip --list-secret-keys "$FLATPAK_GPG_KEY_ID" | awk -F: '$1 == "grp" { print $10 }')"
  [ -n "$grips" ] || { echo "No keygrips found for $FLATPAK_GPG_KEY_ID"; exit 1; }
  for grip in $grips; do
    printf '%s' "$FLATPAK_GPG_PASSPHRASE" | "$preset" --preset "$grip" \
      || { echo "gpg-preset-passphrase failed for keygrip $grip"; exit 1; }
  done
fi

# Prove the key unlocks and signs before spending an hour on the build.
printf 'test' | gpg --batch --pinentry-mode loopback --passphrase "${FLATPAK_GPG_PASSPHRASE:-}" \
  --local-user "$FLATPAK_GPG_KEY_ID" --detach-sign -o /dev/null \
  || { echo "Cannot sign with $FLATPAK_GPG_KEY_ID: check FLATPAK_GPG_PASSPHRASE and the key file"; exit 1; }
echo "Signing key check passed"

# Reject accidental key rotation: installed users trust the published public key.
gpg --batch --export "$FLATPAK_GPG_KEY_ID" | cmp -s - packaging/linux/seemymac.gpg \
  || { echo 'Signing key differs from packaging/linux/seemymac.gpg; review key migration before publishing'; exit 1; }
# The .flatpakrepo users add must carry the same public key.
[ "$(sed -n 's/^GPGKey=//p' packaging/linux/seemymac.flatpakrepo)" = "$(base64 -w0 packaging/linux/seemymac.gpg)" ] \
  || { echo 'GPGKey in packaging/linux/seemymac.flatpakrepo does not match packaging/linux/seemymac.gpg'; exit 1; }

version="$(sed -n 's/^version = "\([0-9.]*\)"/\1/p' Cargo.toml | head -n1)"
[ "$GITHUB_REF_NAME" = "v$version" ] || { echo 'Tag does not match application version'; exit 1; }
flatpak remote-add --if-not-exists flathub https://flathub.org/repo/flathub.flatpakrepo
flatpak install -y --noninteractive flathub org.freedesktop.Platform//25.08 org.freedesktop.Sdk//25.08 org.freedesktop.Sdk.Extension.rust-stable//25.08
mkdir -p /var/lib/dbus
dbus-uuidgen --ensure=/etc/machine-id
# Build from this job's own checkout, so flatpak-builder needs no credentials to
# clone the project. SMM_REF below is the exact commit.
git config --global --add safe.directory '*'
export SMM_SOURCE_URL="file://$GITHUB_WORKSPACE"
export GPG_KEY_ID="$FLATPAK_GPG_KEY_ID" GPG_HOMEDIR="$GNUPGHOME"
export SMM_REF="$GITHUB_SHA"
# The bundle points `flatpak update` at the repository published on GitHub Pages
# (the release workflow's pages job), the same one seemymac.flatpakrepo adds.
export APP_REPO_URL="$(sed -n 's/^Url=//p' packaging/linux/seemymac.flatpakrepo)"
# Export performs the build; do not compile everything twice.
# cmd_repo needs the build directory to exist before invoking flatpak-builder.
mkdir -p packaging/linux/flatpak-build
bash packaging/linux/build-flatpak.sh sources
bash packaging/linux/build-flatpak.sh repo
bash packaging/linux/build-flatpak.sh bundle
for f in summary summary.sig; do
  [ -s "packaging/linux/flatpak-repo/$f" ] || { echo "The exported repository has no $f"; exit 1; }
done
