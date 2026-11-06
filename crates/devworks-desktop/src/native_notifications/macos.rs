//! UserNotifications.framework; no AppleScript helper and no forged bundle identity.
use super::preview;
use block2::RcBlock;
use devworks_core::ports::os_notifications::{
    OsDeliveryOutcome, OsNotification, OsNotificationPermission,
};
use objc2::runtime::Bool;
use objc2_foundation::{NSBundle, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNAuthorizationStatus, UNMutableNotificationContent,
    UNNotificationRequest, UNNotificationSetting, UNNotificationSettings, UNUserNotificationCenter,
};
use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::Duration;
const BOUND: Duration = Duration::from_secs(3);

fn has_identity() -> bool {
    // SAFETY: the framework returns the current process's real immutable main bundle.
    unsafe {
        NSBundle::mainBundle()
            .bundleIdentifier()
            .is_some_and(|id| !id.to_string().is_empty())
    }
}
fn settings(center: &UNUserNotificationCenter) -> OsNotificationPermission {
    let (tx, rx) = mpsc::channel();
    let completion = RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
        // SAFETY: Apple guarantees a non-null settings object for the duration of this block.
        let permission = unsafe {
            let settings = settings.as_ref();
            match settings.authorizationStatus() {
                UNAuthorizationStatus::NotDetermined => OsNotificationPermission::NotDetermined,
                UNAuthorizationStatus::Denied => OsNotificationPermission::Denied,
                UNAuthorizationStatus::Authorized
                | UNAuthorizationStatus::Provisional
                | UNAuthorizationStatus::Ephemeral => {
                    if settings.alertSetting() == UNNotificationSetting::Enabled {
                        OsNotificationPermission::Allowed
                    } else {
                        OsNotificationPermission::Suppressed(
                            "macOS alert presentation is disabled for this app".into(),
                        )
                    }
                }
                _ => OsNotificationPermission::Unavailable(
                    "macOS returned an unknown notification authorization status".into(),
                ),
            }
        };
        let _ = tx.send(permission);
    });
    // SAFETY: UserNotifications copies the block and invokes it with a valid settings object.
    unsafe { center.getNotificationSettingsWithCompletionHandler(&completion) };
    rx.recv_timeout(BOUND).unwrap_or_else(|_| {
        OsNotificationPermission::Unavailable(
            "macOS notification settings did not answer within the host bound".into(),
        )
    })
}
pub(super) fn permission(request: bool) -> OsNotificationPermission {
    objc2::rc::autoreleasepool(|_| permission_inner(request))
}
fn permission_inner(request: bool) -> OsNotificationPermission {
    if !has_identity() {
        return OsNotificationPermission::Unavailable("macOS notifications require the running app's bundle identity; this process is unbundled".into());
    }
    // SAFETY: identity was established before requesting the process's notification center.
    let center = unsafe { UNUserNotificationCenter::currentNotificationCenter() };
    if request && settings(&center) == OsNotificationPermission::NotDetermined {
        let (tx, rx) = mpsc::channel();
        let completion = RcBlock::new(move |granted: Bool, error: *mut NSError| {
            let _ = tx.send((granted.as_bool(), !error.is_null()));
        });
        // SAFETY: called only for an explicit native-human permission request. The framework
        // retains the completion block; the sender remains valid even after our bounded wait.
        unsafe {
            center.requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::UNAuthorizationOptionAlert,
                &completion,
            )
        };
        match rx.recv_timeout(BOUND) {
            Ok((_, true)) => {
                return OsNotificationPermission::Unavailable(
                    "macOS refused the notification authorization request".into(),
                );
            }
            Ok((false, false)) => return OsNotificationPermission::Denied,
            Err(_) => return OsNotificationPermission::NotDetermined,
            Ok((true, false)) => {}
        }
    }
    settings(&center)
}
pub(super) fn submit(notification: &OsNotification) -> OsDeliveryOutcome {
    objc2::rc::autoreleasepool(|_| submit_inner(notification))
}
fn submit_inner(notification: &OsNotification) -> OsDeliveryOutcome {
    let (tx, rx) = mpsc::channel();
    let completion = RcBlock::new(move |error: *mut NSError| {
        let _ = tx.send(error.is_null());
    });
    // SAFETY: immutable NSString copies outlive request creation; UserNotifications retains
    // both the request and completion block. An identical durable ID replaces that OS item.
    unsafe {
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(&preview(&notification.title, 128)));
        content.setBody(&NSString::from_str(&preview(&notification.body, 512)));
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(notification.notification_id.as_str()),
            &content,
            None,
        );
        UNUserNotificationCenter::currentNotificationCenter()
            .addNotificationRequest_withCompletionHandler(&request, Some(&completion));
    }
    match rx.recv_timeout(BOUND) {
        Ok(true) => OsDeliveryOutcome::Submitted {
            native_id: Some(notification.notification_id.as_str().to_owned()),
        },
        Ok(false) => {
            OsDeliveryOutcome::Unavailable("macOS rejected the notification request".into())
        }
        Err(_) => OsDeliveryOutcome::Unknown(
            "macOS accepted the request call but did not confirm its result within the host bound"
                .into(),
        ),
    }
}
