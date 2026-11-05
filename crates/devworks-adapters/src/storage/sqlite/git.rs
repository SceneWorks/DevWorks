//! Owned-worktree persistence; all writes remain inside the application's one write unit.
use super::conn::db_err as db;
use super::repos::{Snapshot, Unit};
use devworks_core::ports::storage::*;
use devworks_core::protocol::Id;
use rusqlite::{Connection, OptionalExtension, params};
fn decode(text: String) -> Result<StoredGitWorktree, StorageError> {
    let value: StoredGitWorktree = serde_json::from_str(&text)
        .map_err(|e| StorageError::unavailable(Unavailable::Corrupt, e.to_string()))?;
    value
        .validate()
        .map_err(|e| StorageError::unavailable(Unavailable::Corrupt, e.to_string()))?;
    Ok(value)
}
fn one(c: &Connection, id: &Id) -> Result<Option<StoredGitWorktree>, StorageError> {
    c.query_row(
        "SELECT payload FROM git_owned_worktrees WHERE id=?1",
        [id.as_str()],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .map_err(db)?
    .map(decode)
    .transpose()
}
macro_rules! reads { ($t:ty) => { impl GitRead for $t {
 fn git_worktree_for_branch(&self, repository: &Id, branch: &str) -> Result<Option<StoredGitWorktree>, StorageError> {
  self.conn.query_row("SELECT payload FROM git_owned_worktrees WHERE repository_id=?1 AND branch=?2 AND json_extract(payload,'$.removed')=0", params![repository.as_str(), branch], |r| r.get::<_,String>(0)).optional().map_err(db)?.map(decode).transpose()
 }
 fn git_pending_worktrees(&self, after: Option<&Id>, limit: u16) -> Result<Vec<StoredGitWorktree>, StorageError> {
  if limit == 0 || limit > MAX_PAGE { return Err(StorageError::InvalidRecord("invalid Git page limit".into())); }
  let mut q=self.conn.prepare("SELECT payload FROM git_owned_worktrees WHERE json_extract(payload,'$.pending_operation_id') IS NOT NULL AND (?1 IS NULL OR id>?1) ORDER BY id LIMIT ?2").map_err(db)?;
  let rows=q.query_map(params![after.map(Id::as_str),i64::from(limit)], |r| r.get::<_,String>(0)).map_err(db)?;
  rows.map(|r| decode(r.map_err(db)?)).collect()
 }
 fn git_worktree(&self, id: &Id) -> Result<Option<StoredGitWorktree>, StorageError> { one(self.conn, id) }
 fn git_worktrees(&self, work: &Id, after: Option<&Id>, limit: u16) -> Result<Vec<StoredGitWorktree>, StorageError> {
  if limit == 0 || limit > MAX_PAGE { return Err(StorageError::InvalidRecord("invalid Git page limit".into())); }
  let mut q=self.conn.prepare("SELECT payload FROM git_owned_worktrees WHERE work_id=?1 AND (?2 IS NULL OR id>?2) ORDER BY id LIMIT ?3").map_err(db)?;
  let rows=q.query_map(params![work.as_str(),after.map(Id::as_str),i64::from(limit)], |r| r.get::<_,String>(0)).map_err(db)?;
  rows.map(|r| decode(r.map_err(db)?)).collect()
 }
} }; }
reads!(Snapshot<'_>);
reads!(Unit<'_>);
impl GitRepository for Unit<'_> {
    fn insert_git_worktree(&mut self, r: &StoredGitWorktree) -> Result<(), StorageError> {
        r.validate()?;
        if r.revision != 1 || r.removed {
            return Err(StorageError::InvalidRecord(
                "initial Git ownership revision/state".into(),
            ));
        }
        let payload =
            serde_json::to_string(r).map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
        self.conn.execute("INSERT INTO git_owned_worktrees(id,repository_id,work_id,owner_id,root_id,path,branch,payload,revision) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,1)",params![r.id.as_str(),r.repository_id.as_str(),r.work_id.as_str(),r.owner.0.as_str(),r.registered_root_id.as_str(),r.path,r.branch,payload]).map_err(db)?;
        Ok(())
    }
    fn update_git_worktree(
        &mut self,
        r: &StoredGitWorktree,
        expected: i64,
    ) -> Result<(), StorageError> {
        r.validate()?;
        let old = one(self.conn, &r.id)?
            .ok_or_else(|| StorageError::InvalidRecord("unknown owned worktree".into()))?;
        let mut permitted = old.clone();
        if r.owner != old.owner {
            if old.ownership_revision.checked_add(1) != Some(r.ownership_revision) {
                return Err(StorageError::InvalidRecord(
                    "ownership transfer epoch".into(),
                ));
            }
            permitted.owner = r.owner.clone();
            permitted.ownership_revision = r.ownership_revision;
            permitted.owner_root_id = r.owner_root_id.clone();
            permitted.owner_assignment_id = r.owner_assignment_id.clone();
        }
        if old.location.is_none()
            && old.pending_operation_id.is_some()
            && r.pending_operation_id.is_none()
            && r.last_operation_id == old.pending_operation_id
        {
            permitted.location = r.location.clone();
        }
        permitted.head = r.head.clone();
        permitted.pending_operation_id = r.pending_operation_id.clone();
        permitted.last_operation_id = r.last_operation_id.clone();
        permitted.last_result = r.last_result.clone();
        permitted.removed = r.removed;
        permitted.revision = r.revision;
        if old.head.format != r.head.format
            || old.removed
            || old.revision != expected
            || expected.checked_add(1) != Some(r.revision)
            || permitted != *r
        {
            return Err(StorageError::InvalidRecord(
                "stale or changed worktree ownership".into(),
            ));
        }
        let payload =
            serde_json::to_string(r).map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
        let count = self
            .conn
            .execute(
                "UPDATE git_owned_worktrees SET payload=?1,revision=?2,owner_id=?5 WHERE id=?3 AND revision=?4",
                params![payload, r.revision, r.id.as_str(), expected, r.owner.0.as_str()],
            )
            .map_err(db)?;
        if count != 1 {
            return Err(StorageError::InvalidRecord(
                "stale worktree revision".into(),
            ));
        }
        Ok(())
    }
}
