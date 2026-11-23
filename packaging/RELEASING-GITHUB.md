# Releasing installers on GitHub

Pushing a `vX.Y.Z` tag runs `.github/workflows/release.yml`. It builds three installers and
attaches them to a GitHub release with the same file names the in-app updater expects:

| Job | Runner | Output |
| --- | --- | --- |
| `flatpak` | `ubuntu-latest` in the `flatpak-github-actions:freedesktop-25.08` container | `SeeMyMac.flatpak`, `seemymac.flatpakrepo`, `seemymac.gpg` |
| `windows` | `windows-2022` | `SeeMyMac-X.Y.Z-setup.exe` (ARM64 and x86, unsigned) |
| `macos` | `macos-15` | `SeeMyMac-X.Y.Z-macOS.zip` and `.pkg` (signed, notarized stapled) |
| `release` | `ubuntu-latest` | creates the release from `packaging/gitlab/RELEASE_NOTES.md` and attaches all six files |

The tag must match `version` in `Cargo.toml`. A manual run (**Actions → Release installers →
Run workflow**) builds the Windows and macOS installers as workflow artifacts without creating a
release. The Flatpak job only runs for tags.

## Repository secrets

Set these under **Settings → Secrets and variables → Actions**.

| Secret | Contents |
| --- | --- |
| `MACOS_CERTIFICATE` | Base64 of a `.p12` that holds **Developer ID Application** and **Developer ID Installer** (certificate and private key). Export from Keychain Access, then `base64 -i certs.p12 \| pbcopy`. |
| `MACOS_CERTIFICATE_PASSWORD` | Password used when exporting that `.p12` |
| `KEYCHAIN_PASSWORD` | Any new random password for the CI keychain (`openssl rand -base64 32`) |
| `NOTARY_API_KEY` | Base64 of the App Store Connect API key (`AuthKey_XXXX.p8`): `base64 -i AuthKey_XXXX.p8 \| pbcopy` |
| `NOTARY_API_KEY_ID` | The key's ID (the `XXXX` in the file name) |
| `NOTARY_API_ISSUER_ID` | The issuer ID shown on the App Store Connect Keys page |
| `FLATPAK_GPG_PRIVATE_KEY` | ASCII-armored private key (`gpg --armor --export-secret-keys "SeeMyMac Flatpak Signing"`) |
| `FLATPAK_GPG_KEY_ID` | Fingerprint of that key |
| `FLATPAK_GPG_PASSPHRASE` | Passphrase, if the key has one (optional) |

The Flatpak key must be the one already in `packaging/linux/seemymac.gpg`. The build refuses to
publish a different key, because installed users trust the published one.

## Before the first release

- Export the `Developer ID Application` and `Developer ID Installer` certificates together (select
  both in Keychain Access, then File → Export Items) as one `.p12`. Do not use the
  `3rd Party Mac Developer Installer` certificate; it cannot sign notarized installers.
- Build locally without notarizing if you want a quick check: `packaging/macos/build_app.sh`.
- Run a manual workflow run first to confirm the Windows and macOS builds succeed.

## Notes

- Flatpak update remotes still point at the GitLab Pages URL in `packaging/linux/RELEASING.md`
  (`FLATPAK_REPO_URL` in the workflow). The workflow uploads the bundle and repository files as
  release assets; it does not publish a Flatpak repository.
- The in-app updater (`src/update.rs`) now reads the GitHub latest-release API. Copies built before
  this change (0.3.1 and earlier) still check GitLab, so they will not be offered 0.5.0 unless the
  GitLab release is also kept up to date.
