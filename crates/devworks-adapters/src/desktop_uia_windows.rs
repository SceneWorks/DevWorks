//! Focused native window through Microsoft UI Automation, with per-call COM ownership.
use devworks_core::ports::desktop::{AccessibilityNode, AccessibilitySnapshot, DesktopFault};
use devworks_core::protocol::records::DesktopTargetAction;
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::time::{Duration, Instant};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    SAFEARRAY,
};
use windows::Win32::System::Ole::{
    SafeArrayDestroy, SafeArrayGetDim, SafeArrayGetElement, SafeArrayGetElemsize,
    SafeArrayGetLBound, SafeArrayGetUBound,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationElement,
    IUIAutomationExpandCollapsePattern, IUIAutomationInvokePattern, IUIAutomationRangeValuePattern,
    IUIAutomationSelectionItemPattern, IUIAutomationTogglePattern, IUIAutomationValuePattern,
    TreeScope_Children, UIA_ExpandCollapsePatternId, UIA_InvokePatternId, UIA_RangeValuePatternId,
    UIA_SelectionItemPatternId, UIA_TogglePatternId, UIA_ValuePatternId,
};
use windows::core::{BSTR, Interface};

struct ComSession;
impl Drop for ComSession {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

fn automation() -> Result<(IUIAutomation, ComSession), DesktopFault> {
    // SAFETY: this thread owns one balanced COM initialization for the lifetime of its proxies.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .map_err(|_| DesktopFault::Unsupported)?;
    let session = ComSession;
    let v2: IUIAutomation2 =
        unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) }
            .map_err(|_| DesktopFault::Unsupported)?;
    unsafe { v2.SetConnectionTimeout(750) }.map_err(|_| DesktopFault::Unsupported)?;
    unsafe { v2.SetTransactionTimeout(750) }.map_err(|_| DesktopFault::Unsupported)?;
    let automation = v2
        .cast::<IUIAutomation>()
        .map_err(|_| DesktopFault::Unsupported)?;
    // A temporary tuple (notably in available()) drops fields in order. Release the
    // automation proxy before balancing COM initialization on this thread.
    Ok((automation, session))
}

pub fn available() -> bool {
    automation().is_ok()
}

fn normalized(value: String, fallback: &str, max: usize) -> String {
    let value = if value.is_empty() { fallback } else { &value };
    value.chars().filter(|c| *c != '\0').take(max).collect()
}

fn has_pattern<T: Interface>(
    element: &IUIAutomationElement,
    pattern: windows::Win32::UI::Accessibility::UIA_PATTERN_ID,
) -> bool {
    unsafe { element.GetCurrentPatternAs::<T>(pattern) }.is_ok()
}

struct OwnedRuntimeId(*mut SAFEARRAY);
impl Drop for OwnedRuntimeId {
    fn drop(&mut self) {
        unsafe {
            let _ = SafeArrayDestroy(self.0);
        }
    }
}

fn runtime_id(element: &IUIAutomationElement, pid: i32) -> Result<String, DesktopFault> {
    let array =
        OwnedRuntimeId(unsafe { element.GetRuntimeId() }.map_err(|_| DesktopFault::NativeFailure)?);
    if array.0.is_null()
        || unsafe { SafeArrayGetDim(array.0) } != 1
        || unsafe { SafeArrayGetElemsize(array.0) } != 4
    {
        return Err(DesktopFault::NativeFailure);
    }
    let first =
        unsafe { SafeArrayGetLBound(array.0, 1) }.map_err(|_| DesktopFault::NativeFailure)?;
    let last =
        unsafe { SafeArrayGetUBound(array.0, 1) }.map_err(|_| DesktopFault::NativeFailure)?;
    if last < first || last - first >= 64 {
        return Err(DesktopFault::NativeFailure);
    }
    let mut parts = Vec::new();
    for index in first..=last {
        let mut value = 0_i32;
        unsafe { SafeArrayGetElement(array.0, &index, (&mut value as *mut i32).cast::<c_void>()) }
            .map_err(|_| DesktopFault::NativeFailure)?;
        parts.push(format!("{value:x}"));
    }
    Ok(format!("win:{pid}:{}", parts.join(".")))
}

