//! A secret split over several fixed-size entries (the Windows Credential Manager holds at most
//! 2560 bytes per credential). The first entry (`<base>`) starts with a two-byte header (format
//! 1, part count) and later parts are `<base>/part<i>`. Later parts are written first and the
//! first entry last, so a reader never sees a header naming parts that do not exist yet; a
//! delete removes the first entry first, so a partial delete reads as absent.
//!
//! An interrupted write or delete can leave `/part<i>` entries without their first entry. They
//! are never lost: [`list`] reports them under their base handle, and [`delete`] removes every
//! part it finds by enumeration (never by the header alone), so reconciliation removes them.
use super::handle_of;
use devworks_core::ports::secrets::SecretError;
use devworks_core::protocol::Id;
use std::collections::BTreeSet;

/// The single-entry operations of a facility.
pub(crate) trait Entries {
    fn write(&self, name: &str, blob: &[u8]) -> Result<(), SecretError>;
    /// The blob of `name`, or `None` when it does not exist.
    fn read(&self, name: &str) -> Result<Option<Vec<u8>>, SecretError>;
    /// Deletes `name`; `false` when it did not exist.
    fn remove(&self, name: &str) -> Result<bool, SecretError>;
    /// The names of every entry starting with `prefix`.
    fn names(&self, prefix: &str) -> Result<Vec<String>, SecretError>;
}

const FORMAT: u8 = 1;
const HEADER: usize = 2;
const MAX_PARTS: usize = 255;

pub(crate) fn part(base: &str, i: usize) -> String {
    format!("{base}/part{i}")
}

/// The part numbers present under `base`.
fn parts_of(e: &dyn Entries, base: &str) -> Result<Vec<usize>, SecretError> {
    let prefix = format!("{base}/part");
    Ok(e.names(&prefix)?
        .iter()
        .filter_map(|n| n.strip_prefix(&prefix)?.parse::<usize>().ok())
        .filter(|i| *i >= 1)
        .collect())
}

pub(crate) fn put(
    e: &dyn Entries,
    base: &str,
    bytes: &[u8],
    blob: usize,
) -> Result<(), SecretError> {
    let first = (blob - HEADER).min(bytes.len());
    let rest: Vec<&[u8]> = bytes[first..].chunks(blob).collect();
    if rest.len() + 1 > MAX_PARTS {
        return Err(SecretError::TooLarge);
    }
    for (i, chunk) in rest.iter().enumerate() {
        e.write(&part(base, i + 1), chunk)?;
    }
    let mut head = Vec::with_capacity(HEADER + first);
    head.push(FORMAT);
    head.push((rest.len() + 1) as u8);
    head.extend_from_slice(&bytes[..first]);
    let written = e.write(base, &head);
    head.iter_mut().for_each(|b| *b = 0);
    written?;
    // Parts a longer previous value (or an interrupted write) left beyond this one.
    for i in parts_of(e, base)? {
        if i > rest.len() {
            e.remove(&part(base, i))?;
        }
    }
    Ok(())
}

pub(crate) fn get(e: &dyn Entries, base: &str) -> Result<Option<Vec<u8>>, SecretError> {
    let Some(head) = e.read(base)? else {
        return Ok(None);
    };
    if head.len() < HEADER || head[0] != FORMAT || head[1] == 0 {
        return Err(SecretError::Failed("unrecognized credential format".into()));
    }
    let mut bytes = head[HEADER..].to_vec();
    for i in 1..head[1] as usize {
        match e.read(&part(base, i))? {
            Some(chunk) => bytes.extend_from_slice(&chunk),
            None => return Err(SecretError::Failed("a credential part is missing".into())),
        }
    }
    Ok(Some(bytes))
}

/// Removes the first entry, then every part present (found by enumeration, so parts left by an
/// interrupted write or delete go too). `true` when anything existed.
pub(crate) fn delete(e: &dyn Entries, base: &str) -> Result<bool, SecretError> {
    let mut existed = e.remove(base)?;
    for i in parts_of(e, base)? {
        existed |= e.remove(&part(base, i))?;
    }
    Ok(existed)
}

