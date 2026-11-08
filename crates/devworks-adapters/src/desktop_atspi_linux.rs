//! Session AT-SPI tree and actions; works on X11 and Wayland when the desktop exposes a bus.
use atspi::{Action, Interface, InterfaceSet, ObjectRefOwned, State, StateSet};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::desktop::{AccessibilityNode, AccessibilitySnapshot, DesktopFault};
use devworks_core::protocol::records::DesktopTargetAction;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use zbus::blocking::{Connection, Proxy, connection::Builder};

const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const REGISTRY: &str = "org.a11y.atspi.Registry";
const ROOT: &str = "/org/a11y/atspi/accessible/root";

fn connect() -> Result<Connection, DesktopFault> {
    let session = Builder::session()
        .map_err(|_| DesktopFault::Unsupported)?
        .method_timeout(Duration::from_millis(500))
        .build()
        .map_err(|_| DesktopFault::Unsupported)?;
    let bus = Proxy::new(&session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus")
        .map_err(|_| DesktopFault::Unsupported)?;
    let address: String = bus
        .call("GetAddress", &())
        .map_err(|_| DesktopFault::Unsupported)?;
    Builder::address(address.as_str())
        .map_err(|_| DesktopFault::Unsupported)?
        .method_timeout(Duration::from_millis(250))
        .build()
        .map_err(|_| DesktopFault::PermissionRequired)
}

pub fn available() -> bool {
    connect().is_ok()
}

fn proxy<'a>(
    connection: &'a Connection,
    reference: &'a ObjectRefOwned,
    interface: &'static str,
) -> Result<Proxy<'a>, DesktopFault> {
    let name = reference.name_as_str().ok_or(DesktopFault::NativeFailure)?;
    Proxy::new(connection, name, reference.path_as_str(), interface)
        .map_err(|_| DesktopFault::NativeFailure)
}

fn normalized(value: String, fallback: &str, max: usize) -> String {
    let value = if value.is_empty() { fallback } else { &value };
    value.chars().filter(|c| *c != '\0').take(max).collect()
}

fn target_id(reference: &ObjectRefOwned) -> String {
    sha256_hex(
        format!(
            "{}:{}",
            reference.name_as_str().unwrap_or(""),
            reference.path_as_str()
        )
        .as_bytes(),
    )
}

