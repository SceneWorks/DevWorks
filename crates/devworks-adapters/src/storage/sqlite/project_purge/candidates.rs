//! Scoped row materialization for the ownership classifier, never deletion authority.
use super::super::conn::db;
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::storage::{ProjectPurgeCandidate, ProjectPurgeCandidates, StorageError};
use devworks_core::protocol::{ProjectId, data::DataScope};
use rusqlite::{Connection, types::ValueRef};
use serde_json::Value;
use sha2::{Digest, Sha256};

const POLICY: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/architecture/validation/project-purge/native-row-selectors.json"
));
fn invalid(message: &str) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
pub(super) fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
fn feed(hash: &mut Sha256, tag: u8, bytes: &[u8]) {
    hash.update([tag]);
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}
pub(super) fn key_value(value: ValueRef<'_>) -> Result<Value, StorageError> {
    Ok(match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => Value::from(value),
        ValueRef::Real(value) => serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| invalid("non-finite purge candidate key"))?,
        ValueRef::Text(value) => Value::String(
            std::str::from_utf8(value)
                .map_err(|_| invalid("invalid purge candidate key encoding"))?
                .into(),
        ),
        ValueRef::Blob(_) => return Err(invalid("unclassified binary purge candidate key")),
    })
}

pub(super) fn primary_keys(conn: &Connection, table: &str) -> Result<Vec<String>, StorageError> {
    let mut primary =
        db(conn.prepare("SELECT name FROM pragma_table_info(?1) WHERE pk>0 ORDER BY pk"))?;
    db(primary.query_map([table], |r| r.get::<_, String>(0)))?
        .map(db)
        .collect()
}

