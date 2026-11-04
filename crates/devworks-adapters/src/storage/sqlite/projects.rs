//! SQLite project and repository registration records: `projects`, `repositories` and
//! `project_repositories` (migration 1) with their side tables `project_details` and
//! `repository_details`, and the app index views `repository_index_views` / `repository_texts`
//! (migration 14).
//!
//! A row of a migration 1 table without its side row reads with the migration's defaults, so a
//! project or repository recorded before migration 14 (or by a fixture) stays readable.
use super::conn::db;
use super::repos::{Snapshot, Unit};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{Id, ProjectId, Revision, RevisionDimension};
use rusqlite::types::Value as Sql;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};

fn invalid(m: impl Into<String>) -> StorageError {
    StorageError::InvalidRecord(m.into())
}
fn corrupt(m: impl Into<String>) -> StorageError {
    StorageError::unavailable(Unavailable::Corrupt, m)
}
fn id_of(s: String) -> Result<Id, StorageError> {
    Id::new(s).map_err(|e| corrupt(format!("stored identifier: {e}")))
}
fn limit(l: u16) -> Result<i64, StorageError> {
    if l == 0 || l > MAX_PAGE {
        return Err(invalid(format!("limit must be 1..={MAX_PAGE}")));
    }
    Ok(i64::from(l))
}
fn revision(n: i64) -> Result<Revision, StorageError> {
    Revision::new(n).map_err(|_| corrupt("stored revision is not positive"))
}
fn lifecycle_ok(s: &str) -> bool {
    LIFECYCLES.contains(&s)
}
fn coverage_ok(s: &str) -> bool {
    COVERAGE.contains(&s)
}

// ---------------------------------------------------------------------------------------------
// Projects

const PROJECT_COLUMNS: &str = "p.id,p.title,COALESCE(d.description,''),p.lifecycle,p.revision,p.scope_revision,p.created_at,COALESCE(d.updated_at,p.created_at),d.archived_at,d.deleted_at,COALESCE(d.archive_epoch,0),COALESCE(d.archive_stop_pending,0)";
const PROJECT_FROM: &str = "projects p LEFT JOIN project_details d ON d.project_id=p.id";

