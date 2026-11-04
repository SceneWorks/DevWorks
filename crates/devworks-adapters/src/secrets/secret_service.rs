//! The freedesktop Secret Service over the user's session bus (GNOME Keyring, KWallet's
//! service, KeePassXC ...). One item per handle in the default collection, found by the
//! attributes `application=DevWorks`, `devworks-namespace`, `devworks-handle`; values travel in
//! a `plain` session, which the specification allows on the local session bus.
//!
//! A locked collection or item asks the service to unlock it, which may show the desktop's
//! unlock prompt; the prompt is bounded ([`PROMPT_TIMEOUT`]) and dismissed when it is not
//! answered. No session bus, no service, no default collection or a dismissed prompt are
//! reported as unavailable, never as a missing item.
use super::{SERVICE, handle_of};
use devworks_core::ports::secrets::{Secret, SecretError, SecretKey, SecretStore};
use devworks_core::protocol::Id;
use std::collections::HashMap;
use std::time::Duration;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

const DEST: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SERVICE_IFACE: &str = "org.freedesktop.Secret.Service";
const COLLECTION_IFACE: &str = "org.freedesktop.Secret.Collection";
const ITEM_IFACE: &str = "org.freedesktop.Secret.Item";
const PROMPT_IFACE: &str = "org.freedesktop.Secret.Prompt";
/// How long an unlock prompt may stay open before it is dismissed.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);

type SecretStruct = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

pub struct SecretService;

fn unavailable(what: &str) -> SecretError {
    SecretError::Unavailable(what.into())
}
fn failed(e: zbus::Error) -> SecretError {
    match e {
        zbus::Error::MethodError(name, _, _) => SecretError::Failed(name.to_string()),
        _ => SecretError::Failed("the Secret Service call failed".into()),
    }
}

/// A session-bus connection, the service proxy and an open `plain` session.
struct Session {
    conn: Connection,
    path: OwnedObjectPath,
}
impl Session {
    fn open() -> Result<Self, SecretError> {
        let conn = Connection::session().map_err(|_| unavailable("no session bus"))?;
        let service = Proxy::new(&conn, DEST, SERVICE_PATH, SERVICE_IFACE)
            .map_err(|_| unavailable("no Secret Service"))?;
        let (_, path): (OwnedValue, OwnedObjectPath) = service
            .call("OpenSession", &("plain", Value::from("")))
            .map_err(|_| unavailable("no Secret Service is running"))?;
        Ok(Self { conn, path })
    }
    fn proxy<'a>(&'a self, path: &'a str, iface: &'a str) -> Result<Proxy<'a>, SecretError> {
        Proxy::new(&self.conn, DEST, path, iface).map_err(failed)
    }
    fn service(&self) -> Result<Proxy<'_>, SecretError> {
        self.proxy(SERVICE_PATH, SERVICE_IFACE)
    }

    /// Runs a prompt the service returned; `/` means none was needed.
    fn prompt(&self, path: &ObjectPath<'_>) -> Result<(), SecretError> {
        if path.as_str() == "/" {
            return Ok(());
        }
        let prompt = Proxy::new(&self.conn, DEST, path.to_owned(), PROMPT_IFACE).map_err(failed)?;
        let mut completed = prompt.receive_signal("Completed").map_err(failed)?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let dismissed = completed
                .next()
                .and_then(|m| m.body().deserialize::<(bool, OwnedValue)>().ok())
                .is_none_or(|(d, _)| d);
            let _ = tx.send(dismissed);
        });
        prompt.call::<_, _, ()>("Prompt", &("",)).map_err(failed)?;
        match rx.recv_timeout(PROMPT_TIMEOUT) {
            Ok(false) => Ok(()),
            Ok(true) => Err(unavailable("the unlock prompt was dismissed")),
            Err(_) => {
                // Ends the waiting thread with a dismissed Completed signal.
                let _ = prompt.call::<_, _, ()>("Dismiss", &());
                Err(unavailable("the unlock prompt was not answered"))
            }
        }
    }

    /// The items whose attributes match, unlocking locked ones.
    fn search(&self, attrs: &HashMap<&str, &str>) -> Result<Vec<OwnedObjectPath>, SecretError> {
        let (mut unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) = self
            .service()?
            .call("SearchItems", &(attrs,))
            .map_err(failed)?;
        if !locked.is_empty() {
            let (now, prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) = self
                .service()?
                .call("Unlock", &(&locked,))
                .map_err(failed)?;
            self.prompt(&prompt)?;
            if prompt.as_str() == "/" {
                unlocked.extend(now);
            } else {
                unlocked.extend(locked);
            }
        }
        Ok(unlocked)
    }
}

