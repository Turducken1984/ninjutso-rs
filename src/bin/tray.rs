//! Standalone battery tray for Ninjutso mice.
//!
//! Links no GUI toolkit at all -- just D-Bus -- so it stays small enough to
//! leave running all day. The full GUI is a separate process, launched on demand.

use std::collections::HashSet;
use std::env;
use std::path::PathBuf;
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use zbus::zvariant::Value;

use ninjutso::device::Device;
use ninjutso::icons::battery_pixmap;
use ninjutso::tray::{Action, MenuEntry, Tray};

const APP_ID: &str = "co.ninjutso.Configurator";
const ICON_SIZES: [i32; 3] = [22, 32, 48];

/// Warn once as the battery falls past each of these.
const THRESHOLDS: [u8; 3] = [20, 10, 5];
const NOTIFY_IFACE: &str = "org.freedesktop.Notifications";

const USAGE: &str = "usage: ninjutso-tray [--poll MINUTES] [--gui PATH]\n\
    \n\
    \x20 --poll MINUTES  how often to read the battery (1-1440, default 10)\n\
    \x20 --gui PATH      ninjutso-gui to launch from the menu";

struct Args {
    poll_minutes: u64,
    gui_path: Option<PathBuf>,
}

/// Parse argv, rejecting anything unrecognised.
///
/// A tray is started from a generated `.desktop` file, so a typo here is
/// invisible unless it is reported.
fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut parsed = Args {
        poll_minutes: 10,
        gui_path: None,
    };
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--help" | "-h" => return Err(String::new()),
            "--poll" => {
                let value = rest.next().ok_or("--poll needs a number of minutes")?;
                let minutes: u64 = value
                    .parse()
                    .map_err(|_| format!("--poll wants a number, not {value:?}"))?;
                // Clamped rather than merely non-zero: `minutes * 60` would
                // otherwise overflow on a fat-fingered value.
                if !(1..=1440).contains(&minutes) {
                    return Err(format!("--poll must be between 1 and 1440, not {minutes}"));
                }
                parsed.poll_minutes = minutes;
            }
            "--gui" => {
                let value = rest.next().ok_or("--gui needs a path")?;
                parsed.gui_path = Some(PathBuf::from(value));
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(parsed)
}

/// How the icon looks: level rounded to 5%, and the charge state. Repainting
/// only when this changes keeps three pixmaps from being redrawn every poll.
fn render_key(percent: Option<u8>, charging: Option<bool>) -> (Option<u8>, Option<bool>) {
    (percent.map(|value| value.div_ceil(5) * 5), charging)
}

/// The threshold to warn about, if any, updating what has already been warned.
///
/// Picks the *lowest* threshold crossed, so starting the tray with the mouse
/// already at 4% gives the critical warning rather than the 20% one. A warning
/// is only rearmed once the level recovers 5 points past its threshold.
fn warn_threshold(warned: &mut HashSet<u8>, percent: u8, charging: Option<bool>) -> Option<u8> {
    // Unknown charge state is not evidence of discharging; staying quiet beats
    // crying "critical" at a mouse sitting on the charger.
    if charging != Some(false) {
        if charging == Some(true) {
            warned.clear();
        }
        return None;
    }
    warned.retain(|&threshold| percent <= threshold + 5);
    // Every crossed threshold is marked, so a drop straight past two of them
    // reports the lower one and stays quiet about the other.
    let mut lowest = None;
    for threshold in THRESHOLDS {
        if percent <= threshold && warned.insert(threshold) {
            lowest = Some(lowest.map_or(threshold, |seen: u8| seen.min(threshold)));
        }
    }
    lowest
}

struct BatteryTray {
    tray: Tray,
    poll: Duration,
    gui_path: Option<PathBuf>,
    notify_id: u32,
    warned: HashSet<u8>,
    last_render: Option<(Option<u8>, Option<bool>)>,
    /// Launched GUIs, kept only so they can be reaped. Dropping a `Child`
    /// does not wait, and GApplication uniqueness means every launch after the
    /// first exits within milliseconds -- one zombie per click, otherwise.
    children: Vec<Child>,
}

