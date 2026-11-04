//! The freedesktop Secret Service over the user's session bus (GNOME Keyring, KWallet's
//! service, KeePassXC ...). One item per handle in the default collection, found by the
//! attributes `application=DevWorks`, `devworks-namespace`, `devworks-handle`; values travel in
//! a `plain` session, which the specification allows on the local session bus.
//!
//! A locked collection or item is unlocked first (writing, reading and deleting all need it),
//! which may show the desktop's unlock prompt; the prompt is bounded ([`PROMPT_TIMEOUT`]) and
//! dismissed when it is not answered. A call the service still refuses as locked is retried
//! once after another unlock. No session bus, no service, no default collection, a dismissed
//! or unanswered prompt and a collection that stays locked are reported as unavailable, never
//! as a missing item ([`super::faults::secret_service`]); an item that vanished under a call is
//! absent.
use super::faults::{self, BusFault};
use super::{SERVICE, handle_of};
use devworks_core::ports::secrets::{Secret, SecretError, SecretKey, SecretStore};
use devworks_core::protocol::Id;
use std::collections::HashMap;
use std::time::Duration;
use zbus::DBusError;
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
/// The D-Bus error name of a reply, if it is one.
fn name_of(e: &zbus::Error) -> Option<String> {
    match e {
        zbus::Error::MethodError(name, _, _) => Some(name.as_str().to_owned()),
        zbus::Error::FDO(f) => Some(f.name().as_str().to_owned()),
        _ => None,
    }
}
fn mapped(e: zbus::Error) -> SecretError {
    match (&e, name_of(&e)) {
        (_, Some(name)) => faults::secret_service(BusFault::Named(&name)),
        (
            zbus::Error::InputOutput(_)
            | zbus::Error::Connection(..)
            | zbus::Error::Handshake(_)
            | zbus::Error::Address(_),
            None,
        ) => faults::secret_service(BusFault::Connection),
        _ => faults::secret_service(BusFault::Other),
    }
}
fn is_locked(e: &zbus::Error) -> bool {
    name_of(e).is_some_and(|n| faults::locked(&n))
}
fn is_gone(e: &zbus::Error) -> bool {
    name_of(e).is_some_and(|n| faults::gone(&n))
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
        Proxy::new(&self.conn, DEST, path, iface).map_err(mapped)
    }
    fn service(&self) -> Result<Proxy<'_>, SecretError> {
        self.proxy(SERVICE_PATH, SERVICE_IFACE)
    }

    /// Runs a prompt the service returned; `/` means none was needed.
    fn prompt(&self, path: &ObjectPath<'_>) -> Result<(), SecretError> {
        if path.as_str() == "/" {
            return Ok(());
        }
        let prompt = Proxy::new(&self.conn, DEST, path.to_owned(), PROMPT_IFACE).map_err(mapped)?;
        let mut completed = prompt.receive_signal("Completed").map_err(mapped)?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let dismissed = completed
                .next()
                .and_then(|m| m.body().deserialize::<(bool, OwnedValue)>().ok())
                .is_none_or(|(d, _)| d);
            let _ = tx.send(dismissed);
        });
        prompt.call::<_, _, ()>("Prompt", &("",)).map_err(mapped)?;
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

    /// Unlocks `objects` (asking the user when the service needs to); the objects unlocked.
    fn unlock(&self, objects: &[OwnedObjectPath]) -> Result<Vec<OwnedObjectPath>, SecretError> {
        let (now, prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) = self
            .service()?
            .call("Unlock", &(objects,))
            .map_err(mapped)?;
        if prompt.as_str() == "/" {
            return Ok(now);
        }
        self.prompt(&prompt)?;
        Ok(objects.to_vec())
    }

    /// The default collection, unlocked; unavailable when there is none or it stays locked.
    fn default_collection(&self) -> Result<OwnedObjectPath, SecretError> {
        let collection: OwnedObjectPath = self
            .service()?
            .call("ReadAlias", &("default",))
            .map_err(mapped)?;
        if collection.as_str() == "/" {
            return Err(unavailable("there is no default keyring"));
        }
        let locked = |s: &Self| -> Result<bool, SecretError> {
            s.proxy(collection.as_str(), COLLECTION_IFACE)?
                .get_property::<bool>("Locked")
                .map_err(mapped)
        };
        if locked(self)? {
            self.unlock(std::slice::from_ref(&collection))?;
            if locked(self)? {
                return Err(unavailable("the keyring stays locked"));
            }
        }
        Ok(collection)
    }

    /// The items whose attributes match, unlocking locked ones.
    fn search(&self, attrs: &HashMap<&str, &str>) -> Result<Vec<OwnedObjectPath>, SecretError> {
        let (mut unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) = self
            .service()?
            .call("SearchItems", &(attrs,))
            .map_err(mapped)?;
        if !locked.is_empty() {
            unlocked.extend(self.unlock(&locked)?);
        }
        Ok(unlocked)
    }

    /// Calls `f` on `item`; an item reported locked is unlocked and retried once, an item
    /// that vanished meanwhile is `None`.
    fn on_item<T>(
        &self,
        item: &OwnedObjectPath,
        f: impl Fn(&Proxy<'_>) -> zbus::Result<T>,
    ) -> Result<Option<T>, SecretError> {
        let proxy = self.proxy(item.as_str(), ITEM_IFACE)?;
        let first = match f(&proxy) {
            Err(e) if is_locked(&e) => {
                self.unlock(std::slice::from_ref(item))?;
                f(&proxy)
            }
            other => other,
        };
        match first {
            Ok(v) => Ok(Some(v)),
            Err(e) if is_gone(&e) => Ok(None),
            Err(e) => Err(mapped(e)),
        }
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
        let collection = s.default_collection()?;
        let attrs = attributes(&key.namespace, Some(key.handle.as_str()));
        let label = format!("{SERVICE} credential {}", key.handle.as_str());
        let value: SecretStruct = (
            s.path.clone(),
            Vec::new(),
            secret.expose().as_bytes().to_vec(),
            "text/plain".into(),
        );
        let create = || -> zbus::Result<(OwnedObjectPath, OwnedObjectPath)> {
            let props: HashMap<&str, Value<'_>> = HashMap::from([
                (
                    "org.freedesktop.Secret.Item.Label",
                    Value::from(label.as_str()),
                ),
                (
                    "org.freedesktop.Secret.Item.Attributes",
                    Value::from(attrs.clone()),
                ),
            ]);
            Proxy::new(&s.conn, DEST, collection.as_str(), COLLECTION_IFACE)?
                .call("CreateItem", &(props, value.clone(), true))
        };
        // The collection may lock again between the unlock and the write: unlock once more.
        let created = match create() {
            Err(e) if is_locked(&e) => {
                s.unlock(std::slice::from_ref(&collection))?;
                create()
            }
            other => other,
        };
        let (_, prompt) = created.map_err(mapped)?;
        s.prompt(&prompt)
    }

    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
        let s = Session::open()?;
        let items = s.search(&attributes(&key.namespace, Some(key.handle.as_str())))?;
        let Some(item) = items.first() else {
            return Ok(None);
        };
        let Some((_, _, value, _)) = s.on_item(item, |p| {
            p.call::<_, _, SecretStruct>("GetSecret", &(&s.path,))
        })?
        else {
            return Ok(None);
        };
        String::from_utf8(value)
            .map(|v| Some(Secret::new(v)))
            .map_err(|_| SecretError::Failed("the item is not UTF-8".into()))
    }

    fn delete(&self, key: &SecretKey) -> Result<bool, SecretError> {
        let s = Session::open()?;
        let items = s.search(&attributes(&key.namespace, Some(key.handle.as_str())))?;
        let mut deleted = false;
        for item in &items {
            if let Some(prompt) =
                s.on_item(item, |p| p.call::<_, _, OwnedObjectPath>("Delete", &()))?
            {
                s.prompt(&prompt)?;
                deleted = true;
            }
        }
        Ok(deleted)
    }

    fn list(&self, namespace: &str) -> Result<Vec<Id>, SecretError> {
        let s = Session::open()?;
        let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) = s
            .service()?
            .call("SearchItems", &(attributes(namespace, None),))
            .map_err(mapped)?;
        let mut out = Vec::new();
        // Attributes are readable without unlocking, so listing never prompts.
        for item in unlocked.iter().chain(locked.iter()) {
            let Some(attrs) = s.on_item(item, |p| {
                p.get_property::<HashMap<String, String>>("Attributes")
            })?
            else {
                continue;
            };
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

    /// A locked default keyring is unlocked before an item is created (answer the desktop's
    /// unlock prompt when it appears), and dismissing the prompt reports the store unavailable.
    /// Opt-in and interactive (`--ignored`).
    #[test]
    #[ignore = "locks the real default keyring and needs the unlock prompt answered"]
    fn a_locked_keyring_is_unlocked_before_writing() {
        use super::*;
        let s = Session::open().expect("a Secret Service");
        let collection: OwnedObjectPath = s
            .service()
            .unwrap()
            .call("ReadAlias", &("default",))
            .unwrap();
        let lock = |s: &Session| {
            let (_, prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) = s
                .service()
                .unwrap()
                .call("Lock", &(vec![collection.clone()],))
                .unwrap();
            s.prompt(&prompt).unwrap();
        };
        lock(&s);
        let key = SecretKey::new(
            format!("devworks-test-{}", std::process::id()),
            Id::new("locked-write").unwrap(),
        );
        eprintln!("answer the unlock prompt to store the test item");
        SecretService
            .put(&key, &Secret::new("written after unlock"))
            .expect("the keyring is unlocked and the item created");
        assert_eq!(
            SecretService.get(&key).unwrap(),
            Some(Secret::new("written after unlock"))
        );
        assert!(SecretService.delete(&key).unwrap());
        lock(&s);
        eprintln!("now dismiss the unlock prompt");
        assert!(matches!(
            SecretService.put(&key, &Secret::new("x")),
            Err(SecretError::Unavailable(_))
        ));
    }
}
