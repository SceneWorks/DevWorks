//! Scoped work evidence. Summary rows carry the exact content hash required for a later read.
use super::invalid;
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::storage::{
    DreamRunRow, DreamTrailPosition, DreamTrailQuery, StorageError, WriteTransaction,
};
use devworks_core::services::dreaming::windows::{
    WindowPosition, sign_cursor, trail_timestamp, verify_owned_cursor,
};
use serde_json::{Value, json};

const TOOL: &str = "dream_work_trail";

pub(super) fn read(
    tx: &mut dyn WriteTransaction,
    run: &DreamRunRow,
    args: &Value,
    limit: u16,
) -> Result<Value, StorageError> {
    let key = run
        .checkpoint
        .as_ref()
        .and_then(|checkpoint| checkpoint.cursor_key.as_ref())
        .ok_or(StorageError::CursorExpired)?;
    let scope = run.scope.key();
    let (selection, before) = match args["cursor"].as_str() {
        Some(cursor) => {
            let position = verify_owned_cursor(tx.dreaming()?, run, TOOL, cursor)?;
            let selection = serde_json::from_str(&position.selection)
                .map_err(|_| StorageError::CursorExpired)?;
            let before = serde_json::from_str::<DreamTrailPosition>(&position.window)
                .map_err(|_| StorageError::CursorExpired)?;
            (selection, Some(before))
        }
        None => (args.clone(), None),
    };
    let id = selection["id"].as_str();
    let expected = selection["expected_hash"].as_str();
    if expected.is_some_and(|hash| {
        id.is_none() || hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Ok(json!({"error":"ExpectedHashRequiresExactEvidenceId"}));
    }
    let since_ms = match selection["since"].as_str().map(trail_timestamp).transpose() {
        Ok(since) => since,
        Err(_) => return Ok(json!({"error":"InvalidWorkTrailTimestamp"})),
    };
    let mut rows = match tx.dreaming()?.dream_trail(&DreamTrailQuery {
        scope: &run.scope,
        through_ms: run.started_at_ms,
        knowledge_through: run.cutoffs.knowledge,
        reviews_through: run.cutoffs.reviews,
        since_ms,
        kind: selection["kind"].as_str(),
        id,
        item_id: selection["item"].as_str(),
        before: before.as_ref(),
        offset: if before.is_none() {
            selection["offset"].as_u64().unwrap_or(0)
        } else {
            0
        },
        limit: limit + 1,
    }) {
        Ok(rows) => rows,
        Err(StorageError::InvalidRecord(_)) => {
            return Ok(json!({"error":"InvalidWorkTrailSelection"}));
        }
        Err(error) => return Err(error),
    };
    if id.is_some() && rows.is_empty() {
        return Ok(json!({"error":"WorkEvidenceUnavailable"}));
    }
    let more = rows.len() > usize::from(limit);
    rows.truncate(usize::from(limit));
    let next = match (more, rows.last()) {
        (true, Some(last)) => {
            let selection = devworks_core::jcs::canonicalize(&selection)
                .map_err(|_| invalid("invalid Dreaming trail selection"))?;
            let window = serde_json::to_string(&last.position)
                .map_err(|_| invalid("invalid Dreaming trail cursor"))?;
            Some(sign_cursor(
                key,
                &WindowPosition {
                    run: run.id.as_str().to_owned(),
                    scope,
                    tool: TOOL.into(),
                    selection,
                    window,
                    offset: 0,
                },
            )?)
        }
        _ => None,
    };
    let full = selection["detail"] == "full" || (selection["detail"].is_null() && id.is_some());
    let mut items = Vec::new();
    let mut proofs = Vec::new();
    for row in rows {
        let encoded = devworks_core::jcs::canonicalize(&row.content)
            .map_err(|_| invalid("invalid work evidence"))?;
        let hash = sha256_hex(encoded.as_bytes());
        if expected.is_some_and(|expected| !expected.eq_ignore_ascii_case(&hash)) {
            return Ok(json!({"error":"WorkEvidenceChanged"}));
        }
        let position = row.position;
        let mut item = json!({"id":position.id,"kind":position.kind,"item_id":row.item_id,"created_at_ms":position.at_ms,"content_hash":hash});
        if full {
            item["content"] = row.content;
            if id.is_some()
                && expected.is_some()
                && position.kind != "revision"
                && position.at_ms <= run.started_at_ms
            {
                proofs.push(json!({"kind":position.kind,"id":position.id,"fingerprint":hash}));
            }
        } else {
            item["read"] = json!({"tool":TOOL,"arguments":{
                "kind":position.kind,"id":position.id,"detail":"full","expected_hash":hash,
            }});
            let preview: String = encoded.chars().take(400).collect();
            item["preview_truncated"] = json!(preview.len() < encoded.len());
            item["preview"] = json!(preview);
        }
        items.push(item);
    }
    Ok(
        json!({"items":items,"has_more":more,"next_cursor":next,"proofs":proofs,"untrusted_data":true}),
    )
}
