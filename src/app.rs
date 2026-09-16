//! GTK4 / libadwaita front end for Ninjutso mice.
//!
//! The device is owned by a single worker thread and driven by messages, so the
//! main loop never blocks on a 2.4 GHz round trip and no lock is ever held
//! across one.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::mpsc::{self, Sender};
use std::thread;

use adw::prelude::*;
use gtk4 as gtk;
use gtk::gdk::RGBA;
use libadwaita as adw;

use crate::device::{Device, Status};
use crate::protocol as p;
use crate::Error;

pub const APP_ID: &str = "co.ninjutso.Configurator";

/// Ninjutso's own accent (#36ad6a) and its dim variant (#0c7a43), lifted from
/// the NinjaForce stylesheet.
const CSS: &str = concat!(
    ":root {\n",
    "  --accent-bg-color: #36ad6a;\n",
    "  --accent-fg-color: #ffffff;\n",
    "  --accent-color: #0c7a43;\n",
    "}\n",
    ".battery-good { color: #36ad6a; font-weight: 700; }\n",
    ".battery-low  { color: #e10600; font-weight: 700; }\n",
    ".dot-online   { color: #36ad6a; font-size: 1.2em; }\n",
);

const UDEV_HINT: &str = "Install the udev rule, then replug the receiver:\n\n\
     sudo install -m644 70-ninjutso.rules /etc/udev/rules.d/\n\
     sudo udevadm control --reload && sudo udevadm trigger --settle";

/// Work for the device thread.
enum Command {
    Refresh,
    SetDpi(u32),
    SetRate(u32),
    SetLod(String),
    SetMotion(bool),
    SetLightMode(String),
    SetColor(String),
    SetBrightness(u8),
}

/// What the device thread sends back to the main loop.
enum Reply {
    Status(Box<Status>),
    Failed { title: String, detail: String },
    Toast(String),
    /// A setter that did not land. The widget is now showing a value the device
    /// does not hold, so the main loop re-reads instead of leaving it lying.
    Rejected(String),
}

fn describe(err: &Error) -> (String, String) {
    match err {
        Error::PermissionDenied { .. } => (
            "Permission denied".to_string(),
            format!("{err}\n\n{UDEV_HINT}"),
        ),
        Error::DeviceNotFound => ("No Ninjutso device".to_string(), err.to_string()),
        // The receiver is fine and only the mouse has dozed off, which the user
        // fixes by touching the mouse. Reporting it as a fault in the software
        // sends them looking in the wrong place.
        Error::Offline => (
            "Mouse is asleep".to_string(),
            format!("{err}\n\nClick the mouse, then press Refresh."),
        ),
        _ => (
            "Could not talk to the device".to_string(),
            err.to_string(),
        ),
    }
}

