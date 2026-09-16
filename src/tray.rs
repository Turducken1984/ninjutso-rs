//! StatusNotifierItem tray icon, spoken directly over D-Bus.
//!
//! libayatana-appindicator is GTK3-only and cannot share a process with GTK4, so
//! this talks the SNI + dbusmenu protocols itself. Plasma, Waybar, swaybar and
//! GNOME's AppIndicator extension all host these.
//!
//! The caller drives the reading; this module only serves the item and keeps it
//! registered. Registration is re-sent whenever the host restarts, which is the
//! only way a tray survives a plasmashell crash.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use zbus::blocking::{connection, Connection};
use zbus::interface;
use zbus::zvariant::{ObjectPath, OwnedValue, Structure, Value};

pub const SNI_IFACE: &str = "org.kde.StatusNotifierItem";
pub const WATCHER: &str = "org.kde.StatusNotifierWatcher";
pub const ITEM_PATH: &str = "/StatusNotifierItem";
pub const MENU_PATH: &str = "/MenuBar";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";

/// Bound every outgoing call. zbus defaults to waiting forever, and the caller
/// drives this from one thread: a notification daemon that accepts a call and
/// never answers would otherwise wedge polling and menu handling with it.
const METHOD_TIMEOUT: Duration = Duration::from_secs(5);

/// An ARGB32 icon: width, height, pixels in network byte order.
pub type Pixmap = (i32, i32, Vec<u8>);

/// What a click on the tray asks the application to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    OpenGui,
    Refresh,
    Quit,
}

/// One menu row. `label: None` renders as a separator.
#[derive(Debug, Clone)]
pub struct MenuEntry {
    pub id: i32,
    pub label: Option<String>,
    pub action: Option<Action>,
}

impl MenuEntry {
    pub fn item(id: i32, label: &str, action: Action) -> Self {
        Self {
            id,
            label: Some(label.to_string()),
            action: Some(action),
        }
    }

    pub fn separator(id: i32) -> Self {
        Self {
            id,
            label: None,
            action: None,
        }
    }
}

/// Icon and tooltip, shared between the caller and the served interfaces.
#[derive(Debug, Default)]
struct IconState {
    pixmaps: Vec<Pixmap>,
    tooltip: String,
}

fn owned(value: impl Into<Value<'static>>) -> OwnedValue {
    // Every value we build here is a str or a bool, neither of which can fail
    // to convert -- OwnedValue only rejects values holding a file descriptor.
    OwnedValue::try_from(value.into()).expect("no fds in menu properties")
}

fn item_props(label: Option<&str>) -> HashMap<String, OwnedValue> {
    let mut props = HashMap::new();
    match label {
        None => {
            props.insert("type".to_string(), owned("separator"));
            props.insert("visible".to_string(), owned(true));
        }
        Some(label) => {
            props.insert("label".to_string(), owned(label.to_string()));
            props.insert("enabled".to_string(), owned(true));
            props.insert("visible".to_string(), owned(true));
        }
    }
    props
}

/// A dbusmenu node: id, properties, children.
type MenuNode = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

fn node(id: i32, props: HashMap<String, OwnedValue>, children: Vec<OwnedValue>) -> MenuNode {
    (id, props, children)
}

struct SniItem {
    app_id: String,
    icon_name: String,
    state: Arc<Mutex<IconState>>,
    actions: Sender<Action>,
}

#[interface(name = "org.kde.StatusNotifierItem")]
impl SniItem {
    fn activate(&self, _x: i32, _y: i32) {
        let _ = self.actions.send(Action::OpenGui);
    }

    fn secondary_activate(&self, _x: i32, _y: i32) {
        let _ = self.actions.send(Action::OpenGui);
    }

    fn context_menu(&self, _x: i32, _y: i32) {}

    fn scroll(&self, _delta: i32, _orientation: String) {}

    #[zbus(property)]
    fn category(&self) -> String {
        "Hardware".to_string()
    }

    #[zbus(property)]
    fn id(&self) -> String {
        self.app_id.clone()
    }

    #[zbus(property)]
    fn title(&self) -> String {
        "Ninjutso".to_string()
    }

    #[zbus(property)]
    fn status(&self) -> String {
        "Active".to_string()
    }

