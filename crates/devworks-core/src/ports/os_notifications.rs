//! Native OS alerts are an optional delivery channel for canonical persistent notifications.
//! Neither permission, submission nor OS dismissal grants decision or resumption authority.
use crate::protocol::Id;
use std::fmt::Debug;

/// What the native platform currently permits. Even `Allowed` cannot promise visibility:
/// focus modes and the notification service may defer or suppress an accepted request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OsNotificationPermission {
    Allowed,
    /// The desktop service enforces its policy without exposing a portable authorization query
    /// (the freedesktop notification protocol). Submission still goes through that service.
    SystemManaged,
    NotDetermined,
    Denied,
    Suppressed(String),
    Unavailable(String),
}
impl OsNotificationPermission {
    pub fn can_submit(&self) -> bool {
        matches!(self, Self::Allowed | Self::SystemManaged)
    }
}

/// The alert content of one already persisted, recipient-qualified required-input item.
#[derive(Clone, Debug)]
pub struct OsNotification {
    pub notification_id: Id,
    pub title: String,
    pub body: String,
}

/// Outcome of one native submission attempt. `Submitted` means accepted by the system API,
/// not displayed to the person. Every outcome leaves the persistent in-app item untouched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OsDeliveryOutcome {
    Submitted {
        native_id: Option<String>,
    },
    Denied,
    Suppressed(String),
    Unavailable(String),
    /// The request may have reached the OS, but its result could not be established. Never
    /// automatically repeat this attempt after a timeout, crash or lost acknowledgment.
    Unknown(String),
}

/// Native platform effects. Call these outside the GUI frame loop. The caller checks current
/// focus, canonical recipient, required-input state and configuration before claiming delivery.
/// Explicit permission requests originate only from the native human Settings interaction.
pub trait OsNotificationPort: Debug + Send + Sync {
    fn permission(&self) -> OsNotificationPermission;
    fn request_permission(&self) -> OsNotificationPermission;
    fn submit(&self, notification: &OsNotification) -> OsDeliveryOutcome;
}