/// Own the device on one thread and serialise every request onto it.
fn spawn_worker(
    commands: mpsc::Receiver<Command>,
    replies: async_channel::Sender<Reply>,
    device_path: Option<PathBuf>,
) {
    thread::spawn(move || {
        let mut held: Option<Device> = None;
        while let Ok(command) = commands.recv() {
            // Re-open lazily so unplugging and replugging recovers on Refresh.
            if held.is_none() {
                let opened = match &device_path {
                    Some(path) => Device::open_path(path.clone()),
                    None => Device::open(),
                };
                match opened {
                    Ok(device) => held = Some(device),
                    Err(err) => {
                        let (title, detail) = describe(&err);
                        let _ = replies.send_blocking(Reply::Failed { title, detail });
                        continue;
                    }
                }
            }
            let device = held.as_mut().expect("opened above");

            let reply = match command {
                Command::Refresh => match device.status() {
                    Ok(status) => Reply::Status(Box::new(status)),
                    Err(err) => {
                        let (title, detail) = describe(&err);
                        held = None;
                        Reply::Failed { title, detail }
                    }
                },
                setter => {
                    let (label, outcome) = match setter {
                        Command::SetDpi(value) => (
                            "DPI",
                            device.set_dpi(value, None).map(|v| (v as i64).to_string()),
                        ),
                        Command::SetRate(value) => (
                            "Report rate",
                            device.set_polling_rate(value).map(|v| format!("{v} Hz")),
                        ),
                        Command::SetLod(value) => {
                            ("Lift-off", device.set_lift_off(&value).map(str::to_string))
                        }
                        Command::SetMotion(value) => (
                            "Motion Sync",
                            device
                                .set_motion_sync(value)
                                .map(|on| if on { "on" } else { "off" }.to_string()),
                        ),
                        Command::SetLightMode(mode) => (
                            "Light mode",
                            device.set_light_mode(&mode).map(str::to_string),
                        ),
                        Command::SetColor(hex) => {
                            ("Light colour", device.set_color(&hex))
                        }
                        Command::SetBrightness(value) => (
                            "Brightness",
                            device.set_brightness(value).map(|v| v.to_string()),
                        ),
                        Command::Refresh => unreachable!("handled above"),
                    };
                    match outcome {
                        Ok(value) => Reply::Toast(format!("{label} → {value}")),
                        Err(err) => {
                            // A transport failure may also mean the receiver is
                            // gone; drop the handle so the refresh re-opens it.
                            if !matches!(err, Error::Unsupported(_)) {
                                held = None;
                            }
                            Reply::Rejected(format!("{label} failed: {err}"))
                        }
                    }
                }
            };
            if replies.send_blocking(reply).is_err() {
                return; // window closed
            }
        }
    });
}

/// What a status reading means for the widgets, worked out before any of them
/// are touched.
///
/// Keeping the mapping separate from the setting is what makes it testable: a
/// DPI range that clamps a Sora V3's reading, or a missing battery dressed up
/// as 0%, is a bug in this function and nowhere near a display.
#[derive(Debug, PartialEq)]
struct View {
    title: String,
    subtitle: String,
    connection: String,
    battery: String,
    battery_class: Option<&'static str>,
    /// lower, upper, step -- set on the adjustment *before* the value, or it
    /// clamps a reading the row then reports as the device's own.
    dpi_range: (f64, f64, f64),
    dpi: Option<f64>,
    dpi_subtitle: String,
    rate_index: Option<usize>,
    lod_index: Option<usize>,
    motion_sync: bool,
    light: Option<LightView>,
    firmware: String,
}

#[derive(Debug, PartialEq)]
struct LightView {
    mode_index: Option<usize>,
    color: Option<String>,
    brightness: Option<u8>,
}

/// GTK keeps colour channels as 0..1 floats; the protocol wants `#rrggbb`.
fn hex_of(rgba: RGBA) -> String {
    let byte = |channel: f32| (channel.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        byte(rgba.red()),
        byte(rgba.green()),
        byte(rgba.blue())
    )
}

fn view(status: &Status) -> View {
    let link = if status.is_receiver { "Receiver" } else { "Wired" };
    let (dpi_step, dpi_upper) = if status.direct {
        (1.0, f64::from(p::DPI_MAX_DIRECT))
    } else {
        (50.0, f64::from(p::DPI_MAX))
    };
    let dpi_lower = if status.direct {
        f64::from(p::DPI_MIN_DIRECT)
    } else {
        f64::from(p::DPI_MIN)
    };

    View {
        title: status.name.replace("Ninjutso Inc. ", ""),
        subtitle: format!("{link} · profile {}", status.profile),
        connection: format!("{link} · {}", status.path.display()),
        battery: match status.battery {
            // No answer is not a flat battery, and must not be dressed as one.
            None => "—".to_string(),
            Some(percent) => format!(
                "{percent}%{}",
                if status.charging == Some(true) { " ⚡" } else { "" }
            ),
        },
        battery_class: status.battery.map(|percent| {
            if percent < 20 {
                "battery-low"
            } else {
                "battery-good"
            }
        }),
        dpi_range: (dpi_lower, dpi_upper, dpi_step),
        dpi: status.dpi,
        dpi_subtitle: format!(
            "Stage {} of {}",
            status.dpi_stage + 1,
            status.dpi_stages.len()
        ),
        rate_index: p::POLLING_RATES
            .iter()
            .position(|&rate| rate == status.polling_rate),
        lod_index: status
            .lift_off
            .and_then(|value| p::LOD_VALUES.iter().position(|&v| v == value)),
        motion_sync: status.motion_sync,
        light: status.lighting.as_ref().map(|light| LightView {
            mode_index: p::LIGHT_MODES.iter().position(|&m| m == light.mode),
            color: light.color.clone(),
            brightness: light.brightness,
        }),
        firmware: status
            .firmware
            .iter()
            .map(|(part, version)| format!("{}{} {version}", part[..1].to_uppercase(), &part[1..]))
            .collect::<Vec<_>>()
            .join(" · "),
    }
}