fn observe(
    automation: &IUIAutomation,
) -> Result<(AccessibilitySnapshot, HashMap<String, IUIAutomationElement>), DesktopFault> {
    let focused =
        unsafe { automation.GetFocusedElement() }.map_err(|_| DesktopFault::Unsupported)?;
    let pid = unsafe { focused.CurrentProcessId() }.map_err(|_| DesktopFault::NativeFailure)?;
    if pid <= 0 || pid as u32 == std::process::id() {
        return Ok((
            AccessibilitySnapshot {
                platform: "windows-uia".into(),
                nodes: Vec::new(),
                truncated: false,
            },
            HashMap::new(),
        ));
    }
    let desktop =
        unsafe { automation.GetRootElement() }.map_err(|_| DesktopFault::NativeFailure)?;
    let condition =
        unsafe { automation.CreateTrueCondition() }.map_err(|_| DesktopFault::NativeFailure)?;
    let top = unsafe { desktop.FindAll(TreeScope_Children, &condition) }
        .map_err(|_| DesktopFault::NativeFailure)?;
    let length = unsafe { top.Length() }.map_err(|_| DesktopFault::NativeFailure)?;
    let mut roots = Vec::new();
    for index in 0..length.min(128) {
        if let Ok(window) = unsafe { top.GetElement(index) }
            && unsafe { window.CurrentProcessId() }.ok() == Some(pid)
        {
            roots.push(window);
        }
    }
    if roots.is_empty() {
        roots.push(focused);
    }
    let started = Instant::now();
    let mut queue = VecDeque::new();
    for root in roots {
        queue.push_back((root, None, 0_u8));
    }
    let mut nodes = Vec::new();
    let mut handles = HashMap::new();
    let mut truncated = false;
    while let Some((element, parent_id, depth)) = queue.pop_front() {
        if nodes.len() >= 500 || started.elapsed() >= Duration::from_secs(3) {
            truncated = true;
            break;
        }
        let id = match runtime_id(&element, pid) {
            Ok(id) => id,
            Err(_) => {
                truncated = true;
                continue;
            }
        };
        let role = unsafe { element.CurrentLocalizedControlType() }
            .ok()
            .map(|value| value.to_string())
            .unwrap_or_default();
        let name = unsafe { element.CurrentName() }
            .ok()
            .map(|value| value.to_string())
            .unwrap_or_default();
        let description = unsafe { element.CurrentHelpText() }
            .ok()
            .map(|value| value.to_string())
            .filter(|s| !s.is_empty())
            .map(|value| normalized(value, "", 4096));
        let enabled = unsafe { element.CurrentIsEnabled() }.is_ok_and(|value| value.as_bool());
        let mut actions = Vec::new();
        if has_pattern::<IUIAutomationInvokePattern>(&element, UIA_InvokePatternId) {
            actions.push("invoke".into());
        }
        if has_pattern::<IUIAutomationTogglePattern>(&element, UIA_TogglePatternId) {
            actions.push("toggle".into());
        }
        if has_pattern::<IUIAutomationSelectionItemPattern>(&element, UIA_SelectionItemPatternId) {
            actions.push("select".into());
        }
        if has_pattern::<IUIAutomationExpandCollapsePattern>(&element, UIA_ExpandCollapsePatternId)
        {
            actions.extend(["expand".into(), "collapse".into()]);
        }
        if has_pattern::<IUIAutomationValuePattern>(&element, UIA_ValuePatternId)
            || has_pattern::<IUIAutomationRangeValuePattern>(&element, UIA_RangeValuePatternId)
        {
            actions.push("setValue".into());
        }
        actions.push("focus".into());
        if depth < 16 {
            match unsafe { element.FindAll(TreeScope_Children, &condition) } {
                Ok(children) => {
                    let n = unsafe { children.Length() }.unwrap_or(0);
                    if n > 64 {
                        truncated = true;
                    }
                    for index in 0..n.min(64) {
                        if let Ok(child) = unsafe { children.GetElement(index) } {
                            queue.push_back((child, Some(id.clone()), depth + 1));
                        }
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
            enabled,
            actions,
        });
        handles.insert(id, element);
    }
    Ok((
        AccessibilitySnapshot {
            platform: "windows-uia".into(),
            nodes,
            truncated,
        },
        handles,
    ))
}

pub fn snapshot() -> Result<AccessibilitySnapshot, DesktopFault> {
    let (automation, _session) = automation()?;
    observe(&automation).map(|(snapshot, _)| snapshot)
}

pub fn inject_target(
    target_id: &str,
    revision: &str,
    action: &DesktopTargetAction,
) -> Result<(), DesktopFault> {
    let (automation, _session) = automation()?;
    let (snapshot, handles) = observe(&automation)?;
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
    match action {
        DesktopTargetAction::Invoke { action } => {
            if !node.actions.iter().any(|listed| listed == &action.0) {
                return Err(DesktopFault::Unsupported);
            }
            match action.0.as_str() {
                "invoke" => unsafe {
                    element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
                }
                .and_then(|p| unsafe { p.Invoke() }),
                "toggle" => unsafe {
                    element.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
                }
                .and_then(|p| unsafe { p.Toggle() }),
                "select" => unsafe {
                    element.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                        UIA_SelectionItemPatternId,
                    )
                }
                .and_then(|p| unsafe { p.Select() }),
                "expand" => unsafe {
                    element.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                        UIA_ExpandCollapsePatternId,
                    )
                }
                .and_then(|p| unsafe { p.Expand() }),
                "collapse" => unsafe {
                    element.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                        UIA_ExpandCollapsePatternId,
                    )
                }
                .and_then(|p| unsafe { p.Collapse() }),
                _ => return Err(DesktopFault::Unsupported),
            }
            .map_err(|_| DesktopFault::NativeFailure)
        }
        DesktopTargetAction::SetValue { value } => {
            if !node.actions.iter().any(|listed| listed == "setValue") {
                return Err(DesktopFault::Unsupported);
            }
            if let Ok(pattern) = unsafe {
                element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            } {
                return unsafe { pattern.SetValue(&BSTR::from(value.0.as_str())) }
                    .map_err(|_| DesktopFault::NativeFailure);
            }
            let numeric = value
                .0
                .parse::<f64>()
                .map_err(|_| DesktopFault::Unsupported)?;
            unsafe {
                element
                    .GetCurrentPatternAs::<IUIAutomationRangeValuePattern>(UIA_RangeValuePatternId)
            }
            .and_then(|p| unsafe { p.SetValue(numeric) })
            .map_err(|_| DesktopFault::NativeFailure)
        }
        DesktopTargetAction::Focus {} => {
            unsafe { element.SetFocus() }.map_err(|_| DesktopFault::NativeFailure)
        }
    }
}