fn observe(
    connection: &Connection,
) -> Result<(AccessibilitySnapshot, HashMap<String, ObjectRefOwned>), DesktopFault> {
    let registry = Proxy::new(connection, REGISTRY, ROOT, ACCESSIBLE)
        .map_err(|_| DesktopFault::Unsupported)?;
    let apps: Vec<ObjectRefOwned> = registry
        .call("GetChildren", &())
        .map_err(|_| DesktopFault::Unsupported)?;
    let dbus = Proxy::new(
        connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .map_err(|_| DesktopFault::NativeFailure)?;
    let mut queue = VecDeque::new();
    let mut unidentified_apps = 0;
    for app in apps {
        let Some(name) = app.name_as_str() else {
            continue;
        };
        let pid: u32 = match dbus.call("GetConnectionUnixProcessID", &(name,)) {
            Ok(pid) => pid,
            Err(_) => {
                unidentified_apps += 1; // Missing ownership proof must never expose our own confirmation UI.
                continue;
            }
        };
        if pid != std::process::id() {
            queue.push_back((app, None, 0_u8));
        }
    }
    if queue.is_empty() && unidentified_apps > 0 {
        return Err(DesktopFault::PermissionRequired);
    }
    let started = Instant::now();
    let mut nodes = Vec::new();
    let mut handles = HashMap::new();
    let mut truncated = false;
    while let Some((reference, parent_id, depth)) = queue.pop_front() {
        if nodes.len() >= 500 || started.elapsed() >= Duration::from_secs(3) {
            truncated = true;
            break;
        }
        let accessible = match proxy(connection, &reference, ACCESSIBLE) {
            Ok(proxy) => proxy,
            Err(_) => {
                truncated = true;
                continue;
            }
        };
        let id = target_id(&reference);
        let role: String = accessible
            .call("GetRoleName", &())
            .unwrap_or_else(|_| "unknown".into());
        let name: String = accessible.get_property("Name").unwrap_or_default();
        let description: Option<String> = accessible
            .get_property::<String>("Description")
            .ok()
            .filter(|value| !value.is_empty())
            .map(|value| normalized(value, "", 4096));
        let state: StateSet = accessible
            .call("GetState", &())
            .unwrap_or_else(|_| StateSet::empty());
        let interfaces: InterfaceSet = accessible
            .call("GetInterfaces", &())
            .unwrap_or_else(|_| InterfaceSet::empty());
        let mut actions = Vec::new();
        if interfaces.contains(Interface::Action)
            && let Ok(action) = proxy(connection, &reference, "org.a11y.atspi.Action")
        {
            let listed: Vec<Action> = action.call("GetActions", &()).unwrap_or_default();
            actions.extend(
                listed
                    .into_iter()
                    .map(|action| normalized(action.name, "unknown", 256)),
            );
        }
        if interfaces.contains(Interface::EditableText) || interfaces.contains(Interface::Value) {
            actions.push("setValue".into());
        }
        if interfaces.contains(Interface::Component) {
            actions.push("focus".into());
        }
        actions.truncate(128);
        if depth < 16 {
            match accessible.call::<_, _, Vec<ObjectRefOwned>>("GetChildren", &()) {
                Ok(children) => {
                    if children.len() > 64 {
                        truncated = true;
                    }
                    for child in children.into_iter().take(64) {
                        queue.push_back((child, Some(id.clone()), depth + 1));
                    }
                }
                Err(_) => truncated = true,
            }
        } else {
            truncated = true;
        }
        nodes.push(AccessibilityNode {
            target_id: id.clone(),
            parent_id,
            role: normalized(role, "unknown", 256),
            name: normalized(name, "(unnamed)", 256),
            description,
            enabled: state.contains(State::Enabled),
            actions,
        });
        handles.insert(id, reference.clone());
    }
    Ok((
        AccessibilitySnapshot {
            platform: "linux-atspi".into(),
            nodes,
            truncated,
        },
        handles,
    ))
}

pub fn snapshot() -> Result<AccessibilitySnapshot, DesktopFault> {
    let connection = connect()?;
    observe(&connection).map(|(snapshot, _)| snapshot)
}

pub fn inject_target(
    target_id: &str,
    revision: &str,
    action: &DesktopTargetAction,
) -> Result<(), DesktopFault> {
    let connection = connect()?;
    let (snapshot, handles) = observe(&connection)?;
    if snapshot.revision() != revision {
        return Err(DesktopFault::StaleTarget);
    }
    let reference = handles.get(target_id).ok_or(DesktopFault::StaleTarget)?;
    let node = snapshot
        .nodes
        .iter()
        .find(|node| node.target_id == target_id)
        .ok_or(DesktopFault::StaleTarget)?;
    if !node.enabled {
        return Err(DesktopFault::Unsupported);
    }
    match action {
        DesktopTargetAction::Invoke { action } => {
            let actions = proxy(&connection, reference, "org.a11y.atspi.Action")?;
            let listed: Vec<Action> = actions
                .call("GetActions", &())
                .map_err(|_| DesktopFault::NativeFailure)?;
            let index = listed
                .iter()
                .position(|listed| listed.name == action.0)
                .ok_or(DesktopFault::Unsupported)?;
            let applied: bool = actions
                .call("DoAction", &(index as i32,))
                .map_err(|_| DesktopFault::NativeFailure)?;
            if applied {
                Ok(())
            } else {
                Err(DesktopFault::NativeFailure)
            }
        }
        DesktopTargetAction::SetValue { value } => {
            if !node.actions.iter().any(|listed| listed == "setValue") {
                return Err(DesktopFault::Unsupported);
            }
            let accessible = proxy(&connection, reference, ACCESSIBLE)?;
            let interfaces: InterfaceSet = accessible
                .call("GetInterfaces", &())
                .map_err(|_| DesktopFault::NativeFailure)?;
            if interfaces.contains(Interface::EditableText) {
                let editable = proxy(&connection, reference, "org.a11y.atspi.EditableText")?;
                let applied: bool = editable
                    .call("SetTextContents", &(value.0.as_str(),))
                    .map_err(|_| DesktopFault::NativeFailure)?;
                return if applied {
                    Ok(())
                } else {
                    Err(DesktopFault::NativeFailure)
                };
            }
            if !interfaces.contains(Interface::Value) {
                return Err(DesktopFault::Unsupported);
            }
            let numeric: f64 = value.0.parse().map_err(|_| DesktopFault::Unsupported)?;
            let numeric_value = proxy(&connection, reference, "org.a11y.atspi.Value")?;
            numeric_value
                .set_property("CurrentValue", numeric)
                .map_err(|_| DesktopFault::NativeFailure)
        }
        DesktopTargetAction::Focus {} => {
            if !node.actions.iter().any(|listed| listed == "focus") {
                return Err(DesktopFault::Unsupported);
            }
            let component = proxy(&connection, reference, "org.a11y.atspi.Component")?;
            let applied: bool = component
                .call("GrabFocus", &())
                .map_err(|_| DesktopFault::NativeFailure)?;
            if applied {
                Ok(())
            } else {
                Err(DesktopFault::NativeFailure)
            }
        }
    }
}