fn attributes<'a>(namespace: &'a str, handle: Option<&'a str>) -> HashMap<&'a str, &'a str> {
    let mut a = HashMap::from([("application", SERVICE), ("devworks-namespace", namespace)]);
    if let Some(h) = handle {
        a.insert("devworks-handle", h);
    }
    a
}

impl SecretStore for SecretService {
    fn name(&self) -> &'static str {
        "Secret Service"
    }

    fn put(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError> {
        let s = Session::open()?;
        let collection: OwnedObjectPath = s
            .service()?
            .call("ReadAlias", &("default",))
            .map_err(failed)?;
        if collection.as_str() == "/" {
            return Err(unavailable("there is no default keyring"));
        }
        let attrs = attributes(&key.namespace, Some(key.handle.as_str()));
        let label = format!("{SERVICE} credential {}", key.handle.as_str());
        let props: HashMap<&str, Value<'_>> = HashMap::from([
            (
                "org.freedesktop.Secret.Item.Label",
                Value::from(label.as_str()),
            ),
            ("org.freedesktop.Secret.Item.Attributes", Value::from(attrs)),
        ]);
        let value: SecretStruct = (
            s.path.clone(),
            Vec::new(),
            secret.expose().as_bytes().to_vec(),
            "text/plain".into(),
        );
        let (_, prompt): (OwnedObjectPath, OwnedObjectPath) = s
            .proxy(collection.as_str(), COLLECTION_IFACE)?
            .call("CreateItem", &(props, value, true))
            .map_err(failed)?;
        s.prompt(&prompt)
    }

    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
        let s = Session::open()?;
        let items = s.search(&attributes(&key.namespace, Some(key.handle.as_str())))?;
        let Some(item) = items.first() else {
            return Ok(None);
        };
        let (_, _, value, _): SecretStruct = s
            .proxy(item.as_str(), ITEM_IFACE)?
            .call("GetSecret", &(&s.path,))
            .map_err(failed)?;
        String::from_utf8(value)
            .map(|v| Some(Secret::new(v)))
            .map_err(|_| SecretError::Failed("the item is not UTF-8".into()))
    }

    fn delete(&self, key: &SecretKey) -> Result<bool, SecretError> {
        let s = Session::open()?;
        let items = s.search(&attributes(&key.namespace, Some(key.handle.as_str())))?;
        for item in &items {
            let prompt: OwnedObjectPath = s
                .proxy(item.as_str(), ITEM_IFACE)?
                .call("Delete", &())
                .map_err(failed)?;
            s.prompt(&prompt)?;
        }
        Ok(!items.is_empty())
    }

    fn list(&self, namespace: &str) -> Result<Vec<Id>, SecretError> {
        let s = Session::open()?;
        let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) = s
            .service()?
            .call("SearchItems", &(attributes(namespace, None),))
            .map_err(failed)?;
        let mut out = Vec::new();
        // Attributes are readable without unlocking, so listing never prompts.
        for item in unlocked.iter().chain(locked.iter()) {
            let attrs: HashMap<String, String> = s
                .proxy(item.as_str(), ITEM_IFACE)?
                .get_property("Attributes")
                .map_err(failed)?;
            if let Some(h) = attrs.get("devworks-handle")
                && let Some(id) = handle_of(&format!("{namespace}/{h}"), namespace)
            {
                out.push(id);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    /// The real Secret Service of the desktop session. Opt-in (`--ignored`).
    #[test]
    #[ignore = "touches the real Secret Service; run locally with --ignored"]
    fn secret_service_round_trip() {
        super::super::round_trip(&super::SecretService);
    }
}
