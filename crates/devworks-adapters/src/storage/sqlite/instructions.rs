//! Versioned skill and role instruction items with immutable revisions (DW-034).
use super::conn::db;
use super::repos::{Snapshot, Unit};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, Id, ProjectId, Revision, RevisionDimension};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;

fn corrupt(message: &str) -> StorageError {
    StorageError::unavailable(Unavailable::Corrupt, message)
}
fn invalid(message: &str) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
fn id(value: String) -> Result<Id, StorageError> {
    Id::new(value).map_err(|_| corrupt("invalid instruction identifier"))
}
fn json(value: &str) -> Result<Value, StorageError> {
    serde_json::from_str(value).map_err(|_| corrupt("invalid instruction JSON"))
}
fn page(limit: u16) -> Result<(), StorageError> {
    if limit == 0 || limit > MAX_PAGE {
        Err(invalid("instruction page limit out of bounds"))
    } else {
        Ok(())
    }
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

const ITEM_COLUMNS: &str = "id,kind,name,scope_key,origin,root_path,source_path,latest_revision,active_revision,enabled,materialized_digest,observed_digest,present,version,created_at,updated_at,confined_project_id";
const REVISION_COLUMNS: &str = "instruction_id,revision,content_json,content_digest,state,author,origin_json,resources_json,applicability_json,approval_id,created_at,decided_at,confined_project_id";

fn item(r: &Row<'_>) -> rusqlite::Result<RawItem> {
    Ok(RawItem {
        id: r.get(0)?,
        kind: r.get(1)?,
        name: r.get(2)?,
        scope: r.get(3)?,
        origin: r.get(4)?,
        root_path: r.get(5)?,
        source_path: r.get(6)?,
        latest_revision: r.get(7)?,
        active_revision: r.get(8)?,
        enabled: r.get(9)?,
        materialized_digest: r.get(10)?,
        observed_digest: r.get(11)?,
        present: r.get(12)?,
        version: r.get(13)?,
        created_at: r.get(14)?,
        updated_at: r.get(15)?,
        confined: r.get(16)?,
    })
}
struct RawItem {
    id: String,
    kind: String,
    name: String,
    scope: String,
    origin: String,
    root_path: Option<String>,
    source_path: Option<String>,
    latest_revision: i64,
    active_revision: Option<i64>,
    enabled: bool,
    materialized_digest: Option<String>,
    observed_digest: Option<String>,
    present: bool,
    version: i64,
    created_at: i64,
    updated_at: i64,
    confined: Option<String>,
}
impl RawItem {
    fn decode(self) -> Result<StoredInstructionItem, StorageError> {
        Ok(StoredInstructionItem {
            id: id(self.id)?,
            kind: InstructionKind::parse(&self.kind).ok_or_else(|| corrupt("instruction kind"))?,
            name: self.name,
            scope: ConfigScope::from_key(&self.scope)
                .ok_or_else(|| corrupt("instruction scope"))?,
            origin: InstructionOrigin::parse(&self.origin)
                .ok_or_else(|| corrupt("instruction origin"))?,
            root_path: self.root_path,
            source_path: self.source_path,
            latest_revision: self.latest_revision,
            active_revision: self.active_revision,
            enabled: self.enabled,
            materialized_digest: self.materialized_digest,
            observed_digest: self.observed_digest,
            present: self.present,
            version: self.version,
            created_at_ms: self.created_at,
            updated_at_ms: self.updated_at,
            confined_project: self.confined.map(id).transpose()?.map(ProjectId),
        })
    }
}

struct RawRevision {
    instruction_id: String,
    revision: i64,
    content: String,
    content_digest: String,
    state: String,
    author: String,
    origin: String,
    resources: String,
    applicability: String,
    approval_id: Option<String>,
    created_at: i64,
    decided_at: Option<i64>,
    confined: Option<String>,
}
fn revision(r: &Row<'_>) -> rusqlite::Result<RawRevision> {
    Ok(RawRevision {
        instruction_id: r.get(0)?,
        revision: r.get(1)?,
        content: r.get(2)?,
        content_digest: r.get(3)?,
        state: r.get(4)?,
        author: r.get(5)?,
        origin: r.get(6)?,
        resources: r.get(7)?,
        applicability: r.get(8)?,
        approval_id: r.get(9)?,
        created_at: r.get(10)?,
        decided_at: r.get(11)?,
        confined: r.get(12)?,
    })
}
impl RawRevision {
    fn decode(self) -> Result<StoredInstructionRevision, StorageError> {
        Ok(StoredInstructionRevision {
            instruction_id: id(self.instruction_id)?,
            revision: self.revision,
            content: json(&self.content)?,
            content_digest: self.content_digest,
            state: InstructionRevisionState::parse(&self.state)
                .ok_or_else(|| corrupt("instruction revision state"))?,
            author: ActorId(id(self.author)?),
            origin: json(&self.origin)?,
            resources: json(&self.resources)?,
            applicability: serde_json::from_str(&self.applicability)
                .map_err(|_| corrupt("instruction applicability"))?,
            approval_id: self.approval_id.map(id).transpose()?,
            created_at_ms: self.created_at,
            decided_at_ms: self.decided_at,
            confined_project: self.confined.map(id).transpose()?.map(ProjectId),
        })
    }
}

fn get_item(conn: &Connection, key: &Id) -> Result<Option<StoredInstructionItem>, StorageError> {
    db(conn
        .query_row(
            &format!("SELECT {ITEM_COLUMNS} FROM instructions WHERE id=?1"),
            [key.as_str()],
            item,
        )
        .optional())?
    .map(RawItem::decode)
    .transpose()
}
fn get_revision(
    conn: &Connection,
    key: &Id,
    number: i64,
) -> Result<Option<StoredInstructionRevision>, StorageError> {
    db(conn
        .query_row(
            &format!(
                "SELECT {REVISION_COLUMNS} FROM instruction_revisions WHERE instruction_id=?1 AND revision=?2"
            ),
            params![key.as_str(), number],
            revision,
        )
        .optional())?
    .map(RawRevision::decode)
    .transpose()
}
fn list_items(
    conn: &Connection,
    filter: &InstructionFilter,
    after: Option<&Id>,
    limit: u16,
) -> Result<Vec<StoredInstructionItem>, StorageError> {
    page(limit)?;
    let scopes: Vec<String> = filter.scopes.iter().map(ConfigScope::key).collect();
    let scopes = serde_json::to_string(&scopes).expect("scope keys");
    let mut statement = db(conn.prepare(&format!(
        "SELECT {ITEM_COLUMNS} FROM instructions
         WHERE (?1 IS NULL OR kind=?1)
           AND (?2 = '[]' OR scope_key IN (SELECT value FROM json_each(?2)))
           AND (?3 IS NULL OR name=?3)
           AND (?4 IS NULL OR id > ?4)
         ORDER BY id LIMIT ?5"
    )))?;
    db(statement.query_map(
        params![
            filter.kind.map(InstructionKind::as_str),
            scopes,
            filter.name,
            after.map(Id::as_str),
            i64::from(limit)
        ],
        item,
    ))?
    .map(|r| db(r).and_then(RawItem::decode))
    .collect()
}
fn list_revisions(
    conn: &Connection,
    key: &Id,
    before: Option<i64>,
    limit: u16,
) -> Result<Vec<StoredInstructionRevision>, StorageError> {
    page(limit)?;
    let mut statement = db(conn.prepare(&format!(
        "SELECT {REVISION_COLUMNS} FROM instruction_revisions
         WHERE instruction_id=?1 AND (?2 IS NULL OR revision < ?2)
         ORDER BY revision DESC LIMIT ?3"
    )))?;
    db(statement.query_map(params![key.as_str(), before, i64::from(limit)], revision))?
        .map(|r| db(r).and_then(RawRevision::decode))
        .collect()
}
fn by_approval(
    conn: &Connection,
    approval: &Id,
) -> Result<Option<StoredInstructionRevision>, StorageError> {
    db(conn
        .query_row(
            &format!("SELECT {REVISION_COLUMNS} FROM instruction_revisions WHERE approval_id=?1"),
            [approval.as_str()],
            revision,
        )
        .optional())?
    .map(RawRevision::decode)
    .transpose()
}

macro_rules! instruction_reads {
    ($ty:ty) => {
        impl InstructionRead for $ty {
            fn instruction_item(
                &self,
                key: &Id,
            ) -> Result<Option<StoredInstructionItem>, StorageError> {
                get_item(self.conn, key)
            }
            fn instruction_items(
                &self,
                filter: &InstructionFilter,
                after: Option<&Id>,
                limit: u16,
            ) -> Result<Vec<StoredInstructionItem>, StorageError> {
                list_items(self.conn, filter, after, limit)
            }
            fn instruction_revision(
                &self,
                key: &Id,
                number: i64,
            ) -> Result<Option<StoredInstructionRevision>, StorageError> {
                get_revision(self.conn, key, number)
            }
            fn instruction_revisions(
                &self,
                key: &Id,
                before: Option<i64>,
                limit: u16,
            ) -> Result<Vec<StoredInstructionRevision>, StorageError> {
                list_revisions(self.conn, key, before, limit)
            }
            fn instruction_revision_by_approval(
                &self,
                approval: &Id,
            ) -> Result<Option<StoredInstructionRevision>, StorageError> {
                by_approval(self.conn, approval)
            }
        }
    };
}
instruction_reads!(Snapshot<'_>);
instruction_reads!(Unit<'_>);

fn check_item(item: &StoredInstructionItem) -> Result<(), StorageError> {
    let file_backed = item.kind == InstructionKind::Skill;
    if item.name.is_empty()
        || item.name.chars().count() > 128
        || item.name.contains(['\0', '/', '\\'])
        || (item.kind == InstructionKind::Role) != (item.origin == InstructionOrigin::Role)
        || file_backed != item.source_path.is_some()
        || file_backed != item.root_path.is_some()
        || item.latest_revision < 0
        || item.version < 1
        || item
            .active_revision
            .is_some_and(|a| a < 1 || a > item.latest_revision)
        || item
            .materialized_digest
            .as_deref()
            .is_some_and(|d| !digest(d))
        || item.observed_digest.as_deref().is_some_and(|d| !digest(d))
        || (item.confined_project.is_some()
            && (item.scope != ConfigScope::Workspace || item.active_revision.is_some()))
    {
        return Err(invalid("instruction item"));
    }
    Ok(())
}

impl InstructionRepository for Unit<'_> {
    fn insert_instruction_item(
        &mut self,
        item: &StoredInstructionItem,
    ) -> Result<(), StorageError> {
        check_item(item)?;
        if item.version != 1 {
            return Err(invalid("a new instruction item starts at version 1"));
        }
        db(self.conn.execute(
            &format!("INSERT INTO instructions({ITEM_COLUMNS},project_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)"),
            params![
                item.id.as_str(),
                item.kind.as_str(),
                item.name,
                item.scope.key(),
                item.origin.as_str(),
                item.root_path,
                item.source_path,
                item.latest_revision,
                item.active_revision,
                item.enabled,
                item.materialized_digest,
                item.observed_digest,
                item.present,
                item.version,
                item.created_at_ms,
                item.updated_at_ms,
                item.confined_project.as_ref().map(|p| p.0.as_str()),
                item.scope.project_id().map(|p| p.0.as_str()),
            ],
        ))?;
        Ok(())
    }
    fn update_instruction_item(
        &mut self,
        item: &StoredInstructionItem,
    ) -> Result<(), StorageError> {
        check_item(item)?;
        let current = get_item(self.conn, &item.id)?.ok_or_else(|| StorageError::NotFound {
            kind: "instruction",
            id: item.id.as_str().into(),
        })?;
        if current.version != item.version - 1 {
            return Err(StorageError::conflict(
                RevisionDimension::Source,
                "instruction",
                item.id.clone(),
                Revision::new((item.version - 1).max(1)).expect("positive"),
                Revision::new(current.version.max(1)).expect("positive"),
            ));
        }
        if current.kind != item.kind
            || current.scope != item.scope
            || current.origin != item.origin
            || current.name != item.name
            || current.created_at_ms != item.created_at_ms
            || item.latest_revision < current.latest_revision
            || (item.confined_project.is_some()
                && item.confined_project != current.confined_project)
        {
            return Err(invalid("instruction identity is immutable"));
        }
        if let Some(active) = item.active_revision {
            let state = get_revision(self.conn, &item.id, active)?.map(|r| r.state);
            if state != Some(InstructionRevisionState::Approved) {
                return Err(invalid("only an approved revision can be active"));
            }
        }
        db(self.conn.execute(
            "UPDATE instructions SET root_path=?2,source_path=?3,latest_revision=?4,active_revision=?5,enabled=?6,materialized_digest=?7,observed_digest=?8,present=?9,version=?10,updated_at=?11,confined_project_id=?13 WHERE id=?1 AND version=?12",
            params![
                item.id.as_str(),
                item.root_path,
                item.source_path,
                item.latest_revision,
                item.active_revision,
                item.enabled,
                item.materialized_digest,
                item.observed_digest,
                item.present,
                item.version,
                item.updated_at_ms,
                item.version - 1,
                item.confined_project.as_ref().map(|p| p.0.as_str()),
            ],
        ))?;
        Ok(())
    }
    fn insert_instruction_revision(
        &mut self,
        r: &StoredInstructionRevision,
    ) -> Result<(), StorageError> {
        let parent =
            get_item(self.conn, &r.instruction_id)?.ok_or_else(|| StorageError::NotFound {
                kind: "instruction",
                id: r.instruction_id.as_str().into(),
            })?;
        if r.revision != parent.latest_revision + 1
            || !digest(&r.content_digest)
            || !r.content.is_object()
            || !r.resources.is_array()
            || (r.approval_id.is_some() && r.state == InstructionRevisionState::Draft)
            || (r.confined_project.is_some()
                && (r.state == InstructionRevisionState::Approved
                    || parent.scope != ConfigScope::Workspace))
        {
            return Err(invalid("instruction revision"));
        }
        db(self.conn.execute(
            &format!("INSERT INTO instruction_revisions({REVISION_COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)"),
            params![
                r.instruction_id.as_str(),
                r.revision,
                r.content.to_string(),
                r.content_digest,
                r.state.as_str(),
                r.author.0.as_str(),
                r.origin.to_string(),
                r.resources.to_string(),
                serde_json::to_string(&r.applicability).expect("labels"),
                r.approval_id.as_ref().map(Id::as_str),
                r.created_at_ms,
                r.decided_at_ms,
                r.confined_project.as_ref().map(|p| p.0.as_str()),
            ],
        ))?;
        Ok(())
    }
    fn set_instruction_revision_state(
        &mut self,
        key: &Id,
        number: i64,
        from: InstructionRevisionState,
        to: InstructionRevisionState,
        approval_id: Option<&Id>,
        decided_at_ms: Option<i64>,
    ) -> Result<(), StorageError> {
        let allowed = matches!(
            (from, to),
            (
                InstructionRevisionState::Draft,
                InstructionRevisionState::Proposed
            ) | (
                InstructionRevisionState::Draft,
                InstructionRevisionState::Approved
            ) | (
                InstructionRevisionState::Proposed,
                InstructionRevisionState::Approved
            ) | (
                InstructionRevisionState::Proposed,
                InstructionRevisionState::Rejected
            ) | (
                InstructionRevisionState::Proposed,
                InstructionRevisionState::Draft
            )
        );
        if !allowed {
            return Err(invalid("instruction revision transition"));
        }
        let changed = db(self.conn.execute(
            "UPDATE instruction_revisions SET state=?4,approval_id=?5,decided_at=?6 WHERE instruction_id=?1 AND revision=?2 AND state=?3",
            params![
                key.as_str(),
                number,
                from.as_str(),
                to.as_str(),
                approval_id.map(Id::as_str),
                decided_at_ms,
            ],
        ))?;
        if changed != 1 {
            return Err(StorageError::conflict(
                RevisionDimension::Source,
                "instruction_revision",
                key.clone(),
                Revision::new(number.max(1)).expect("positive"),
                Revision::new(number.max(1)).expect("positive"),
            ));
        }
        Ok(())
    }
    fn release_instruction_revision(
        &mut self,
        key: &Id,
        number: i64,
        content: &Value,
    ) -> Result<(), StorageError> {
        let current =
            get_revision(self.conn, key, number)?.ok_or_else(|| StorageError::NotFound {
                kind: "instruction_revision",
                id: key.as_str().into(),
            })?;
        if current.confined_project.is_none()
            || !content.is_object()
            || content.get("sha256").is_none()
            || content.get("sha256") != current.content.get("sha256")
        {
            return Err(invalid("instruction revision release"));
        }
        db(self.conn.execute(
            "UPDATE instruction_revisions SET content_json=?3,confined_project_id=NULL WHERE instruction_id=?1 AND revision=?2 AND confined_project_id IS NOT NULL",
            params![key.as_str(), number, content.to_string()],
        ))?;
        Ok(())
    }
}
