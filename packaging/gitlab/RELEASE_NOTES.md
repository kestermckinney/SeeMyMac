# SeeMyMac 0.3.0

- **Find computers to connect to.** A search button beside the host name in the connection dialog lists Macs and PCs on your local network (Bonjour) and on your tailnet (Tailscale) that offer Screen Sharing, VNC or Remote Desktop. Choosing one fills in the host name, protocol and port for you.
- **Slide the pull tab out of the way.** When the toolbar is unpinned (for example in full screen), press and drag the connection tab at the top of the screen left or right to reach what is underneath it. A click still pins the toolbar.
- **Space types once on Windows.** In Remote Desktop sessions, pressing Space no longer types two spaces.
- **The Windows app has its icon.** Explorer, the taskbar and the Start menu now show SeeMyMac's icon instead of the Windows default, and no console window flashes up when SeeMyMac asks Tailscale for a computer's name.
- **Linux builds never check for updates.** The Flatpak is kept up to date by your software manager, so SeeMyMac no longer contacts GitLab to check for updates there.

## Installing on macOS

1. Download `SeeMyMac-0.3.0-macOS.pkg` and open it. It is signed with a Developer ID and notarized by Apple, so it opens without a Gatekeeper warning.
2. Follow the installer. It puts **SeeMyMac** in the `Applications` folder inside your home folder (`~/Applications`) without asking for a password, which also lets SeeMyMac update itself.
3. Open **SeeMyMac** from `~/Applications` (or Spotlight).

## Installing on Windows

Download and run `SeeMyMac-0.3.0-setup.exe`. One installer covers both ARM64 and Intel/AMD PCs and installs for your account only, without administrator rights.
