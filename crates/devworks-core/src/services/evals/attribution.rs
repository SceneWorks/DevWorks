//! Historical evaluation attribution, exclusively from host-recorded transcript sources.
//!
//! A manifest attests the complete effective evaluation configuration, including absent
//! categories. The host records it with its source set BEFORE the steps it governs. Session
//! batches record their effective source boundary. Historical sources stamped at the end of a
//! batch cannot prove the revisions of earlier steps in that batch. Never substitute present
//! configuration or an assignment's mutable revisions.
use crate::fingerprint::sha256_hex;
use crate::ports::storage::{
    SessionRead, StorageError, StoredContextSource, StoredEntry, StoredSession,
};
use crate::protocol::records::{ContextSource, EvalRevisionContext};
use crate::protocol::support::{BoundedList, BoundedText};
use crate::protocol::{Counter, Id, Revision};
use crate::services::sessions::transcript::{ContextSourceInput, SourceDisposition};

pub const MANIFEST_KIND: &str = "EvaluationRevisionManifest";
pub const REVISION_CATEGORIES: [&str; 7] = [
    "eval.skill",
    "eval.role",
    "eval.override",
    "eval.convention",
    "eval.rule",
    "eval.routing",
    "eval.app",
];
const MAX_SOURCES: usize = 127;

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
fn relevant(kind: &str) -> bool {
    kind.starts_with("eval.")
}
fn digest(sources: &[ContextSourceInput]) -> Result<String, StorageError> {
    let value = serde_json::to_value(sources).map_err(|e| invalid(&e.to_string()))?;
    let canonical = crate::jcs::canonicalize(&value).map_err(|e| invalid(&e.to_string()))?;
    Ok(sha256_hex(canonical.as_bytes()))
}
fn complete(sources: &[ContextSourceInput]) -> bool {
    !sources.is_empty()
        && sources.len() <= MAX_SOURCES
        && sources.iter().all(|s| {
            relevant(&s.kind)
                && s.kind.len() <= 220
                && s.source_revision > 0
                && s.token_estimate >= 0
                && s.disposition == SourceDisposition::Included
                && s.artifact.as_ref().is_none_or(|a| a.validate().is_ok())
                && (!s.source_id.as_str().starts_with("none-eval-")
                    || (s.source_id.as_str() == format!("none-{}", s.kind.replace('.', "-"))
                        && s.source_revision == 1
                        && s.token_estimate == 0
                        && s.artifact.is_none()
                        && sources.iter().filter(|p| p.kind == s.kind).count() == 1))
        })
        && REVISION_CATEGORIES
            .iter()
            .all(|kind| sources.iter().any(|s| s.kind == *kind))
        && !sources.iter().enumerate().any(|(i, s)| {
            sources[..i]
                .iter()
                .any(|p| p.kind == s.kind && p.source_id == s.source_id)
        })
}

/// Build the host-only completeness witness for a complete ordered evaluation source set.
/// Supply every effective skill, role, override, convention, rule, route and app revision.
/// Multiple sources per category are supported. An absent category must be explicitly declared
/// as Included, revision 1, zero tokens, no artifact, ID `none-eval-skill` (and analogous IDs).
/// `Omitted` and `Truncated` describe lost evidence, never absence. This helper attests the
/// caller's complete set; only the host SessionWriter can append sources, never public input.
/// Append the returned marker AFTER these sources in the same boundary batch.
pub fn revision_manifest(
    sources: &[ContextSourceInput],
) -> Result<ContextSourceInput, StorageError> {
    if !complete(sources) {
        return Err(invalid(
            "evaluation revision manifest requires a complete Included source set",
        ));
    }
    Ok(ContextSourceInput {
        source_id: Id::new(digest(sources)?).map_err(invalid)?,
        kind: MANIFEST_KIND.into(),
        source_revision: 1,
        artifact: None,
        token_estimate: 0,
        disposition: SourceDisposition::Included,
    })
}