/// The handles of `namespace` among `accounts` (`<namespace>/<handle>` or
/// `<namespace>/<handle>/part<i>`): a part without its first entry is reported under its base
/// handle, so reconciliation can delete it.
pub(crate) fn handles<'a>(accounts: impl IntoIterator<Item = &'a str>, namespace: &str) -> Vec<Id> {
    let mut out = BTreeSet::new();
    for account in accounts {
        let base = match account.rsplit_once("/part") {
            Some((base, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => base,
            _ => account,
        };
        if let Some(h) = handle_of(base, namespace) {
            out.insert(h.as_str().to_owned());
        }
    }
    out.into_iter().filter_map(|h| Id::new(h).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// An in-memory facility that can fail its `n`th write or remove (an interruption).
    #[derive(Default)]
    struct Memory {
        items: Mutex<BTreeMap<String, Vec<u8>>>,
        fail_after: Mutex<Option<usize>>,
    }
    impl Memory {
        fn step(&self) -> Result<(), SecretError> {
            let mut f = self.fail_after.lock().unwrap();
            match f.as_mut() {
                Some(0) => Err(SecretError::Failed("interrupted".into())),
                Some(n) => {
                    *n -= 1;
                    Ok(())
                }
                None => Ok(()),
            }
        }
        fn names_all(&self) -> Vec<String> {
            self.items.lock().unwrap().keys().cloned().collect()
        }
    }
    impl Entries for Memory {
        fn write(&self, name: &str, blob: &[u8]) -> Result<(), SecretError> {
            self.step()?;
            self.items
                .lock()
                .unwrap()
                .insert(name.into(), blob.to_vec());
            Ok(())
        }
        fn read(&self, name: &str) -> Result<Option<Vec<u8>>, SecretError> {
            Ok(self.items.lock().unwrap().get(name).cloned())
        }
        fn remove(&self, name: &str) -> Result<bool, SecretError> {
            self.step()?;
            Ok(self.items.lock().unwrap().remove(name).is_some())
        }
        fn names(&self, prefix: &str) -> Result<Vec<String>, SecretError> {
            Ok(self
                .items
                .lock()
                .unwrap()
                .keys()
                .filter(|k| k.starts_with(prefix))
                .cloned()
                .collect())
        }
    }
    const BASE: &str = "DevWorks/ns/h1";
    const BLOB: usize = 8;

    fn accounts(m: &Memory) -> Vec<Id> {
        let names = m.names_all();
        handles(
            names.iter().filter_map(|n| n.strip_prefix("DevWorks/")),
            "ns",
        )
    }

    #[test]
    fn values_round_trip_over_parts_and_shrink_cleanly() {
        let m = Memory::default();
        put(&m, BASE, b"0123456789abcdefghijklmnop", BLOB).unwrap();
        assert_eq!(
            get(&m, BASE).unwrap().unwrap(),
            b"0123456789abcdefghijklmnop"
        );
        put(&m, BASE, b"short", BLOB).unwrap();
        assert_eq!(get(&m, BASE).unwrap().unwrap(), b"short");
        assert_eq!(m.names_all(), [BASE]);
    }

    #[test]
    fn an_interrupted_write_leaves_parts_that_are_listed_and_deleted() {
        let m = Memory::default();
        // Parts are written, the first entry is not.
        *m.fail_after.lock().unwrap() = Some(2);
        assert!(put(&m, BASE, b"0123456789abcdefghijklmnop", BLOB).is_err());
        *m.fail_after.lock().unwrap() = None;
        assert_eq!(
            get(&m, BASE).unwrap(),
            None,
            "absent without its first entry"
        );
        assert_eq!(
            accounts(&m),
            [Id::new("h1").unwrap()],
            "the orphan parts are listed"
        );
        assert!(delete(&m, BASE).unwrap());
        assert!(m.names_all().is_empty(), "{:?}", m.names_all());
    }

    #[test]
    fn a_retried_delete_removes_every_part_without_the_first_entry() {
        let m = Memory::default();
        put(&m, BASE, b"0123456789abcdefghijklmnopqrstuvwxyz", BLOB).unwrap();
        assert!(m.names_all().len() > 3);
        // The first entry and one part go; the rest remain.
        *m.fail_after.lock().unwrap() = Some(2);
        assert!(delete(&m, BASE).is_err());
        *m.fail_after.lock().unwrap() = None;
        assert!(m.names_all().len() > 1);
        assert_eq!(accounts(&m), [Id::new("h1").unwrap()]);
        assert!(delete(&m, BASE).unwrap());
        assert!(m.names_all().is_empty(), "{:?}", m.names_all());
        assert!(!delete(&m, BASE).unwrap());
    }

    #[test]
    fn only_numbered_parts_map_to_their_base_handle() {
        let listed = handles(
            [
                "ns/h1",
                "ns/h2/part3",
                "ns/h3/partx",
                "other/h4/part1",
                "ns/h1/part1",
            ],
            "ns",
        );
        let ids: Vec<&str> = listed.iter().map(Id::as_str).collect();
        assert_eq!(ids, ["h1", "h2"]);
    }
}
