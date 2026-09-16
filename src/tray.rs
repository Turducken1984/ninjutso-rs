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

/// Keep only the properties the caller asked for; an empty list means all of
/// them, which is what the dbusmenu spec says and what Plasma relies on.
fn selected(
    mut props: HashMap<String, OwnedValue>,
    wanted: &[String],
) -> HashMap<String, OwnedValue> {
    if !wanted.is_empty() {
        props.retain(|name, _| wanted.contains(name));
    }
    props
}

/// The root node's own properties. It is a menu, not an item, so it carries
/// only the hint that it has children.
fn root_props() -> HashMap<String, OwnedValue> {
    let mut props = HashMap::new();
    props.insert("children-display".to_string(), owned("submenu"));
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
    fn children(&self, property_names: &[String]) -> Vec<OwnedValue> {
        self.items
            .iter()
            .map(|entry| {
                let props = selected(item_props(entry.label.as_deref()), property_names);
                let child: Structure = node(entry.id, props, Vec::new()).into();
                owned(child)
            })
            .collect()
    }
}

#[interface(name = "com.canonical.dbusmenu")]
impl DbusMenu {
    /// The menu is flat: the root has every item as a child and no item has
    /// children of its own. `recursion_depth` of 0 means properties only.
    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, MenuNode)> {
        if parent_id == 0 {
            let children = if recursion_depth == 0 {
                Vec::new()
            } else {
                self.children(&property_names)
            };
            return Ok((self.revision, node(0, root_props(), children)));
        }
        let entry = self
            .items
            .iter()
            .find(|entry| entry.id == parent_id)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("no menu item {parent_id}")))?;
        let props = selected(item_props(entry.label.as_deref()), &property_names);
        Ok((self.revision, node(entry.id, props, Vec::new())))
    }

    /// An empty `ids` means every node -- including the root, which a host may
    /// ask about by id 0.
    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        property_names: Vec<String>,
    ) -> Vec<(i32, HashMap<String, OwnedValue>)> {
        let wanted = |id: i32| ids.is_empty() || ids.contains(&id);
        let mut out = Vec::with_capacity(self.items.len() + 1);
        if wanted(0) {
            out.push((0, selected(root_props(), &property_names)));
        }
        out.extend(
            self.items
                .iter()
                .filter(|entry| wanted(entry.id))
                .map(|entry| {
                    (
                        entry.id,
                        selected(item_props(entry.label.as_deref()), &property_names),
                    )
                }),
        );
        out
    }

    fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedValue> {
        let props = if id == 0 {
            root_props()
        } else {
            let entry = self
                .items
                .iter()
                .find(|entry| entry.id == id)
                .ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("no menu item {id}")))?;
            item_props(entry.label.as_deref())
        };
        props
            .into_iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("no property {name}")))
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

/// Ask for a poll as soon as the machine wakes from suspend.
///
/// The caller's poll timer runs on `CLOCK_MONOTONIC`, which does not advance
/// while suspended: after a nine-hour laptop sleep the tray would otherwise go
/// on showing the pre-suspend battery level for up to another full interval.
/// logind announces the resume on the system bus, and a poll is one round trip.
///
/// Silently does nothing where there is no logind or no system bus -- this is a
/// refinement on the timer, never the only thing driving it.
pub fn wake_on_resume(actions: Sender<Action>) {
    thread::spawn(move || {
        let Ok(system) = Connection::system() else {
            return;
        };
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.freedesktop.login1.Manager")
            .and_then(|builder| builder.member("PrepareForSleep"));
        let Ok(rule) = rule else { return };
        let Ok(signals) =
            zbus::blocking::MessageIterator::for_match_rule(rule.build(), &system, None)
        else {
            return;
        };
        for message in signals.flatten() {
            // True is "about to suspend", false is "just resumed".
            if message.body().deserialize::<bool>() == Ok(false)
                && actions.send(Action::Refresh).is_err()
            {
                return; // the tray has quit
            }
        }
    });
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

    /// Signalling failure is how a session bus that has gone away presents;
    /// the caller counts these rather than living on invisibly.
    fn emit(&self, signal: &str) -> zbus::Result<()> {
        self.conn
            .emit_signal(None::<&str>, ITEM_PATH, SNI_IFACE, signal, &())
    }

    pub fn set_tooltip(&self, text: &str) -> zbus::Result<()> {
        {
            let mut state = self.state.lock().unwrap();
            if state.tooltip == text {
                return Ok(()); // nothing to tell the host
            }
            state.tooltip = text.to_string();
        }
        self.emit("NewToolTip")
    }

    /// Replace the icon with generated ARGB32 bitmaps and tell the host.
    pub fn set_pixmaps(&self, pixmaps: Vec<Pixmap>) -> zbus::Result<()> {
        self.state.lock().unwrap().pixmaps = pixmaps;
        self.emit("NewIcon")
    }

    /// Withdraw a notification that no longer describes the situation.
    pub fn close_notification(&self, id: u32) -> zbus::Result<()> {
        self.conn.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "CloseNotification",
            &(id,),
        )?;
        Ok(())
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu() -> DbusMenu {
        let (actions, _rx) = std::sync::mpsc::channel();
        DbusMenu {
            items: vec![
                MenuEntry::item(1, "Open Ninjutso", Action::OpenGui),
                MenuEntry::separator(2),
                MenuEntry::item(3, "Quit", Action::Quit),
            ],
            revision: 1,
            actions,
        }
    }

    #[test]
    fn the_root_layout_carries_every_item() {
        let (_revision, (id, props, children)) = menu().get_layout(0, -1, Vec::new()).unwrap();
        assert_eq!(id, 0);
        assert_eq!(props["children-display"], owned("submenu"));
        assert_eq!(children.len(), 3);
    }

    #[test]
    fn a_layout_request_is_answered_for_what_was_asked_for() {
        let menu = menu();
        // Depth 0 is "this node's properties, no children".
        let (_, (_, _, children)) = menu.get_layout(0, 0, Vec::new()).unwrap();
        assert!(children.is_empty());

        // A subtree request gets that item, not the root relabelled.
        let (_, (id, props, children)) = menu.get_layout(3, -1, Vec::new()).unwrap();
        assert_eq!(id, 3);
        assert_eq!(props["label"], owned("Quit".to_string()));
        assert!(children.is_empty());

        assert!(menu.get_layout(99, -1, Vec::new()).is_err());
    }

    #[test]
    fn property_requests_are_filtered_as_asked() {
        let menu = menu();
        let wanted = vec!["label".to_string()];
        let (_, (_, _, children)) = menu.get_layout(0, -1, wanted.clone()).unwrap();
        assert_eq!(children.len(), 3);

        let groups = menu.get_group_properties(vec![1], wanted);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, 1);
        assert_eq!(groups[0].1.len(), 1, "only the property that was asked for");

        // No ids means everything, including the root.
        let all = menu.get_group_properties(Vec::new(), Vec::new());
        assert_eq!(all.len(), 4);
        assert_eq!(all[0].0, 0);
    }

    #[test]
    fn an_unknown_property_is_an_error_not_an_empty_string() {
        let menu = menu();
        assert_eq!(menu.get_property(1, "label".into()).unwrap(), owned("Open Ninjutso".to_string()));
        assert!(menu.get_property(1, "nonesuch".into()).is_err());
        assert!(menu.get_property(99, "label".into()).is_err());
    }

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