type ProjectRaw = (
    String,
    String,
    String,
    String,
    i64,
    i64,
    i64,
    i64,
    Option<i64>,
    Option<i64>,
    i64,
    i64,
);
fn project_row(r: &Row<'_>) -> rusqlite::Result<ProjectRaw> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
        r.get(10)?,
        r.get(11)?,
    ))
}
fn project_of(raw: ProjectRaw) -> Result<StoredProject, StorageError> {
    let (
        id,
        name,
        description,
        lifecycle,
        rev,
        scope,
        created,
        updated,
        archived,
        deleted,
        epoch,
        pending,
    ) = raw;
    if !lifecycle_ok(&lifecycle) {
        return Err(corrupt(format!("stored project lifecycle {lifecycle}")));
    }
    Ok(StoredProject {
        id: ProjectId(id_of(id)?),
        name,
        description,
        lifecycle,
        revision: rev,
        scope_revision: scope,
        created_at_ms: created,
        updated_at_ms: updated,
        archived_at_ms: archived,
        deleted_at_ms: deleted,
        archive_epoch: epoch,
        archive_stop_pending: pending != 0,
    })
}
fn one_project(conn: &Connection, id: &ProjectId) -> Result<Option<StoredProject>, StorageError> {
    let raw = db(db(conn.prepare_cached(&format!(
        "SELECT {PROJECT_COLUMNS} FROM {PROJECT_FROM} WHERE p.id=?1"
    )))?
    .query_row([id.0.as_str()], project_row)
    .optional())?;
    raw.map(project_of).transpose()
}
fn list_projects(
    conn: &Connection,
    filter: &ProjectFilter,
    after: Option<&ProjectPosition>,
    n: u16,
) -> Result<Vec<StoredProject>, StorageError> {
    let n = limit(n)?;
    if filter.lifecycles.is_empty() || !filter.lifecycles.iter().all(|l| lifecycle_ok(l)) {
        return Err(invalid("a project listing names known lifecycles"));
    }
    let mut sql = format!("SELECT {PROJECT_COLUMNS} FROM {PROJECT_FROM} WHERE p.lifecycle IN (");
    let mut values: Vec<Sql> = Vec::new();
    for (i, l) in filter.lifecycles.iter().enumerate() {
        if i > 0 {
            sql.push(',');
        }
        sql.push('?');
        values.push(Sql::Text(l.clone()));
    }
    sql.push(')');
    if let Some(p) = &filter.only {
        sql.push_str(" AND p.id=?");
        values.push(Sql::Text(p.0.as_str().to_owned()));
    }
    if let Some(a) = after {
        sql.push_str(" AND (p.created_at>? OR (p.created_at=? AND p.id>?))");
        values.push(Sql::Integer(a.created_at_ms));
        values.push(Sql::Integer(a.created_at_ms));
        values.push(Sql::Text(a.id.0.as_str().to_owned()));
    }
    sql.push_str(" ORDER BY p.created_at, p.id LIMIT ?");
    values.push(Sql::Integer(n));
    let mut s = db(conn.prepare(&sql))?;
    let rows = db(db(s.query_map(params_from_iter(values), project_row))?
        .collect::<rusqlite::Result<Vec<_>>>())?;
    rows.into_iter().map(project_of).collect()
}
fn ids(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> Result<Vec<Id>, StorageError> {
    let mut s = db(conn.prepare_cached(sql))?;
    let rows: Vec<String> =
        db(db(s.query_map(p, |r| r.get(0)))?.collect::<rusqlite::Result<Vec<_>>>())?;
    rows.into_iter().map(id_of).collect()
}

// ---------------------------------------------------------------------------------------------
// Repositories

const REPOSITORY_COLUMNS: &str = "r.id,r.canonical_location,d.name,d.remote_url,COALESCE(d.is_primary,1),COALESCE(d.lifecycle,'Active'),COALESCE(d.languages_json,'[]'),r.revision,COALESCE(d.registered_at,0)";
const REPOSITORY_FROM: &str =
    "repositories r LEFT JOIN repository_details d ON d.repository_id=r.id";

type RepositoryRaw = (
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    String,
    String,
    i64,
    i64,
);
fn repository_row(r: &Row<'_>) -> rusqlite::Result<RepositoryRaw> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
    ))
}
/// The name a registration without a recorded one reads with: its location's last component.
pub(crate) fn location_name(location: &str) -> String {
    let trimmed = location.trim_end_matches(['/', '\\']);
    let last = trimmed.rsplit(['/', '\\']).next().unwrap_or("");
    let name: String = if last.is_empty() { trimmed } else { last }
        .chars()
        .take(256)
        .collect();
    if name.is_empty() {
        "repository".into()
    } else {
        name
    }
}
fn repository_of(raw: RepositoryRaw) -> Result<StoredRepository, StorageError> {
    let (id, location, name, remote, primary, lifecycle, languages, rev, registered) = raw;
    if !lifecycle_ok(&lifecycle) {
        return Err(corrupt(format!("stored repository lifecycle {lifecycle}")));
    }
    let languages: Vec<String> = serde_json::from_str(&languages)
        .map_err(|e| corrupt(format!("stored repository languages: {e}")))?;
    Ok(StoredRepository {
        id: id_of(id)?,
        name: name.unwrap_or_else(|| location_name(&location)),
        location,
        remote_url: remote,
        primary: primary != 0,
        lifecycle,
        languages,
        revision: rev,
        registered_at_ms: registered,
    })
}
fn one_repository(conn: &Connection, id: &Id) -> Result<Option<StoredRepository>, StorageError> {
    let raw = db(db(conn.prepare_cached(&format!(
        "SELECT {REPOSITORY_COLUMNS} FROM {REPOSITORY_FROM} WHERE r.id=?1"
    )))?
    .query_row([id.as_str()], repository_row)
    .optional())?;
    raw.map(repository_of).transpose()
}
/// The one case folding of repository search, applied in Rust to the query and to every stored
/// value alike (SQLite's `lower()` and `LIKE` fold only ASCII, so `Äpp` would not match `äpp`).
pub(crate) fn fold(s: &str) -> String {
    s.to_lowercase()
}
/// Whether the folded `needle` occurs in the repository's name, location or shown remote (the
/// remote as displayed, so a query can never probe a stored credential).
fn text_matches(r: &StoredRepository, needle: &str) -> bool {
    let remote = r
        .remote_url
        .as_deref()
        .and_then(devworks_core::services::projects::display_remote)
        .map(|u| u.0)
        .unwrap_or_default();
    [r.name.as_str(), r.location.as_str(), remote.as_str()]
        .iter()
        .any(|v| fold(v).contains(needle))
}

