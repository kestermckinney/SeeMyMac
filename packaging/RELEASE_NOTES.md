# SeeMyMac 0.5.0

This release moves SeeMyMac's builds and downloads to GitHub. Nothing else in the app has changed.

- Installers for Windows, macOS and Linux (Flatpak) are now built on GitHub and attached to the GitHub release.
- The macOS app and installer are signed and notarized by Apple.

## Installing on macOS

1. Download `SeeMyMac-0.5.0-macOS.pkg` and open it. It is signed with a Developer ID and notarized by Apple, so it opens without a Gatekeeper warning.
2. Follow the installer. It puts **SeeMyMac** in the `Applications` folder inside your home folder (`~/Applications`) without asking for a password, which also lets SeeMyMac update itself.
3. Open **SeeMyMac** from `~/Applications` (or Spotlight).

## Installing on Windows

Download and run `SeeMyMac-0.5.0-setup.exe`. One installer covers both ARM64 and Intel/AMD PCs and installs for your account only, without administrator rights.

## Installing on Linux

Download `SeeMyMac.flatpak` from the release and install it with `flatpak install SeeMyMac.flatpak`.

All downloads are on the [GitHub releases page](https://github.com/kestermckinney/SeeMyMac/releases/latest).
