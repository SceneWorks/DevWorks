//! A private cursor authorizes only an already selected window position. It carries no
//! gateway, source or mutation authority, and its signing key never enters a model prompt.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::ports::storage::{DreamRunRow, DreamingRead, StorageError};
use crate::protocol::Id;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WindowPosition {
    pub run: String,
    pub scope: String,
    pub tool: String,
    pub selection: String,
    pub window: String,
    pub offset: usize,
}

pub fn trail_timestamp(text: &str) -> Result<i64, StorageError> {
    text.parse::<jiff::Timestamp>()
        .map(|time| time.as_millisecond())
        .map_err(|_| StorageError::InvalidRecord("invalid work trail timestamp".into()))
}

pub fn cursor_key() -> Result<[u8; 32], StorageError> {
    let mut key = [0; 32];
    getrandom::fill(&mut key).map_err(|err| {
        StorageError::unavailable(
            crate::ports::storage::Unavailable::Io,
            format!("Dreaming cursor entropy: {err}"),
        )
    })?;
    Ok(key)
}

fn mac(key: &[u8; 32], payload: &[u8]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts a fixed key");
    mac.update(b"devworks-dreaming-cursor-v1\0");
    mac.update(payload);
    mac
}

pub fn sign_cursor(key: &[u8; 32], position: &WindowPosition) -> Result<String, StorageError> {
    let payload = serde_json::to_vec(position)
        .map_err(|_| StorageError::InvalidRecord("invalid Dreaming cursor position".into()))?;
    let tag = mac(key, &payload).finalize().into_bytes();
    Ok(format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(payload),
        URL_SAFE_NO_PAD.encode(tag)
    ))
}

pub fn verify_cursor(
    key: &[u8; 32],
    cursor: &str,
    run: &str,
    scope: &str,
    tool: &str,
) -> Result<WindowPosition, StorageError> {
    let position = decoded_cursor(key, cursor)?;
    if position.run != run || position.scope != scope || position.tool != tool {
        return Err(StorageError::CursorExpired);
    }
    Ok(position)
}

/// Restore a signed cursor only along the owner’s surviving continuation lineage.
pub fn verify_owned_cursor(
    repo: &dyn DreamingRead,
    run: &DreamRunRow,
    tool: &str,
    cursor: &str,
) -> Result<WindowPosition, StorageError> {
    let key = run
        .checkpoint
        .as_ref()
        .and_then(|checkpoint| checkpoint.cursor_key.as_ref())
        .ok_or(StorageError::CursorExpired)?;
    let position = decoded_cursor(key, cursor)?;
    if position.scope != run.scope.key() || position.tool != tool {
        return Err(StorageError::CursorExpired);
    }
    if position.run == run.id.as_str() {
        return Ok(position);
    }
    let mut previous = run.continued_from.clone();
    while let Some(id) = previous {
        let parent = repo
            .dream_run(&id)?
            .filter(|parent| parent.scope == run.scope)
            .ok_or(StorageError::CursorExpired)?;
        if parent.id.as_str() == position.run {
            return Ok(position);
        }
        previous = parent.continued_from;
    }
    Err(StorageError::CursorExpired)
}

pub fn prior_action(
    repo: &dyn DreamingRead,
    run: &DreamRunRow,
    index: i64,
) -> Result<crate::ports::storage::DreamActionRow, StorageError> {
    let mut next: Option<Id> = Some(run.id.clone());
    while let Some(id) = next {
        let row = repo
            .dream_run(&id)?
            .filter(|row| row.scope == run.scope)
            .ok_or(StorageError::ViewUnavailable)?;
        if let Some(action) = repo
            .dream_actions(&id, index.checked_add(1), 1)?
            .into_iter()
            .find(|action| action.position == index)
        {
            return Ok(action);
        }
        next = row.continued_from;
    }
    Err(StorageError::ViewUnavailable)
}

fn decoded_cursor(key: &[u8; 32], cursor: &str) -> Result<WindowPosition, StorageError> {
    if cursor.len() > 4096 {
        return Err(StorageError::CursorExpired);
    }
    let (payload, tag) = cursor.split_once('.').ok_or(StorageError::CursorExpired)?;
    let payload = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| StorageError::CursorExpired)?;
    let tag = URL_SAFE_NO_PAD
        .decode(tag)
        .map_err(|_| StorageError::CursorExpired)?;
    mac(key, &payload)
        .verify_slice(&tag)
        .map_err(|_| StorageError::CursorExpired)?;
    let position: WindowPosition =
        serde_json::from_slice(&payload).map_err(|_| StorageError::CursorExpired)?;
    Ok(position)
}

/// Preserve complete JSON without inventing a truncated object. The same byte offset can
/// resume only the original text digest, and each fragment ends at a UTF-8 boundary.
pub fn json_fragment(
    raw: &str,
    offset: usize,
    limit: usize,
) -> Result<(String, Option<usize>), StorageError> {
    if offset > raw.len() || !raw.is_char_boundary(offset) || limit < 4 {
        return Err(StorageError::CursorExpired);
    }
    let mut end = offset.saturating_add(limit).min(raw.len());
    while !raw.is_char_boundary(end) {
        end -= 1;
    }
    Ok((
        raw[offset..end].to_owned(),
        (end < raw.len()).then_some(end),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_requires_the_exact_run_scope_tool_and_authentication_tag() {
        let point = WindowPosition {
            run: "run-a".into(),
            scope: "project:a".into(),
            tool: "dream_read".into(),
            selection: "active".into(),
            window: "frozen-1".into(),
            offset: 17,
        };
        let key = [12; 32];
        let token = sign_cursor(&key, &point).unwrap();
        assert_eq!(
            verify_cursor(&key, &token, &point.run, &point.scope, &point.tool).unwrap(),
            point
        );
        assert!(verify_cursor(&key, &token, "run-b", &point.scope, &point.tool).is_err());
        assert!(verify_cursor(&key, &token, &point.run, "project:b", &point.tool).is_err());
        assert!(
            verify_cursor(&key, &token, &point.run, &point.scope, "dream_review_queue").is_err()
        );
        assert!(verify_cursor(&[13; 32], &token, &point.run, &point.scope, &point.tool).is_err());
    }

    #[test]
    fn fragments_round_trip_exact_multibyte_json() {
        let data = r#"{"description":"繁🦀é🌿"}"#;
        let mut result = String::new();
        let mut offset = Some(0);
        while let Some(next) = offset {
            let (text, following) = json_fragment(data, next, 5).unwrap();
            result.push_str(&text);
            offset = following;
        }
        assert_eq!(result, data);
    }
}