fn table_rows(
    conn: &Connection,
    table: &str,
    predicate: &str,
    anchors: &str,
    project: &ProjectId,
    scope_digest: &str,
) -> Result<Vec<ProjectPurgeCandidate>, StorageError> {
    let keys = primary_keys(conn, table)?;
    let projection = if keys.is_empty() {
        "rowid AS __purge_rowid,*"
    } else {
        "*"
    };
    let sql = format!(
        "WITH {anchors} SELECT {projection} FROM {} WHERE ({predicate})",
        quote(table)
    );
    let mut statement = db(conn.prepare(&sql))?;
    let columns = statement
        .column_names()
        .map(|name| name.to_string())
        .collect::<Vec<_>>();
    let indices = if keys.is_empty() {
        vec![0]
    } else {
        keys.iter()
            .map(|key| {
                columns
                    .iter()
                    .position(|column| column == key)
                    .ok_or_else(|| invalid("missing purge candidate primary key"))
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut rows = db(statement.query([project.0.as_str(), scope_digest]))?;
    let mut output = Vec::new();
    while let Some(row) = db(rows.next())? {
        let key = indices
            .iter()
            .map(|index| key_value(db(row.get_ref(*index))?))
            .collect::<Result<Vec<_>, _>>()?;
        let key_digest = super::guard::key_digest(
            &serde_json::to_string(&key).map_err(|_| invalid("purge candidate key encoding"))?,
        )?;
        let mut hash = Sha256::new();
        feed(&mut hash, b'T', table.as_bytes());
        for (index, column) in columns.iter().enumerate() {
            feed(&mut hash, b'C', column.as_bytes());
            match db(row.get_ref(index))? {
                ValueRef::Null => feed(&mut hash, b'N', &[]),
                ValueRef::Integer(value) => feed(&mut hash, b'I', &value.to_be_bytes()),
                ValueRef::Real(value) => feed(&mut hash, b'R', &value.to_bits().to_be_bytes()),
                ValueRef::Text(value) => feed(&mut hash, b'S', value),
                ValueRef::Blob(value) => feed(&mut hash, b'B', value),
            }
        }
        let row_digest = hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        output.push(ProjectPurgeCandidate {
            table: table.into(),
            key_digest,
            row_digest,
        });
    }
    Ok(output)
}

pub(super) struct Selection {
    project: ProjectId,
    anchors: String,
    scope_digest: String,
    predicates: serde_json::Map<String, Value>,
}
impl Selection {
    pub(super) fn sql(
        &self,
        table: &str,
        columns: &str,
        condition: &str,
    ) -> Result<String, StorageError> {
        let predicate = self
            .predicates
            .get(table)
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("unmapped purge partition table"))?;
        Ok(format!(
            "WITH {} SELECT {columns} FROM {} WHERE ({predicate}) AND ({condition})",
            self.anchors,
            quote(table)
        ))
    }
    pub(super) fn parameters(&self) -> [&str; 2] {
        [self.project.0.as_str(), &self.scope_digest]
    }
    pub(super) fn rows_where(
        &self,
        conn: &Connection,
        table: &str,
        condition: &str,
    ) -> Result<Vec<ProjectPurgeCandidate>, StorageError> {
        let predicate = self
            .predicates
            .get(table)
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("unmapped purge partition table"))?;
        table_rows(
            conn,
            table,
            &format!("({predicate}) AND ({condition})"),
            &self.anchors,
            &self.project,
            &self.scope_digest,
        )
    }
}
pub(super) fn selection(project: &ProjectId) -> Result<Selection, StorageError> {
    let policy: Value =
        serde_json::from_str(POLICY).map_err(|_| invalid("purge candidate policy"))?;
    let anchors = policy["anchors"]
        .as_object()
        .ok_or_else(|| invalid("purge candidate anchors"))?
        .iter()
        .map(|(name, sql)| {
            Ok(format!(
                "{} AS ({})",
                quote(name),
                sql.as_str()
                    .ok_or_else(|| invalid("purge candidate anchor SQL"))?
            ))
        })
        .collect::<Result<Vec<_>, StorageError>>()?
        .join(",");
    let scope = DataScope::Project {
        project_id: project.clone(),
    };
    let scope_digest = sha256_hex(
        serde_json::to_string(&scope)
            .map_err(|_| invalid("purge project scope"))?
            .as_bytes(),
    );
    let mut predicates = policy["candidatePredicates"]
        .as_object()
        .ok_or_else(|| invalid("purge candidate predicates"))?
        .clone();
    predicates.insert("projects".into(), Value::String("id=?1".into()));
    predicates.insert(
        "project_details".into(),
        Value::String("project_id=?1".into()),
    );
    predicates.insert(
        "project_managed_files".into(),
        Value::String("project_id=?1".into()),
    );
    predicates.insert(
        "owned_containers".into(),
        Value::String("project_id=?1".into()),
    );
    for table in [
        "agent_messages",
        "agent_message_deliveries",
        "agent_message_events",
        "agent_assignment_context",
        "watchers",
        "watcher_events",
    ] {
        predicates.insert(table.into(), Value::String("project_id=?1".into()));
    }
    // A workspace item or revision a project-confined agent drafted is confined to (and purged
    // with) that project until a person approves it.
    predicates.insert(
        "instructions".into(),
        Value::String("project_id=?1 OR confined_project_id=?1".into()),
    );
    predicates.insert(
        "instruction_revisions".into(),
        Value::String(
            "confined_project_id=?1 OR instruction_id IN (SELECT id FROM instructions WHERE project_id=?1 OR confined_project_id=?1)"
                .into(),
        ),
    );
    Ok(Selection {
        project: project.clone(),
        anchors,
        scope_digest,
        predicates,
    })
}

pub(in crate::storage::sqlite) fn snapshot(
    conn: &Connection,
    project: &ProjectId,
) -> Result<Option<ProjectPurgeCandidates>, StorageError> {
    super::schema::verify(conn)?;
    if !db(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id=?1)",
        [project.0.as_str()],
        |row| row.get::<_, bool>(0),
    ))? {
        return Ok(None);
    }
    let Selection {
        anchors,
        scope_digest,
        predicates,
        ..
    } = selection(project)?;
    let mut rows = Vec::new();
    for (table, predicate) in &predicates {
        rows.extend(table_rows(
            conn,
            table,
            predicate
                .as_str()
                .ok_or_else(|| invalid("purge candidate predicate SQL"))?,
            &anchors,
            project,
            &scope_digest,
        )?);
    }
    let references =
        super::references::snapshot(conn, project, &scope_digest, &anchors, &predicates)?;
    rows.sort_by(|a, b| (&a.table, &a.key_digest).cmp(&(&b.table, &b.key_digest)));
    if rows
        .windows(2)
        .any(|pair| pair[0].table == pair[1].table && pair[0].key_digest == pair[1].key_digest)
    {
        return Err(invalid("duplicate purge candidate identity"));
    }
    let policy_digest = sha256_hex(
        serde_json::to_string(&serde_json::json!({
            "format":"dw83-native-selection-v2", "policy":sha256_hex(POLICY.as_bytes()),
            "anchors":anchors,"predicates":predicates
        }))
        .map_err(|_| invalid("purge selection policy digest"))?
        .as_bytes(),
    );
    let digest = sha256_hex(
        serde_json::to_string(
            &serde_json::json!({"project":project,"policy":policy_digest,"rows":rows,"references":references}),
        )
        .map_err(|_| invalid("purge candidate digest"))?
        .as_bytes(),
    );
    Ok(Some(ProjectPurgeCandidates {
        project_id: project.clone(),
        policy_digest,
        digest,
        rows,
        references,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use devworks_core::ports::storage::{ReadOptions, read_snapshot};
    use devworks_core::protocol::Id;
    #[test]
    fn confined_instruction_drafts_are_purged_with_their_project_only() {
        let h = crate::storage::sqlite::tests::Harness::new();
        let store = h.store();
        let c = h.raw();
        for p in ["purge-a", "purge-b"] {
            c.execute("INSERT INTO projects(id,title,lifecycle,revision,scope_revision,created_at) VALUES(?1,'Fixture','Deleted',1,1,0)",[p]).unwrap();
        }
        let item = |id: &str, confined: Option<&str>| {
            c.execute("INSERT INTO instructions(id,kind,name,scope_key,project_id,origin,root_path,source_path,latest_revision,active_revision,enabled,present,version,created_at,updated_at,confined_project_id) VALUES(?1,'Skill',?1,'workspace',NULL,'Harness','/r',?2,2,NULL,1,0,1,0,0,?3)",
                rusqlite::params![id, format!("/r/{id}"), confined]).unwrap();
        };
        let revision = |item: &str, n: i64, confined: Option<&str>| {
            c.execute("INSERT INTO instruction_revisions(instruction_id,revision,content_json,content_digest,state,author,origin_json,resources_json,applicability_json,created_at,confined_project_id) VALUES(?1,?2,'{}',?3,'Draft','agent','{}','[]','[]',0,?4)",
                rusqlite::params![item, n, "a".repeat(64), confined]).unwrap();
        };
        // An item a project-A agent created, with its confined draft and a later unconfined one.
        item("made-by-a", Some("purge-a"));
        revision("made-by-a", 1, Some("purge-a"));
        revision("made-by-a", 2, None);
        // A shared workspace item: its own draft stays, project A's draft of it goes.
        item("shared", None);
        revision("shared", 1, None);
        revision("shared", 2, Some("purge-a"));
        // Project B's confinement is untouched by purging A.
        item("made-by-b", Some("purge-b"));
        revision("made-by-b", 1, Some("purge-b"));
        let p = ProjectId(Id::new("purge-a").unwrap());
        let selected = read_snapshot(&store, &ReadOptions::default(), |r| {
            r.projects().purge_candidates(&p)
        })
        .unwrap()
        .unwrap();
        let count = |table: &str| selected.rows.iter().filter(|r| r.table == table).count();
        assert_eq!(count("instructions"), 1, "only the item project A created");
        assert_eq!(
            count("instruction_revisions"),
            3,
            "its revisions and A's draft of the shared item"
        );
    }

    #[test]
    fn candidate_primary_key_hash_matches_the_native_exact_row_guard() {
        let h = crate::storage::sqlite::tests::Harness::new();
        let store = h.store();
        let c = h.raw();
        c.execute("INSERT INTO projects(id,title,lifecycle,revision,scope_revision,created_at) VALUES('purge-a','Fixture','Deleted',1,1,0)",[]).unwrap();
        let key = "a".repeat(64);
        c.execute("INSERT INTO allocation_replays(project_id,key,intent_digest,result_json) VALUES('purge-a',?1,?2,'{}')",rusqlite::params![key,"b".repeat(64)]).unwrap();
        let p = ProjectId(Id::new("purge-a").unwrap());
        let selected = read_snapshot(&store, &ReadOptions::default(), |r| {
            r.projects().purge_candidates(&p)
        })
        .unwrap()
        .unwrap();
        let candidate = selected
            .rows
            .iter()
            .find(|row| row.table == "allocation_replays")
            .unwrap();
        super::super::guard::register(&c, true).unwrap();
        c.execute_batch("BEGIN IMMEDIATE").unwrap();
        let permits = [super::super::guard::Permit {
            table: candidate.table.clone(),
            key_digest: candidate.key_digest.clone(),
            action: "Delete".into(),
        }]
        .into_iter()
        .collect();
        super::super::guard::with_guard(&c, permits, || {
            assert_eq!(
                db(c.execute("DELETE FROM allocation_replays WHERE key=?1", [&key]))?,
                1
            );
            Ok(())
        })
        .unwrap();
        c.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            c.query_row("SELECT count(*) FROM allocation_replays", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert!(
            c.execute("DELETE FROM allocation_replays WHERE key=?1", [&key])
                .is_err()
        );
    }
}
