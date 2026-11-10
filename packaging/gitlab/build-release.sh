#!/usr/bin/env bash
set -euo pipefail
: "${FLATPAK_GPG_PRIVATE_KEY:?Set the protected GitLab file variable containing the signing key}"
: "${FLATPAK_GPG_KEY_ID:?Set the signing key fingerprint}"
: "${CI_COMMIT_SHA:?}"
: "${CI_COMMIT_TAG:?}"
: "${FLATPAK_REPO_URL:?}"

# Signing material lives only in an ephemeral keyring, never in artifacts.
export GNUPGHOME
GNUPGHOME="$(mktemp -d)"
chmod 700 "$GNUPGHOME"
trap 'gpgconf --kill gpg-agent || true; rm -rf "$GNUPGHOME"' EXIT
printf '%s\n' allow-loopback-pinentry allow-preset-passphrase \
  'default-cache-ttl 21600' 'max-cache-ttl 21600' > "$GNUPGHOME/gpg-agent.conf"
gpg --batch --pinentry-mode loopback --passphrase "${FLATPAK_GPG_PASSPHRASE:-}" --import "$FLATPAK_GPG_PRIVATE_KEY" \
  || { echo "gpg could not import FLATPAK_GPG_PRIVATE_KEY (wrong passphrase or damaged key file?)"; exit 1; }
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

# Reject accidental key rotation: installed users trust the published public key.
python3 - <<'PY'
import base64, configparser, os, subprocess
repo = configparser.ConfigParser(interpolation=None)
repo.read('packaging/linux/seemymac.flatpakrepo')
expected = base64.b64decode(repo['Flatpak Repo']['GPGKey'])
actual = subprocess.check_output(['gpg', '--export', os.environ['FLATPAK_GPG_KEY_ID']])
if expected != actual:
    raise SystemExit('Signing key differs from seemymac.flatpakrepo; review key migration before publishing')
PY

version="$(sed -n 's/^version = "\([0-9.]*\)"/\1/p' Cargo.toml | head -n1)"
[ "$CI_COMMIT_TAG" = "v$version" ] || { echo 'Tag does not match application version'; exit 1; }
flatpak remote-add --if-not-exists flathub https://flathub.org/repo/flathub.flatpakrepo
flatpak install -y --noninteractive flathub org.freedesktop.Platform//25.08 org.freedesktop.Sdk//25.08 org.freedesktop.Sdk.Extension.rust-stable//25.08
mkdir -p /var/lib/dbus
dbus-uuidgen --ensure=/etc/machine-id
export GPG_KEY_ID="$FLATPAK_GPG_KEY_ID" GPG_HOMEDIR="$GNUPGHOME"
export APP_REPO_URL="$FLATPAK_REPO_URL" SMM_REF="$CI_COMMIT_SHA"
# Export performs the build; do not compile everything twice.
# cmd_repo needs the build directory to exist before invoking flatpak-builder.
mkdir -p packaging/linux/flatpak-build
bash packaging/linux/build-flatpak.sh sources
bash packaging/linux/build-flatpak.sh repo
flatpak build-update-repo --gpg-sign="$GPG_KEY_ID" --gpg-homedir="$GPG_HOMEDIR" \
  --generate-static-deltas packaging/linux/flatpak-repo
bash packaging/linux/build-flatpak.sh bundle
mkdir -p public/flatpak
cp -a packaging/linux/flatpak-repo/. public/flatpak/
cp packaging/linux/seemymac.flatpakrepo packaging/linux/seemymac.gpg public/flatpak/
cp assets/icons/seemac.png public/flatpak/icon.png
# Flatpak clients must not follow Pages' unique-domain redirects for every object.
printf '%s\n' 'SeeMyMac Linux update repository' > public/index.html
