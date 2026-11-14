# Windows and macOS installers on GitLab

Pushing a `vX.Y.Z` tag builds every installer and attaches it to the release:

| Job | Runner | Output |
| --- | --- | --- |
| `flatpak-release` | GitLab-hosted Linux | `SeeMyMac.flatpak`, `seemymac.flatpakrepo`, `seemymac.gpg` |
| `windows-installer` | GitLab-hosted Windows (`saas-windows-medium-amd64`) | `SeeMyMac-X.Y.Z-setup.exe` (ARM64 and x86) |
| `macos-installer` | The maintainer's Mac (tag `macos`) | `SeeMyMac-X.Y.Z-macOS.zip`, `SeeMyMac-X.Y.Z-macOS.pkg` (signed, notarized) |

`prepare-release` waits for all three, uploads the files to the generic package
registry and links them from an *upcoming* release. The manual `publish-release`
job then makes the release current, which is when the in-app updater starts
offering it. See `packaging/linux/RELEASING.md` for the Linux setup and tagging steps.

The Mac must be on and its runner running when you tag. If it is not, the
`macos-installer` job waits; start the runner and the pipeline carries on.

## One-time setup: the Mac runner

The macOS job runs on your own Mac, so it signs with the Developer ID
certificates already in your login keychain and notarizes with the
`ProjectNotes-Notarize` profile already stored for `notarytool`. No Apple
credentials go into GitLab.

1. Check that a local release build works first:
   `packaging/macos/build_app.sh --all`.
2. In GitLab, open **Settings → CI/CD → Runners → New project runner**.
   - Tags: `macos`. Leave **Run untagged jobs** off.
   - Turn on **Protected**, so only protected refs (the `v*` tags) can use it.
     This matters: any job that runs on this runner can sign with your Developer ID.
   - Create it and copy the `glrt-…` token.
3. On the Mac:

   ```bash
   brew install gitlab-runner
   gitlab-runner register --url https://gitlab.com --token glrt-… --executor shell
   brew services start gitlab-runner
   ```

   `brew services` runs the runner as you, in your login session, so it can use
   your keychain, Rust (`~/.cargo/bin`) and Xcode.
4. If the Mac might be logged out when you release, add a protected, masked CI/CD
   variable `MACOS_KEYCHAIN_PASSWORD` with your login password so the job can
   unlock the keychain. Otherwise leave it unset.

## The Windows runner

Nothing to set up: `windows-installer` uses GitLab's hosted Windows runners.
`packaging/gitlab/build-windows.ps1` installs Inno Setup, NASM, LLVM, the ARM64
C++ tools and Rust on each run, then calls `tools/build-installers.ps1`. That
takes a while (expect 30–60 minutes). The installer is unsigned, as it is for local builds.

## Fallback

If a build job fails, fix the cause and press **Retry** on that job; `prepare-release`
runs once all three have succeeded. To replace a file on a release that already
exists, build it by hand and attach it with `packaging/gitlab/upload-installer.py`,
which replaces a link with the same name.
