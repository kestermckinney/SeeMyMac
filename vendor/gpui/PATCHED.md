# gpui 0.2.2, patched for SeeMyMac

A copy of gpui 0.2.2 from crates.io (examples removed), used through
`[patch.crates-io]` in SeeMyMac's Cargo.toml. Only
`src/platform/linux/wayland/clipboard.rs` is changed in behaviour:

- Clipboard reads wait at most 300 ms for the program that owns the clipboard
  (they ran on the window thread with no limit; one source took 15 s and GNOME
  reported SeeMyMac as not responding).
- Reads are text only (SeeMyMac never reads images from the clipboard).
- A failed read is remembered until the clipboard changes, like a successful one.

`src/taffy.rs` also writes two float literals as `0.0_f32` and `1.0_f32`, so
newer compilers don't warn about the `f32` fallback (rust-lang/rust#154024).

When upgrading gpui, re-apply these changes or drop the patch if upstream fixed it.
