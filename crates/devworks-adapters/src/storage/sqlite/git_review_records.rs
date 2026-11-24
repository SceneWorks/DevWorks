//! Code review state in the existing project-owned integration records.
use crate::storage::sqlite::conn::db;
use crate::storage::sqlite::support::{conflict_strict, invalid, page_limit};
use devworks_core::ports::storage::{StorageError, StoredCodeFinding, StoredCodeReview};
use devworks_core::protocol::{Id, ProjectId, RevisionDimension, RootId};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};

fn decode<T: DeserializeOwned>(json: String) -> Result<T, StorageError> {
    serde_json::from_str(&json).map_err(|_| invalid("invalid stored code review record"))
}

pub(super) fn lookup<T: DeserializeOwned>(
    conn: &Connection,
    kind: &str,
    id: &Id,
) -> Result<Option<T>, StorageError> {
    db(conn
        .query_row(
            "SELECT body_json FROM integration_records WHERE id=?1 AND kind=?2",
            params![id.as_str(), kind],
            |row| row.get::<_, String>(0),
        )
        .optional())?
    .map(decode)
    .transpose()
}

pub(super) fn reviews(
    conn: &Connection,
    project: Option<&ProjectId>,
    root: Option<&RootId>,
    after: Option<&Id>,
    limit: u16,
) -> Result<Vec<StoredCodeReview>, StorageError> {
    page_limit(limit)?;
    let mut query = db(conn.prepare(
        "SELECT body_json FROM integration_records WHERE kind='CodeReview'
         AND (?1 IS NULL OR project_id=?1) AND (?2 IS NULL OR root_id=?2)
         AND (?3 IS NULL OR id>?3) ORDER BY id LIMIT ?4",
    ))?;
    let rows = db(query.query_map(
        params![
            project.map(|value| value.0.as_str()),
            root.map(|value| value.0.as_str()),
            after.map(Id::as_str),
            limit,
        ],
        |row| row.get::<_, String>(0),
    ))?;
    rows.map(|row| decode(db(row)?)).collect()
}

pub(super) fn findings(
    conn: &Connection,
    review: &Id,
    after: Option<&Id>,
    limit: u16,
) -> Result<Vec<StoredCodeFinding>, StorageError> {
    page_limit(limit)?;
    let Some(owner) = lookup::<StoredCodeReview>(conn, "CodeReview", review)? else {
        return Ok(Vec::new());
    };
    let mut query = db(conn.prepare(
        "SELECT body_json FROM integration_records WHERE kind='CodeReviewFinding'
         AND root_id=?1 AND json_extract(body_json, '$.review_id')=?2
         AND (?3 IS NULL OR id>?3) ORDER BY id LIMIT ?4",
    ))?;
    let rows = db(query.query_map(
        params![
            owner.root_id.0.as_str(),
            review.as_str(),
            after.map(Id::as_str),
            limit
        ],
        |row| row.get::<_, String>(0),
    ))?;
    rows.map(|row| decode(db(row)?)).collect()
}

pub(super) fn save_review(
    conn: &Connection,
    row: &StoredCodeReview,
    expected: Option<i64>,
) -> Result<(), StorageError> {
    if row.state.is_empty() || !row.body.is_object() {
        return Err(invalid("invalid code review state"));
    }
    let owned = db(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_items WHERE id=?1 AND project_id=?2)",
        params![row.root_id.0.as_str(), row.project_id.0.as_str()],
        |r| r.get::<_, bool>(0),
    ))?;
    if !owned {
        return Err(StorageError::ScopeDenied);
    }
    save(
        conn,
        "CodeReview",
        &row.id,
        &row.project_id,
        &row.root_id,
        row.revision,
        expected,
        &row.state,
        row,
    )
}

pub(super) fn save_finding(
    conn: &Connection,
    row: &StoredCodeFinding,
    expected: Option<i64>,
) -> Result<(), StorageError> {
    let owner = lookup::<StoredCodeReview>(conn, "CodeReview", &row.review_id)?;
    if !owner
        .is_some_and(|owner| owner.root_id == row.root_id && owner.project_id == row.project_id)
        || lookup::<StoredCodeFinding>(conn, "CodeReviewFinding", &row.id)?
            .is_some_and(|old| old.review_id != row.review_id)
    {
        return Err(StorageError::ScopeDenied);
    }
    if !row.body.is_object() {
        return Err(invalid("invalid code review finding"));
    }
    save(
        conn,
        "CodeReviewFinding",
        &row.id,
        &row.project_id,
        &row.root_id,
        row.revision,
        expected,
        "Stored",
        row,
    )
}

#[allow(clippy::too_many_arguments)]
fn save(
    conn: &Connection,
    kind: &'static str,
    id: &Id,
    project: &ProjectId,
    root: &RootId,
    revision: i64,
    expected: Option<i64>,
    state: &str,
    body: &impl Serialize,
) -> Result<(), StorageError> {
    if expected.unwrap_or_default().checked_add(1) != Some(revision) {
        return Err(invalid("code review revision must advance once"));
    }
    let body = serde_json::to_string(body).map_err(|_| invalid("invalid code review body"))?;
    if let Some(expected) = expected {
        let actual: Option<i64> = db(conn
            .query_row(
                "SELECT revision FROM integration_records WHERE id=?1 AND kind=?2
             AND project_id=?3 AND root_id=?4",
                params![id.as_str(), kind, project.0.as_str(), root.0.as_str()],
                |row| row.get(0),
            )
            .optional())?;
        let actual = actual.ok_or_else(|| StorageError::NotFound {
            kind,
            id: id.as_str().into(),
        })?;
        if actual != expected {
            return Err(conflict_strict(
                RevisionDimension::Scope,
                kind,
                id.as_str(),
                expected,
                actual,
            ));
        }
        db(conn.execute(
            "UPDATE integration_records SET revision=?3,state=?4,body_json=?5 WHERE id=?1 AND kind=?2",
            params![id.as_str(), kind, revision, state, body],
        ))?;
    } else {
        let written = db(conn.execute(
            "INSERT INTO integration_records(id,kind,project_id,root_id,revision,state,body_json)
             VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(id) DO NOTHING",
            params![
                id.as_str(),
                kind,
                project.0.as_str(),
                root.0.as_str(),
                revision,
                state,
                body
            ],
        ))?;
        if written != 1 {
            return Err(StorageError::Duplicate {
                kind,
                id: id.as_str().into(),
            });
        }
    }
    Ok(())
}
