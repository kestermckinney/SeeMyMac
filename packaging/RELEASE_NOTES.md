# SeeMyMac 0.5.1

This release adds a SeeMyMac Flatpak remote, so Linux installs update with `flatpak update`.

- The signed Flatpak repository is published on GitHub Pages with each release. Add it once and your software centre or `flatpak update` keeps SeeMyMac up to date.
- The `SeeMyMac.flatpak` bundle now points at the same remote, so a bundle install updates the same way.

## Installing on macOS

1. Download `SeeMyMac-0.5.1-macOS.pkg` and open it. It is signed with a Developer ID and notarized by Apple, so it opens without a Gatekeeper warning.
2. Follow the installer. It puts **SeeMyMac** in the `Applications` folder inside your home folder (`~/Applications`) without asking for a password, which also lets SeeMyMac update itself.
3. Open **SeeMyMac** from `~/Applications` (or Spotlight).

## Installing on Windows

Download and run `SeeMyMac-0.5.1-setup.exe`. One installer covers both ARM64 and Intel/AMD PCs and installs for your account only, without administrator rights.

## Installing on Linux

Add the SeeMyMac remote (and Flathub, which supplies the runtime) and install the app:

```bash
flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak remote-add --user --if-not-exists seemymac https://kestermckinney.github.io/SeeMyMac/flatpak/seemymac.flatpakrepo
flatpak install --user seemymac io.github.kestermckinney.SeeMyMac
```

Then open SeeMyMac from your applications menu or run `flatpak run io.github.kestermckinney.SeeMyMac`. Builds are x86_64 only. You can still download `SeeMyMac.flatpak` from the release and install it with `flatpak install --user SeeMyMac.flatpak`.

If you installed the 0.5.0 bundle, add the remote as above and switch to it with `flatpak install --user --reinstall seemymac io.github.kestermckinney.SeeMyMac`. Your settings are kept.

If you had a release before 0.5.0 installed (app ID `io.gitlab.kestermckinney.SeeMyMac`), copy your saved connections and settings across, then remove the old app:

```bash
mkdir -p ~/.var/app/io.github.kestermckinney.SeeMyMac/config
cp -r ~/.var/app/io.gitlab.kestermckinney.SeeMyMac/config/SeeMyMac ~/.var/app/io.github.kestermckinney.SeeMyMac/config/
flatpak uninstall io.gitlab.kestermckinney.SeeMyMac
```

All downloads are on the [GitHub releases page](https://github.com/kestermckinney/SeeMyMac/releases/latest).
