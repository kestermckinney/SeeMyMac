# Releasing SeeMyMac

Releases are built by hand and published as GitHub Releases. The in-app updater
reads the latest published release from
`https://api.github.com/repos/kestermckinney/SeeMyMac/releases/latest`; drafts and
pre-releases are never offered.

Each release carries these files:

| File | Built with | Built on |
| --- | --- | --- |
| `SeeMyMac-X.Y.Z-setup.exe` (ARM64 and x86) | `tools/build-installers.ps1` | Windows |
| `SeeMyMac-X.Y.Z-macOS.pkg`, `SeeMyMac-X.Y.Z-macOS.zip` (signed, notarized) | `packaging/macos/build_app.sh --all` | macOS |
| `SeeMyMac.flatpak` | `packaging/linux/build-flatpak.sh` | Linux |

The updater installs the `setup.exe` on Windows and the macOS `.zip` on macOS, so
keep those file names. On Linux it only links to the release page.

## Cutting a release

1. Update `version` in `Cargo.toml` and `<releases>` in
   `packaging/linux/io.github.kestermckinney.SeeMyMac.metainfo.xml`, and write
   `packaging/RELEASE_NOTES.md`.
2. If dependencies changed, regenerate the Flatpak's offline crate list:
   `packaging/linux/build-flatpak.sh sources`.
3. Commit, push, then tag `vX.Y.Z` on that commit and push the tag. The Flatpak
   build clones from GitHub, so the tag must be pushed before you build it.
4. Build the three installers (below).
5. Publish the release:

   ```bash
   gh release create vX.Y.Z --title "SeeMyMac X.Y.Z" --notes-file packaging/RELEASE_NOTES.md \
     target/installers/SeeMyMac-X.Y.Z-setup.exe \
     target/macos/SeeMyMac-X.Y.Z-macOS.pkg target/macos/SeeMyMac-X.Y.Z-macOS.zip \
     packaging/linux/SeeMyMac.flatpak
   ```

   Add `--draft` to check the release before it goes out; the updater ignores it
   until you publish it. To replace a file on an existing release, use
   `gh release upload vX.Y.Z <file> --clobber`.

## Windows

On a Windows PC with the tools listed under *Building* in the README (Inno Setup 6,
NASM, LLVM/Clang, and Visual Studio's C++ tools for ARM64 and x86):

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\build-installers.ps1
```

The two optimized builds take about 45 minutes each. The installer is unsigned.

## macOS

```bash
packaging/macos/build_app.sh --all
```

This signs with the Developer ID certificates in your login keychain and notarizes
with the `ProjectNotes-Notarize` profile stored for `notarytool`.

## Linux

The app is Rust, so the Flatpak builds offline from `generated-sources.json`
(every crate in `Cargo.lock`, with checksums) using the
`org.freedesktop.Sdk.Extension.rust-stable` extension on the freedesktop 25.08
runtime.

```bash
packaging/linux/build-flatpak.sh setup      # runtime, SDK, rust extension, lint tools (once)
export SMM_REF=vX.Y.Z GPG_KEY_ID=<fingerprint>
packaging/linux/build-flatpak.sh build && packaging/linux/build-flatpak.sh repo && packaging/linux/build-flatpak.sh bundle
```

`SMM_REF` defaults to the newest `v*` tag on GitHub; `SMM_REF=main` builds the
branch tip. Unpushed commits are not included. `GPG_KEY_ID` is optional: when it is
set, the repository is signed and the bundle carries the public key, so
`flatpak install` can verify it.

To try a build locally:

```bash
packaging/linux/build-flatpak.sh install
flatpak run io.github.kestermckinney.SeeMyMac
```

### Signing key

`packaging/linux/seemymac.gpg` is the public half of the Flatpak signing key. To
create a new key:

```bash
gpg --quick-generate-key "SeeMyMac Flatpak Signing" ed25519 sign never
gpg --export "SeeMyMac Flatpak Signing" > packaging/linux/seemymac.gpg
```

Keep the private key, its passphrase and a revocation certificate out of Git.

### Notes

- App ID `io.github.kestermckinney.SeeMyMac` is also the window `app_id` in
  `src/main.rs` and the macOS bundle ID, so Wayland shells match the window to the
  desktop entry and icon.
- `--filesystem=home` is deliberate: file copy saves to a user-typed folder and
  accepts drops from anywhere. The Flathub linter flags it; the app is not on Flathub.
- Audio needs `--socket=pulseaudio`; saved passwords need
  `--talk-name=org.freedesktop.secrets`.
