# ninjutso-rs

Linux configuration tools for Ninjutso gaming mice — a GTK4 app, a CLI, and a
battery tray — talking to the device over `hidraw` with no vendor software and
no browser. A Rust port of [ninjutso](../ninjutso), same protocol, same
hardware findings.

Ninjutso ship [NinjaForce](https://www.ninjaforce.co), a WebHID panel that
gates on `"Support Chrome and Edge Only"`. This does the same job natively.

## Status

Verified working against a **Ninjutso Ten** (receiver `093a:eb01`, paired mouse
`0xe020`) on Fedora 44 / KDE Plasma / Wayland.

| | |
|---|---|
| DPI, all stages | ✅ read + write, confirmed by read-back |
| Report rate (1/2/4/8 kHz) | ✅ |
| Lift-off distance | ✅ |
| Motion sync, angle tuning | ✅ |
| System mode | ✅ (Ten exposes High Speed + Competitive only) |
| Battery + charging | ✅ |
| Firmware versions | ✅ read, plus an online up-to-date check |
| Receiver lighting: mode, colour, speed | ✅ |
| Receiver lighting: brightness | ❌ not implemented in the Ten's firmware |
| Optical engine | ❌ Sora V3 only |
| Firmware flashing | ❌ deliberately unimplemented — see below |

The table is what the **CLI** covers, which is everything the protocol layer
supports. The GUI is a subset: it edits the active DPI stage only, and does not
yet expose lighting speed or the online firmware check — use `ninjutso-cli` for
those. The tray reads battery and charge only, by design.

Sora V2 (legacy protocol) and Sora V3 paths are written but **untested** — I
only own a Ten. Reports welcome.

## Build

```sh
cargo build --release                              # all three binaries
cargo build --release --no-default-features        # CLI only, no system deps
```

The CLI needs nothing but a Rust toolchain. The GUI needs `gtk4-devel` 4.10 or
newer and `libadwaita-devel` (`gtk4` + `libadwaita` feature `gui`) — 4.10 for
the colour picker's `GtkColorDialogButton`; the tray is pure Rust over D-Bus
(feature `tray`). Both are on by default.

```sh
sudo dnf install cargo rust gtk4-devel libadwaita-devel   # Fedora
```

## Install

```sh
sudo install -m644 70-ninjutso.rules /etc/udev/rules.d/
sudo udevadm control --reload
sudo udevadm trigger --action=add --subsystem-match=hidraw --settle
```

Replug the receiver, then `./check-access` to confirm. The rule grants access
via `uaccess`, so the device belongs to whoever is logged in at the seat —
no group membership, no root.

The filename matters: `uaccess` is acted on by systemd's `73-seat-late.rules`,
so a rule that sets the tag must sort **before** 73. At `99-` the tag is set
too late to be seen, `MODE` still applies, and you get a `0660 root:root` node
with no ACL — which looks like a permissions quirk rather than an ordering bug.

`udevadm trigger` is asynchronous; without `--settle` a `check-access` run
immediately afterwards can read the node before the ACL lands and report a
false denial.

```sh
./install-desktop              # add the GUI to your application launcher
./install-desktop --autostart  # start the battery tray at login
```

## Use

```sh
ninjutso-cli status            # everything
ninjutso-cli stages            # list DPI stages
ninjutso-cli dpi 1600          # active stage
ninjutso-cli dpi 800 --stage 3
ninjutso-cli rate 4000
ninjutso-cli light --mode Static --color '#36ad6a'
ninjutso-cli firmware          # installed vs latest
ninjutso-gui                   # GTK4 app
ninjutso-tray --poll 10        # battery in the system tray
```

All three take `--device /dev/hidrawN` to pick a node when more than one
Ninjutso device is attached. `ninjutso-tray` also takes `--gui <path>`; both
`--help` on anything they do not understand rather than starting up with a
silently ignored flag.

Every setter writes, re-reads, and reports what the device actually confirmed.
Nothing claims success on a blind write.

## How it works

The receiver's own report descriptor declares the config channel:

```
06 01 ff  09 01  a1 01  85 06  ... 95 0f ... b1 02
vendor page FF01, report ID 6, 15-byte Feature report
```

Requests are `[command, 0, 0, 1, 0, argc, profile, ...args]`, sent with
`HIDIOCSFEATURE` and read back with `HIDIOCGFEATURE` — both hand-built ioctl
requests over `libc`, so there is no `hidapi` or `libusb` dependency. Command
numbering was derived from NinjaForce's own JavaScript and then verified on
hardware.

Unsupported commands are distinguishable: the Ten answers `lighting_mode` (32)
but times out on `lighting_brightness` (48) exactly as it does on a command
that does not exist, which is how the brightness gap above was established
rather than assumed.

## Design notes

**The GUI never blocks on the radio.** One worker thread owns the `Device` and
is driven by messages; replies reach the main loop over an async channel. No
lock is ever held across a round trip, and because the device is owned rather
than shared there is no `Mutex<Device>` to deadlock.

**Tray links no GUI toolkit.** It speaks StatusNotifierItem and dbusmenu
directly over D-Bus via `zbus`, because libayatana-appindicator is GTK3-only
and cannot share a process with GTK4. Tray icons are ARGB32 bitmaps rasterised
from a signed-distance field in plain Rust, so the battery level is exact and
never touches the icon theme cache.

**The tray survives the desktop restarting.** It re-registers with the
StatusNotifier watcher whenever that name gains an owner, so a plasmashell
crash or a panel reload brings the icon back by itself. A missing watcher at
login is not fatal either — it waits for one rather than exiting. The tray
holds `co.ninjutso.Configurator.Tray` on the bus, so a second copy refuses to
start instead of adding a duplicate icon.

**Nothing is ever shown as a value it did not read.** An unanswered battery is
"no reading", not 0%; an unknown charge state says so rather than claiming
"not charging"; and a failed write re-reads the device instead of leaving the
widget showing what you asked for. The tray draws "no reading" as a bare grey
silhouette, distinct from the red of a genuinely flat battery.

**Battery polling is deliberately cheap.** `Device::battery` is two round-trips
(~60 ms), not `status`'s twenty-one (~644 ms), because each read wakes the
2.4 GHz link. At the 10-minute default that is 8.6 s of radio time per day.
The timer runs on `CLOCK_MONOTONIC`, which does not advance across a suspend,
so the tray also listens for logind's `PrepareForSleep` and polls on resume
rather than showing a nine-hour-old reading.

**The protocol layer is testable without a mouse.** `Device` talks through a
`Transport` trait; `Hidraw` is the real one and a scripted fake stands in for
tests, holding device state keyed by command and applying writes through the
same setter/getter pairing the hardware implements. Everything above the trait
— encoding, read-back confirmation, timeout classification, all of `status()` —
runs unchanged against it, so `cargo test` covers the Sora V3 DPI range, the
sleeping-mouse-versus-dead-receiver distinction and every setter round trip on
hardware the author does not own.

**No firmware writing, ever.** These tools report versions and stop there.
Flashing is Windows-only from the vendor, and a half-written dongle is a brick.

`ninjutso-cli firmware` does tell you whether you are current, by asking the
same endpoint NinjaForce's web panel uses:

```
GET https://api.ninjaforce.co/firmware/get_latest_version?pid=<decimal pid>
Authorization: ninjaforce:win
```

The pid must be decimal — hex spellings 404 — and that `Authorization` value is
a constant shipped in NinjaForce's public JavaScript, not a per-user secret.
The endpoint is undocumented, so the check fails soft: if it cannot be reached
you still get your installed versions, and `--offline` skips it entirely.
The command exits non-zero only when an update is genuinely available.

## Layout

```
src/protocol.rs     command table, encoders, packet builder
src/transport.rs    the Transport trait, hidraw ioctl, test fake
src/device.rs       device discovery and the command layer
src/app.rs          GTK4 / libadwaita GUI
src/tray.rs         StatusNotifierItem + dbusmenu over D-Bus
src/icons.rs        ARGB32 icon rasteriser
src/firmware.rs     online version check (fails soft)
src/bin/cli.rs      terminal front end
src/bin/gui.rs      GUI entry point
src/bin/tray.rs     battery tray daemon
```

## Differences from the Python original

Behaviour is intended to match; these are the deliberate divergences.

- **Toggle rows are linked `GtkToggleButton`s**, not `AdwToggleGroup`. The
  widget needs libadwaita 1.7, and pinning the binding that exposes it would
  buy nothing the buttons do not already do.
- **The GUI reopens the device on refresh** after a failed read, so unplugging
  and replugging the receiver recovers without restarting the app.
- **`--device` identifies the node it is given** against the same sysfs scan
  autodetect uses, so a hand-passed path still reports the right product id
  instead of falling back to `0`.
- **The tray takes `--gui <path>`** to override where it looks for the GUI
  binary, which the Python version inferred from its own location only.
- **Spin rows write once they settle**, not once per step. `value-changed`
  fires per click, key repeat and scroll notch, and each write costs a round
  trip plus a confirming read — the Python version sent them all, so a flick of
  the wheel over the DPI row wrote every value it passed through to flash.
- **The tray reaps the GUIs it launches** and bounds its D-Bus calls, both of
  which matter only after days of uptime, which is how the tray is meant to run.

## Credit

Protocol groundwork from [OpenMouse](https://github.com/OpenMouse-Project/mouse-protocol),
whose Ninjutso driver is derived from NinjaForce's shipped JavaScript. Their
catalogue marks every Ninjutso entry `verified: false`; this repo confirms the
Ten's transport and command set on real hardware, including two capability
gaps their gating predicted correctly.
