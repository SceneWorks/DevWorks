//! freedesktop Notifications over the user's session bus (Wayland and X11 desktops).
//! The service owns authorization/suppression; this protocol does not expose those settings.
use super::preview;
use devworks_core::ports::os_notifications::{
    OsDeliveryOutcome, OsNotification, OsNotificationPermission,
};
use std::collections::HashMap;
use std::time::Duration;
use zbus::blocking::{Connection, Proxy, connection::Builder};

fn connection() -> Result<Connection, zbus::Error> {
    Builder::session()?
        .method_timeout(Duration::from_secs(1))
        .build()
}
fn proxy(connection: &Connection) -> Result<Proxy<'_>, zbus::Error> {
    Proxy::new(
        connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
}
pub(super) fn permission(_request: bool) -> OsNotificationPermission {
    let checked = connection().and_then(|connection| {
        proxy(&connection)?
            .call::<_, _, (String, String, String, String)>("GetServerInformation", &())
    });
    match checked {
        Ok(_) => OsNotificationPermission::SystemManaged,
        Err(e) => OsNotificationPermission::Unavailable(format!(
            "The desktop notification service is unavailable: {e}"
        )),
    }
}
pub(super) fn submit(notification: &OsNotification) -> OsDeliveryOutcome {
    let connection = match connection() {
        Ok(c) => c,
        Err(e) => {
            return OsDeliveryOutcome::Unavailable(format!(
                "The desktop session bus is unavailable: {e}"
            ));
        }
    };
    let proxy = match proxy(&connection) {
        Ok(p) => p,
        Err(e) => {
            return OsDeliveryOutcome::Unavailable(format!(
                "The desktop notification service is unavailable: {e}"
            ));
        }
    };
    // Plain text avoids body-markup interpretation. No urgency override, action with decision
    // authority, or forced timeout bypasses the desktop's own quiet/focus policy.
    let hints: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
    let title = preview(&notification.title, 128);
    let capabilities: Vec<String> = match proxy.call("GetCapabilities", &()) {
        Ok(capabilities) => capabilities,
        Err(e) => {
            return OsDeliveryOutcome::Unavailable(format!(
                "The desktop notification service could not report its capabilities: {e}"
            ));
        }
    };
    let body = preview(&notification.body, 512);
    let body = if capabilities.iter().any(|c| c == "body-markup") {
        body.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    } else {
        body
    };
    let result: Result<u32, zbus::Error> = proxy.call(
        "Notify",
        &(
            "DevWorks",
            0u32,
            "",
            title.as_str(),
            body.as_str(),
            Vec::<&str>::new(),
            hints,
            -1i32,
        ),
    );
    match result {
        Ok(id) if id > 0 => OsDeliveryOutcome::Submitted {
            native_id: Some(id.to_string()),
        },
        Ok(_) => OsDeliveryOutcome::Unknown(
            "The desktop accepted Notify but returned an invalid zero notification identity".into(),
        ),
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.freedesktop.DBus.Error.AccessDenied" =>
        {
            OsDeliveryOutcome::Denied
        }
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown" =>
        {
            OsDeliveryOutcome::Unavailable(
                "The desktop notification service disappeared before delivery".into(),
            )
        }
        Err(e) => OsDeliveryOutcome::Unknown(format!(
            "The desktop notification request has an unconfirmed outcome: {e}"
        )),
    }
}
