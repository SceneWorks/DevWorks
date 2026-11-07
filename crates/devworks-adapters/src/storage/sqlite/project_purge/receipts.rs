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
use rusqlite::{Connection, OptionalExtension, params_from_iter, types::Value as SqlValue};
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

pub(super) fn compact(
    conn: &Connection,
    predicate: &str,
    keys: &[SqlValue],
    purge: &devworks_core::protocol::Id,
) -> Result<(), StorageError> {
    use devworks_core::protocol::records::PurgedDecisionKind;
    let encoded: String = db(conn.query_row(
        &format!("SELECT result_json FROM command_receipts WHERE {predicate}"),
        params_from_iter(keys.iter()),
        |r| r.get(0),
    ))?;
    let body: Envelope = serde_json::from_str(&encoded)
        .map_err(|_| StorageError::InvalidRecord("purge receipt body".into()))?;
    if body.purged.is_some() {
        return Ok(());
    }
    let receipt = body.receipt.ok_or(StorageError::ScopeDenied)?;
    let (decision, fault) = match &receipt.decision {
        DomainDecision::Applied { .. } => (PurgedDecisionKind::Applied, None),
        DomainDecision::NoChange { .. } => (PurgedDecisionKind::NoChange, None),
        DomainDecision::Accepted { .. } => (PurgedDecisionKind::Accepted, None),
        DomainDecision::Refused { fault } => (PurgedDecisionKind::Refused, Some(fault.code)),
    };
    let purged = PurgedReceiptOutcome {
        command_id: receipt.command_id,
        committed_sequence: receipt.committed_sequence,
        accepted_at_ms: receipt.accepted_at_ms,
        request_digest: receipt
            .request_digest
            .try_into()
            .map_err(|_| StorageError::ScopeDenied)?,
        operation_id: receipt.operation_id,
        original_decision: decision,
        original_fault_code: fault,
        purge_id: purge.clone(),
    };
    purged.check().map_err(|_| StorageError::ScopeDenied)?;
    let mut bindings = keys.to_vec();
    let parameter = bindings.len() + 1;
    bindings.push(SqlValue::Text(serde_json::json!({"contract":body.contract,"schemaRevision":body.schema_revision,"purged":purged}).to_string()));
    if db(conn.execute(
        &format!("UPDATE command_receipts SET result_json=?{parameter} WHERE {predicate}"),
        params_from_iter(bindings.iter()),
    ))? != 1
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
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

fn published_reply(
    conn: &Connection,
    identity: [&str; 3],
    receipt: &Receipt<DomainDecision>,
    op: &str,
    revision: u32,
    control: Option<&ProjectId>,
) -> Result<Vec<Key>, StorageError> {
    let Some(descriptor) = devworks_core::registry::find(op) else {
        return Ok(vec![]);
    };
    let Some(schema) = devworks_core::services::artifacts::replies::reply_schema(
        descriptor,
        SchemaRevision(revision),
    ) else {
        return Ok(vec![]);
    };
    type Published = (String, String, String, String);
    let raw:Option<Published>=db(conn.query_row("SELECT r.command_id,r.artifact_id,r.version_id,r.schema_json FROM receipt_replies r JOIN artifacts a ON a.id=r.artifact_id JOIN artifact_versions v ON v.id=r.version_id AND v.artifact_id=a.id WHERE r.actor_id=?1 AND r.scope_key=?2 AND r.idempotency_key=?3 AND a.retention='Receipt' AND a.owner_actor_id=r.actor_id AND a.current_version_id=r.version_id AND v.version_no=1 AND v.media_type='application/json' AND NOT EXISTS(SELECT 1 FROM artifact_versions other WHERE other.artifact_id=a.id AND other.id<>v.id)",identity,|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional())?;
    let Some((command, artifact, version, encoded)) = raw else {
        return Ok(vec![]);
    };
    if command != receipt.command_id.0.as_str()
        || serde_json::from_str::<Value>(&encoded).ok() != serde_json::to_value(schema).ok()
    {
        return Ok(vec![]);
    }
    let mut keys = vec![
        (
            "receipt_replies".into(),
            super::guard::key_digest(&serde_json::to_string(&identity).expect("reply key"))?,
        ),
        (
            "artifacts".into(),
            super::guard::key_digest(&serde_json::to_string(&[&artifact]).expect("artifact key"))?,
        ),
        (
            "artifact_versions".into(),
            super::guard::key_digest(&serde_json::to_string(&[&version]).expect("version key"))?,
        ),
    ];
    if let Some(project) = control {
        let Some(blob) = minimal_control_publication(
            conn,
            &artifact,
            &version,
            receipt,
            op,
            revision,
            (project, identity[0]),
        )?
        else {
            return Ok(vec![]);
        };
        keys.push((
            "blobs".into(),
            super::guard::key_digest(&serde_json::to_string(&[blob]).expect("control blob key"))?,
        ));
    }
    Ok(keys)
}

fn minimal_control_publication(
    conn: &Connection,
    artifact: &str,
    version: &str,
    receipt: &Receipt<DomainDecision>,
    op: &str,
    revision: u32,
    scope: (&ProjectId, &str),
) -> Result<Option<String>, StorageError> {
    let (project, actor) = scope;
    type Minimal = (String, String, String, String);
    let raw:Option<Minimal>=db(conn.query_row("SELECT a.title,a.provenance_json,v.source_json,v.blob_digest FROM artifacts a JOIN artifact_versions v ON v.artifact_id=a.id WHERE a.id=?1 AND v.id=?2 AND a.state='Live' AND ((a.scope_kind='Project' AND a.current_project_id=?3 AND a.origin_project_id=?3) OR (a.scope_kind='Workspace' AND a.current_project_id IS NULL AND a.origin_project_id IS NULL)) AND a.origin_work_id IS NULL AND v.deleted_at IS NULL AND NOT EXISTS(SELECT 1 FROM artifact_shares WHERE artifact_id=a.id)",[artifact,version,project.0.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional())?;
    let Some((title, provenance, source, blob)) = raw else {
        return Ok(None);
    };
    let (Ok(provenance), Ok(source)) = (
        serde_json::from_str::<Value>(&provenance),
        serde_json::from_str::<Value>(&source),
    ) else {
        return Ok(None);
    };
    let Some(host) = provenance["producedBy"].as_str() else {
        return Ok(None);
    };
    let valid_host: bool = db(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM actors WHERE id=?1 AND kind=?2)",
        [
            host,
            super::super::repos::principal_name(devworks_core::context::PrincipalKind::Host),
        ],
        |r| r.get(0),
    ))?;
    if !valid_host
        || title != format!("{op} reply")
        || provenance
            != serde_json::json!({"producedBy":host,"principalKind":"Host","sourceKind":"ReceiptReply","receiptActor":actor,"commandId":receipt.command_id,"operation":op})
    {
        return Ok(None);
    }
    let Some(operation) = source["operationId"].as_str() else {
        return Ok(None);
    };
    let valid_stage: bool = db(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND capability='artifacts.stage_file')",
        [operation],
        |r| r.get(0),
    ))?;
    if !valid_stage
        || source
            != serde_json::json!({"source":{"kind":"ReceiptReply","commandId":receipt.command_id},"operationId":operation,"producedBy":host,"principalKind":"Host"})
    {
        return Ok(None);
    }
    let Some(fingerprint) = Fingerprint::from_hex(&receipt.request_digest) else {
        return Ok(None);
    };
    let stored = devworks_core::ports::StoredReceipt {
        operation: op.into(),
        contract: CONTRACT.into(),
        schema_revision: SchemaRevision(revision),
        fingerprint,
        receipt: receipt.clone(),
    };
    let Ok(bytes) = devworks_core::services::artifacts::replies::reply_bytes(&stored) else {
        return Ok(None);
    };
    Ok((devworks_core::fingerprint::sha256_hex(&bytes) == blob).then_some(blob))
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
        for key in published_reply(
            conn,
            [&actor, &scope, &key],
            receipt,
            &op,
            body.schema_revision,
            control.then_some(project),
        )? {
            if let Some(candidate) = candidates.get(&key) {
                assign(
                    candidate,
                    if control {
                        Action::Keep
                    } else {
                        Action::Delete
                    },
                    if control {
                        Reason::PurgeControlAudit
                    } else {
                        Reason::OwnedLineage
                    },
                    rows,
                    unresolved,
                );
            }
        }
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
