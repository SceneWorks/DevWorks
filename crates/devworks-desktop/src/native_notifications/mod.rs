//! Native notification APIs. Application identity comes from the running installed app;
//! an unbundled/unregistered process reports unavailable and keeps its persistent inbox.
use devworks_core::ports::os_notifications::{
    OsDeliveryOutcome, OsNotification, OsNotificationPermission, OsNotificationPort,
};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[derive(Clone, Copy, Debug, Default)]
pub struct NativeOsNotifications;
impl NativeOsNotifications {
    pub const fn new() -> Self {
        Self
    }
}

impl OsNotificationPort for NativeOsNotifications {
    fn permission(&self) -> OsNotificationPermission {
        platform_permission(false)
    }
    fn request_permission(&self) -> OsNotificationPermission {
        platform_permission(true)
    }
    fn submit(&self, notification: &OsNotification) -> OsDeliveryOutcome {
        match self.permission() {
            OsNotificationPermission::Allowed | OsNotificationPermission::SystemManaged => {}
            OsNotificationPermission::Denied => return OsDeliveryOutcome::Denied,
            OsNotificationPermission::Suppressed(reason) => {
                return OsDeliveryOutcome::Suppressed(reason);
            }
            OsNotificationPermission::NotDetermined => {
                return OsDeliveryOutcome::Unavailable(
                    "OS notification authorization has not been granted".into(),
                );
            }
            OsNotificationPermission::Unavailable(reason) => {
                return OsDeliveryOutcome::Unavailable(reason);
            }
        }
        platform_submit(notification)
    }
}

fn platform_permission(request: bool) -> OsNotificationPermission {
    #[cfg(target_os = "macos")]
    {
        macos::permission(request)
    }
    #[cfg(target_os = "windows")]
    {
        windows::permission(request)
    }
    #[cfg(target_os = "linux")]
    {
        linux::permission(request)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = request;
        OsNotificationPermission::Unavailable(
            "Native OS notifications are not implemented for this operating system".into(),
        )
    }
}
fn platform_submit(notification: &OsNotification) -> OsDeliveryOutcome {
    #[cfg(target_os = "macos")]
    {
        macos::submit(notification)
    }
    #[cfg(target_os = "windows")]
    {
        windows::submit(notification)
    }
    #[cfg(target_os = "linux")]
    {
        linux::submit(notification)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = notification;
        OsDeliveryOutcome::Unavailable(
            "Native OS notifications are not implemented for this operating system".into(),
        )
    }
}

/// OS content is a short preview. The complete canonical text and identity remain in-app.
fn preview(text: &str, characters: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(characters)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_permission_probe_uses_the_current_process_boundary_without_requesting_authorization()
    {
        let status = NativeOsNotifications.permission();
        eprintln!("native OS notification boundary: {status:?}");
        if let OsNotificationPermission::Unavailable(reason)
        | OsNotificationPermission::Suppressed(reason) = status
        {
            assert!(
                !reason.is_empty(),
                "an unavailable platform must explain its actual boundary"
            );
        }
    }
    #[test]
    fn previews_are_unicode_safe_and_remove_control_characters() {
        assert_eq!(preview("é\0🙂abc", 3), "é🙂a");
    }
}
