//! Native desktop boundary. The service owns consent, reservation, authority, and the effect
//! journal; an adapter may only report OS capability, capture pixels, or inject one input.
use crate::protocol::records::DesktopInput;

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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DesktopFault {
    PermissionRequired,
    Unsupported,
    NativeFailure,
}

/// Capture and input are deliberately separate capabilities: screen recording permission does
/// not imply accessibility/input permission. Consent to take control is checked by the service.
pub trait DesktopPlatform: Send + Sync {
    fn capabilities(&self) -> DesktopCapabilities;
    fn capture_png(&self) -> Result<Vec<u8>, DesktopFault>;
    fn inject(&self, action: &DesktopInput) -> Result<(), DesktopFault>;
}
