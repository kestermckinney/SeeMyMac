# Releasing SeeMyMac

Pushing a `vX.Y.Z` tag runs `.github/workflows/release.yml`. It builds the installers and
attaches them to a GitHub release with the file names the in-app updater expects:

| Job | Runner | Output |
| --- | --- | --- |
| `flatpak` | `ubuntu-latest` in the `flatpak-github-actions:freedesktop-25.08` container | `SeeMyMac.flatpak` (signed) |
| `windows` | `windows-2022` | `SeeMyMac-X.Y.Z-setup.exe` (ARM64 and x86, unsigned) |
| `macos` | `macos-15` | `SeeMyMac-X.Y.Z-macOS.zip` and `.pkg` (signed, notarized, stapled) |
| `release` | `ubuntu-latest` | creates the release from `packaging/RELEASE_NOTES.md` and attaches the files |
| `pages` | `ubuntu-latest` | publishes the signed Flatpak repository to GitHub Pages |

The `flatpak` job also exports a signed OSTree repository. After the release is created,
the `pages` job publishes it, with `packaging/linux/seemymac.flatpakrepo` and the icon, at
`https://kestermckinney.github.io/SeeMyMac/flatpak/`. That is the remote Linux users add
(see the README), and the bundle names it too, so `flatpak update` finds new releases
either way. Each release replaces the whole site, so the repository holds only the newest
build (x86_64); older versions stay on the GitHub releases.

### One-time GitHub Pages setup

1. **Settings → Pages → Build and deployment → Source:** *GitHub Actions*.
2. **Settings → Environments → github-pages → Deployment branches and tags:** add a tag
   rule for `v*`. By default the environment only accepts the default branch, and the
   `pages` job runs for a tag.

The updater reads the latest published release from
`https://api.github.com/repos/kestermckinney/SeeMyMac/releases/latest`; drafts and
pre-releases are never offered. It installs the `setup.exe` on Windows and the macOS
`.zip` on macOS, so keep those file names. On Linux it only links to the release page.

A manual run (**Actions → Release installers → Run workflow**) builds the Windows and macOS
installers as workflow artifacts without creating a release. The Flatpak job only runs for tags.

## Cutting a release

1. Update `version` in `Cargo.toml` and `<releases>` in
   `packaging/linux/io.github.kestermckinney.SeeMyMac.metainfo.xml`, and write
   `packaging/RELEASE_NOTES.md`.
2. Commit and push, then tag `vX.Y.Z` on that commit and push the tag. The tag must
   match `version` in `Cargo.toml`; each job checks it.
3. If a job fails, fix the cause and re-run it. To replace a file on a release that
   already exists, build it locally (below) and run
   `gh release upload vX.Y.Z <file> --clobber`.

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

Export the `Developer ID Application` and `Developer ID Installer` certificates together
(select both in Keychain Access, then File → Export Items) as one `.p12`. Do not use the
`3rd Party Mac Developer Installer` certificate; it cannot sign notarized installers.

The Flatpak key must be the one in `packaging/linux/seemymac.gpg`.
`packaging/ci/build-flatpak-release.sh` refuses to build with a different key, because
installed users trust the published one.

## Building locally

### Windows

On a Windows PC with the tools listed under *Building* in the README (Inno Setup 6, NASM,
LLVM/Clang, and Visual Studio's C++ tools for ARM64 and x86):

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\build-installers.ps1
```

The two optimized builds take about 45 minutes each. `packaging/ci/build-windows.ps1`
installs those tools on a fresh runner and then runs the same script.

### macOS

```bash
packaging/macos/build_app.sh --all
```

Locally this signs with the Developer ID certificates in your login keychain and
notarizes with the `ProjectNotes-Notarize` profile stored for `notarytool`; the workflow
uses an App Store Connect API key instead. Leave out `--all` for a quick unsigned build.

### Linux

The app is Rust, so the Flatpak builds offline from `generated-sources.json` (every crate
in `Cargo.lock`, with checksums) using the `org.freedesktop.Sdk.Extension.rust-stable`
extension on the freedesktop 25.08 runtime. The release job regenerates that file; after
changing dependencies locally, run `packaging/linux/build-flatpak.sh sources` and commit it.

```bash
packaging/linux/build-flatpak.sh setup      # runtime, SDK, rust extension, lint tools (once)
packaging/linux/build-flatpak.sh install    # builds the newest v* tag from GitHub; SMM_REF=main for the branch tip
flatpak run io.github.kestermckinney.SeeMyMac
```

Builds clone from GitHub, so unpushed commits are not included. A signed bundle:

```bash
export SMM_REF=vX.Y.Z GPG_KEY_ID=<fingerprint>
packaging/linux/build-flatpak.sh build && packaging/linux/build-flatpak.sh repo && packaging/linux/build-flatpak.sh bundle
```

### Notes

- App ID `io.github.kestermckinney.SeeMyMac` is also the window `app_id` in `src/main.rs`
  and the macOS bundle ID, so Wayland shells match the window to the desktop entry and icon.
- `--filesystem=home` is deliberate: file copy saves to a user-typed folder and accepts
  drops from anywhere. The Flathub linter flags it; the app is not on Flathub.
- Audio needs `--socket=pulseaudio`; saved passwords need
  `--talk-name=org.freedesktop.secrets`.
