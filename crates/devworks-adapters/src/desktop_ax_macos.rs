//! Focused-application AX tree. Every target action re-reads the bounded tree and its digest.
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::string::{CFString, CFStringRef};
use devworks_core::ports::desktop::{AccessibilityNode, AccessibilitySnapshot, DesktopFault};
use devworks_core::protocol::records::DesktopTargetAction;
use objc2_app_kit::NSWorkspace;
use std::collections::{HashMap, VecDeque};
use std::os::raw::c_void;
use std::ptr;
use std::time::{Duration, Instant};

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateSystemWide() -> CFTypeRef;
    fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
    fn AXUIElementCopyAttributeValue(
        element: CFTypeRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementCopyActionNames(element: CFTypeRef, names: *mut *const c_void) -> i32;
    fn AXUIElementIsAttributeSettable(
        element: CFTypeRef,
        attribute: CFStringRef,
        settable: *mut u8,
    ) -> i32;
    fn AXUIElementPerformAction(element: CFTypeRef, action: CFStringRef) -> i32;
    fn AXUIElementSetAttributeValue(
        element: CFTypeRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> i32;
    fn AXUIElementGetPid(element: CFTypeRef, pid: *mut i32) -> i32;
    fn AXUIElementSetMessagingTimeout(element: CFTypeRef, timeout: f32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
    fn CFArrayGetTypeID() -> usize;
    fn CFGetTypeID(value: CFTypeRef) -> usize;
}

fn attribute(element: &CFType, key: &str) -> Option<CFType> {
    let key = CFString::new(key);
    let mut value = ptr::null();
    // SAFETY: AX owns the returned CF object on success; CFType takes the create-rule reference.
    let status = unsafe {
        AXUIElementCopyAttributeValue(
            element.as_CFTypeRef(),
            key.as_concrete_TypeRef(),
            &mut value,
        )
    };
    (status == 0 && !value.is_null()).then(|| unsafe { CFType::wrap_under_create_rule(value) })
}

fn string(value: &CFType) -> Option<String> {
    value.downcast::<CFString>().map(|value| value.to_string())
}

fn string_attribute(element: &CFType, key: &str) -> Option<String> {
    attribute(element, key).and_then(|value| string(&value))
}

fn bool_attribute(element: &CFType, key: &str) -> Option<bool> {
    attribute(element, key).and_then(|value| value.downcast::<CFBoolean>().map(bool::from))
}

fn settable(element: &CFType, key: &str) -> bool {
    let key = CFString::new(key);
    let mut settable = 0_u8;
    // SAFETY: the out pointer is valid for one Boolean and the element/key are live CF objects.
    unsafe {
        AXUIElementIsAttributeSettable(
            element.as_CFTypeRef(),
            key.as_concrete_TypeRef(),
            &mut settable,
        ) == 0
            && settable != 0
    }
}

fn array_values(value: &CFType, cap: usize) -> Vec<CFType> {
    let raw = value.as_CFTypeRef();
    if unsafe { CFGetTypeID(raw) != CFArrayGetTypeID() } {
        return Vec::new();
    }
    // SAFETY: callers pass only values returned for AXChildren/AX action names, both CFArrayRef.
    let len = unsafe { CFArrayGetCount(raw) }.max(0) as usize;
    (0..len.min(cap))
        .filter_map(|index| {
            let child = unsafe { CFArrayGetValueAtIndex(raw, index as isize) };
            (!child.is_null()).then(|| unsafe { CFType::wrap_under_get_rule(child) })
        })
        .collect()
}

fn actions(element: &CFType) -> Vec<String> {
    let mut raw = ptr::null();
    // SAFETY: AX returns a create-rule CFArrayRef, which CFType releases after use.
    let status = unsafe { AXUIElementCopyActionNames(element.as_CFTypeRef(), &mut raw) };
    let mut names = if status == 0 && !raw.is_null() {
        let value = unsafe { CFType::wrap_under_create_rule(raw) };
        array_values(&value, 126)
            .into_iter()
            .filter_map(|value| string(&value))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if settable(element, "AXValue") {
        names.push("setValue".into());
    }
    if settable(element, "AXFocused") {
        names.push("focus".into());
    }
    names.truncate(128);
    names
}

fn normalized(value: Option<String>, fallback: &str, max: usize) -> String {
    let value = value
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.into());
    value.chars().filter(|c| *c != '\0').take(max).collect()
}

fn observe() -> Result<(AccessibilitySnapshot, HashMap<String, CFType>), DesktopFault> {
    let raw = unsafe { AXUIElementCreateSystemWide() };
    if raw.is_null() {
        return Err(DesktopFault::NativeFailure);
    }
    let system = unsafe { CFType::wrap_under_create_rule(raw) };
    let app = if let Some(app) = attribute(&system, "AXFocusedApplication") {
        app
    } else {
        let pid = NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .ok_or(DesktopFault::Unsupported)?
            .processIdentifier();
        if pid <= 0 {
            return Err(DesktopFault::Unsupported);
        }
        let raw = unsafe { AXUIElementCreateApplication(pid) };
        if raw.is_null() {
            return Err(DesktopFault::NativeFailure);
        }
        unsafe { CFType::wrap_under_create_rule(raw) }
    };
    let mut pid = 0_i32;
    if unsafe { AXUIElementGetPid(app.as_CFTypeRef(), &mut pid) } != 0 || pid <= 0 {
        return Err(DesktopFault::NativeFailure);
    }
    if pid as u32 == std::process::id() {
        return Ok((
            AccessibilitySnapshot {
                platform: "macos-ax".into(),
                nodes: Vec::new(),
                truncated: false,
            },
            HashMap::new(),
        ));
    }
    let started = Instant::now();
    let mut queue = VecDeque::from([(app, None, String::from("0"), 0_u8)]);
    let mut nodes = Vec::new();
    let mut handles = HashMap::new();
    let mut truncated = false;
    while let Some((element, parent_id, path, depth)) = queue.pop_front() {
        if nodes.len() >= 500 || started.elapsed() >= Duration::from_secs(2) {
            truncated = true;
            break;
        }
        // SAFETY: this live AX element is owned by this traversal; bound each remote call.
        unsafe {
            AXUIElementSetMessagingTimeout(element.as_CFTypeRef(), 0.25);
        }
        let target_id = format!("mac:{pid}:{path}");
        let role = normalized(string_attribute(&element, "AXRole"), "unknown", 256);
        let name = normalized(
            string_attribute(&element, "AXTitle")
                .or_else(|| string_attribute(&element, "AXDescription")),
            "(unnamed)",
            256,
        );
        let description = string_attribute(&element, "AXHelp")
            .filter(|value| !value.is_empty())
            .map(|value| normalized(Some(value), "", 4096));
        let enabled = bool_attribute(&element, "AXEnabled").unwrap_or(true);
        let actions = actions(&element)
            .into_iter()
            .map(|value| normalized(Some(value), "unknown", 256))
            .collect();
        if depth < 16 {
            if let Some(children) = attribute(&element, "AXChildren") {
                let child_count = unsafe { CFArrayGetCount(children.as_CFTypeRef()) };
                if child_count > 64 {
                    truncated = true;
                }
                for (index, child) in array_values(&children, 64).into_iter().enumerate() {
                    queue.push_back((
                        child,
                        Some(target_id.clone()),
                        format!("{path}.{index}"),
                        depth + 1,
                    ));
                }
            }
        } else {
            truncated = true;
        }
        nodes.push(AccessibilityNode {
            target_id: target_id.clone(),
            parent_id,
            role,
            name,
            description,
            enabled,
            actions,
        });
        handles.insert(target_id, element);
    }
    Ok((
        AccessibilitySnapshot {
            platform: "macos-ax".into(),
            nodes,
            truncated,
        },
        handles,
    ))
}

pub fn snapshot() -> Result<AccessibilitySnapshot, DesktopFault> {
    observe().map(|(snapshot, _)| snapshot)
}

pub fn inject_target(
    target_id: &str,
    revision: &str,
    action: &DesktopTargetAction,
) -> Result<(), DesktopFault> {
    let (snapshot, handles) = observe()?;
    if snapshot.revision() != revision {
        return Err(DesktopFault::StaleTarget);
    }
    let element = handles.get(target_id).ok_or(DesktopFault::StaleTarget)?;
    let node = snapshot
        .nodes
        .iter()
        .find(|node| node.target_id == target_id)
        .ok_or(DesktopFault::StaleTarget)?;
    if !node.enabled {
        return Err(DesktopFault::Unsupported);
    }
    // Reading a tree uses short timeouts; a user action needs time for the app to respond.
    unsafe {
        AXUIElementSetMessagingTimeout(element.as_CFTypeRef(), 1.0);
    }
    let status = match action {
        DesktopTargetAction::Invoke { action } => {
            if !node.actions.iter().any(|listed| listed == &action.0) {
                return Err(DesktopFault::Unsupported);
            }
            let action = CFString::new(&action.0);
            unsafe {
                AXUIElementPerformAction(element.as_CFTypeRef(), action.as_concrete_TypeRef())
            }
        }
        DesktopTargetAction::SetValue { value } => {
            if !node.actions.iter().any(|listed| listed == "setValue") {
                return Err(DesktopFault::Unsupported);
            }
            let value = CFString::new(&value.0);
            let key = CFString::new("AXValue");
            unsafe {
                AXUIElementSetAttributeValue(
                    element.as_CFTypeRef(),
                    key.as_concrete_TypeRef(),
                    value.as_CFTypeRef(),
                )
            }
        }
        DesktopTargetAction::Focus {} => {
            if !node.actions.iter().any(|listed| listed == "focus") {
                return Err(DesktopFault::Unsupported);
            }
            let key = CFString::new("AXFocused");
            unsafe {
                AXUIElementSetAttributeValue(
                    element.as_CFTypeRef(),
                    key.as_concrete_TypeRef(),
                    CFBoolean::true_value().as_CFTypeRef(),
                )
            }
        }
    };
    match status {
        0 => Ok(()),
        -25202 => Err(DesktopFault::StaleTarget),
        -25205 | -25206 => Err(DesktopFault::Unsupported),
        _ => Err(DesktopFault::NativeFailure),
    }
}