/// Every widget the status refresh has to write into.
struct Ui {
    toasts: adw::ToastOverlay,
    stack: gtk::Stack,
    error_page: adw::StatusPage,
    title: adw::WindowTitle,
    conn_row: adw::ActionRow,
    dot: gtk::Label,
    battery: gtk::Label,
    dpi_row: adw::SpinRow,
    rate_toggles: Vec<gtk::ToggleButton>,
    lod_toggles: Vec<gtk::ToggleButton>,
    motion_row: adw::SwitchRow,
    light_group: adw::PreferencesGroup,
    mode_toggles: Vec<gtk::ToggleButton>,
    color_button: gtk::ColorDialogButton,
    bright_row: adw::SpinRow,
    fw_row: adw::ActionRow,
    /// Suppresses handlers while repopulating from a fresh status read.
    loading: Cell<bool>,
    commands: Sender<Command>,
}

impl Ui {
    fn send(&self, command: Command) {
        if self.loading.get() {
            return;
        }
        // A dead worker would otherwise turn every control into a silent
        // no-op while the window went on looking perfectly responsive.
        if self.commands.send(command).is_err() {
            self.toast("The device thread has stopped — restart the app");
        }
    }

    fn refresh(&self) {
        self.stack.set_visible_child_name("loading");
        if self.commands.send(Command::Refresh).is_err() {
            self.toast("The device thread has stopped — restart the app");
        }
    }

    fn show_error(&self, title: &str, detail: &str) {
        self.error_page.set_title(title);
        // AdwStatusPage renders its description as Pango markup, and these
        // strings carry io::Error text and device paths. An ampersand in one
        // would otherwise come out as a parse warning and garbled text.
        self.error_page
            .set_description(Some(&gtk::glib::markup_escape_text(detail)));
        // Nothing here is connected any more; say so rather than leaving the
        // last good reading behind the error page.
        self.conn_row.set_title("Disconnected");
        self.conn_row.set_subtitle("");
        self.dot.set_visible(false);
        self.stack.set_visible_child_name("error");
    }

    fn toast(&self, text: &str) {
        self.toasts.add_toast(
            // Same reason: AdwToast has use-markup on by default since 1.4.
            adw::Toast::builder()
                .title(text)
                .use_markup(false)
                .timeout(2)
                .build(),
        );
    }

