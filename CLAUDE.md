# ninjutso-rs

## Purpose
Linux configuration tools for Ninjutso gaming mice (GTK4 app, CLI, battery tray) talking to the device
over `hidraw`, with no vendor software. The current, published Rust port of the older `ninjutso` repo.
Verified on a Ninjutso Ten on Fedora 44 / KDE Plasma / Wayland; Sora V2/V3 paths are untested.

## Build/Test
    cargo build --release                         # all three binaries
    cargo build --release --no-default-features   # CLI only, no system deps
    cargo test
GUI needs `gtk4-devel` (4.10+) and `libadwaita-devel`; the tray is pure Rust over D-Bus.

## Workflow
- Branch `main`. This repo is **PUBLIC** — commit locally, and push only with the user's explicit OK.
- Hardware behaviour is verified by the user on the real mouse; say what to check and wait.

## Gotchas
- Two Ninjutso repos exist; see "ninjutso vs ninjutso-rs" in the ClaudeInConcert hub `CLAUDE.md`.
  In short: the old `ninjutso/rust/` has ~2,000 lines of GUI/CLI integration tests this repo lacks —
  port that harness for lifetime/end-to-end bugs; check the old port before assuming a feature is missing.
- Don't run both projects' trays at once — they contend for the same hidraw node.
- Firmware flashing is deliberately unimplemented (see README).
- The GUI is a subset of the CLI (active DPI stage only; no lighting speed or online firmware check).
