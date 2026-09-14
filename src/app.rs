//! GTK4 / libadwaita front end for Ninjutso mice.
//!
//! The device is owned by a single worker thread and driven by messages, so the
//! main loop never blocks on a 2.4 GHz round trip and no lock is ever held
//! across one.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::{self, Sender};
use std::thread;

use adw::prelude::*;
use gtk4 as gtk;
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
    ".fw-footer    { font-size: 0.85em; opacity: 0.6; }\n",
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
    SetBrightness(u8),
}

/// What the device thread sends back to the main loop.
enum Reply {
    Status(Box<Status>),
    Failed { title: String, detail: String },
    Toast(String),
}

fn describe(err: &Error) -> (String, String) {
    match err {
        Error::PermissionDenied { .. } => (
            "Permission denied".to_string(),
            format!("{err}\n\n{UDEV_HINT}"),
        ),
        Error::DeviceNotFound => ("No Ninjutso device".to_string(), err.to_string()),
        _ => (
            "Could not talk to the device".to_string(),
            err.to_string(),
        ),
    }
}

/// Own the device on one thread and serialise every request onto it.
fn spawn_worker(commands: mpsc::Receiver<Command>, replies: async_channel::Sender<Reply>) {
    thread::spawn(move || {
        let mut held: Option<Device> = None;
        while let Ok(command) = commands.recv() {
            // Re-open lazily so unplugging and replugging recovers on Refresh.
            if held.is_none() {
                match Device::open() {
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
                Command::SetDpi(value) => confirm("DPI", device.set_dpi(value, None).map(|v| v as i64)),
                Command::SetRate(value) => confirm(
                    "Report rate",
                    device.set_polling_rate(value).map(|v| format!("{v} Hz")),
                ),
                Command::SetLod(value) => confirm("Lift-off", device.set_lift_off(&value)),
                Command::SetMotion(value) => confirm(
                    "Motion Sync",
                    device
                        .set_motion_sync(value)
                        .map(|on| if on { "on" } else { "off" }),
                ),
                Command::SetLightMode(mode) => {
                    confirm_optional("Light mode", device.set_light_mode(&mode))
                }
                Command::SetBrightness(value) => {
                    confirm_optional("Brightness", device.set_brightness(value))
                }
            };
            if replies.send_blocking(reply).is_err() {
                return; // window closed
            }
        }
    });
}

fn confirm<T: std::fmt::Display>(label: &str, result: Result<T, Error>) -> Reply {
    match result {
        Ok(value) => Reply::Toast(format!("{label} → {value}")),
        Err(err) => Reply::Toast(format!("{label} failed: {err}")),
    }
}

fn confirm_optional<T: std::fmt::Display>(label: &str, result: Result<Option<T>, Error>) -> Reply {
    match result {
        Ok(Some(value)) => Reply::Toast(format!("{label} → {value}")),
        Ok(None) => Reply::Toast(format!("{label}: device did not confirm")),
        Err(err) => Reply::Toast(format!("{label} failed: {err}")),
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
    bright_row: adw::SpinRow,
    fw_row: adw::ActionRow,
    /// Suppresses handlers while repopulating from a fresh status read.
    loading: Cell<bool>,
    commands: RefCell<Sender<Command>>,
}

impl Ui {
    fn send(&self, command: Command) {
        if self.loading.get() {
            return;
        }
        let _ = self.commands.borrow().send(command);
    }

    fn refresh(&self) {
        self.stack.set_visible_child_name("loading");
        let _ = self.commands.borrow().send(Command::Refresh);
    }

    fn show_error(&self, title: &str, detail: &str) {
        self.error_page.set_title(title);
        self.error_page.set_description(Some(detail));
        self.stack.set_visible_child_name("error");
    }

    fn toast(&self, text: &str) {
        self.toasts
            .add_toast(adw::Toast::builder().title(text).timeout(2).build());
    }

    fn apply(&self, status: &Status) {
        self.loading.set(true);

        let name = status.name.replace("Ninjutso Inc. ", "");
        self.title.set_title(&name);
        let link = if status.is_receiver { "Receiver" } else { "Wired" };
        self.title
            .set_subtitle(&format!("{link} · profile {}", status.profile));
        self.conn_row.set_title("Connected");
        self.conn_row
            .set_subtitle(&format!("{link} · {}", status.path.display()));
        self.dot.set_visible(true);

        let charging = status.charging.unwrap_or(false);
        self.battery.set_label(&format!(
            "{}%{}",
            status.battery,
            if charging { " ⚡" } else { "" }
        ));
        self.battery.remove_css_class("battery-low");
        self.battery.remove_css_class("battery-good");
        self.battery.add_css_class(if status.battery < 20 {
            "battery-low"
        } else {
            "battery-good"
        });

        if let Some(dpi) = status.dpi {
            self.dpi_row.set_value(dpi);
            self.dpi_row.set_subtitle(&format!(
                "Stage {} of {}",
                status.dpi_stage + 1,
                status.dpi_stages.len()
            ));
        }
        // A Sora V3 takes 1-DPI steps; the TEN only multiples of 50.
        let step = if status.direct { 1.0 } else { 50.0 };
        self.dpi_row.adjustment().set_step_increment(step);

        if let Some(index) = p::POLLING_RATES.iter().position(|&r| r == status.polling_rate) {
            self.rate_toggles[index].set_active(true);
        }
        if let Some(index) = status
            .lift_off
            .and_then(|value| p::LOD_VALUES.iter().position(|&v| v == value))
        {
            self.lod_toggles[index].set_active(true);
        }
        self.motion_row.set_active(status.motion_sync);

        self.light_group.set_visible(status.lighting.is_some());
        if let Some(light) = &status.lighting {
            if let Some(index) = p::LIGHT_MODES.iter().position(|&m| m == light.mode) {
                self.mode_toggles[index].set_active(true);
            }
            self.bright_row.set_visible(light.brightness.is_some());
            if let Some(brightness) = light.brightness {
                self.bright_row.set_value(f64::from(brightness));
            }
        }

        let firmware = status
            .firmware
            .iter()
            .map(|(part, version)| {
                format!("{}{} {version}", part[..1].to_uppercase(), &part[1..])
            })
            .collect::<Vec<_>>()
            .join(" · ");
        self.fw_row.set_subtitle(if firmware.is_empty() {
            "unavailable"
        } else {
            &firmware
        });

        self.loading.set(false);
        self.stack.set_visible_child_name("device");
    }
}

/// A row of linked toggle buttons acting as one exclusive choice.
fn toggle_row(
    title: &str,
    labels: &[String],
    ui: &Rc<RefCell<Option<Rc<Ui>>>>,
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
            if let Some(ui) = ui.borrow().as_ref() {
                ui.send(make(index));
            }
        });
        holder.append(&button);
        toggles.push(button);
    }
    row.add_suffix(&holder);
    (row, toggles)
}