    fn apply(&self, status: &Status) {
        let view = view(status);
        self.loading.set(true);

        self.title.set_title(&view.title);
        self.title.set_subtitle(&view.subtitle);
        self.conn_row.set_title("Connected");
        self.conn_row.set_subtitle(&view.connection);
        self.dot.set_visible(true);

        self.battery.set_label(&view.battery);
        self.battery.remove_css_class("battery-low");
        self.battery.remove_css_class("battery-good");
        if let Some(class) = view.battery_class {
            self.battery.add_css_class(class);
        }

        let (lower, upper, step) = view.dpi_range;
        let adjustment = self.dpi_row.adjustment();
        adjustment.set_lower(lower);
        adjustment.set_upper(upper);
        adjustment.set_step_increment(step);
        if let Some(dpi) = view.dpi {
            self.dpi_row.set_value(dpi);
            self.dpi_row.set_subtitle(&view.dpi_subtitle);
        }

        if let Some(index) = view.rate_index {
            self.rate_toggles[index].set_active(true);
        }
        if let Some(index) = view.lod_index {
            self.lod_toggles[index].set_active(true);
        }
        self.motion_row.set_active(view.motion_sync);

        self.light_group.set_visible(view.light.is_some());
        if let Some(light) = &view.light {
            if let Some(index) = light.mode_index {
                self.mode_toggles[index].set_active(true);
            }
            if let Some(colour) = light.color.as_deref().and_then(|hex| RGBA::parse(hex).ok()) {
                self.color_button.set_rgba(&colour);
            }
            self.bright_row.set_visible(light.brightness.is_some());
            if let Some(brightness) = light.brightness {
                self.bright_row.set_value(f64::from(brightness));
            }
        }

        self.fw_row.set_subtitle(if view.firmware.is_empty() {
            "unavailable"
        } else {
            &view.firmware
        });

        self.loading.set(false);
        self.stack.set_visible_child_name("device");
    }
}

/// How long a spin row must sit still before its value is written.
///
/// `value-changed` fires on every click, key repeat and scroll notch, and each
/// write costs a round trip plus a confirming read of up to 750 ms. Sending them
/// all would queue a dozen real flash writes for one flick of the wheel, take
/// ten seconds to drain, and -- if the window is closed meanwhile -- leave the
/// mouse on whatever intermediate value the drain had reached.
const SPIN_SETTLE: std::time::Duration = std::time::Duration::from_millis(400);

/// Write a spin row's value once it stops moving, not once per step.
fn on_spin_settled(
    row: &adw::SpinRow,
    ui: &Rc<Ui>,
    make: impl Fn(f64) -> Command + 'static,
) {
    let pending: Rc<RefCell<Option<gtk::glib::SourceId>>> = Rc::new(RefCell::new(None));
    let ui = Rc::downgrade(ui);
    let make = Rc::new(make);
    row.connect_value_notify(move |row| {
        let Some(ui) = ui.upgrade() else { return };
        if ui.loading.get() {
            return;
        }
        if let Some(timer) = pending.borrow_mut().take() {
            timer.remove();
        }
        let value = row.value();
        let ui = Rc::downgrade(&ui);
        let make = Rc::clone(&make);
        let armed = Rc::clone(&pending);
        *pending.borrow_mut() = Some(gtk::glib::timeout_add_local_once(SPIN_SETTLE, move || {
            // This source is firing; forget it so the next notify does not try
            // to cancel an id that is already spent.
            armed.borrow_mut().take();
            if let Some(ui) = ui.upgrade() {
                ui.send(make(value));
            }
        }));
    });
}

/// A row of linked toggle buttons acting as one exclusive choice.
fn toggle_row(
    title: &str,
    labels: &[String],
    ui: &Rc<RefCell<Weak<Ui>>>,
    make: impl Fn(usize) -> Command + 'static,
) -> (adw::ActionRow, Vec<gtk::ToggleButton>) {
    let row = adw::ActionRow::builder().title(title).build();
    let holder = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .valign(gtk::Align::Center)
        .build();
    holder.add_css_class("linked");

    let make = Rc::new(make);
    let mut toggles: Vec<gtk::ToggleButton> = Vec::with_capacity(labels.len());
    for (index, label) in labels.iter().enumerate() {
        let button = gtk::ToggleButton::with_label(label);
        if let Some(first) = toggles.first() {
            button.set_group(Some(first));
        }
        let ui = Rc::clone(ui);
        let make = Rc::clone(&make);
        button.connect_toggled(move |button| {
            if !button.is_active() {
                return;
            }
            if let Some(ui) = ui.borrow().upgrade() {
                ui.send(make(index));
            }
        });
        holder.append(&button);
        toggles.push(button);
    }
    row.add_suffix(&holder);
    (row, toggles)
}

