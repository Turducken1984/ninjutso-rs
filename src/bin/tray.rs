//! Standalone battery tray for Ninjutso mice.
//!
//! Links no GUI toolkit at all -- just D-Bus -- so it stays small enough to
//! leave running all day. The full GUI is a separate process, launched on demand.

use std::collections::HashSet;
use std::env;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
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

struct BatteryTray {
    tray: Tray,
    poll: Duration,
    gui_path: Option<PathBuf>,
    notify_id: u32,
    warned: HashSet<u8>,
    last_render: Option<(u8, bool, bool)>,
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
            poll: Duration::from_secs(poll_minutes.max(1) * 60),
            gui_path,
            notify_id: 0,
            warned: HashSet::new(),
            last_render: None,
        })
    }

    /// Launch the GUI. GApplication uniqueness means a second launch just
    /// presents the window that is already open.
    fn open_gui(&self) {
        let path = self.gui_path.clone().unwrap_or_else(|| {
            env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|dir| dir.join("ninjutso-gui")))
                .unwrap_or_else(|| PathBuf::from("ninjutso-gui"))
        });
        if let Err(err) = Command::new(&path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            eprintln!("could not launch GUI: {err}");
        }
    }

    // -- polling -------------------------------------------------------------

    fn poll_device(&mut self) {
        let reading = Device::open().map(|device| device.battery());
        let (percent, charging) = match reading {
            Err(_) => {
                self.render(None, false, "Ninjutso — receiver not connected");
                return;
            }
            Ok((None, _)) => {
                self.render(None, false, "Ninjutso — no battery reading");
                return;
            }
            Ok((Some(percent), charging)) => (percent, charging.unwrap_or(false)),
        };

        let state = if charging { "charging" } else { "battery" };
        self.render(
            Some(percent),
            charging,
            &format!("Ninjutso Ten\n{percent}% {state}"),
        );
        self.maybe_warn(percent, charging);
    }

    fn render(&mut self, percent: Option<u8>, charging: bool, tooltip: &str) {
        let level = percent.unwrap_or(0);
        // Round to 5% so we only repaint when it visibly changes.
        let key = (level.div_ceil(5) * 5, charging, percent.is_none());
        if self.last_render != Some(key) {
            self.last_render = Some(key);
            self.tray.set_pixmaps(
                ICON_SIZES
                    .iter()
                    .map(|&size| battery_pixmap(level, charging, size))
                    .collect(),
            );
        }
        self.tray.set_tooltip(tooltip);
    }

    // -- notifications -------------------------------------------------------

    fn maybe_warn(&mut self, percent: u8, charging: bool) {
        if charging {
            self.warned.clear();
            return;
        }
        // Clear a warning once the level recovers well past it.
        self.warned.retain(|&threshold| percent <= threshold + 5);
        for threshold in THRESHOLDS {
            if percent <= threshold && self.warned.insert(threshold) {
                self.notify(percent, threshold);
                break;
            }
        }
    }

    fn notify(&mut self, percent: u8, threshold: u8) {
        let urgency: u8 = if threshold <= 10 { 2 } else { 1 }; // 2 = critical
        let hints = [
            ("urgency", Value::from(urgency)),
            ("category", Value::from("device.battery.low")),
        ]
        .into_iter()
        .collect::<std::collections::HashMap<_, _>>();
        let body =
            format!("Your Ninjutso Ten is at {percent}%. Plug it in to keep using it.");

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

fn parse_poll_minutes(args: &[String]) -> u64 {
    args.iter()
        .position(|arg| arg == "--poll")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(10)
        .max(1)
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let poll = parse_poll_minutes(&args);
    let gui_path = args
        .iter()
        .position(|arg| arg == "--gui")
        .and_then(|index| args.get(index + 1))
        .map(PathBuf::from);

    let (sender, receiver) = mpsc::channel();
    let mut tray = match BatteryTray::new(poll, gui_path, sender) {
        Ok(tray) => tray,
        Err(_) => {
            eprintln!("error: no system tray (StatusNotifier host) on this desktop");
            return ExitCode::from(1);
        }
    };
    ExitCode::from(tray.run(receiver))
}
