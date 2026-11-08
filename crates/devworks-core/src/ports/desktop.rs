//! Native desktop boundary. The service owns consent, reservation, authority, and the effect
//! journal; an adapter may only report OS capability, capture pixels, or inject one input.
use crate::protocol::records::DesktopInput;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DesktopAvailability {
    Available,
    PermissionRequired(&'static str),
    Unsupported(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopCapabilities {
    pub target: String,
    pub capture: DesktopAvailability,
    pub input: DesktopAvailability,
    pub accessibility: DesktopAvailability,
}

/// A bounded observation of the native accessibility tree. Target IDs are opaque and valid only
/// with the digest of this observation; adapters must re-observe before an action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccessibilityNode {
    pub target_id: String,
    pub parent_id: Option<String>,
    pub role: String,
    pub name: String,
    pub description: Option<String>,
    pub enabled: bool,
    pub actions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccessibilitySnapshot {
    pub platform: String,
    pub nodes: Vec<AccessibilityNode>,
    pub truncated: bool,
}

impl AccessibilitySnapshot {
    pub fn revision(&self) -> String {
        crate::fingerprint::sha256_hex(
            &serde_json::to_vec(self).expect("desktop snapshot serializes"),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DesktopFault {
    PermissionRequired,
    Unsupported,
    StaleTarget,
    NativeFailure,
}

/// Capture and input are deliberately separate capabilities: screen recording permission does
/// not imply accessibility/input permission. Consent to take control is checked by the service.
pub trait DesktopPlatform: Send + Sync {
    fn capabilities(&self) -> DesktopCapabilities;
    fn capture_png(&self) -> Result<Vec<u8>, DesktopFault>;
    fn inject(&self, action: &DesktopInput) -> Result<(), DesktopFault>;
    fn accessibility_snapshot(&self) -> Result<AccessibilitySnapshot, DesktopFault> {
        Err(DesktopFault::Unsupported)
    }
}