fn build_window(app: &adw::Application, device_path: Option<PathBuf>) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Ninjutso")
        .default_width(480)
        .default_height(720)
        .build();

    let (commands, command_rx) = mpsc::channel();
    let (reply_tx, reply_rx) = async_channel::unbounded();
    spawn_worker(command_rx, reply_tx, device_path);

    // The toggle handlers need the Ui, which does not exist until they are
    // built; this cell is filled in the moment it does.
    // Weak, not strong: the Ui owns the widgets that own these closures, so a
    // strong handle here would be a cycle and nothing would ever be freed.
    let late: Rc<RefCell<Weak<Ui>>> = Rc::new(RefCell::new(Weak::new()));

    let toasts = adw::ToastOverlay::new();
    let stack = gtk::Stack::new();

    // Loading ---------------------------------------------------------------
    let spinner_page = adw::StatusPage::builder()
        .title("Looking for your mouse…")
        .build();
    // AdwSpinner rather than GtkSpinner: it matches the platform style and
    // does not need starting, as the Python original also preferred.
    spinner_page.set_child(Some(&adw::Spinner::new()));

    let error_page = adw::StatusPage::builder()
        .icon_name("dialog-warning-symbolic")
        .build();

    // Body ------------------------------------------------------------------
    let page = adw::PreferencesPage::new();

    let conn_group = adw::PreferencesGroup::new();
    let conn_row = adw::ActionRow::builder().title("Disconnected").build();
    let dot = gtk::Label::new(Some("●"));
    dot.add_css_class("dot-online");
    let battery = gtk::Label::new(None);
    conn_row.add_prefix(&dot);
    conn_row.add_suffix(&battery);
    conn_group.add(&conn_row);
    page.add(&conn_group);

    let sensor = adw::PreferencesGroup::builder().title("Sensor").build();

    let dpi_row = adw::SpinRow::with_range(50.0, 30000.0, 50.0);
    dpi_row.set_title("DPI");
    dpi_row.set_subtitle("Active stage");
    sensor.add(&dpi_row);

    let rate_labels: Vec<String> = p::POLLING_RATES
        .iter()
        .map(|rate| format!("{}k", rate / 1000))
        .collect();
    let (rate_row, rate_toggles) = toggle_row("Report Rate", &rate_labels, &late, |index| {
        Command::SetRate(p::POLLING_RATES[index])
    });
    sensor.add(&rate_row);

    let lod_labels: Vec<String> = p::LOD_VALUES.iter().map(|v| v.to_string()).collect();
    let (lod_row, lod_toggles) = toggle_row("Lift-Off Distance", &lod_labels, &late, |index| {
        Command::SetLod(p::LOD_VALUES[index].to_string())
    });
    sensor.add(&lod_row);

    let motion_row = adw::SwitchRow::builder().title("Motion Sync").build();
    sensor.add(&motion_row);
    page.add(&sensor);

    let light_group = adw::PreferencesGroup::builder()
        .title("Receiver Light")
        .description("Lighting lives on the receiver, not the mouse")
        .build();
    let mode_labels: Vec<String> = p::LIGHT_MODES.iter().map(|m| m.to_string()).collect();
    let (mode_row, mode_toggles) = toggle_row("Mode", &mode_labels, &late, |index| {
        Command::SetLightMode(p::LIGHT_MODES[index].to_string())
    });
    light_group.add(&mode_row);

    let color_row = adw::ActionRow::builder().title("Colour").build();
    let color_button = gtk::ColorDialogButton::builder()
        .dialog(&gtk::ColorDialog::builder().with_alpha(false).build())
        .valign(gtk::Align::Center)
        .build();
    color_row.add_suffix(&color_button);
    light_group.add(&color_row);

    let bright_row = adw::SpinRow::with_range(25.0, 100.0, 25.0);
    bright_row.set_title("Brightness");
    light_group.add(&bright_row);
    page.add(&light_group);

    let fw_group = adw::PreferencesGroup::builder().title("Firmware").build();
    let fw_row = adw::ActionRow::builder()
        .title("Versions")
        .subtitle("—")
        .build();
    fw_group.add(&fw_row);
    let note = adw::ActionRow::builder()
        .title("Updates are Windows-only")
        .subtitle(
            "NinjaForce flashes firmware from a Windows executable; this app \
             reports versions but never writes them.",
        )
        .subtitle_lines(3)
        .build();
    fw_group.add(&note);
    page.add(&fw_group);

    let scroller = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .child(&page)
        .build();

    stack.add_named(&spinner_page, Some("loading"));
    stack.add_named(&error_page, Some("error"));
    stack.add_named(&scroller, Some("device"));
    stack.set_visible_child_name("loading");

    // Chrome ----------------------------------------------------------------
    let title = adw::WindowTitle::new("Ninjutso", "");
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some("Re-read settings from the device"));
    header.pack_end(&refresh);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&header);
    stack.set_vexpand(true);
    root.append(&stack);
    toasts.set_child(Some(&root));
    window.set_content(Some(&toasts));

    let ui = Rc::new(Ui {
        toasts,
        stack,
        error_page,
        title,
        conn_row,
        dot,
        battery,
        dpi_row: dpi_row.clone(),
        rate_toggles,
        lod_toggles,
        motion_row: motion_row.clone(),
        light_group,
        mode_toggles,
        color_button: color_button.clone(),
        bright_row: bright_row.clone(),
        fw_row,
        loading: Cell::new(true),
        commands,
    });
    *late.borrow_mut() = Rc::downgrade(&ui);

    // Handlers that need the finished Ui ------------------------------------
    on_spin_settled(&dpi_row, &ui, |value| {
        // Rounded, not truncated: a direct-mode reading can carry tenths.
        Command::SetDpi(value.round() as u32)
    });
    on_spin_settled(&bright_row, &ui, |value| {
        Command::SetBrightness(value.round() as u8)
    });
    let handler_ui = Rc::downgrade(&ui);
    color_button.connect_rgba_notify(move |button| {
        if let Some(ui) = handler_ui.upgrade() {
            ui.send(Command::SetColor(hex_of(button.rgba())));
        }
    });
    let handler_ui = Rc::downgrade(&ui);
    motion_row.connect_active_notify(move |row| {
        if let Some(ui) = handler_ui.upgrade() {
            ui.send(Command::SetMotion(row.is_active()));
        }
    });
    let handler_ui = Rc::downgrade(&ui);
    refresh.connect_clicked(move |_| {
        if let Some(ui) = handler_ui.upgrade() {
            ui.refresh();
        }
    });

    // Drain the worker's replies on the main loop ---------------------------
    let loop_ui = Rc::downgrade(&ui);
    gtk::glib::spawn_future_local(async move {
        while let Ok(reply) = reply_rx.recv().await {
            // Weak again: holding the Ui here would keep it -- and with it the
            // command sender, and so the worker thread -- alive for good.
            let Some(ui) = loop_ui.upgrade() else { return };
            match reply {
                Reply::Status(status) => ui.apply(&status),
                Reply::Failed { title, detail } => ui.show_error(&title, &detail),
                Reply::Toast(text) => ui.toast(&text),
                Reply::Rejected(text) => {
                    ui.toast(&text);
                    ui.refresh();
                }
            }
        }
    });

    // Somebody outside the widget graph has to own the Ui: it holds the
    // widgets, and their handlers point back at it, so every handle inside the
    // graph is deliberately Weak. The window owns it and lets go on close,
    // which drops the command sender, ends the worker, and so ends the reply
    // loop above. Without this the Ui dies the moment this function returns
    // and the window sits on "Looking for your mouse..." for ever.
    //
    // `destroy` rather than `close-request`: close-request is only emitted when
    // the user closes the window, so quitting the application or calling
    // `destroy()` would skip it and leak the lot.
    let owner = RefCell::new(Some(Rc::clone(&ui)));
    window.connect_destroy(move |_| {
        owner.borrow_mut().take();
    });

    ui.refresh();
    window
}