fn list_repositories(
    conn: &Connection,
    filter: &RepositoryFilter,
    after: Option<&Id>,
    n: u16,
) -> Result<Vec<StoredRepository>, StorageError> {
    let n = usize::try_from(limit(n)?).unwrap_or(0);
    if filter.languages.len() > 128 {
        return Err(invalid("at most 128 languages"));
    }
    let mut sql = format!("SELECT {REPOSITORY_COLUMNS} FROM {REPOSITORY_FROM} WHERE r.id>?");
    let mut values: Vec<Sql> = Vec::new();
    if !filter.include_deleted {
        sql.push_str(" AND COALESCE(d.lifecycle,'Active')<>'Deleted'");
    }
    if let Some(p) = &filter.project {
        sql.push_str(
            " AND EXISTS(SELECT 1 FROM project_repositories m WHERE m.repository_id=r.id AND m.project_id=?)",
        );
        values.push(Sql::Text(p.0.as_str().to_owned()));
    }
    // Languages are stored folded by this adapter, so an exact match on the folded name is the
    // same comparison on both sides.
    for l in &filter.languages {
        sql.push_str(
            " AND EXISTS(SELECT 1 FROM json_each(COALESCE(d.languages_json,'[]')) j WHERE j.value=?)",
        );
        values.push(Sql::Text(fold(l)));
    }
    sql.push_str(" ORDER BY r.id LIMIT ?");
    let needle = filter.text.as_deref().map(fold);
    let mut out = Vec::new();
    let mut cursor = after.map_or(String::new(), |a| a.as_str().to_owned());
    // The text filter runs in Rust over bounded batches in ID order, so a page still ends at its
    // last row and continues after it.
    loop {
        let batch = i64::from(MAX_PAGE);
        let mut params = vec![Sql::Text(cursor.clone())];
        params.extend(values.iter().cloned());
        params.push(Sql::Integer(batch));
        let mut st = db(conn.prepare(&sql))?;
        let rows = db(db(st.query_map(params_from_iter(params), repository_row))?
            .collect::<rusqlite::Result<Vec<_>>>())?;
        let full = rows.len() == usize::from(MAX_PAGE);
        for raw in rows {
            let r = repository_of(raw)?;
            cursor = r.id.as_str().to_owned();
            if needle.as_deref().is_none_or(|t| text_matches(&r, t)) {
                out.push(r);
                if out.len() == n {
                    return Ok(out);
                }
            }
        }
        if !full {
            return Ok(out);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Index views and texts

fn view_row(r: &Row<'_>) -> rusqlite::Result<(String, String, String, String, i64)> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
}
fn view_of(raw: (String, String, String, String, i64)) -> Result<StoredIndexView, StorageError> {
    let (view, repo, source, coverage, published) = raw;
    if !coverage_ok(&coverage) {
        return Err(corrupt(format!("stored view coverage {coverage}")));
    }
    Ok(StoredIndexView {
        view_id: id_of(view)?,
        repository_id: id_of(repo)?,
        source_revision: source,
        coverage,
        published_at_ms: published,
    })
}
const VIEW_COLUMNS: &str = "view_id,repository_id,source_revision,coverage,published_at";

type TextRaw = (
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
    String,
    String,
);
fn text_of(raw: TextRaw) -> Result<StoredRepositoryText, StorageError> {
    let (view, repo, kind, artifact, version, sha, bytes, media, coverage) = raw;
    let kind = TextKind::parse(&kind).ok_or_else(|| corrupt("stored repository text kind"))?;
    if !coverage_ok(&coverage) {
        return Err(corrupt(format!("stored text coverage {coverage}")));
    }
    Ok(StoredRepositoryText {
        view_id: id_of(view)?,
        repository_id: id_of(repo)?,
        kind,
        artifact_id: id_of(artifact)?,
        version_id: id_of(version)?,
        sha256: sha,
        byte_length: bytes,
        media_type: media,
        coverage,
    })
}

macro_rules! project_reads {
    ($t:ty) => {
        impl ProjectRead for $t {
            fn project(&self, id: &ProjectId) -> Result<Option<StoredProject>, StorageError> {
                one_project(self.conn, id)
            }
            fn projects(
                &self,
                filter: &ProjectFilter,
                after: Option<&ProjectPosition>,
                n: u16,
            ) -> Result<Vec<StoredProject>, StorageError> {
                list_projects(self.conn, filter, after, n)
            }
            fn members(&self, project: &ProjectId) -> Result<Vec<Id>, StorageError> {
                ids(
                    self.conn,
                    "SELECT repository_id FROM project_repositories WHERE project_id=?1 ORDER BY repository_id",
                    [project.0.as_str()],
                )
            }
            fn repository(&self, id: &Id) -> Result<Option<StoredRepository>, StorageError> {
                one_repository(self.conn, id)
            }
            fn repositories(
                &self,
                filter: &RepositoryFilter,
                after: Option<&Id>,
                n: u16,
            ) -> Result<Vec<StoredRepository>, StorageError> {
                list_repositories(self.conn, filter, after, n)
            }
            fn repository_projects(&self, repository: &Id) -> Result<Vec<ProjectId>, StorageError> {
                Ok(ids(
                    self.conn,
                    "SELECT m.project_id FROM project_repositories m JOIN projects p ON p.id=m.project_id WHERE m.repository_id=?1 AND p.lifecycle<>'Deleted' ORDER BY m.project_id",
                    [repository.as_str()],
                )?
                .into_iter()
                .map(ProjectId)
                .collect())
            }
            fn index_views(&self, repository: &Id) -> Result<Vec<StoredIndexView>, StorageError> {
                let mut s = db(self.conn.prepare_cached(&format!(
                    "SELECT {VIEW_COLUMNS} FROM repository_index_views WHERE repository_id=?1 ORDER BY published_at DESC, view_id DESC LIMIT 500"
                )))?;
                let rows = db(db(s.query_map([repository.as_str()], view_row))?
                    .collect::<rusqlite::Result<Vec<_>>>())?;
                rows.into_iter().map(view_of).collect()
            }
            fn index_view(&self, view: &Id) -> Result<Option<StoredIndexView>, StorageError> {
                let raw = db(db(self.conn.prepare_cached(&format!(
                    "SELECT {VIEW_COLUMNS} FROM repository_index_views WHERE view_id=?1"
                )))?
                .query_row([view.as_str()], view_row)
                .optional())?;
                raw.map(view_of).transpose()
            }
            fn repository_text(
                &self,
                view: &Id,
                kind: TextKind,
            ) -> Result<Option<StoredRepositoryText>, StorageError> {
                let raw = db(db(self.conn.prepare_cached(
                    "SELECT view_id,repository_id,kind,artifact_id,version_id,sha256,byte_length,media_type,coverage FROM repository_texts WHERE view_id=?1 AND kind=?2",
                ))?
                .query_row(params![view.as_str(), kind.as_str()], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                    ))
                })
                .optional())?;
                raw.map(text_of).transpose()
            }
            fn work_naming_repository(
                &self,
                project: &ProjectId,
                repository: &Id,
            ) -> Result<i64, StorageError> {
                db(self.conn.query_row(
                    "SELECT count(*) FROM work_repositories wr JOIN work_items w ON w.id=wr.work_id WHERE w.project_id=?1 AND wr.repository_id=?2",
                    params![project.0.as_str(), repository.as_str()],
                    |r| r.get(0),
                ))
            }
            fn pending_archive_stops(&self, n: u16) -> Result<Vec<ProjectId>, StorageError> {
                let n = limit(n)?;
                Ok(ids(
                    self.conn,
                    "SELECT project_id FROM project_details WHERE archive_stop_pending=1 ORDER BY project_id LIMIT ?1",
                    [n],
                )?
                .into_iter()
                .map(ProjectId)
                .collect())
            }
        }
    };
}
project_reads!(Snapshot<'_>);
project_reads!(Unit<'_>);

fn check_project(p: &StoredProject) -> Result<(), StorageError> {
    if !lifecycle_ok(&p.lifecycle) {
        return Err(invalid(format!(
            "unknown project lifecycle {}",
            p.lifecycle
        )));
    }
    let name = p.name.chars().count();
    if !(1..=256).contains(&name) || p.name.contains('\0') {
        return Err(invalid("a project name is 1-256 characters without NUL"));
    }
    if p.description.chars().count() > 65536 || p.description.contains('\0') {
        return Err(invalid("a project description is at most 65536 characters"));
    }
    if p.revision < 1 || p.scope_revision < 1 {
        return Err(invalid("project revisions are positive"));
    }
    Ok(())
}
fn write_details(conn: &Connection, p: &StoredProject) -> Result<(), StorageError> {
    db(db(conn.prepare_cached(
        "INSERT INTO project_details(project_id,description,updated_at,archived_at,deleted_at,archive_epoch,archive_stop_pending) VALUES(?1,?2,?3,?4,?5,?6,?7) \
         ON CONFLICT(project_id) DO UPDATE SET description=excluded.description,updated_at=excluded.updated_at,archived_at=excluded.archived_at,deleted_at=excluded.deleted_at,archive_epoch=excluded.archive_epoch,archive_stop_pending=excluded.archive_stop_pending",
    ))?
    .execute(params![
        p.id.0.as_str(),
        p.description,
        p.updated_at_ms,
        p.archived_at_ms,
        p.deleted_at_ms,
        p.archive_epoch,
        i64::from(p.archive_stop_pending)
    ]))?;
    Ok(())
}
fn repository_exists(conn: &Connection, id: &Id) -> Result<bool, StorageError> {
    db(db(conn.prepare_cached("SELECT 1 FROM repositories WHERE id=?1"))?.exists([id.as_str()]))
}

impl ProjectRepository for Unit<'_> {
    fn insert_project(&mut self, p: &StoredProject) -> Result<(), StorageError> {
        check_project(p)?;
        if p.revision != 1 {
            return Err(invalid("a new project starts at revision 1"));
        }
        if one_project(self.conn, &p.id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "project",
                id: p.id.0.as_str().to_owned(),
            });
        }
        db(db(self.conn.prepare_cached(
            "INSERT INTO projects(id,title,lifecycle,revision,scope_revision,created_at) VALUES(?1,?2,?3,1,?4,?5)",
        ))?
        .execute(params![
            p.id.0.as_str(),
            p.name,
            p.lifecycle,
            p.scope_revision,
            p.created_at_ms
        ]))?;
        write_details(self.conn, p)
    }
    fn update_project(&mut self, p: &StoredProject, expected: i64) -> Result<(), StorageError> {
        check_project(p)?;
        let Some(current) = one_project(self.conn, &p.id)? else {
            return Err(StorageError::NotFound {
                kind: "project",
                id: p.id.0.as_str().to_owned(),
            });
        };
        if current.revision != expected {
            return Err(StorageError::conflict(
                RevisionDimension::Scope,
                "project",
                p.id.0.clone(),
                Revision::new(expected).map_err(|_| invalid("expected revision is positive"))?,
                revision(current.revision)?,
            ));
        }
        if p.revision != expected + 1
            || p.scope_revision < current.scope_revision
            || p.created_at_ms != current.created_at_ms
        {
            return Err(invalid(
                "a project update advances its revision by one and keeps its creation",
            ));
        }
        let n = db(db(self.conn.prepare_cached(
            "UPDATE projects SET title=?1,lifecycle=?2,revision=?3,scope_revision=?4 WHERE id=?5 AND revision=?6",
        ))?
        .execute(params![
            p.name,
            p.lifecycle,
            p.revision,
            p.scope_revision,
            p.id.0.as_str(),
            expected
        ]))?;
        if n != 1 {
            return Err(corrupt("project changed inside its transaction"));
        }
        write_details(self.conn, p)
    }
    fn set_members(&mut self, project: &ProjectId, repos: &[Id]) -> Result<(), StorageError> {
        if one_project(self.conn, project)?.is_none() {
            return Err(StorageError::NotFound {
                kind: "project",
                id: project.0.as_str().to_owned(),
            });
        }
        let mut seen = std::collections::BTreeSet::new();
        for r in repos {
            if !seen.insert(r.as_str()) {
                return Err(invalid("a membership names each repository once"));
            }
            if !repository_exists(self.conn, r)? {
                return Err(StorageError::NotFound {
                    kind: "repository",
                    id: r.as_str().to_owned(),
                });
            }
        }
        db(db(self
            .conn
            .prepare_cached("DELETE FROM project_repositories WHERE project_id=?1"))?
        .execute([project.0.as_str()]))?;
        for r in repos {
            db(db(self.conn.prepare_cached(
                "INSERT INTO project_repositories(project_id,repository_id) VALUES(?1,?2)",
            ))?
            .execute(params![project.0.as_str(), r.as_str()]))?;
        }
        Ok(())
    }
    fn insert_repository(&mut self, r: &StoredRepository) -> Result<(), StorageError> {
        if !lifecycle_ok(&r.lifecycle) || r.revision != 1 {
            return Err(invalid(
                "a new repository is revision 1 with a known lifecycle",
            ));
        }
        let name = r.name.chars().count();
        if !(1..=256).contains(&name) || r.location.is_empty() {
            return Err(invalid(
                "a repository has a 1-256 character name and a location",
            ));
        }
        if r.remote_url
            .as_ref()
            .is_some_and(|u| u.is_empty() || u.len() > 4096)
            || r.languages.len() > 128
        {
            return Err(invalid("repository remote or languages out of bounds"));
        }
        if r.remote_url
            .as_deref()
            .is_some_and(devworks_core::services::projects::carries_credentials)
        {
            return Err(invalid("a registered remote never carries credentials"));
        }
        if repository_exists(self.conn, &r.id)? {
            return Err(StorageError::Duplicate {
                kind: "repository",
                id: r.id.as_str().to_owned(),
            });
        }
        let taken: bool = db(db(self
            .conn
            .prepare_cached("SELECT 1 FROM repositories WHERE canonical_location=?1"))?
        .exists([r.location.as_str()]))?;
        if taken {
            return Err(StorageError::Duplicate {
                kind: "repository_location",
                id: r.id.as_str().to_owned(),
            });
        }
        let languages: Vec<String> = r.languages.iter().map(|l| fold(l)).collect();
        db(db(self.conn.prepare_cached(
            "INSERT INTO repositories(id,canonical_location,revision) VALUES(?1,?2,1)",
        ))?
        .execute(params![r.id.as_str(), r.location]))?;
        db(db(self.conn.prepare_cached(
            "INSERT INTO repository_details(repository_id,name,remote_url,is_primary,lifecycle,languages_json,registered_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        ))?
        .execute(params![
            r.id.as_str(),
            r.name,
            r.remote_url,
            i64::from(r.primary),
            r.lifecycle,
            serde_json::to_string(&languages).map_err(|e| invalid(e.to_string()))?,
            r.registered_at_ms
        ]))?;
        Ok(())
    }
    fn touch_repository(&mut self, id: &Id, expected: i64) -> Result<i64, StorageError> {
        let Some(current) = one_repository(self.conn, id)? else {
            return Err(StorageError::NotFound {
                kind: "repository",
                id: id.as_str().to_owned(),
            });
        };
        if current.revision != expected {
            return Err(StorageError::conflict(
                RevisionDimension::Registry,
                "repository",
                id.clone(),
                Revision::new(expected).map_err(|_| invalid("expected revision is positive"))?,
                revision(current.revision)?,
            ));
        }
        let n = db(db(self.conn.prepare_cached(
            "UPDATE repositories SET revision=revision+1 WHERE id=?1 AND revision=?2",
        ))?
        .execute(params![id.as_str(), expected]))?;
        if n != 1 {
            return Err(corrupt("repository changed inside its transaction"));
        }
        Ok(expected + 1)
    }
    fn publish_index_view(&mut self, v: &StoredIndexView) -> Result<(), StorageError> {
        if !coverage_ok(&v.coverage) || v.source_revision.is_empty() {
            return Err(invalid(
                "an index view has a source revision and a known coverage",
            ));
        }
        if !repository_exists(self.conn, &v.repository_id)? {
            return Err(StorageError::NotFound {
                kind: "repository",
                id: v.repository_id.as_str().to_owned(),
            });
        }
        db(db(self.conn.prepare_cached(&format!(
            "INSERT INTO repository_index_views({VIEW_COLUMNS}) VALUES(?1,?2,?3,?4,?5)"
        )))?
        .execute(params![
            v.view_id.as_str(),
            v.repository_id.as_str(),
            v.source_revision,
            v.coverage,
            v.published_at_ms
        ]))?;
        Ok(())
    }
    fn put_repository_text(&mut self, t: &StoredRepositoryText) -> Result<(), StorageError> {
        let Some(view) = self.index_view(&t.view_id)? else {
            return Err(StorageError::NotFound {
                kind: "index_view",
                id: t.view_id.as_str().to_owned(),
            });
        };
        if view.repository_id != t.repository_id || !coverage_ok(&t.coverage) {
            return Err(invalid(
                "a repository text belongs to its view's repository",
            ));
        }
        db(db(self.conn.prepare_cached(
            "INSERT INTO repository_texts(view_id,kind,repository_id,artifact_id,version_id,sha256,byte_length,media_type,coverage) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) \
             ON CONFLICT(view_id,kind) DO UPDATE SET artifact_id=excluded.artifact_id,version_id=excluded.version_id,sha256=excluded.sha256,byte_length=excluded.byte_length,media_type=excluded.media_type,coverage=excluded.coverage",
        ))?
        .execute(params![
            t.view_id.as_str(),
            t.kind.as_str(),
            t.repository_id.as_str(),
            t.artifact_id.as_str(),
            t.version_id.as_str(),
            t.sha256,
            t.byte_length,
            t.media_type,
            t.coverage
        ]))?;
        Ok(())
    }
    fn finish_archive_stop(
        &mut self,
        project: &ProjectId,
        epoch: i64,
    ) -> Result<bool, StorageError> {
        let n = db(db(self.conn.prepare_cached(
            "UPDATE project_details SET archive_stop_pending=0 WHERE project_id=?1 AND archive_epoch=?2 AND archive_stop_pending=1",
        ))?
        .execute(params![project.0.as_str(), epoch]))?;
        Ok(n == 1)
    }
    fn remove_index(&mut self, repository: &Id) -> Result<u64, StorageError> {
        db(db(self
            .conn
            .prepare_cached("DELETE FROM repository_texts WHERE repository_id=?1"))?
        .execute([repository.as_str()]))?;
        let n = db(db(self
            .conn
            .prepare_cached("DELETE FROM repository_index_views WHERE repository_id=?1"))?
        .execute([repository.as_str()]))?;
        Ok(n as u64)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn location_names_use_the_last_component() {
        assert_eq!(super::location_name("/Users/me/src/engine/"), "engine");
        assert_eq!(super::location_name("C:\\src\\tools"), "tools");
        assert_eq!(super::location_name("/"), "repository");
    }
}
