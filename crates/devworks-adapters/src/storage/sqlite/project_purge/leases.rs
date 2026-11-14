//! Read leases belong to their typed data scope, never their opaque owner hash.
use super::super::conn::db;
use super::classification::{Key, assign};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::storage::{
    GraphScope, GraphUnavailableRepository, GraphViewWitness, ProjectPurgeCandidates,
    ProjectPurgeRowAction as Action, ProjectPurgeRowReason as Reason, ProjectPurgeRowWitness,
    ProjectPurgeUnresolved, StorageError, StoredSchemaReadPin,
};
use devworks_core::protocol::{Id, ProjectId, data::DataScope};
use rusqlite::Connection;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

fn closed<T: DeserializeOwned + Serialize>(encoded: &str) -> Option<T> {
    let raw: Value = serde_json::from_str(encoded).ok()?;
    let typed: T = serde_json::from_value(raw.clone()).ok()?;
    (serde_json::to_value(&typed).ok()? == raw).then_some(typed)
}
fn digest(value: &str) -> bool {
    devworks_core::fingerprint::is_sha256_hex(value)
}
pub(super) fn partition(
    conn: &Connection,
    project: &ProjectId,
    snapshot: &ProjectPurgeCandidates,
    rows: &mut BTreeMap<Key, ProjectPurgeRowWitness>,
    unresolved: &mut BTreeMap<Key, ProjectPurgeUnresolved>,
) -> Result<(), StorageError> {
    let selection = super::candidates::selection(project)?;
    let scope_digest = sha256_hex(
        serde_json::to_string(&DataScope::Project {
            project_id: project.clone(),
        })
        .expect("typed scope")
        .as_bytes(),
    );
    let sql = selection.sql(
        "graph_read_pins",
        "id,scope_json,witness_json,missing_json",
        "1=1",
    )?;
    let mut statement = db(conn.prepare(&sql))?;
    let mut records = db(statement.query(selection.parameters()))?;
    while let Some(row) = db(records.next())? {
        let key = db(row.get::<_, String>(0))?;
        let (scope, witnesses, missing) = (
            db(row.get::<_, String>(1))?,
            db(row.get::<_, String>(2))?,
            db(row.get::<_, String>(3))?,
        );
        let (Some(scope), Some(_), Some(_)) = (
            closed::<GraphScope>(&scope),
            closed::<Vec<GraphViewWitness>>(&witnesses),
            closed::<Vec<GraphUnavailableRepository>>(&missing),
        ) else {
            continue;
        };
        let Ok(id) = Id::new(key.clone()) else {
            continue;
        };
        // Reuse the native reader's exact view/lease joins; extra children are
        // independently rejected rather than inheriting an ownership decision.
        let Ok(Some(pin)) = super::super::graph::pin(conn, &id) else {
            continue;
        };
        let children: i64 = db(conn.query_row(
            "SELECT count(*) FROM graph_read_pin_views WHERE pin_id=?1",
            [&key],
            |r| r.get(0),
        ))?;
        if pin.scope_digest != scope_digest
            || children != scope.views.len() as i64
            || !digest(&pin.intent_digest)
        {
            continue;
        }
        let mut selected = BTreeSet::from([(
            "graph_read_pins".to_owned(),
            super::guard::key_digest(&serde_json::to_string(&[&key]).expect("pin key"))?,
        )]);
        let mut q = db(conn.prepare(
            "SELECT repository_id,index_pin_id FROM graph_read_pin_views WHERE pin_id=?1",
        ))?;
        let mut children = db(q.query([&key]))?;
        while let Some(child) = db(children.next())? {
            let repository = db(child.get::<_, String>(0))?;
            let index = db(child.get::<_, String>(1))?;
            selected.insert((
                "graph_read_pin_views".into(),
                super::guard::key_digest(
                    &serde_json::to_string(&[&key, &repository]).expect("graph child key"),
                )?,
            ));
            selected.insert((
                "index_pins".into(),
                super::guard::key_digest(
                    &serde_json::to_string(&[index]).expect("index child key"),
                )?,
            ));
        }
        for candidate in snapshot
            .rows
            .iter()
            .filter(|r| selected.contains(&(r.table.clone(), r.key_digest.clone())))
        {
            assign(
                candidate,
                Action::Delete,
                Reason::ScopedReadLease,
                rows,
                unresolved,
            );
        }
    }
    let sql = selection.sql(
        "schema_read_pins",
        "id,view_id,intent_digest,expires_at,body_json",
        "1=1",
    )?;
    let mut statement = db(conn.prepare(&sql))?;
    let mut records = db(statement.query(selection.parameters()))?;
    while let Some(row) = db(records.next())? {
        let (key, view, intent, expires, encoded) = (
            db(row.get::<_, String>(0))?,
            db(row.get::<_, String>(1))?,
            db(row.get::<_, String>(2))?,
            db(row.get::<_, i64>(3))?,
            db(row.get::<_, String>(4))?,
        );
        let Some(pin) = closed::<StoredSchemaReadPin>(&encoded) else {
            continue;
        };
        if pin.id.as_str() != key
            || pin.view_id.as_str() != view
            || pin.intent_digest != intent
            || pin.expires_at_ms != expires
            || pin.scope_digest != scope_digest
            || !digest(&pin.intent_digest)
            || !digest(&pin.view_digest)
            || pin.created_at_ms >= pin.expires_at_ms
        {
            continue;
        }
        let key_digest =
            super::guard::key_digest(&serde_json::to_string(&[key]).expect("schema pin key"))?;
        if let Some(candidate) = snapshot
            .rows
            .iter()
            .find(|r| r.table == "schema_read_pins" && r.key_digest == key_digest)
        {
            assign(
                candidate,
                Action::Delete,
                Reason::ScopedReadLease,
                rows,
                unresolved,
            );
        }
    }
    Ok(())
}
