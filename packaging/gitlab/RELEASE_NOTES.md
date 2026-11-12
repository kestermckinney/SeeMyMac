# SeeMyMac 0.2.0

- **Self-update on Windows and macOS.** SeeMyMac checks for a new version at start-up (at most once a day; switch it off in Settings → General → Updates) and can install it and restart. **About… → Check for updates** checks on demand. On Linux, update the Flatpak through your software manager.
- **One Windows installer.** `SeeMyMac-0.2.0-setup.exe` installs the native ARM64 build on ARM PCs and the x86 build on Intel and AMD PCs.
- **RDP client.** Connect to Windows PCs over Remote Desktop, as well as to Macs.
- **Span all monitors.** Full screen can use one window per local display.
- **About screen**, and scrolling around a zoomed-in picture, including by pushing the pointer against a window edge.
- Menu icons and an Exit command.

## Installing on macOS

1. Download `SeeMyMac-0.2.0-macOS.dmg` (or `SeeMyMac-0.2.0-macOS.zip`) and open it. The app is signed with a Developer ID and notarized by Apple, so it opens without a Gatekeeper warning.
2. Drag **SeeMyMac** into the `Applications` folder inside your own home folder (`~/Applications`; create it if it is not there). Keeping it in your user profile lets SeeMyMac update itself without asking for a password.
3. Open **SeeMyMac** from `~/Applications` (or Spotlight). The first time you open it, macOS asks you to confirm because it was downloaded from the internet; choose **Open**.
