//! Bounded lexical candidates from the frozen project lesson revisions.
use super::invalid;
use devworks_core::ports::storage::{
    DreamClaimRef, DreamClaimWindow, DreamRunRow, StorageError, WriteTransaction,
};
use devworks_core::protocol::Id;
use devworks_core::services::dreaming::windows::{
    WindowPosition, sign_cursor, verify_owned_cursor,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

const TOOL: &str = "dream_similar";
const PAIRS_PER_PAGE: u16 = 128;
const STOP: &str = "a an the to of in on for and or is are be been with when while it its this that these those as at by from into than then so if you your we our i my any each every all should must always do does use make";
const NEGATIONS: &str =
    "never not no dont doesnt cant cannot shouldnt mustnt wont avoid without nor stop isnt arent";

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
    let (mut left, mut right, mut offset) = match args["cursor"].as_str() {
        Some(cursor) => {
            let position = verify_owned_cursor(tx.dreaming()?, run, TOOL, cursor)?;
            serde_json::from_str(&position.window).map_err(|_| StorageError::CursorExpired)?
        }
        None => (None, None, args["offset"].as_u64().unwrap_or(0)),
    };
    let mut out = Vec::new();
    let mut examined = 0u16;
    let next = loop {
        let current = lesson_refs(tx, run, None, left.as_ref(), 1)?
            .into_iter()
            .next();
        let Some(current) = current else { break None };
        let other = right.as_ref().unwrap_or(&current.id);
        if examined >= PAIRS_PER_PAGE || out.len() >= usize::from(limit) {
            break Some((Some(current.id), right, offset));
        }
        let max = PAIRS_PER_PAGE - examined;
        let next_right = lesson_refs(tx, run, Some(other), None, max)?;
        let reached_end = next_right.len() < usize::from(max);
        let Some(summary) = lesson_text(tx, &current)? else {
            let next_left = lesson_refs(tx, run, Some(&current.id), None, 1)?
                .into_iter()
                .next();
            let Some(next) = next_left else { break None };
            left = Some(next.id);
            right = None;
            examined += 1;
            continue;
        };
        let (words, negated) = words(&summary);
        let links = tx
            .knowledge()
            .knowledge_relations_touching(std::slice::from_ref(&current.id))?;
        for candidate in next_right {
            if out.len() >= usize::from(limit) {
                break;
            }
            examined += 1;
            right = Some(candidate.id.clone());
            if links.iter().any(|relation| {
                relation.scope == run.scope
                    && relation.created_at_ms <= run.started_at_ms
                    && (relation.subject_id == candidate.id || relation.object_id == candidate.id)
            }) {
                continue;
            }
            let Some(other) = lesson_text(tx, &candidate)? else {
                continue;
            };
            let (other_words, other_negated) = self::words(&other);
            let common = words.intersection(&other_words).count();
            let all = words.len() + other_words.len() - common;
            if all == 0 {
                continue;
            }
            let score = ((common as f64 / all as f64) * 1000.0).round_ties_even() / 1000.0;
            if score < 0.34 {
                continue;
            }
            if offset != 0 {
                offset -= 1;
                continue;
            }
            out.push(json!({"left":current.id,"right":candidate.id,
                "left_version":current.revision,"right_version":candidate.revision,
                "left_summary":summary,"right_summary":other,"score":score,
                "possible_contradiction":negated != other_negated}));
        }
        if out.len() >= usize::from(limit) || !reached_end {
            break Some((Some(current.id), right, offset));
        }
        let next_left = lesson_refs(tx, run, Some(&current.id), None, 1)?
            .into_iter()
            .next();
        let Some(next) = next_left else { break None };
        left = Some(next.id);
        right = None;
        examined += 1;
    };
    let following = next
        .map(|position| {
            sign_cursor(
                key,
                &WindowPosition {
                    run: run.id.as_str().into(),
                    scope,
                    tool: TOOL.into(),
                    selection: "lesson-pairs".into(),
                    window: serde_json::to_string(&position)
                        .map_err(|_| invalid("invalid similarity continuation"))?,
                    offset: 0,
                },
            )
        })
        .transpose()?;
    Ok(json!({"items":out,"has_more":following.is_some(),"next_cursor":following}))
}

fn lesson_refs(
    tx: &mut dyn WriteTransaction,
    run: &DreamRunRow,
    after: Option<&Id>,
    exact: Option<&Id>,
    limit: u16,
) -> Result<Vec<DreamClaimRef>, StorageError> {
    tx.dreaming()?.dream_claim_refs(&DreamClaimWindow {
        scope: &run.scope,
        through: run.cutoffs.knowledge,
        after,
        exact,
        kind: Some("Lesson"),
        status: Some("Active"),
        possibly_stale: None,
        lessons: None,
        delivered_since_ms: 0,
        captured_at_ms: run.started_at_ms,
        limit,
    })
}

fn words(summary: &str) -> (BTreeSet<String>, bool) {
    let cleaned = summary.to_lowercase().replace(['\'', '’'], "");
    let stop = |token: &str| STOP.split_ascii_whitespace().any(|word| word == token);
    let negation = |token: &str| NEGATIONS.split_ascii_whitespace().any(|word| word == token);
    let mut negated = false;
    let words = cleaned
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty() && !stop(word))
        .filter_map(|word| {
            if negation(word) {
                negated = true;
                return None;
            }
            let stem = ["ing", "ed", "es", "s"]
                .iter()
                .find_map(|suffix| {
                    word.strip_suffix(suffix)
                        .filter(|prefix| prefix.chars().count() > 3)
                })
                .unwrap_or(word);
            Some(stem.to_owned())
        })
        .collect();
    (words, negated)
}

fn lesson_text(
    tx: &mut dyn WriteTransaction,
    item: &DreamClaimRef,
) -> Result<Option<String>, StorageError> {
    let row = tx
        .knowledge()
        .knowledge_history_at(&item.id, item.revision)?;
    Ok(row.and_then(|row| row.snapshot["value"].as_str().map(str::to_owned)))
}
