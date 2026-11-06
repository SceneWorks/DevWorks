//! Maintained app documentation embedded in the exact executable, never a checkout/runtime URL.
use crate::fingerprint::sha256_hex;
use crate::ports::storage::StorageError;
use crate::protocol::Id;
use serde::Deserialize;
use std::collections::BTreeSet;

include!(concat!(env!("OUT_DIR"), "/help_bundle.rs"));

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Entry {
    id: String,
    title: String,
    file: String,
    screen_ids: Vec<String>,
    settings_sections: Vec<String>,
    related_capability_ids: Vec<String>,
}

#[derive(Clone, Debug)]
pub(super) struct Topic {
    pub id: Id,
    pub title: String,
    pub body: String,
    pub screens: Vec<String>,
    pub operations: Vec<String>,
    pub digest: String,
}

pub(super) fn load() -> Result<Vec<Topic>, StorageError> {
    let entries: Vec<Entry> = serde_json::from_str(CATALOG)
        .map_err(|_| StorageError::InvalidRecord("bundled Help catalog is invalid".into()))?;
    let mut ids = BTreeSet::new();
    let mut topics = Vec::new();
    for entry in entries {
        let id = Id::new(&entry.id)
            .map_err(|_| StorageError::InvalidRecord("invalid Help topic ID".into()))?;
        if !ids.insert(id.as_str().to_owned())
            || entry.title.trim().is_empty()
            || entry.title.chars().count() > 256
            || entry.title.contains('\0')
        {
            return Err(StorageError::InvalidRecord(
                "duplicate or invalid bundled Help topic".into(),
            ));
        }
        let body = FILES
            .iter()
            .find(|(name, _)| *name == entry.file)
            .map(|(_, body)| *body)
            .ok_or_else(|| StorageError::InvalidRecord("bundled Help file is missing".into()))?;
        if body.is_empty() || body.contains('\0') {
            return Err(StorageError::InvalidRecord(
                "invalid bundled Help body".into(),
            ));
        }
        let mut screens = entry.screen_ids;
        screens.extend(entry.settings_sections);
        screens.sort();
        screens.dedup();
        for screen in &screens {
            Id::new(screen)
                .map_err(|_| StorageError::InvalidRecord("invalid Help screen ID".into()))?;
        }
        topics.push(Topic {
            id,
            title: entry.title,
            body: body.to_owned(),
            screens,
            operations: entry.related_capability_ids,
            digest: sha256_hex(body.as_bytes()),
        });
    }
    topics.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    Ok(topics)
}
