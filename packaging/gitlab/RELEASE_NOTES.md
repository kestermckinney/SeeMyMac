# SeeMyMac 0.2.1

- **GNOME Remote Desktop.** SeeMyMac can now connect over RDP to GNOME Remote Desktop (tested with GNOME 51 on Fedora), including its hand-over to your own session after sign-in.
- **Windows RDP pictures no longer freeze.** After the remote desktop was resized, which happens right after connecting, the picture stopped updating. It now keeps redrawing at the new size. Windows servers also use the graphics pipeline now.
- **Typing works in Remote Desktop sessions.** When SeeMyMac itself runs on a Windows PC you reach through Remote Desktop, text fields and the Mac now accept what you type.
- **Linux no longer reports "not responding".** On Wayland, GNOME flagged the app as unresponsive whenever it had focus and an image was on the clipboard.
- **No console window** opens alongside the Windows app.

## Installing on macOS

1. Download `SeeMyMac-0.2.1-macOS.dmg` (or `SeeMyMac-0.2.1-macOS.zip`) and open it. The app is signed with a Developer ID and notarized by Apple, so it opens without a Gatekeeper warning.
2. Drag **SeeMyMac** into the `Applications` folder inside your own home folder (`~/Applications`; create it if it is not there). Keeping it in your user profile lets SeeMyMac update itself without asking for a password.
3. Open **SeeMyMac** from `~/Applications` (or Spotlight). The first time you open it, macOS asks you to confirm because it was downloaded from the internet; choose **Open**.
