//! Typed receipt ownership and replay-fence decisions. Raw receipt keys and
//! private results stay inside this reader and never become audit payloads.
use super::super::conn::db;
use super::classification::{Key, assign};
use devworks_core::fingerprint::Fingerprint;
use devworks_core::ports::storage::{
    ProjectPurgeCandidates, ProjectPurgeRowAction as Action, ProjectPurgeRowReason as Reason,
    ProjectPurgeRowWitness, ProjectPurgeUnresolved, StorageError,
};
use devworks_core::protocol::support::Revisioned;
use devworks_core::protocol::{
    CONTRACT, Check, ProjectId, Receipt,
    records::{ProjectPurgeManifest, PurgePhase, PurgedReceiptOutcome},
    results::{DomainDecision, OperationResult},
};
use devworks_core::registry::SchemaRevision;
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Envelope {
    contract: String,
    schema_revision: u32,
    #[serde(default)]
    receipt: Option<Receipt<DomainDecision>>,
    #[serde(default)]
    purged: Option<PurgedReceiptOutcome>,
}
fn current_valid(
    receipt: &Receipt<DomainDecision>,
    op: &str,
    revision: u32,
    digest: &str,
    operation: Option<&str>,
) -> bool {
    if receipt.request_digest != digest
        || receipt.operation_id.as_ref().map(|id| id.0.as_str()) != operation
        || receipt.command_id.check().is_err()
        || receipt.committed_sequence.check().is_err()
        || receipt.accepted_at_ms.check().is_err()
    {
        return false;
    }
    match &receipt.decision {
        DomainDecision::Applied { result } | DomainDecision::NoChange { result } => {
            result.operation() == op
                && result.check().is_ok()
                && result.check_revision(revision).is_ok()
        }
        DomainDecision::Accepted { operation_id } => {
            receipt.operation_id.as_ref() == Some(operation_id)
                && devworks_core::registry::find(op).is_some_and(|descriptor| descriptor.completion)
        }
        DomainDecision::Refused { fault } => fault.check().is_ok(),
    }
}
fn creator_snapshot(receipt: &Receipt<DomainDecision>, op: &str, project: &ProjectId) -> bool {
    matches!(&receipt.decision,DomainDecision::Applied{result:OperationResult::ProjectsCreate{value}} if op=="projects.create" && value.id==*project)
}
fn safe_manifest(value: &ProjectPurgeManifest, project: &ProjectId) -> bool {
    value.project_id == *project && value.check().is_ok() && value.error_code.is_none()
}
fn safe_subject(value: &Value) -> bool {
    if value.is_null() {
        return true;
    }
    let Some(fields) = value.as_object() else {
        return false;
    };
    fields.len() == 4
        && fields
            .keys()
            .all(|k| matches!(k.as_str(), "kind" | "id" | "dimension" | "revision"))
        && matches!(
            (value["kind"].as_str(), value["dimension"].as_str()),
            (Some("root"), Some("Work")) | (Some("operation"), Some("Operation"))
        )
        && serde_json::from_value::<devworks_core::protocol::Id>(value["id"].clone())
            .is_ok_and(|v| v.check().is_ok())
        && value["revision"].as_i64().is_some_and(|v| v > 0)
}
fn control_receipt(
    receipt: &Receipt<DomainDecision>,
    op: &str,
    revision: u32,
    project: &ProjectId,
) -> bool {
    if revision != 6 {
        return false;
    }
    match &receipt.decision {
        DomainDecision::Applied {
            result: OperationResult::ProjectsPreparePurge { value },
        } => {
            op == "projects.prepare_purge"
                && safe_manifest(value, project)
                && matches!(value.phase, PurgePhase::Prepared)
        }
        DomainDecision::Applied {
            result: OperationResult::ProjectsPurge { value },
        } => op == "projects.purge" && safe_manifest(value, project),
        _ => false,
    }
}

