# SeeMyMac

SeeMyMac is a cross-platform screen sharing application built with Rust and GPUI.

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
reachable. Audio, clipboard sync and remote cursor shapes are not implemented over
HP; the local pointer is shown instead. **Settings → Apple HP → Check decoder**
verifies the bundled decoders; `SEEMYMAC_DEMO=1` starts a synthetic HP source for
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

```bash
# Build debug version
cargo build

# Build release version  
cargo build --release

# Run UI driver directly
cargo run --bin ui_driver click 740 280
```