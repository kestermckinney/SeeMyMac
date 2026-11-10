# Linux releases on GitLab

The GitLab pipeline builds a signed OSTree repository, publishes it at
`https://kestermckinney.gitlab.io/SeeMyMac/flatpak/`, and uploads durable
`SeeMyMac.flatpak`, `seemymac.flatpakrepo` and `seemymac.gpg` assets to the
generic package registry. Layout and workflow follow ProjectNotes.

The app is Rust, so the Flatpak builds offline from `generated-sources.json`
(every crate in `Cargo.lock`, with checksums) using the
`org.freedesktop.Sdk.Extension.rust-stable` extension on the freedesktop 25.08
runtime. The release pipeline regenerates that file; after changing
dependencies locally, run `packaging/linux/build-flatpak.sh sources` and commit it.

## One-time setup

1. Make the project's releases and packages public, and enable GitLab Pages with
   public access. Disable **Use unique domain** in Pages settings so the stable
   URL above serves without login or redirects.
2. Create the repository signing key (once; never rotate it casually, installed
   users trust it):

   ```bash
   gpg --quick-generate-key "SeeMyMac Flatpak Signing (GitLab)" ed25519 sign never
   gpg --export "SeeMyMac Flatpak Signing" | base64 --wrap=0     # -> GPGKey= in seemymac.flatpakrepo
   gpg --export "SeeMyMac Flatpak Signing" > packaging/linux/seemymac.gpg
   gpg --armor --export-secret-keys "SeeMyMac Flatpak Signing"   # -> FLATPAK_GPG_PRIVATE_KEY
   ```

   Commit `seemymac.gpg` and the `GPGKey=` line. Keep the private key, its
   passphrase and a revocation certificate out of Git.
3. Protect `v*` tags and add protected CI/CD variables:

   | Variable | Type | Contents |
   | --- | --- | --- |
   | `FLATPAK_GPG_PRIVATE_KEY` | File | ASCII-armored signing key |
   | `FLATPAK_GPG_KEY_ID` | Variable | Signing-key fingerprint |
   | `FLATPAK_GPG_PASSPHRASE` | Masked variable | Passphrase, if the key uses one |
4. Use a runner that supports Flatpak/bubblewrap builds in the
   `flatpak-github-actions:freedesktop-25.08` container. A self-managed Docker
   runner may need privileged mode.

## Cutting a release

Update `version` in `Cargo.toml` and `<releases>` in
`io.gitlab.kestermckinney.SeeMyMac.metainfo.xml`, review
`packaging/gitlab/RELEASE_NOTES.md`, commit and push, then tag `vX.Y.Z` on that
commit. The pipeline checks that the tag matches `Cargo.toml`.

`flatpak-release` builds and signs; `prepare-release` uploads assets and creates an
*upcoming* release dated 2099-01-01; the manual `publish-release` job makes it
current and deploys Pages (and refuses to publish an older version over a newer one).

## Local build

```bash
packaging/linux/build-flatpak.sh setup      # runtime, SDK, rust extension, lint tools
packaging/linux/build-flatpak.sh install    # builds the newest v* tag from GitLab; SMM_REF=main for the branch tip
flatpak run io.gitlab.kestermckinney.SeeMyMac
```

Builds clone from GitLab, so unpushed edits are not included. Signed local build:

```bash
export SMM_REF=v0.1.0 GPG_KEY_ID=<fingerprint> APP_REPO_URL=https://kestermckinney.gitlab.io/SeeMyMac/flatpak/
packaging/linux/build-flatpak.sh build && packaging/linux/build-flatpak.sh repo && packaging/linux/build-flatpak.sh bundle
```

## Installing

```bash
flatpak remote-add --if-not-exists seemymac https://kestermckinney.gitlab.io/SeeMyMac/flatpak/seemymac.flatpakrepo
flatpak install seemymac io.gitlab.kestermckinney.SeeMyMac
```

## Notes

- App ID `io.gitlab.kestermckinney.SeeMyMac` is also the window `app_id` in
  `src/main.rs`, so Wayland shells match the window to the desktop entry and icon.
- `--filesystem=home` is deliberate: file copy saves to a user-typed folder and
  accepts drops from anywhere. The Flathub linter flags it; this repo is self-hosted.
- Audio needs `--socket=pulseaudio`; saved passwords need
  `--talk-name=org.freedesktop.secrets`.