fn input(source: &StoredContextSource) -> Result<ContextSourceInput, StorageError> {
    Ok(ContextSourceInput {
        source_id: source.source_id.clone(),
        kind: source.kind.clone(),
        source_revision: source.source_revision,
        artifact: source
            .artifact
            .clone()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| invalid("invalid stored evaluation source artifact"))?,
        token_estimate: source.token_estimate,
        disposition: match source.disposition.as_str() {
            "Included" => SourceDisposition::Included,
            "Truncated" => SourceDisposition::Truncated,
            "Omitted" => SourceDisposition::Omitted,
            _ => return Err(invalid("invalid stored source disposition")),
        },
    })
}
fn wire(source: &ContextSourceInput) -> Result<ContextSource, StorageError> {
    Ok(ContextSource {
        id: source.source_id.clone(),
        kind: BoundedText(match source.disposition {
            SourceDisposition::Included => source.kind.clone(),
            other => format!("{}:{}", source.kind, other.as_str().to_ascii_lowercase()),
        }),
        source_revision: Revision::new(source.source_revision).map_err(invalid)?,
        artifact: source.artifact.clone(),
        token_estimate: Counter::new(source.token_estimate).map_err(invalid)?,
    })
}

/// Attribute one recorded step. Reading all epochs or present configuration would create false
/// evidence: use only this step's epoch and source records effective at/before its sequence.
pub fn revision_context(
    read: &dyn SessionRead,
    session: &StoredSession,
    step: &StoredEntry,
) -> Result<EvalRevisionContext, StorageError> {
    if step.session_id != session.id
        || step.sequence < 1
        || step.sequence > session.last_durable_sequence()
    {
        return Err(invalid(
            "evaluation step does not belong to the recorded session",
        ));
    }
    let mut rows = Vec::new();
    let mut after = 0;
    loop {
        let page = read.context_sources(&session.id, step.context_epoch, after, 500)?;
        if page.is_empty() {
            break;
        }
        let next = page.last().expect("nonempty").ordinal;
        if next <= after {
            return Err(invalid("context source pagination did not advance"));
        }
        let more = page.len() == 500;
        rows.extend(page.into_iter().filter(|s| {
            s.sequence <= step.sequence && (relevant(&s.kind) || s.kind == MANIFEST_KIND)
        }));
        if rows.len() > 4096 {
            let inputs: Vec<_> = rows
                .iter()
                .filter(|s| relevant(&s.kind))
                .take(128)
                .map(input)
                .collect::<Result<_, _>>()?;
            return Ok(EvalRevisionContext::Mixed {
                context_epoch: Counter::new(step.context_epoch).map_err(invalid)?,
                sources: BoundedList(inputs.iter().map(wire).collect::<Result<_, _>>()?),
                reason: BoundedText("historical evaluation revision evidence exceeds the 4096-record attribution read bound".into()),
            });
        }
        after = next;
        if !more {
            break;
        }
    }
    evaluate(&rows, step.context_epoch)
}