const USAGE: &str = "usage: ninjutso-gui [--device PATH]\n\n\
    \x20 --device PATH   hidraw node to use instead of the first one found";

/// `--device PATH`, matching the CLI's own flag.
///
/// Parsed here rather than handed to GTK: `run_with_args` would treat it as an
/// unknown GTK option and refuse to start.
fn device_arg(args: &[String]) -> Result<Option<PathBuf>, String> {
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--help" | "-h" => return Err(String::new()),
            "--device" => {
                let path = rest.next().ok_or("--device needs a path")?;
                return Ok(Some(PathBuf::from(path)));
            }
            other => {
                if let Some(path) = other.strip_prefix("--device=") {
                    return Ok(Some(PathBuf::from(path)));
                }
                // Anything else is left for GTK's own option handling.
            }
        }
    }
    Ok(None)
}

pub fn main() -> gtk::glib::ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let device_path = match device_arg(&argv) {
        Ok(path) => path,
        Err(message) => {
            if !message.is_empty() {
                eprintln!("error: {message}");
            }
            println!("{USAGE}");
            return gtk::glib::ExitCode::from(i32::from(!message.is_empty()));
        }
    };

    // Wayland compositors match a window to its .desktop file by app_id, which
    // GTK takes from the program name. Without this it is "ninjutso-gui" and
    // Plasma's task manager finds no matching desktop entry, hence no icon.
    gtk::glib::set_prgname(Some(APP_ID));
    gtk::glib::set_application_name("Ninjutso");

    let app = adw::Application::builder().application_id(APP_ID).build();

    app.connect_startup(|_| {
        let provider = gtk::CssProvider::new();
        provider.load_from_data(CSS);
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });

    app.connect_activate(move |app| {
        // GApplication uniqueness: a second launch presents the open window.
        let window = app
            .active_window()
            .and_then(|window| window.downcast::<adw::ApplicationWindow>().ok())
            .unwrap_or_else(|| build_window(app, device_path.clone()));
        window.present();
    });

    app.run_with_args::<&str>(&[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::Lighting;
    use std::path::PathBuf;

    /// A Ten as `status()` reports it. Only the fields a test varies are
    /// interesting; the rest just have to be present.
    fn ten() -> Status {
        Status {
            name: "Ninjutso Inc. Ten".to_string(),
            path: PathBuf::from("/dev/hidraw3"),
            product_id: 0xEB01,
            effective_product_id: 0xE020,
            direct: false,
            profile: 1,
            dpi_stage: 0,
            dpi_stages: vec![1600.0, 3200.0, 6400.0, 12800.0],
            dpi: Some(1600.0),
            polling_rate: 1000,
            lift_off: Some("Medium"),
            motion_sync: true,
            angle_tuning: false,
            system_mode: Some("High Speed"),
            sleep_minutes: Some(2),
            battery: Some(73),
            charging: Some(false),
            is_receiver: true,
            firmware: vec![("mouse", "1.3".to_string())],
            lighting: Some(Lighting {
                mode: "Static",
                color: Some("#36ad6a".to_string()),
                speed: Some(10),
                brightness: None,
            }),
        }
    }

    #[test]
    fn a_sora_v3_gets_a_range_that_can_hold_its_dpi() {
        let mut status = ten();
        status.direct = true;
        status.dpi = Some(36000.0);
        let view = view(&status);
        // 36000 is above the stepped ceiling: a range that stops at 30000
        // would clamp it, and the row would then report a DPI the mouse does
        // not hold as if the user had chosen it.
        assert_eq!(view.dpi_range, (1.0, 45000.0, 1.0));
        assert!(view.dpi.unwrap() <= view.dpi_range.1);
    }

    #[test]
    fn a_ten_is_held_to_multiples_of_fifty() {
        assert_eq!(view(&ten()).dpi_range, (50.0, 30000.0, 50.0));
    }

    #[test]
    fn a_missing_battery_is_shown_as_unknown_not_as_flat() {
        let mut status = ten();
        status.battery = None;
        let view = view(&status);
        assert_eq!(view.battery, "—");
        // No colour class either: red would read as critically low.
        assert_eq!(view.battery_class, None);
    }

    #[test]
    fn battery_colouring_follows_the_level_and_marks_charging() {
        assert_eq!(view(&ten()).battery_class, Some("battery-good"));
        let mut status = ten();
        status.battery = Some(19);
        assert_eq!(view(&status).battery_class, Some("battery-low"));
        status.charging = Some(true);
        assert_eq!(view(&status).battery, "19% ⚡");
        // An unknown charge state is not evidence of charging.
        status.charging = None;
        assert_eq!(view(&status).battery, "19%");
    }

    #[test]
    fn selections_map_to_the_right_toggle() {
        let view = view(&ten());
        assert_eq!(view.rate_index, Some(0));
        assert_eq!(view.lod_index, Some(1));
        assert_eq!(
            view.light.as_ref().and_then(|light| light.mode_index),
            Some(1)
        );
        assert_eq!(
            view.light.and_then(|light| light.color),
            Some("#36ad6a".to_string())
        );
    }

    #[test]
    fn a_wired_mouse_hides_the_receiver_only_controls() {
        let mut status = ten();
        status.is_receiver = false;
        status.lighting = None;
        let view = view(&status);
        assert!(view.light.is_none());
        assert!(view.subtitle.starts_with("Wired"));
    }

    #[test]
    fn firmware_reads_as_a_sentence_and_says_so_when_absent() {
        assert_eq!(view(&ten()).firmware, "Mouse 1.3");
        let mut status = ten();
        status.firmware = vec![("mouse", "1.3".into()), ("receiver", "2.7".into())];
        assert_eq!(view(&status).firmware, "Mouse 1.3 · Receiver 2.7");
        status.firmware = vec![];
        assert!(view(&status).firmware.is_empty());
    }

    #[test]
    fn colours_survive_the_trip_through_gdk() {
        assert_eq!(hex_of(RGBA::new(0.0, 0.0, 0.0, 1.0)), "#000000");
        assert_eq!(hex_of(RGBA::new(1.0, 1.0, 1.0, 1.0)), "#ffffff");
        // What the picker hands back for the Ninjutso green it was set to.
        let green = RGBA::parse("#36ad6a").unwrap();
        assert_eq!(hex_of(green), "#36ad6a");
    }

    #[test]
    fn the_device_flag_matches_the_cli_and_refuses_a_dangling_value() {
        let args = |list: &[&str]| -> Vec<String> {
            list.iter().map(|s| s.to_string()).collect()
        };
        assert_eq!(device_arg(&args(&[])).unwrap(), None);
        assert_eq!(
            device_arg(&args(&["--device", "/dev/hidraw3"])).unwrap(),
            Some(PathBuf::from("/dev/hidraw3"))
        );
        assert_eq!(
            device_arg(&args(&["--device=/dev/hidraw3"])).unwrap(),
            Some(PathBuf::from("/dev/hidraw3"))
        );
        // Used to be silently ignored, and the GUI opened on the wrong mouse.
        assert!(device_arg(&args(&["--device"])).is_err());
        // GTK's own options are still GTK's business.
        assert_eq!(device_arg(&args(&["--gdk-debug=misc"])).unwrap(), None);
    }

    #[test]
    fn a_sleeping_mouse_is_reported_as_asleep_rather_than_as_a_fault() {
        let (title, detail) = describe(&Error::Offline);
        assert_eq!(title, "Mouse is asleep");
        assert!(detail.contains("Refresh"), "tell them how to wake it");
        // A real fault still reads as one.
        assert_eq!(describe(&Error::Timeout(4)).0, "Could not talk to the device");
    }
}
