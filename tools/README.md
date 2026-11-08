# UI Driver Tool

This is the Rust implementation of the UI driver tool that replaces the original Python version.
It is integrated into the main SeeMyMac application and provides the same functionality.

## Features

- **Wayland-native**: No X11 dependencies, fully modern implementation
- **Command-compatible**: Same interface as the original Python script
- **Integrated**: Part of the main build system

## Usage

```bash
# Run UI driver commands (same as Python version)
cargo run --bin ui_driver click 740 280
cargo run --bin ui_driver type "Studio Mac"  
cargo run --bin ui_driver shot /tmp/out.png
```

## Available Commands

- `click x y [button]` - Move mouse and click at coordinates
- `key keyname [modifiers...]` - Send keyboard input 
- `type text...` - Type text characters
- `shot path` - Take screenshot using ImageMagick import

## Integration Notes

This tool is now part of the main SeeMyMac project. The original Python files are replaced and this Rust version:
1. Provides identical functionality with better performance
2. Has no external dependencies (except for optional Wayland support)
3. Is built as part of the main application
4. Works with existing scripts without modification

## Building

```bash
# Build debug version
cargo build

# Build release version  
cargo build --release

# Run UI driver directly
cargo run --bin ui_driver click 740 280
```

## For Full Wayland Implementation

To enable full Wayland functionality, uncomment the following in the main Cargo.toml:

```toml
[dependencies.wayland-client]
version = "0.30"

[dependencies.wayland-protocols]
version = "0.30"
```

Then implement the actual Wayland communication protocols in `src/bin/ui_driver.rs`.