impl BatteryTray {
    fn new(poll_minutes: u64, gui_path: Option<PathBuf>, actions: Sender<Action>) -> zbus::Result<Self> {
        let items = vec![
            MenuEntry::item(1, "Open Ninjutso", Action::OpenGui),
            MenuEntry::separator(2),
            MenuEntry::item(3, "Refresh now", Action::Refresh),
            MenuEntry::separator(4),
            MenuEntry::item(5, "Quit", Action::Quit),
        ];
        Ok(Self {
            tray: Tray::register(APP_ID, APP_ID, items, actions)?,
            poll: Duration::from_secs(poll_minutes.clamp(1, 1440) * 60),
            gui_path,
            notify_id: 0,
            warned: HashSet::new(),
            last_render: None,
            children: Vec::new(),
        })
    }

    /// Launch the GUI. GApplication uniqueness means a second launch just
    /// presents the window that is already open.
    fn open_gui(&mut self) {
        let path = self.gui_path.clone().unwrap_or_else(|| {
            env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|dir| dir.join("ninjutso-gui")))
                .unwrap_or_else(|| PathBuf::from("ninjutso-gui"))
        });
        match Command::new(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => self.children.push(child),
            Err(err) => eprintln!("could not launch GUI: {err}"),
        }
    }

    fn reap(&mut self) {
        self.children
            .retain_mut(|child| matches!(child.try_wait(), Ok(None)));
    }

    // -- polling -------------------------------------------------------------

    fn poll_device(&mut self) {
        let Ok(device) = Device::open() else {
            self.render(None, None, "Ninjutso — receiver not connected");
            return;
        };
        let (percent, charging) = device.battery();
        let Some(level) = percent else {
            self.render(None, charging, "Ninjutso — no battery reading");
            return;
        };

        let state = match charging {
            Some(true) => "charging",
            Some(false) => "battery",
            None => "battery, charging status unavailable",
        };
        self.render(
            percent,
            charging,
            &format!("Ninjutso\n{level}% {state}"),
        );
        if let Some(threshold) = warn_threshold(&mut self.warned, level, charging) {
            self.notify(level, threshold);
        }
    }

    fn render(&mut self, percent: Option<u8>, charging: Option<bool>, tooltip: &str) {
        let key = render_key(percent, charging);
        if self.last_render != Some(key) {
            self.last_render = Some(key);
            self.tray.set_pixmaps(
                ICON_SIZES
                    .iter()
                    .map(|&size| battery_pixmap(percent, charging == Some(true), size))
                    .collect(),
            );
        }
        self.tray.set_tooltip(tooltip);
    }

    // -- notifications -------------------------------------------------------

    fn notify(&mut self, percent: u8, threshold: u8) {
        let urgency: u8 = if threshold <= 10 { 2 } else { 1 }; // 2 = critical
        let hints = [
            ("urgency", Value::from(urgency)),
            ("category", Value::from("device.battery.low")),
        ]
        .into_iter()
        .collect::<std::collections::HashMap<_, _>>();
        let body = format!("Your Ninjutso mouse is at {percent}%. Plug it in to keep using it.");

        let reply = self.tray.connection().call_method(
            Some(NOTIFY_IFACE),
            "/org/freedesktop/Notifications",
            Some(NOTIFY_IFACE),
            "Notify",
            &(
                "Ninjutso",
                self.notify_id,
                "battery-low",
                "Mouse battery low",
                body,
                Vec::<String>::new(),
                hints,
                0i32,
            ),
        );
        // No notification daemon; the icon still shows the level.
        if let Ok(reply) = reply {
            if let Ok(id) = reply.body().deserialize::<u32>() {
                self.notify_id = id;
            }
        }
    }

    fn run(&mut self, actions: Receiver<Action>) -> u8 {
        self.poll_device();
        loop {
            self.reap();
            match actions.recv_timeout(self.poll) {
                Ok(Action::Quit) => return 0,
                Ok(Action::OpenGui) => self.open_gui(),
                Ok(Action::Refresh) => self.poll_device(),
                // The poll interval elapsed with nothing clicked.
                Err(RecvTimeoutError::Timeout) => self.poll_device(),
                Err(RecvTimeoutError::Disconnected) => return 0,
            }
        }
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = env::args().skip(1).collect();
    let args = match parse_args(&argv) {
        Ok(args) => args,
        Err(message) => {
            if !message.is_empty() {
                eprintln!("error: {message}");
            }
            println!("{USAGE}");
            return ExitCode::from(u8::from(!message.is_empty()));
        }
    };

    let (sender, receiver) = mpsc::channel();
    let mut tray = match BatteryTray::new(args.poll_minutes, args.gui_path, sender) {
        Ok(tray) => tray,
        Err(zbus::Error::NameTaken) => {
            eprintln!("a Ninjutso tray is already running");
            return ExitCode::from(0);
        }
        Err(err) => {
            eprintln!("error: could not reach the session bus: {err}");
            return ExitCode::from(1);
        }
    };
    ExitCode::from(tray.run(receiver))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn poll_is_parsed_and_bad_values_are_refused() {
        assert_eq!(parse_args(&args(&[])).unwrap().poll_minutes, 10);
        assert_eq!(parse_args(&args(&["--poll", "3"])).unwrap().poll_minutes, 3);
        assert!(parse_args(&args(&["--poll", "abc"])).is_err());
        assert!(parse_args(&args(&["--poll"])).is_err());
        assert!(parse_args(&args(&["--poll", "0"])).is_err());
        // Would overflow `minutes * 60` seconds.
        assert!(parse_args(&args(&["--poll", "400000000000000000"])).is_err());
        assert!(parse_args(&args(&["--wat"])).is_err());
    }

    #[test]
    fn gui_path_is_optional() {
        assert_eq!(parse_args(&args(&[])).unwrap().gui_path, None);
        assert_eq!(
            parse_args(&args(&["--gui", "/usr/bin/x"])).unwrap().gui_path,
            Some(PathBuf::from("/usr/bin/x"))
        );
    }

    #[test]
    fn the_icon_is_only_redrawn_when_it_would_look_different() {
        assert_eq!(render_key(Some(61), Some(false)), render_key(Some(65), Some(false)));
        assert_ne!(render_key(Some(61), Some(false)), render_key(Some(66), Some(false)));
        assert_ne!(render_key(Some(61), Some(false)), render_key(Some(61), Some(true)));
        // No reading is its own state, not 0%.
        assert_ne!(render_key(None, None), render_key(Some(0), None));
    }

    #[test]
    fn each_threshold_warns_once_per_descent() {
        let mut warned = HashSet::new();
        assert_eq!(warn_threshold(&mut warned, 25, Some(false)), None);
        assert_eq!(warn_threshold(&mut warned, 19, Some(false)), Some(20));
        assert_eq!(warn_threshold(&mut warned, 18, Some(false)), None);
        assert_eq!(warn_threshold(&mut warned, 9, Some(false)), Some(10));
        assert_eq!(warn_threshold(&mut warned, 4, Some(false)), Some(5));
        assert_eq!(warn_threshold(&mut warned, 3, Some(false)), None);
    }

    #[test]
    fn a_low_start_warns_at_the_level_it_is_actually_at() {
        let mut warned = HashSet::new();
        assert_eq!(warn_threshold(&mut warned, 4, Some(false)), Some(5));
    }

    #[test]
    fn charging_and_unknown_charge_never_warn() {
        let mut warned = HashSet::new();
        assert_eq!(warn_threshold(&mut warned, 4, Some(true)), None);
        assert_eq!(warn_threshold(&mut warned, 4, None), None);
        // ...and an unknown state does not clear what a discharge armed.
        assert_eq!(warn_threshold(&mut warned, 4, Some(false)), Some(5));
        assert_eq!(warn_threshold(&mut warned, 4, None), None);
        assert_eq!(warn_threshold(&mut warned, 4, Some(false)), None);
    }

    #[test]
    fn a_warning_rearms_only_after_a_real_recovery() {
        let mut warned = HashSet::new();
        assert_eq!(warn_threshold(&mut warned, 9, Some(false)), Some(10));
        // Still inside the band: no repeat.
        assert_eq!(warn_threshold(&mut warned, 14, Some(false)), None);
        // Recovered past it, then fell again.
        assert_eq!(warn_threshold(&mut warned, 16, Some(false)), None);
        assert_eq!(warn_threshold(&mut warned, 9, Some(false)), Some(10));
    }
}
