# SeeMyMac

SeeMyMac is a cross-platform screen sharing application built with Rust and GPUI.

## Install SeeMyMac

### Windows

Download `SeeMyMac-<version>-setup.exe`. The one installer works on every Windows PC: it installs the native ARM64 build on ARM-based PCs and the 32-bit x86 build on Intel and AMD PCs.

Run the installer, follow the prompts, and open **SeeMyMac** from the Start menu. The installers are unsigned. A Windows installer is not currently published by the release pipeline; to create one locally, follow the [Windows build instructions](#building) below. Debug installers have `-debug-setup.exe` in their names.

### macOS

Open `SeeMyMac-<version>-macOS.pkg` and follow the installer: it shows the licence for you to accept, then installs **SeeMyMac** into the Applications folder inside your own home folder (`~/Applications`) without asking for an administrator password. (The `.zip` holds the same app if you would rather drag it there yourself.) The app and installer are signed and notarized, and because the app lives in your user profile it updates itself without asking for a password. To build, sign and notarize it yourself, see `packaging/macos/build_app.sh`.

### Linux

Install [Flatpak](https://flatpak.org/setup/) for your distribution, then add the SeeMyMac repository and install the app:

```bash
flatpak remote-add --if-not-exists seemymac https://kestermckinney.gitlab.io/SeeMyMac/flatpak/seemymac.flatpakrepo
flatpak install seemymac io.gitlab.kestermckinney.SeeMyMac
```

Open SeeMyMac from your applications menu, or run:

```bash
flatpak run io.gitlab.kestermckinney.SeeMyMac
```

Run `flatpak update` to install updates from your configured Flatpak remote. SeeMyMac also checks for newer GitHub releases and shows a link to the GitHub project when one is available. You can check on demand under **About… → Check for updates**.

### Updates

On Windows and macOS SeeMyMac checks GitHub and GitLab for a newer release at startup and every six hours while it’s running (switch this off in Settings → General → Updates) and asks whether to download and install it. **About… → Check for updates** looks on demand. SeeMyMac offers the newest release with a compatible installer and prefers GitHub when both hosts offer the same version. Until the GitHub mirror has a published release, GitLab supplies updates. The installer replaces the copy in your user profile and SeeMyMac restarts by itself.

After mirroring the repository, create a GitHub Release for the version tag. Attach the Windows setup executable and the macOS zip to each release before publishing. For GitLab releases, use `packaging/gitlab/upload-installer.py`. For GitHub releases, upload the installers directly to the release. For access to a Mac, enable Screen Sharing on that Mac and make sure it is reachable over the network.

## UI Driver Tool

This project includes a Rust replacement for the original Python UI driver tool that was used for automated testing and control.

### Features

- **Wayland-native**: No X11 dependencies, fully modern implementation
- **Command-compatible**: Same interface as the original Python version
- **Integrated**: Part of the main build system

### Usage

```bash
# Build the application (including ui_driver tool)
cargo build

# Run UI driver commands (same as Python version)
cargo run --bin ui_driver click 740 280
cargo run --bin ui_driver type "Studio Mac"  
cargo run --bin ui_driver shot /tmp/out.png
```

### Implementation Notes

This Rust version completely replaces the Python tool with:
1. **No Python dependencies** - Pure Rust implementation (Wayland-only)
2. **Same API** - Zero breaking changes to existing scripts
3. **Integrated build system** - Part of main cargo project

The current implementation provides a demonstration structure. For full Wayland functionality, you would need to:

```toml
# Add to Cargo.toml dependencies
wayland-client = "0.30"
wayland-protocols = "0.30"
```

And implement the actual Wayland protocol communication in the tool's logic.

## RDP (Remote Desktop Protocol)

Connections set to **RDP** use a native client built on the [IronRDP](https://github.com/Devolutions/IronRDP)
crates (`src/rdp.rs`; pure Rust, no FreeRDP). It connects with TLS and NLA (a user name and password),
and supports:

- the desktop picture (IronRDP's bitmap codecs), keyboard and mouse including the wheel;
- clipboard text in both directions, and files: drop files or folders onto the window to copy them to
  the server, and files copied on the server are saved to the download folder;
- audio playback (PCM, through the same PulseAudio/PipeWire or WASAPI output as Apple HP);
- the remote desktop following the window size (Display Control), and "Span all local monitors",
  which asks the server for the same monitor layout as this computer;
- on Linux and macOS, the connection's *Local drives* setting, which shares the Downloads folder (the home folder if
  there is none), and *Printers*, which offers the default CUPS printer (it runs `lp`, so it does
  not work inside the Flatpak sandbox).

Checked against a real Windows 11 machine (2026-11-10): NLA sign-in, the picture, input, clipboard offer (accepted), live resize, drive and printer redirection (Windows replied `STATUS_SUCCESS` to both), the smart-card device (accepted; Windows calls the bridge, and `pcscd` answered `SCARD_W_SECURITY_VIOLATION` to a process that is not an active desktop login, so no card was read), multi-monitor (two requested monitors gave a 2560x800 desktop with a second wallpaper, and the monitor list is filled in from the request because Windows does not report it back), and the RemoteApp handshake (the server accepts RAIL mode and the launch is sent; Windows 11 client editions are not a real RemoteApp host, so the program itself was not seen to start). Windows only starts device redirection when the audio channel is declared too, so the client declares a silent one when audio is off but drives, printers or smart cards are on. Browsing the redirected drive works (a screenshot of the app's session shows `\\tsclient\D` in Explorer listing the local Downloads folder's files). The redirected printer appears in Windows once it is announced with a driver the server has (`Microsoft PS Class Driver` by default; `MS Publisher Imagesetter`, the library default, did not produce a printer on the Windows 11 test machine). A Windows test page printed through it arrived as a PostScript file in the download folder (no CUPS default printer was set up, so the client saved the job there) and rendered correctly. Printing through a real CUPS printer was not exercised.

Implemented but only unit-tested, not yet run against real servers or hardware:

- Kerberos (when the user name has a domain; direct KDC over TCP/UDP, no HTTPS KDC proxy) and following a
  load balancer's server redirection.
- Smart cards (Linux, `src/rdp_scard.rs`): the *Smart cards* setting offers the local PC/SC readers by loading
  `libpcsclite` at run time; waits for card insertion run off the session thread.
- RD Gateway (`src/rdp_gateway.rs`): the *RD Gateway* field on an RDP connection tunnels it through the
  gateway, signing in with the same user and password. A local forwarder bridges IronRDP's async gateway
  client to the blocking RDP client.

- RemoteApp (`src/rdp_rail.rs`): the *RemoteApp program* field runs just that program (a path, or `||alias` for a
  published RemoteApp). The RAIL conversation (handshake, client status, launch) is implemented; the program's
  windows appear in the session picture rather than as separate windows. Only unit-tested.

Not supported: separate native windows per RemoteApp window, and smart cards on macOS/Windows.

The server's TLS certificate is accepted without a prompt, as most RDP
clients do for the self-signed certificates Windows presents by default; NLA still binds the login
to that certificate's key.

### Testing RDP

`tools/rdp-test-server` is a small RDP server (its own Cargo package: the server crate pins older
IronRDP pre-releases than the client uses, so they cannot share one dependency graph) and
`src/bin/rdp_probe.rs` drives the client against it:

```bash
cargo build --manifest-path tools/rdp-test-server/Cargo.toml
tools/rdp-test-server/target/debug/rdp-test-server 3390 tester secret &
SEEMYMAC_USER=tester SEEMYMAC_PASS=secret cargo run --bin rdp_probe -- 127.0.0.1:3390 8
```

Against a real host, leave the password out and the probe asks for it without echoing it
(`SEEMYMAC_USER=me cargo run --bin rdp_probe -- host`), or use a connection saved in the app
(`cargo run --bin rdp_probe -- @CONNECTION_ID`). `SEEMYMAC_PROBE_SHARE=DIR`, `SEEMYMAC_PROBE_PRINTER=1`,
`SEEMYMAC_PROBE_SMARTCARD=1` and `SEEMYMAC_PROBE_MONITORS=WxH,WxH` switch on the other features.

Against a real host, `SEEMYMAC_PROBE_NO_INPUT=1` only watches (use it on a live session: the default probe clicks, types and resizes) and `SEEMYMAC_PROBE_SNAPSHOT=file.png` saves a picture of the remote desktop.

The probe checks the connection and sign-in, the first picture, input (the server prints each event),
clipboard both ways, file copy both ways (with a checksum), audio, and a desktop resize. Drive and printer redirection and multi-monitor
layouts are not covered by this server; test those against a Windows host.

## Apple High Performance Protocol

SeeMyMac speaks both standard VNC and Apple's High Performance (HP) Screen Sharing.
HP is implemented natively in Rust (`src/hp/`); no Python or external bridge is
needed:

- TCP handshake with SRP (or the older RSA1) authentication, the enc1103 encrypted
  control channel, and the AVConference media offer.
- SRTP/SRTCP video over UDP with NACK retransmission, keyframe requests and
  automatic adoption of new encoder streams.
- H.264 decoding with `rust_h264` (pure Rust), vendored in `vendor/rust_h264` with
  one addition: Apple's encoder ends P slices after the last coded macroblock,
  and the uncovered macroblocks are decoded as skipped, as Apple's decoder does.
  Without this, strict decoders reject those frames and FFmpeg leaves a green
  strip along the bottom.
- HEVC decoding with `rust_h265` (pure Rust, multithreaded): Apple's 4:4:4 Range
  Extensions stream, depacketized per RFC 7798. The Mac is offered both codecs and
  usually picks HEVC; screens larger than H.264 allows (a 5K iMac or Studio
  Display) can only be shared as HEVC. A 5K 4:4:4 picture costs about 200 ms of
  CPU, so a native 5K session needs a fast multi-core client (about 30 fps on 8
  cores) and still shows each picture ~250 ms late, longer for the first seconds.
  Such pictures are halved for display (the Mac's point resolution), and the
  session offers to move to a virtual display after login, which decodes in a
  few milliseconds.

Multiple displays: over HP the Mac streams only its main display; its other
physical displays cannot be shown, whatever the viewer asks for once a user is
logged in. Two ways to see two displays:

- **Two virtual displays** (connection dialog → Display → Virtual displays: 2).
  After "Use virtual display" the Mac creates two virtual displays side by side
  and streams each separately (the second over UDP port 5902); they appear as
  monitors 1 and 2, and **Both** (Ctrl+Alt+Home) shows them side by side, all live.
- **Plain VNC** for the Mac's physical displays: it sends the whole desktop. macOS
  does not say where each monitor is over VNC, so SeeMyMac uses the layout it
  learnt during an earlier HP session to the same Mac (when the sizes match).

The monitor selector (toolbar, or Ctrl+Alt+PgUp / PgDn / Home) appears when there
are two monitors.

Requirements: a Mac with Apple silicon on macOS 14 or later with Screen Sharing on,
an IPv4 address, and UDP ports 5900 and 5901 (the connection port and the next one;
also 5902 for two virtual displays)
reachable. Remote cursor shapes are not implemented over HP; the local pointer is
shown instead. Audio from the Mac (AAC-ELD) is decoded by a
pure-Rust decoder built into SeeMyMac, so no system codec is needed (Fedora's
`fdk-aac-free` cannot decode AAC-ELD). It plays through PulseAudio or PipeWire via
libpulse-simple on Linux, falling back to `paplay`, `pw-play` or `aplay`; a Flatpak needs
`--socket=pulseaudio`. Windows uses native shared-mode WASAPI on the default
multimedia output, with automatic format conversion and a 60 ms target buffer.
No external audio player or codec is required on Windows. If the output device
is unavailable or disconnects, the screen-sharing session continues silently.
`SEEMYMAC_NO_AUDIO=1` turns audio off.

Keyboard: on Wayland (GNOME, KDE and others) SeeMyMac asks the desktop to pass
its own shortcuts, such as Super, Super+Space and Alt+Tab, to the Mac while a
connected session has focus (Settings → Input → System shortcuts). GNOME asks once
whether to allow it; Super+Escape gives the shortcuts back to this PC, and clicking
the session sends them to the Mac again. X11 sessions keep the desktop's shortcuts.

Clipboard: text copied on either side is pasted on the other, over HP and plain
VNC, while the connection's **Local resources → Clipboard** option is on. Macs
get Apple's own pasteboard messages (any character; modern apps see the text);
other VNC servers get standard cut text, which is Latin-1 only. This PC's
clipboard is checked while the SeeMyMac window has focus.

**Settings → Apple HP → Check decoder** verifies the bundled decoders; `SEEMYMAC_DEMO=1` starts a synthetic HP source for
testing without a Mac.

Debug builds compile every dependency optimised (see `Cargo.toml`); without that,
the HEVC decoder alone falls seconds behind live video.

For diagnosis, `cargo run --release --bin hp_probe -- @CONNECTION_ID 30` connects
with a saved connection (password from the keyring) and prints each stage and
frame; `SEEMYMAC_HP_RTPDUMP=file` / `SEEMYMAC_HP_DUMP=file` record the decrypted
RTP packets / H.264 or HEVC stream, and `hp_probe --decode file` replays such a dump.

The protocol engine is a port of [iShareScreen](https://github.com/renegadelink/iShareScreen)
(AGPL-3.0-or-later), as pinned in SeeMac.

### Building

On Windows, install the stable Rust MSVC toolchain with Rustup and Visual Studio
2022's **Desktop development with C++** workload (including the Windows SDK).
Open a new PowerShell window after installation, then run:

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\build-windows.ps1 -Test
.\target\debug\see_my_mac.exe
```

The script finds the Windows SDK's `fxc.exe` shader compiler for GPUI. Use
`-Release` for an optimized build, or omit `-Test` to build without tests.

To build Windows installers, install Inno Setup 6, NASM (`winget install NASM.NASM`, needed for the x86 build) and LLVM/Clang in addition to
the Rust MSVC toolchain and Visual Studio C++ tools (including the ARM64 and x86
target tools and Windows SDK). Then run:

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\build-installers.ps1
```

This builds SeeMyMac for ARM64 and 32-bit x86 and packs both into one installer,
`target\installers\SeeMyMac-<version>-setup.exe`, which installs the build that
matches the PC's processor. Add `-Debug` for a debug installer
(`-debug-setup.exe`). The installers are unsigned; signing is
needed before distribution if trusted publisher verification is desired.

Windows uses Credential Manager for saved passwords and Explorer for fetched
files. Received files inherit Windows permissions; files sent to a Mac use
Unix permissions of 0644 (files) or 0755 (folders). Remote audio uses Windows
WASAPI. The `ui_driver` tool is a Wayland demonstration, not a Windows UI
automation tool. Automated UI tests use GPUI's headless test platform; the
ignored live-Mac test requires a configured Mac and saved credentials.

To exercise the real Windows audio device with silence (including buffer
backpressure, shutdown and reopening), run this after setting `GPUI_FXC_PATH`
to your SDK's `fxc.exe`:

```powershell
cargo test --locked --lib wasapi_device_plays_silence_and_releases_cleanly -- --ignored
```

```bash
# Build debug version
cargo build

# Build release version  
cargo build --release

# Run UI driver directly
cargo run --bin ui_driver click 740 280
```

## Future Plans

The VNC client currently targets macOS Screen Sharing. Planned work to make it a general VNC client:

- **More login methods:** VeNCrypt/TLS (security type 19), Tight (16) and MS-Logon, so TightVNC, UltraVNC, TigerVNC and RealVNC servers can be used. Today only None, VNC password and Apple Diffie-Hellman are supported.
- **More encodings:** Tight, Hextile and RRE, plus the Cursor pseudo-encoding. Today only ZRLE, Raw, CopyRect and DesktopSize/ExtendedDesktopSize are handled.
- **Extension messages:** tolerate or implement unknown server messages (such as fence) instead of dropping the connection.
- **Form options for VNC:** make color depth, resolution and local resources take effect, or hide them where they do not apply.
- **Tunnelling:** an SSH tunnel or gateway host option for reaching VNC servers behind a jump host.
# License

SeeMyMac's original code is licensed under the MIT License (see [LICENSE](LICENSE)). Third-party components are subject to their own licenses, including the Fraunhofer FDK AAC license for the audio decoder.