    /// Empty while we have pixmaps: a themed name would otherwise win.
    #[zbus(property)]
    fn icon_name(&self) -> String {
        let state = self.state.lock().unwrap();
        if state.pixmaps.is_empty() {
            self.icon_name.clone()
        } else {
            String::new()
        }
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<Pixmap> {
        self.state.lock().unwrap().pixmaps.clone()
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn menu(&self) -> ObjectPath<'_> {
        ObjectPath::from_static_str(MENU_PATH).expect("MENU_PATH is a valid object path")
    }

    #[zbus(property)]
    fn tool_tip(&self) -> (String, Vec<Pixmap>, String, String) {
        let tooltip = self.state.lock().unwrap().tooltip.clone();
        (
            self.icon_name.clone(),
            Vec::new(),
            "Ninjutso".to_string(),
            tooltip,
        )
    }
}

struct DbusMenu {
    items: Vec<MenuEntry>,
    revision: u32,
    actions: Sender<Action>,
}

impl DbusMenu {
    fn children(&self) -> Vec<OwnedValue> {
        self.items
            .iter()
            .map(|entry| {
                let child: Structure = node(
                    entry.id,
                    item_props(entry.label.as_deref()),
                    Vec::new(),
                )
                .into();
                owned(child)
            })
            .collect()
    }
}

#[interface(name = "com.canonical.dbusmenu")]
impl DbusMenu {
    fn get_layout(
        &self,
        _parent_id: i32,
        _recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> (u32, MenuNode) {
        let mut root_props = HashMap::new();
        root_props.insert("children-display".to_string(), owned("submenu"));
        (self.revision, node(0, root_props, self.children()))
    }

    fn get_group_properties(
        &self,
        _ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> Vec<(i32, HashMap<String, OwnedValue>)> {
        self.items
            .iter()
            .map(|entry| (entry.id, item_props(entry.label.as_deref())))
            .collect()
    }

    fn get_property(&self, id: i32, name: String) -> OwnedValue {
        self.items
            .iter()
            .find(|entry| entry.id == id)
            .and_then(|entry| item_props(entry.label.as_deref()).remove(&name))
            .unwrap_or_else(|| owned(""))
    }

    fn event(&self, id: i32, event_id: String, _data: Value<'_>, _timestamp: u32) {
        if event_id != "clicked" {
            return;
        }
        if let Some(action) = self
            .items
            .iter()
            .find(|entry| entry.id == id)
            .and_then(|entry| entry.action)
        {
            let _ = self.actions.send(action);
        }
    }

    fn about_to_show(&self, _id: i32) -> bool {
        false
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property)]
    fn status(&self) -> String {
        "normal".to_string()
    }

    #[zbus(property)]
    fn text_direction(&self) -> String {
        "ltr".to_string()
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Tell the watcher we exist. Cheap and idempotent, so re-sending it costs
/// nothing and is the whole of the recovery mechanism.
fn register_item(conn: &Connection) -> zbus::Result<()> {
    let unique = conn
        .inner()
        .unique_name()
        .map(|name| name.to_string())
        .unwrap_or_default();
    conn.call_method(
        Some(WATCHER),
        WATCHER_PATH,
        Some(WATCHER),
        "RegisterStatusNotifierItem",
        &(unique,),
    )?;
    Ok(())
}

/// A registered tray item. Dropping it unregisters by closing the connection.
pub struct Tray {
    conn: Connection,
    state: Arc<Mutex<IconState>>,
}

impl Tray {
    /// Serve the SNI + dbusmenu objects and register with the host.
    ///
    /// Takes `<app_id>.Tray` on the bus, so a second instance fails with
    /// [`zbus::Error::NameTaken`] rather than putting a duplicate icon in the
    /// tray. A missing StatusNotifier host is *not* an error: we register
    /// anyway and re-register when one appears, which covers both the race
    /// against the panel at login and the panel restarting later.
    pub fn register(
        app_id: &str,
        icon_name: &str,
        items: Vec<MenuEntry>,
        actions: Sender<Action>,
    ) -> zbus::Result<Self> {
        let state = Arc::new(Mutex::new(IconState::default()));
        let item = SniItem {
            app_id: app_id.to_string(),
            icon_name: icon_name.to_string(),
            state: Arc::clone(&state),
            actions: actions.clone(),
        };
        let menu = DbusMenu {
            items,
            revision: 1,
            actions,
        };

        let conn = connection::Builder::session()?
            .method_timeout(METHOD_TIMEOUT)
            // DoNotQueue without ReplaceExisting: a second instance is told the
            // name is taken instead of stealing it from the running one.
            .replace_existing_names(false)
            .allow_name_replacements(false)
            .name(format!("{app_id}.Tray"))?
            .serve_at(ITEM_PATH, item)?
            .serve_at(MENU_PATH, menu)?
            .build()?;

        let tray = Self { conn, state };
        // Not fatal: at login we often win the race against the panel.
        let _ = tray.register_item();
        tray.watch_for_host();
        Ok(tray)
    }

    fn register_item(&self) -> zbus::Result<()> {
        register_item(&self.conn)
    }

    /// Re-register whenever the StatusNotifierWatcher name gains an owner.
    ///
    /// A panel restart wipes the watcher's item list, and nothing tells us:
    /// without this the process keeps running, keeps polling and keeps emitting
    /// signals into the void, with no icon and no way for the user to know.
    fn watch_for_host(&self) {
        let conn = self.conn.clone();
        thread::spawn(move || {
            let Ok(dbus) = zbus::blocking::fdo::DBusProxy::new(&conn) else {
                return;
            };
            let Ok(changes) = dbus.receive_name_owner_changed_with_args(&[(0, WATCHER)]) else {
                return;
            };
            for change in changes {
                let Ok(args) = change.args() else { continue };
                // An empty new owner is the watcher going away; wait for the
                // one that brings it back.
                if args.new_owner().is_none() {
                    continue;
                }
                // The new host has no icon for us yet; this hands it one.
                if register_item(&conn).is_ok() {
                    let _ = conn.emit_signal(None::<&str>, ITEM_PATH, SNI_IFACE, "NewIcon", &());
                }
            }
        });
    }

    fn emit(&self, signal: &str) {
        let _ = self
            .conn
            .emit_signal(None::<&str>, ITEM_PATH, SNI_IFACE, signal, &());
    }

    pub fn set_tooltip(&self, text: &str) {
        self.state.lock().unwrap().tooltip = text.to_string();
        self.emit("NewToolTip");
    }

    /// Replace the icon with generated ARGB32 bitmaps and tell the host.
    pub fn set_pixmaps(&self, pixmaps: Vec<Pixmap>) {
        self.state.lock().unwrap().pixmaps = pixmaps;
        self.emit("NewIcon");
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separators_and_items_carry_different_properties() {
        let separator = item_props(None);
        assert_eq!(separator["type"], owned("separator"));
        assert!(!separator.contains_key("label"));

        let item = item_props(Some("Quit"));
        assert_eq!(item["label"], owned("Quit".to_string()));
        assert_eq!(item["enabled"], owned(true));
    }
}
