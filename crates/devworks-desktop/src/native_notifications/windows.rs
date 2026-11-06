//! WinRT toast APIs under the running app's real package or explicit AppUserModelID.
use super::preview;
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::os_notifications::{
    OsDeliveryOutcome, OsNotification, OsNotificationPermission,
};
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{
    NotificationSetting, ToastNotification, ToastNotificationManager,
};
use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};
use windows::Win32::Storage::Packaging::Appx::GetCurrentApplicationUserModelId;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::Win32::UI::Shell::{
    GetCurrentProcessExplicitAppUserModelID, QUNS_ACCEPTS_NOTIFICATIONS,
    SHQueryUserNotificationState,
};
use windows::core::{HSTRING, PWSTR};

struct Apartment;
impl Apartment {
    fn new() -> Result<Self, windows::core::Error> {
        // SAFETY: each worker call balances its successful WinRT initialization with a drop.
        unsafe {
            RoInitialize(RO_INIT_MULTITHREADED)?;
        }
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            RoUninitialize();
        }
    }
}
fn identity() -> Option<HSTRING> {
    // SAFETY: the API first returns the required UTF-16 capacity, then fills our owned buffer.
    unsafe {
        let mut length = 0;
        if GetCurrentApplicationUserModelId(&mut length, None) == ERROR_INSUFFICIENT_BUFFER
            && length > 1
        {
            let mut id = vec![0u16; length as usize];
            if GetCurrentApplicationUserModelId(&mut length, Some(PWSTR(id.as_mut_ptr())))
                == ERROR_SUCCESS
            {
                let text = String::from_utf16(&id[..length as usize - 1]).ok()?;
                if !text.is_empty() {
                    return Some(HSTRING::from(text));
                }
            }
        }
        // This reads an identity assigned by the installed app; it never registers another
        // app's ID or creates a Start-menu shortcut as a side effect of notification delivery.
        if let Ok(id) = GetCurrentProcessExplicitAppUserModelID() {
            let text = id.to_string().ok();
            CoTaskMemFree(Some(id.0.cast()));
            return text.filter(|s| !s.is_empty()).map(HSTRING::from);
        }
    }
    None
}
fn unavailable(error: impl std::fmt::Display) -> OsNotificationPermission {
    OsNotificationPermission::Unavailable(format!(
        "Windows notification API is unavailable: {error}"
    ))
}
pub(super) fn permission(_request: bool) -> OsNotificationPermission {
    let Some(id) = identity() else {
        return OsNotificationPermission::Unavailable("Windows notifications require the installed app's package or registered AppUserModelID; this process has neither".into());
    };
    let _apartment = match Apartment::new() {
        Ok(a) => a,
        Err(e) => return unavailable(e),
    };
    let notifier = match ToastNotificationManager::CreateToastNotifierWithId(&id) {
        Ok(n) => n,
        Err(e) => return unavailable(e),
    };
    match notifier.Setting() {
        Ok(NotificationSetting::Enabled) => {}
        Ok(_) => return OsNotificationPermission::Denied,
        Err(e) => return unavailable(e),
    }
    // SAFETY: a read-only shell query; only the explicit accepting state permits an alert.
    match unsafe { SHQueryUserNotificationState() } {
        Ok(QUNS_ACCEPTS_NOTIFICATIONS) => OsNotificationPermission::Allowed,
        Ok(state) => OsNotificationPermission::Suppressed(format!(
            "Windows is suppressing notifications (shell state {})",
            state.0
        )),
        Err(e) => unavailable(e),
    }
}
fn xml_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
pub(super) fn submit(notification: &OsNotification) -> OsDeliveryOutcome {
    let Some(id) = identity() else {
        return OsDeliveryOutcome::Unavailable(
            "Windows application identity is unavailable".into(),
        );
    };
    let _apartment = match Apartment::new() {
        Ok(a) => a,
        Err(e) => return OsDeliveryOutcome::Unavailable(e.to_string()),
    };
    let created = (|| -> windows::core::Result<_> {
        let xml = XmlDocument::new()?;
        xml.LoadXml(&HSTRING::from(format!("<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>", xml_text(&preview(&notification.title, 128)), xml_text(&preview(&notification.body, 512)))))?;
        let toast = ToastNotification::CreateToastNotification(&xml)?;
        let key = sha256_hex(notification.notification_id.as_str().as_bytes());
        toast.SetTag(&HSTRING::from(&key[..16]))?;
        toast.SetGroup(&HSTRING::from(&key[16..32]))?;
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&id)?;
        Ok((notifier, toast, key))
    })();
    let (notifier, toast, key) = match created {
        Ok(v) => v,
        Err(e) => {
            return OsDeliveryOutcome::Unavailable(format!(
                "Windows could not construct the notification: {e}"
            ));
        }
    };
    match notifier.Show(&toast) {
        Ok(()) => OsDeliveryOutcome::Submitted {
            native_id: Some(key[..32].to_owned()),
        },
        Err(e) => OsDeliveryOutcome::Unknown(format!(
            "Windows notification submission has an unconfirmed outcome: {e}"
        )),
    }
}
