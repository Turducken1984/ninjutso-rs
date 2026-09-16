# Changelog

## Unreleased

Everything below came out of a review of the GUI and the tray. The theme is
the same in both: the software should never assert something about the
hardware that it did not read, and a daemon meant to run for days should
survive the things that happen over days.

### Fixed — the tray stops disappearing

- **Re-register with the StatusNotifier watcher** whenever that name gains an
  owner. A plasmashell restart wiped the watcher's item list and nothing told
  us, so the icon vanished for good while the process kept polling the radio
  and emitting signals into the void. A missing watcher at login is no longer
  fatal either: it waits for one instead of exiting 1.
- **Reap launched GUIs.** Dropping a `Child` does not wait, and GApplication
  uniqueness means every launch after the first exits within milliseconds —
  one zombie per menu click, for the life of the daemon.
- **Bound D-Bus calls at 5 s.** zbus waits forever by default, so a
  notification daemon that accepted a call and never answered wedged polling
  and menu handling with it.
- **Exit when the session bus goes away** rather than living on invisibly,
  after three consecutive failed signals.
- **Poll on resume.** The poll timer runs on `CLOCK_MONOTONIC`, which does not
  advance while suspended; the tray now also listens for logind's
  `PrepareForSleep` so a nine-hour sleep does not leave a nine-hour-old
  reading on screen.
- **Refuse to start twice.** The tray takes `co.ninjutso.Configurator.Tray`,
  so a second copy says so instead of adding a duplicate icon, a second poll
  and a second set of notifications.

### Fixed — the GUI stops lying about the device

- **Spin rows write once they settle.** `value-changed` fires on every click,
  key repeat and scroll notch, and each write costs a round trip plus a
  confirming read of up to 750 ms, so one flick of the wheel over the DPI row
  queued a dozen real flash writes. Closing the window mid-drain left the
  mouse on an intermediate DPI nobody chose.
- **A failed write re-reads the device** instead of leaving the widget showing
  the value you asked for, and drops the device handle unless the failure was
  `Unsupported` — an unplugged receiver used to leave every later write failing
  while the header still read "Connected".
- **The DPI range comes from the device.** It was hard-coded to the Ten's
  50–30000; a Sora V3 reporting 36000 was clamped to 30000, so the row showed a
  DPI the mouse did not hold and the next edit wrote from that false baseline.
- **A sleeping mouse says so** — "Mouse is asleep", with what to do about it —
  rather than the generic "Could not talk to the device".
- **A dead worker thread is reported** instead of turning every control into a
  silent no-op on a window that still looks responsive.

### Fixed — both

- An unreadable battery is `None`, not `0`. The tray drew a red, empty mouse
  for a disconnected receiver, identical to a nearly flat one; the GUI showed
  an alarming red 0%.
- An unknown charging state is no longer reported as "not charging", which
  could fire a *critical* low-battery warning at a mouse sitting on a charger.
- Low-battery warnings pick the lowest crossed threshold, so starting the tray
  at 4% warns critically at once instead of working down from 20% over twenty
  minutes, and are withdrawn once the level recovers or charging starts.

### Added

- **A colour picker in the GUI.** Receiver lighting colour was reachable from
  the CLI only.
- **`--device PATH` on the GUI and the tray**, matching the CLI. Both now
  reject arguments they do not understand and print usage, rather than
  silently ignoring them — `ninjutso-tray --poll abc` used to mean 10.
- **A `Transport` trait and a scripted fake**, which is what makes the protocol
  layer testable at all. Tests: 14 → 46, covering the Sora V3 DPI range, the
  sleeping-mouse-versus-dead-receiver distinction, every setter round trip, the
  dbusmenu argument handling, the warning hysteresis and the GUI's whole
  status-to-widgets mapping — none of which needed a mouse plugged in.

### Changed

- The GUI now needs GTK 4.10 (for `GtkColorDialogButton`) and libadwaita 1.6
  (for `AdwSpinner`). Fedora 44 ships 4.22 and 1.9.
- dbusmenu methods honour their arguments: `get_layout` respects `parent_id`
  and `recursion_depth`, property filtering works, and an unknown id is a
  D-Bus error rather than an empty string.
- Toasts and the error page no longer render device strings as Pango markup.
- The README's feature table is scoped to the CLI, naming what the GUI does
  not cover, rather than implying all three front ends are equivalent.