pub(super) fn partition(
    conn: &Connection,
    project: &ProjectId,
    snapshot: &ProjectPurgeCandidates,
    rows: &mut BTreeMap<Key, ProjectPurgeRowWitness>,
    unresolved: &mut BTreeMap<Key, ProjectPurgeUnresolved>,
) -> Result<(), StorageError> {
    let selection = super::candidates::selection(project)?;
    let candidates = snapshot
        .rows
        .iter()
        .map(|r| ((r.table.clone(), r.key_digest.clone()), r))
        .collect::<BTreeMap<_, _>>();
    let sql = selection.sql(
        "command_receipts",
        "actor_id,scope_key,idempotency_key,command_kind,payload_digest,result_json,operation_id",
        "1=1",
    )?;
    let mut statement = db(conn.prepare(&sql))?;
    let mut records = db(statement.query(selection.parameters()))?;
    let mut business_events = BTreeSet::new();
    let mut controls = BTreeSet::new();
    while let Some(row) = db(records.next())? {
        let actor = db(row.get::<_, String>(0))?;
        let scope = db(row.get::<_, String>(1))?;
        let key = db(row.get::<_, String>(2))?;
        let op = db(row.get::<_, String>(3))?;
        let digest = db(row.get::<_, String>(4))?;
        let encoded = db(row.get::<_, String>(5))?;
        let operation = db(row.get::<_, Option<String>>(6))?;
        let key_digest = super::guard::key_digest(
            &serde_json::to_string(&[&actor, &scope, &key]).expect("receipt key"),
        )?;
        let Some(candidate) = candidates
            .get(&("command_receipts".into(), key_digest))
            .copied()
        else {
            continue;
        };
        let Ok(body) = serde_json::from_str::<Envelope>(&encoded) else {
            continue;
        };
        if body.contract != CONTRACT
            || Fingerprint::from_hex(&digest).is_none()
            || !devworks_core::registry::find(&op).is_some_and(|d| {
                d.binding(devworks_core::registry::Mode::Command).is_some()
                    && d.revisions.contains(&SchemaRevision(body.schema_revision))
            })
        {
            continue;
        }
        if let (Some(purged), None) = (&body.purged, &body.receipt) {
            if purged.check().is_ok()
                && purged.request_digest.as_str() == digest
                && purged.operation_id.as_ref().map(|id| id.0.as_str()) == operation.as_deref()
            {
                assign(
                    candidate,
                    Action::Keep,
                    Reason::ReceiptReplayFence,
                    rows,
                    unresolved,
                );
            }
            continue;
        }
        let (Some(receipt), None) = (&body.receipt, &body.purged) else {
            continue;
        };
        if !current_valid(
            receipt,
            &op,
            body.schema_revision,
            &digest,
            operation.as_deref(),
        ) {
            continue;
        }
        type Event = (i64, String, Option<String>, String);
        let event:Option<Event>=db(conn.query_row("SELECT seq,actor_id,project_id,payload_json FROM events WHERE aggregate_kind='command' AND kind='CommandDecided' AND aggregate_id=?1",[receipt.command_id.0.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional())?;
        let Some((sequence, event_actor, event_project, payload)) = event else {
            continue;
        };
        let Ok(payload) = serde_json::from_str::<Value>(&payload) else {
            continue;
        };
        if sequence != receipt.committed_sequence.get()
            || event_actor != actor
            || payload["operation"].as_str() != Some(op.as_str())
            || payload["contract"].as_str() != Some(CONTRACT)
            || payload["schemaRevision"].as_u64() != Some(u64::from(body.schema_revision))
            || payload["requestDigest"].as_str() != Some(digest.as_str())
        {
            continue;
        }
        let owned = event_project.as_deref() == Some(project.0.as_str());
        if !owned && !creator_snapshot(receipt, &op, project) {
            continue;
        }
        let control = owned
            && control_receipt(receipt, &op, body.schema_revision, project)
            && safe_subject(&payload["subject"])
            && payload["decision"].as_str() == Some("Applied")
            && payload.as_object().is_some_and(|p| {
                p.keys().all(|k| {
                    matches!(
                        k.as_str(),
                        "operation"
                            | "contract"
                            | "schemaRevision"
                            | "requestDigest"
                            | "decision"
                            | "subject"
                    )
                })
            });
        assign(
            candidate,
            if control { Action::Keep } else { Action::Scrub },
            if control {
                Reason::PurgeControlAudit
            } else {
                Reason::ReceiptReplayFence
            },
            rows,
            unresolved,
        );
        if owned {
            if control {
                // The gateway's exact minimal publication intent is part of control
                // bookkeeping too. Other consumers or copied error text remain fenced.
                let mut queues=db(conn.prepare("SELECT id,payload_json FROM queue_entries WHERE consumer='receipt_replies' AND event_seq=?1 AND root_id IS NULL AND operation_id IS NULL AND partition_key IS NULL AND last_error IS NULL"))?;
                let intents = db(queues.query_map([sequence], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                }))?;
                for intent in intents {
                    let (queue_id, encoded) = db(intent)?;
                    let Ok(payload) = serde_json::from_str::<Value>(&encoded) else {
                        continue;
                    };
                    if payload
                        != serde_json::json!({"actorId":actor,"scopeKey":scope,"requestKey":key,"commandId":receipt.command_id})
                    {
                        continue;
                    }
                    let queue_key = super::guard::key_digest(
                        &serde_json::to_string(&[queue_id]).expect("queue key"),
                    )?;
                    if let Some(candidate) = candidates.get(&("queue_entries".into(), queue_key)) {
                        assign(
                            candidate,
                            Action::Keep,
                            Reason::PurgeControlAudit,
                            rows,
                            unresolved,
                        );
                    }
                }
                controls.insert(sequence);
            } else {
                business_events.insert(sequence);
            }
        }
    }
    let sql = selection.sql("events", "seq", "kind<>'CommandDecided'")?;
    let mut ordinary = db(conn.prepare(&sql))?;
    let mut ordinary = db(ordinary.query(selection.parameters()))?;
    while let Some(row) = db(ordinary.next())? {
        business_events.insert(db(row.get::<_, i64>(0))?);
    }
    let business_events = business_events
        .into_iter()
        .map(|seq| super::guard::key_digest(&serde_json::to_string(&[seq]).expect("event key")))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let controls = controls
        .into_iter()
        .map(|seq| super::guard::key_digest(&serde_json::to_string(&[seq]).expect("event key")))
        .collect::<Result<BTreeSet<_>, _>>()?;
    for candidate in snapshot.rows.iter().filter(|r| r.table == "events") {
        // Canonical event keys are their actual sequence, not the command ID.
        let control = controls.contains(&candidate.key_digest);
        let owned = business_events.contains(&candidate.key_digest);
        if control {
            assign(
                candidate,
                Action::Keep,
                Reason::PurgeControlAudit,
                rows,
                unresolved,
            );
        } else if owned {
            assign(
                candidate,
                Action::Delete,
                Reason::OwnedProjectRecords,
                rows,
                unresolved,
            );
        }
    }
    Ok(())
}