fn evaluate(rows: &[StoredContextSource], epoch: i64) -> Result<EvalRevisionContext, StorageError> {
    let context_epoch = Counter::new(epoch).map_err(invalid)?;
    let marker = rows.iter().rev().find(|s| s.kind == MANIFEST_KIND);
    let mut reason = "missing complete historical evaluation revision manifest";
    let selected: Vec<_> = match marker {
        Some(m) => rows
            .iter()
            .filter(|s| relevant(&s.kind) && s.sequence >= m.sequence)
            .collect(),
        None => rows.iter().filter(|s| relevant(&s.kind)).collect(),
    };
    let inputs: Vec<_> = selected
        .iter()
        .map(|s| input(s))
        .collect::<Result<_, _>>()?;
    if let Some(m) = marker {
        let boundary = rows
            .iter()
            .filter(|s| s.kind == MANIFEST_KIND && s.sequence == m.sequence)
            .count();
        let ordered = selected
            .iter()
            .all(|s| s.sequence == m.sequence && s.ordinal < m.ordinal);
        if boundary == 1
            && ordered
            && complete(&inputs)
            && m.source_revision == 1
            && m.disposition == "Included"
            && m.artifact.is_none()
            && m.token_estimate == 0
            && m.source_id.as_str() == digest(&inputs)?
        {
            return Ok(EvalRevisionContext::Exact {
                context_epoch,
                sources: BoundedList(inputs.iter().map(wire).collect::<Result<_, _>>()?),
            });
        }
        reason = "historical evaluation revisions are incomplete, truncated, changed without a manifest, or have a mismatched manifest";
    }
    // Preserve the bounded evidence, explicitly reporting incomplete coverage on overflow.
    if inputs.len() > 128 {
        reason = "historical evaluation revision evidence exceeds the 128-source result bound";
    }
    Ok(EvalRevisionContext::Mixed {
        context_epoch,
        sources: BoundedList(
            inputs
                .iter()
                .take(128)
                .map(wire)
                .collect::<Result<_, _>>()?,
        ),
        reason: BoundedText(reason.into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sources() -> Vec<ContextSourceInput> {
        REVISION_CATEGORIES
            .iter()
            .map(|kind| ContextSourceInput {
                source_id: Id::new(format!("none-{}", kind.replace('.', "-"))).unwrap(),
                kind: (*kind).into(),
                source_revision: 1,
                artifact: None,
                token_estimate: 0,
                disposition: SourceDisposition::Included,
            })
            .collect()
    }
    fn row(source: &ContextSourceInput, ordinal: i64, sequence: i64) -> StoredContextSource {
        StoredContextSource {
            session_id: Id::new("session").unwrap(),
            epoch: 2,
            ordinal,
            source_id: source.source_id.clone(),
            kind: source.kind.clone(),
            source_revision: source.source_revision,
            artifact: None,
            token_estimate: source.token_estimate,
            disposition: source.disposition.as_str().into(),
            sequence,
        }
    }
    fn recorded() -> Vec<StoredContextSource> {
        let sources = sources();
        let marker = revision_manifest(&sources).unwrap();
        let mut rows: Vec<_> = sources
            .iter()
            .enumerate()
            .map(|(i, s)| row(s, i as i64 + 1, 10))
            .collect();
        rows.push(row(&marker, 8, 10));
        rows
    }
    #[test]
    fn complete_absence_declarations_are_exact() {
        assert!(matches!(
            evaluate(&recorded(), 2).unwrap(),
            EvalRevisionContext::Exact { .. }
        ));
    }
    #[test]
    fn absent_marker_and_digest_tampering_are_mixed() {
        let mut rows = recorded();
        rows.pop();
        assert!(matches!(
            evaluate(&rows, 2).unwrap(),
            EvalRevisionContext::Mixed { .. }
        ));
        let mut rows = recorded();
        rows[0].source_revision = 2;
        assert!(matches!(
            evaluate(&rows, 2).unwrap(),
            EvalRevisionContext::Mixed { .. }
        ));
    }
    #[test]
    fn missing_categories_and_omissions_cannot_be_attested() {
        let mut incomplete = sources();
        incomplete.pop();
        assert!(revision_manifest(&incomplete).is_err());
        let mut sources = sources();
        sources[0].disposition = SourceDisposition::Omitted;
        assert!(revision_manifest(&sources).is_err());
    }
    #[test]
    fn later_unmanifested_changes_are_mixed_but_complete_replacement_is_exact() {
        let mut rows = recorded();
        rows.push(row(&sources()[0], 9, 11));
        assert!(matches!(
            evaluate(&rows, 2).unwrap(),
            EvalRevisionContext::Mixed { .. }
        ));
        let mut replacement = recorded();
        for r in &mut replacement {
            r.sequence = 20;
            r.ordinal += 10;
        }
        rows.extend(replacement);
        assert!(matches!(
            evaluate(&rows, 2).unwrap(),
            EvalRevisionContext::Exact { .. }
        ));
    }
    #[test]
    fn multiple_sources_in_one_category_are_preserved() {
        let mut sources = sources();
        sources[0].source_id = Id::new("first-skill").unwrap();
        let mut extra = sources[0].clone();
        extra.source_id = Id::new("another-skill").unwrap();
        sources.push(extra);
        let marker = revision_manifest(&sources).unwrap();
        let mut rows: Vec<_> = sources
            .iter()
            .enumerate()
            .map(|(i, s)| row(s, i as i64 + 1, 10))
            .collect();
        rows.push(row(&marker, 9, 10));
        match evaluate(&rows, 2).unwrap() {
            EvalRevisionContext::Exact { sources, .. } => assert_eq!(sources.0.len(), 8),
            _ => panic!("expected exact"),
        }
    }
}