fn build_window(app: &adw::Application) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Ninjutso")
        .default_width(480)
        .default_height(720)
        .build();

    let (commands, command_rx) = mpsc::channel();
    let (reply_tx, reply_rx) = async_channel::unbounded();
    spawn_worker(command_rx, reply_tx);

    // The toggle handlers need the Ui, which does not exist until they are
    // built; this cell is filled in the moment it does.
    let late: Rc<RefCell<Option<Rc<Ui>>>> = Rc::new(RefCell::new(None));

    let toasts = adw::ToastOverlay::new();
    let stack = gtk::Stack::new();

    // Loading ---------------------------------------------------------------
    let spinner_page = adw::StatusPage::builder()
        .title("Looking for your mouse…")
        .build();
    let spinner = gtk::Spinner::new();
    spinner.start();
    spinner_page.set_child(Some(&spinner));

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
        bright_row: bright_row.clone(),
        fw_row,
        loading: Cell::new(true),
        commands: RefCell::new(commands),
    });
    *late.borrow_mut() = Some(Rc::clone(&ui));

    // Handlers that need the finished Ui ------------------------------------
    let handler_ui = Rc::clone(&ui);
    dpi_row.connect_value_notify(move |row| {
        handler_ui.send(Command::SetDpi(row.value() as u32));
    });
    let handler_ui = Rc::clone(&ui);
    bright_row.connect_value_notify(move |row| {
        handler_ui.send(Command::SetBrightness(row.value() as u8));
    });
    let handler_ui = Rc::clone(&ui);
    motion_row.connect_active_notify(move |row| {
        handler_ui.send(Command::SetMotion(row.is_active()));
    });
    let handler_ui = Rc::clone(&ui);
    refresh.connect_clicked(move |_| handler_ui.refresh());

    // Drain the worker's replies on the main loop ---------------------------
    let loop_ui = Rc::clone(&ui);
    gtk::glib::spawn_future_local(async move {
        while let Ok(reply) = reply_rx.recv().await {
            match reply {
                Reply::Status(status) => loop_ui.apply(&status),
                Reply::Failed { title, detail } => loop_ui.show_error(&title, &detail),
                Reply::Toast(text) => loop_ui.toast(&text),
            }
        }
    });

    ui.refresh();
    window
}

pub fn main() -> gtk::glib::ExitCode {
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

    app.connect_activate(|app| {
        // GApplication uniqueness: a second launch presents the open window.
        let window = app
            .active_window()
            .map(|window| window.downcast::<adw::ApplicationWindow>().unwrap())
            .unwrap_or_else(|| build_window(app));
        window.present();
    });

    app.run_with_args::<&str>(&[])
}
