# SeeMyMac 0.5.0

This release moves SeeMyMac's builds and downloads to GitHub.

- Installers for Windows, macOS and Linux (Flatpak) are now built on GitHub and attached to the GitHub release.
- The macOS app and installer are signed and notarized by Apple.
- SeeMyMac checks GitHub for updates. On Linux it now tells you when a newer release is out and links to it.
- The app ID is now `io.github.kestermckinney.SeeMyMac`. On Linux, Flatpak installs this as a new app next to the old one; see *Installing on Linux* below.

## Installing on macOS

1. Download `SeeMyMac-0.5.0-macOS.pkg` and open it. It is signed with a Developer ID and notarized by Apple, so it opens without a Gatekeeper warning.
2. Follow the installer. It puts **SeeMyMac** in the `Applications` folder inside your home folder (`~/Applications`) without asking for a password, which also lets SeeMyMac update itself.
3. Open **SeeMyMac** from `~/Applications` (or Spotlight).

## Installing on Windows

Download and run `SeeMyMac-0.5.0-setup.exe`. One installer covers both ARM64 and Intel/AMD PCs and installs for your account only, without administrator rights.

## Installing on Linux

Download `SeeMyMac.flatpak` from the release and install it with `flatpak install --user SeeMyMac.flatpak`, then open SeeMyMac from your applications menu or run `flatpak run io.github.kestermckinney.SeeMyMac`.

If you had an earlier release installed (app ID `io.gitlab.kestermckinney.SeeMyMac`), copy your saved connections and settings across, then remove the old app:

```bash
mkdir -p ~/.var/app/io.github.kestermckinney.SeeMyMac/config
cp -r ~/.var/app/io.gitlab.kestermckinney.SeeMyMac/config/SeeMyMac ~/.var/app/io.github.kestermckinney.SeeMyMac/config/
flatpak uninstall io.gitlab.kestermckinney.SeeMyMac
```

All downloads are on the [GitHub releases page](https://github.com/kestermckinney/SeeMyMac/releases/latest).
