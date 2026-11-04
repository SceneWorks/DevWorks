//! SQLite repository indexing records (DW-023): migration 19's `index_source_configs`,
//! `repository_sources`, `repository_worktrees`, `index_runs` and `index_overlays`, and the
//! migration 1 graph payload tables (`index_generations`, `source_units`, `index_batches`,
//! `graph_nodes`, `graph_edges`, `index_views`, `published_views`, `index_pins`).
//!
//! Invariants enforced here: known state names, compare-and-set revisions, a terminal run never
//! changes again, only a `Staging` generation takes payload, a sealed generation only retires,
//! a retried unit batch with different content is a conflict, and the published pointer moves
//! only from the revision the publisher read.
use super::conn::db;
use super::repos::{Snapshot, Unit};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, Id, ProjectId, Revision, RevisionDimension};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};

fn invalid(m: impl Into<String>) -> StorageError {
    StorageError::InvalidRecord(m.into())
}
fn corrupt(m: impl Into<String>) -> StorageError {
    StorageError::unavailable(Unavailable::Corrupt, m)
}
fn id_of(s: String) -> Result<Id, StorageError> {
    Id::new(s).map_err(|e| corrupt(format!("stored identifier: {e}")))
}
fn opt_id(s: Option<String>) -> Result<Option<Id>, StorageError> {
    s.map(id_of).transpose()
}
fn limit(l: u16) -> Result<i64, StorageError> {
    if l == 0 || l > MAX_PAGE {
        return Err(invalid(format!("limit must be 1..={MAX_PAGE}")));
    }
    Ok(i64::from(l))
}
fn rev(n: i64) -> Result<Revision, StorageError> {
    Revision::new(n.max(1)).map_err(|_| invalid("revision is not positive"))
}
fn parse(json: &str) -> Result<Value, StorageError> {
    serde_json::from_str(json).map_err(|e| corrupt(format!("stored JSON: {e}")))
}
fn text(v: &Value) -> String {
    v.to_string()
}
fn strings(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}
fn oid_ok(o: &Option<String>) -> bool {
    o.as_ref().is_none_or(|o| {
        matches!(o.len(), 40 | 64)
            && o.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    })
}
fn repository_exists(conn: &Connection, id: &Id) -> Result<bool, StorageError> {
    db(db(conn.prepare_cached("SELECT 1 FROM repositories WHERE id=?1"))?.exists([id.as_str()]))
}

// ---------------------------------------------------------------------------------------------
// Rows

fn config_of(r: &Row<'_>) -> rusqlite::Result<(String, Option<String>, i64, String, i64)> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
}
fn config_from(
    raw: (String, Option<String>, i64, String, i64),
) -> Result<StoredSourceConfig, StorageError> {
    let (scope_key, project, revision, json, updated) = raw;
    let v = parse(&json)?;
    Ok(StoredSourceConfig {
        scope_key,
        project_id: opt_id(project)?.map(ProjectId),
        revision,
        structural_languages: strings(&v, "structuralLanguages"),
        semantic_packages: strings(&v, "semanticPackages"),
        exclude_patterns: strings(&v, "excludePatterns"),
        max_file_bytes: v.get("maxFileBytes").and_then(Value::as_i64).unwrap_or(0),
        updated_at_ms: updated,
    })
}

const SOURCE_COLUMNS: &str = "repository_id,section,section_root,location,state,head_oid,head_ref,object_format,remote_url,revision,observed_at";
fn source_of(r: &Row<'_>) -> rusqlite::Result<StoredRepositorySourceRaw> {
    Ok((
        r.get(2)?,
        r.get(0)?,
        r.get(1)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
        r.get(10)?,
    ))
}
type StoredRepositorySourceRaw = (
    Option<String>,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    i64,
    i64,
);
fn source_from(raw: StoredRepositorySourceRaw) -> Result<StoredRepositorySource, StorageError> {
    let (
        section_root,
        id,
        section,
        location,
        state,
        head_oid,
        head_ref,
        format,
        remote,
        revision,
        at,
    ) = raw;
    Ok(StoredRepositorySource {
        repository_id: id_of(id)?,
        section,
        section_root,
        location,
        state,
        head_oid,
        head_ref,
        object_format: format,
        remote_url: remote,
        revision,
        observed_at_ms: at,
    })
}

const WORKTREE_COLUMNS: &str =
    "repository_id,worktree_id,path,head_oid,head_ref,is_main,observed_at";
type WorktreeRaw = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    i64,
);
fn worktree_of(r: &Row<'_>) -> rusqlite::Result<WorktreeRaw> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
    ))
}
fn worktree_from(raw: WorktreeRaw) -> Result<StoredWorktree, StorageError> {
    let (repo, wt, path, head_oid, head_ref, main, at) = raw;
    Ok(StoredWorktree {
        repository_id: id_of(repo)?,
        worktree_id: id_of(wt)?,
        path,
        head_oid,
        head_ref,
        main: main != 0,
        observed_at_ms: at,
    })
}

const RUN_COLUMNS: &str = "id,batch_id,kind,repository_id,project_id,overlay_id,state,stage,revision,attempt,max_attempts,owner,lease_until,cancel_requested_at,config_revision,config_digest,source_oid,generation_id,completed_units,total_units,output_view_id,error_code,error_message,deterministic,warnings_json,requested_by,created_at,started_at,finished_at,next_attempt_at";
fn run_of(r: &Row<'_>) -> rusqlite::Result<Vec<rusqlite::types::Value>> {
    (0..30)
        .map(|i| r.get::<_, rusqlite::types::Value>(i))
        .collect()
}
fn run_from(v: Vec<rusqlite::types::Value>) -> Result<StoredIndexRun, StorageError> {
    use rusqlite::types::Value as S;
    let s = |i: usize| -> Result<String, StorageError> {
        match &v[i] {
            S::Text(t) => Ok(t.clone()),
            _ => Err(corrupt(format!("index run column {i}"))),
        }
    };
    let os = |i: usize| -> Option<String> {
        match &v[i] {
            S::Text(t) => Some(t.clone()),
            _ => None,
        }
    };
    let n = |i: usize| -> Result<i64, StorageError> {
        match &v[i] {
            S::Integer(x) => Ok(*x),
            _ => Err(corrupt(format!("index run column {i}"))),
        }
    };
    let on = |i: usize| -> Option<i64> {
        match &v[i] {
            S::Integer(x) => Some(*x),
            _ => None,
        }
    };
    let warnings = parse(&s(24)?)?
        .as_array()
        .map(|a| {
            a.iter()
                .map(|w| RunWarning {
                    code: w
                        .get("code")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    message: w
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(StoredIndexRun {
        id: id_of(s(0)?)?,
        batch_id: id_of(s(1)?)?,
        kind: s(2)?,
        repository_id: id_of(s(3)?)?,
        project_id: opt_id(os(4))?.map(ProjectId),
        overlay_id: opt_id(os(5))?,
        state: s(6)?,
        stage: s(7)?,
        revision: n(8)?,
        attempt: n(9)?,
        max_attempts: n(10)?,
        owner: os(11),
        lease_until_ms: on(12),
        cancel_requested_at_ms: on(13),
        config_revision: n(14)?,
        config_digest: s(15)?,
        source_oid: os(16),
        generation_id: opt_id(os(17))?,
        completed_units: n(18)?,
        total_units: on(19),
        output_view_id: opt_id(os(20))?,
        error_code: os(21),
        error_message: os(22),
        deterministic: n(23)? != 0,
        warnings,
        requested_by: ActorId(id_of(s(25)?)?),
        created_at_ms: n(26)?,
        started_at_ms: on(27),
        finished_at_ms: on(28),
        next_attempt_at_ms: n(29)?,
    })
}
fn check_run(run: &StoredIndexRun) -> Result<(), StorageError> {
    if !RUN_KINDS.contains(&run.kind.as_str()) || !RUN_STATES.contains(&run.state.as_str()) {
        return Err(invalid("index run kind or state"));
    }
    if (run.kind == RUN_OVERLAY) != run.overlay_id.is_some() {
        return Err(invalid("an overlay run names its overlay, and only it"));
    }
    if run.owner.is_some() != run.lease_until_ms.is_some() {
        return Err(invalid("an index run claim has an owner and a lease"));
    }
    if run.stage.is_empty() || run.stage.len() > 64 || run.config_digest.len() != 64 {
        return Err(invalid("index run stage or configuration digest"));
    }
    if !oid_ok(&run.source_oid) || run.revision < 1 || run.attempt < 0 {
        return Err(invalid("index run source commit or revision"));
    }
    if !(1..=16).contains(&run.max_attempts) || run.warnings.len() > 64 {
        return Err(invalid("index run attempts or warnings out of bounds"));
    }
    Ok(())
}
fn warnings_json(w: &[RunWarning]) -> String {
    Value::Array(
        w.iter()
            .map(|w| json!({"code": w.code, "message": w.message}))
            .collect(),
    )
    .to_string()
}

const GEN_COLUMNS: &str = "id,source_key,kind,state,source_digest,watermark,manifest_json";
type GenRaw = (String, String, String, String, String, i64, String);
fn gen_of(r: &Row<'_>) -> rusqlite::Result<GenRaw> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
    ))
}
fn gen_from(raw: GenRaw) -> Result<StoredGeneration, StorageError> {
    let (id, repo, layer, state, digest, watermark, manifest) = raw;
    Ok(StoredGeneration {
        id: id_of(id)?,
        repository_id: id_of(repo)?,
        layer,
        state,
        source_digest: digest,
        watermark,
        manifest: parse(&manifest)?,
    })
}

type UnitRaw = (String, String, String, String, String);
fn unit_of(r: &Row<'_>) -> rusqlite::Result<UnitRaw> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
}
fn unit_from(raw: UnitRaw) -> Result<StoredUnit, StorageError> {
    let (generation, key, digest, action, payload) = raw;
    Ok(StoredUnit {
        generation_id: id_of(generation)?,
        unit_key: key,
        content_digest: digest,
        action,
        payload: parse(&payload)?,
    })
}
type NodeRaw = (String, String, String, String, String);
fn node_of(r: &Row<'_>) -> rusqlite::Result<NodeRaw> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
}
fn node_from(raw: NodeRaw) -> Result<StoredNode, StorageError> {
    let (unit, id, kind, name, payload) = raw;
    Ok(StoredNode {
        unit_key: unit,
        node_id: id,
        kind,
        name,
        payload: parse(&payload)?,
    })
}
type EdgeRaw = (String, String, String, String, String, String);
fn edge_of(r: &Row<'_>) -> rusqlite::Result<EdgeRaw> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
    ))
}
fn edge_from(raw: EdgeRaw) -> Result<StoredEdge, StorageError> {
    let (unit, id, source, target, kind, payload) = raw;
    Ok(StoredEdge {
        unit_key: unit,
        edge_id: id,
        source_node_id: source,
        target_node_id: target,
        kind,
        payload: parse(&payload)?,
    })
}

const VIEW_COLUMNS: &str =
    "id,source_key,view_digest,baseline_id,branch_id,worktree_id,coverage_json";
type ViewRaw = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
);
fn view_of(r: &Row<'_>) -> rusqlite::Result<ViewRaw> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
    ))
}
fn view_from(raw: ViewRaw) -> Result<StoredView, StorageError> {
    let (id, repo, digest, baseline, branch, worktree, coverage) = raw;
    Ok(StoredView {
        view_id: id_of(id)?,
        repository_id: id_of(repo)?,
        view_digest: digest,
        baseline_id: id_of(baseline)?,
        branch_id: opt_id(branch)?,
        worktree_id: opt_id(worktree)?,
        coverage: parse(&coverage)?,
    })
}

const OVERLAY_COLUMNS: &str = "id,repository_id,worktree_id,worktree_path,state,revision,base_generation_id,base_oid,head_oid,head_ref,capture_digest,branch_generation_id,worktree_generation_id,view_id,run_id,reason,created_at,updated_at,expires_at";
fn overlay_of(r: &Row<'_>) -> rusqlite::Result<Vec<rusqlite::types::Value>> {
    (0..19)
        .map(|i| r.get::<_, rusqlite::types::Value>(i))
        .collect()
}
fn overlay_from(v: Vec<rusqlite::types::Value>) -> Result<StoredOverlay, StorageError> {
    use rusqlite::types::Value as S;
    let s = |i: usize| match &v[i] {
        S::Text(t) => Ok(t.clone()),
        _ => Err(corrupt(format!("overlay column {i}"))),
    };
    let os = |i: usize| match &v[i] {
        S::Text(t) => Some(t.clone()),
        _ => None,
    };
    let n = |i: usize| match &v[i] {
        S::Integer(x) => Ok(*x),
        _ => Err(corrupt(format!("overlay column {i}"))),
    };
    Ok(StoredOverlay {
        id: id_of(s(0)?)?,
        repository_id: id_of(s(1)?)?,
        worktree_id: opt_id(os(2))?,
        worktree_path: os(3),
        state: s(4)?,
        revision: n(5)?,
        base_generation_id: opt_id(os(6))?,
        base_oid: os(7),
        head_oid: os(8),
        head_ref: os(9),
        capture_digest: os(10),
        branch_generation_id: opt_id(os(11))?,
        worktree_generation_id: opt_id(os(12))?,
        view_id: opt_id(os(13))?,
        run_id: opt_id(os(14))?,
        reason: os(15),
        created_at_ms: n(16)?,
        updated_at_ms: n(17)?,
        expires_at_ms: n(18)?,
    })
}

fn generation_state(conn: &Connection, id: &Id) -> Result<Option<String>, StorageError> {
    db(
        db(conn.prepare_cached("SELECT state FROM index_generations WHERE id=?1"))?
            .query_row([id.as_str()], |r| r.get(0))
            .optional(),
    )
}
fn require_staging(conn: &Connection, id: &Id) -> Result<(), StorageError> {
    match generation_state(conn, id)?.as_deref() {
        Some(GEN_STAGING) => Ok(()),
        Some(other) => Err(invalid(format!(
            "generation {} is {other}: only a staging generation takes payload",
            id.as_str()
        ))),
        None => Err(StorageError::NotFound {
            kind: "index_generation",
            id: id.as_str().to_owned(),
        }),
    }
}
/// The digest a unit batch is identified by: a retried batch must carry the same rows.
fn batch_digest(unit: &StoredUnit, nodes: &[StoredNode], edges: &[StoredEdge]) -> String {
    let mut basis = format!(
        "{}\u{0}{}\u{0}{}\u{0}{}",
        unit.unit_key,
        unit.content_digest,
        unit.action,
        text(&unit.payload)
    );
    for n in nodes {
        basis.push_str(&format!(
            "\u{1}{}\u{0}{}\u{0}{}\u{0}{}",
            n.node_id,
            n.kind,
            n.name,
            text(&n.payload)
        ));
    }
    for e in edges {
        basis.push_str(&format!(
            "\u{2}{}\u{0}{}\u{0}{}\u{0}{}",
            e.edge_id, e.source_node_id, e.target_node_id, e.kind
        ));
    }
    sha256_hex(basis.as_bytes())
}

// ---------------------------------------------------------------------------------------------
// Reads

macro_rules! index_reads {
    ($t:ty) => {
        impl IndexRead for $t {
            fn source_config(
                &self,
                scope_key: &str,
            ) -> Result<Option<StoredSourceConfig>, StorageError> {
                let raw = db(db(self.conn.prepare_cached(
                    "SELECT scope_key,project_id,revision,config_json,updated_at FROM index_source_configs WHERE scope_key=?1",
                ))?
                .query_row([scope_key], config_of)
                .optional())?;
                raw.map(config_from).transpose()
            }
            fn repository_source(
                &self,
                repository: &Id,
            ) -> Result<Option<StoredRepositorySource>, StorageError> {
                let raw = db(db(self.conn.prepare_cached(&format!(
                    "SELECT {SOURCE_COLUMNS} FROM repository_sources WHERE repository_id=?1"
                )))?
                .query_row([repository.as_str()], source_of)
                .optional())?;
                raw.map(source_from).transpose()
            }
            fn repository_at(&self, location: &str) -> Result<Option<Id>, StorageError> {
                let raw: Option<String> = db(db(self
                    .conn
                    .prepare_cached("SELECT id FROM repositories WHERE canonical_location=?1"))?
                .query_row([location], |r| r.get(0))
                .optional())?;
                opt_id(raw)
            }
            fn worktrees(&self, repository: &Id) -> Result<Vec<StoredWorktree>, StorageError> {
                let mut s = db(self.conn.prepare_cached(&format!(
                    "SELECT {WORKTREE_COLUMNS} FROM repository_worktrees WHERE repository_id=?1 ORDER BY is_main DESC, path LIMIT 256"
                )))?;
                let rows = db(db(s.query_map([repository.as_str()], worktree_of))?
                    .collect::<rusqlite::Result<Vec<_>>>())?;
                rows.into_iter().map(worktree_from).collect()
            }
            fn run(&self, id: &Id) -> Result<Option<StoredIndexRun>, StorageError> {
                let raw = db(db(self.conn.prepare_cached(&format!(
                    "SELECT {RUN_COLUMNS} FROM index_runs WHERE id=?1"
                )))?
                .query_row([id.as_str()], run_of)
                .optional())?;
                raw.map(run_from).transpose()
            }
            fn runs(
                &self,
                filter: &IndexRunFilter,
                after: Option<&RunPosition>,
                n: u16,
            ) -> Result<Vec<StoredIndexRun>, StorageError> {
                let n = limit(n)?;
                let mut sql = format!("SELECT {RUN_COLUMNS} FROM index_runs WHERE 1=1");
                let mut values: Vec<rusqlite::types::Value> = Vec::new();
                if let Some(r) = &filter.repository {
                    sql.push_str(" AND repository_id=?");
                    values.push(r.as_str().to_owned().into());
                }
                if let Some(rs) = &filter.repositories {
                    if rs.is_empty() {
                        return Ok(Vec::new());
                    }
                    if rs.len() > 500 {
                        return Err(invalid("a run listing names at most 500 repositories"));
                    }
                    sql.push_str(" AND repository_id IN (");
                    for (i, r) in rs.iter().enumerate() {
                        if i > 0 {
                            sql.push(',');
                        }
                        sql.push('?');
                        values.push(r.as_str().to_owned().into());
                    }
                    sql.push(')');
                }
                for (column, set) in [("state", &filter.states), ("kind", &filter.kinds)] {
                    let Some(set) = set else { continue };
                    if set.is_empty() {
                        return Ok(Vec::new());
                    }
                    if set.len() > 16 {
                        return Err(invalid("a run listing names at most 16 states and kinds"));
                    }
                    sql.push_str(&format!(" AND {column} IN ("));
                    for (i, v) in set.iter().enumerate() {
                        if i > 0 {
                            sql.push(',');
                        }
                        sql.push('?');
                        values.push(v.clone().into());
                    }
                    sql.push(')');
                }
                if let Some(a) = after {
                    sql.push_str(" AND (created_at<? OR (created_at=? AND id<?))");
                    values.push(a.created_at_ms.into());
                    values.push(a.created_at_ms.into());
                    values.push(a.id.as_str().to_owned().into());
                }
                sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
                values.push(n.into());
                let mut s = db(self.conn.prepare(&sql))?;
                let rows = db(db(s.query_map(rusqlite::params_from_iter(values), run_of))?
                    .collect::<rusqlite::Result<Vec<_>>>())?;
                rows.into_iter().map(run_from).collect()
            }
            fn active_run(
                &self,
                repository: &Id,
                overlay: Option<&Id>,
            ) -> Result<Option<StoredIndexRun>, StorageError> {
                let raw = db(db(self.conn.prepare_cached(&format!(
                    "SELECT {RUN_COLUMNS} FROM index_runs WHERE repository_id=?1 AND COALESCE(overlay_id,'')=?2 AND state IN ('Queued','Running')"
                )))?
                .query_row(
                    params![repository.as_str(), overlay.map_or("", Id::as_str)],
                    run_of,
                )
                .optional())?;
                raw.map(run_from).transpose()
            }
            fn batch_runs(&self, batch: &Id) -> Result<Vec<StoredIndexRun>, StorageError> {
                let mut s = db(self.conn.prepare_cached(&format!(
                    "SELECT {RUN_COLUMNS} FROM index_runs WHERE batch_id=?1 ORDER BY id LIMIT 500"
                )))?;
                let rows = db(db(s.query_map([batch.as_str()], run_of))?
                    .collect::<rusqlite::Result<Vec<_>>>())?;
                rows.into_iter().map(run_from).collect()
            }
            fn latest_run(
                &self,
                repository: &Id,
                overlay: Option<&Id>,
            ) -> Result<Option<StoredIndexRun>, StorageError> {
                let raw = db(db(self.conn.prepare_cached(&format!(
                    "SELECT {RUN_COLUMNS} FROM index_runs WHERE repository_id=?1 AND COALESCE(overlay_id,'')=?2 ORDER BY created_at DESC, id DESC LIMIT 1"
                )))?
                .query_row(
                    params![repository.as_str(), overlay.map_or("", Id::as_str)],
                    run_of,
                )
                .optional())?;
                raw.map(run_from).transpose()
            }
            fn claimable_runs(
                &self,
                now_ms: i64,
                n: u16,
            ) -> Result<Vec<StoredIndexRun>, StorageError> {
                let n = limit(n)?;
                let mut s = db(self.conn.prepare_cached(&format!(
                    "SELECT {RUN_COLUMNS} FROM index_runs r WHERE ((state='Queued' AND next_attempt_at<=?1) OR (state='Running' AND lease_until<?1)) AND NOT EXISTS (SELECT 1 FROM index_runs b WHERE b.repository_id=r.repository_id AND b.state='Running' AND b.lease_until>=?1) ORDER BY created_at, id LIMIT ?2"
                )))?;
                let rows = db(db(s.query_map(params![now_ms, n], run_of))?
                    .collect::<rusqlite::Result<Vec<_>>>())?;
                rows.into_iter().map(run_from).collect()
            }
            fn running_runs(&self, n: u16) -> Result<Vec<StoredIndexRun>, StorageError> {
                let n = limit(n)?;
                let mut s = db(self.conn.prepare_cached(&format!(
                    "SELECT {RUN_COLUMNS} FROM index_runs WHERE state='Running' ORDER BY id LIMIT ?1"
                )))?;
                let rows = db(db(s.query_map([n], run_of))?
                    .collect::<rusqlite::Result<Vec<_>>>())?;
                rows.into_iter().map(run_from).collect()
            }
            fn generation(&self, id: &Id) -> Result<Option<StoredGeneration>, StorageError> {
                let raw = db(db(self.conn.prepare_cached(&format!(
                    "SELECT {GEN_COLUMNS} FROM index_generations WHERE id=?1"
                )))?
                .query_row([id.as_str()], gen_of)
                .optional())?;
                raw.map(gen_from).transpose()
            }
            fn generations(
                &self,
                repository: &Id,
            ) -> Result<Vec<StoredGeneration>, StorageError> {
                let mut s = db(self.conn.prepare_cached(&format!(
                    "SELECT {GEN_COLUMNS} FROM index_generations WHERE source_key=?1 ORDER BY id LIMIT 500"
                )))?;
                let rows = db(db(s.query_map([repository.as_str()], gen_of))?
                    .collect::<rusqlite::Result<Vec<_>>>())?;
                rows.into_iter().map(gen_from).collect()
            }
            fn generation_in_view(&self, generation: &Id) -> Result<bool, StorageError> {
                db(db(self.conn.prepare_cached(
                    "SELECT 1 FROM index_views WHERE baseline_id=?1 OR branch_id=?1 OR worktree_id=?1",
                ))?
                .exists([generation.as_str()]))
            }
            fn generations_in_state(
                &self,
                state: &str,
                n: u16,
            ) -> Result<Vec<StoredGeneration>, StorageError> {
                let n = limit(n)?;
                let mut s = db(self.conn.prepare_cached(&format!(
                    "SELECT {GEN_COLUMNS} FROM index_generations WHERE state=?1 ORDER BY id LIMIT ?2"
                )))?;
                let rows = db(db(s.query_map(params![state, n], gen_of))?
                    .collect::<rusqlite::Result<Vec<_>>>())?;
                rows.into_iter().map(gen_from).collect()
            }
            fn units(
                &self,
                generation: &Id,
                after: Option<&str>,
                n: u16,
            ) -> Result<Vec<StoredUnit>, StorageError> {
                let n = limit(n)?;
                let mut s = db(self.conn.prepare_cached(
                    "SELECT generation_id,unit_key,content_digest,action,payload_json FROM source_units WHERE generation_id=?1 AND unit_key>?2 ORDER BY unit_key LIMIT ?3",
                ))?;
                let rows = db(db(s.query_map(
                    params![generation.as_str(), after.unwrap_or(""), n],
                    unit_of,
                ))?
                .collect::<rusqlite::Result<Vec<_>>>())?;
                // The empty key sorts first and is never a unit (paths are non-empty).
                rows.into_iter().map(unit_from).collect()
            }
            fn unit(
                &self,
                generation: &Id,
                unit_key: &str,
            ) -> Result<Option<StoredUnit>, StorageError> {
                let raw = db(db(self.conn.prepare_cached(
                    "SELECT generation_id,unit_key,content_digest,action,payload_json FROM source_units WHERE generation_id=?1 AND unit_key=?2",
                ))?
                .query_row(params![generation.as_str(), unit_key], unit_of)
                .optional())?;
                raw.map(unit_from).transpose()
            }
            fn nodes(
                &self,
                generation: &Id,
                unit_key: &str,
            ) -> Result<Vec<StoredNode>, StorageError> {
                let mut s = db(self.conn.prepare_cached(
                    "SELECT unit_key,node_id,kind,name,payload_json FROM graph_nodes WHERE generation_id=?1 AND unit_key=?2 ORDER BY node_id",
                ))?;
                let rows = db(db(s.query_map(params![generation.as_str(), unit_key], node_of))?
                    .collect::<rusqlite::Result<Vec<_>>>())?;
                rows.into_iter().map(node_from).collect()
            }
            fn edges(
                &self,
                generation: &Id,
                unit_key: &str,
            ) -> Result<Vec<StoredEdge>, StorageError> {
                let mut s = db(self.conn.prepare_cached(
                    "SELECT unit_key,edge_id,source_node_id,target_node_id,kind,payload_json FROM graph_edges WHERE generation_id=?1 AND unit_key=?2 ORDER BY edge_id",
                ))?;
                let rows = db(db(s.query_map(params![generation.as_str(), unit_key], edge_of))?
                    .collect::<rusqlite::Result<Vec<_>>>())?;
                rows.into_iter().map(edge_from).collect()
            }
            fn nodes_named(
                &self,
                generation: &Id,
                name: &str,
                n: u16,
            ) -> Result<Vec<StoredNode>, StorageError> {
                let n = limit(n)?;
                let mut s = db(self.conn.prepare_cached(
                    "SELECT unit_key,node_id,kind,name,payload_json FROM graph_nodes WHERE generation_id=?1 AND name=?2 ORDER BY unit_key,node_id LIMIT ?3",
                ))?;
                let rows = db(db(s.query_map(params![generation.as_str(), name, n], node_of))?
                    .collect::<rusqlite::Result<Vec<_>>>())?;
                rows.into_iter().map(node_from).collect()
            }
            fn nodes_qualified(
                &self,
                generation: &Id,
                qualified_name: &str,
                n: u16,
            ) -> Result<Vec<StoredNode>, StorageError> {
                let n = limit(n)?;
                let mut s = db(self.conn.prepare_cached(
                    "SELECT unit_key,node_id,kind,name,payload_json FROM graph_nodes WHERE generation_id=?1 AND json_extract(payload_json, '$.qualifiedName')=?2 ORDER BY unit_key,node_id LIMIT ?3",
                ))?;
                let rows = db(db(s.query_map(params![generation.as_str(), qualified_name, n], node_of))?
                    .collect::<rusqlite::Result<Vec<_>>>())?;
                rows.into_iter().map(node_from).collect()
            }
            fn published(&self, repository: &Id) -> Result<Option<StoredPublished>, StorageError> {
                let raw: Option<(String, i64)> = db(db(self.conn.prepare_cached(
                    "SELECT view_id,revision FROM published_views WHERE source_key=?1",
                ))?
                .query_row([repository.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional())?;
                raw.map(|(v, revision)| {
                    Ok(StoredPublished {
                        view_id: id_of(v)?,
                        revision,
                    })
                })
                .transpose()
            }
            fn view(&self, view: &Id) -> Result<Option<StoredView>, StorageError> {
                let raw = db(db(self.conn.prepare_cached(&format!(
                    "SELECT {VIEW_COLUMNS} FROM index_views WHERE id=?1"
                )))?
                .query_row([view.as_str()], view_of)
                .optional())?;
                raw.map(view_from).transpose()
            }
            fn live_pins(&self, view: &Id, now_ms: i64) -> Result<i64, StorageError> {
                db(self.conn.query_row(
                    "SELECT count(*) FROM index_pins WHERE view_id=?1 AND lease_until>?2",
                    params![view.as_str(), now_ms],
                    |r| r.get(0),
                ))
            }
            fn overlay(&self, id: &Id) -> Result<Option<StoredOverlay>, StorageError> {
                let raw = db(db(self.conn.prepare_cached(&format!(
                    "SELECT {OVERLAY_COLUMNS} FROM index_overlays WHERE id=?1"
                )))?
                .query_row([id.as_str()], overlay_of)
                .optional())?;
                raw.map(overlay_from).transpose()
            }
            fn overlay_for(
                &self,
                repository: &Id,
                worktree: &Id,
            ) -> Result<Option<StoredOverlay>, StorageError> {
                let raw = db(db(self.conn.prepare_cached(&format!(
                    "SELECT {OVERLAY_COLUMNS} FROM index_overlays WHERE repository_id=?1 AND worktree_id=?2"
                )))?
                .query_row(params![repository.as_str(), worktree.as_str()], overlay_of)
                .optional())?;
                raw.map(overlay_from).transpose()
            }
            fn overlay_at(
                &self,
                repository: &Id,
                commit: &str,
            ) -> Result<Option<StoredOverlay>, StorageError> {
                let raw = db(db(self.conn.prepare_cached(&format!(
                    "SELECT {OVERLAY_COLUMNS} FROM index_overlays WHERE repository_id=?1 AND worktree_id IS NULL AND head_oid=?2"
                )))?
                .query_row(params![repository.as_str(), commit], overlay_of)
                .optional())?;
                raw.map(overlay_from).transpose()
            }
            fn overlays(&self, repository: &Id) -> Result<Vec<StoredOverlay>, StorageError> {
                let mut s = db(self.conn.prepare_cached(&format!(
                    "SELECT {OVERLAY_COLUMNS} FROM index_overlays WHERE repository_id=?1 ORDER BY state IN ('Expired','Unavailable'), worktree_id IS NULL, worktree_id, head_oid LIMIT 256"
                )))?;
                let rows = db(db(s.query_map([repository.as_str()], overlay_of))?
                    .collect::<rusqlite::Result<Vec<_>>>())?;
                rows.into_iter().map(overlay_from).collect()
            }
            fn overlays_due(
                &self,
                now_ms: i64,
                n: u16,
            ) -> Result<Vec<StoredOverlay>, StorageError> {
                let n = limit(n)?;
                let mut s = db(self.conn.prepare_cached(&format!(
                    "SELECT {OVERLAY_COLUMNS} FROM index_overlays WHERE state NOT IN ('Expired','Unavailable') AND expires_at<=?1 ORDER BY expires_at, id LIMIT ?2"
                )))?;
                let rows = db(db(s.query_map(params![now_ms, n], overlay_of))?
                    .collect::<rusqlite::Result<Vec<_>>>())?;
                rows.into_iter().map(overlay_from).collect()
            }
            fn prunable_ref_overlays(
                &self,
                ended_before_ms: i64,
                keep: u32,
                n: u16,
            ) -> Result<Vec<StoredOverlay>, StorageError> {
                let n = limit(n)?;
                let mut s = db(self.conn.prepare_cached(&format!(
                    "SELECT {OVERLAY_COLUMNS} FROM (SELECT *, ROW_NUMBER() OVER (PARTITION BY repository_id ORDER BY updated_at DESC, id DESC) AS rank FROM index_overlays WHERE worktree_id IS NULL AND state IN ('Expired','Unavailable','Failed')) AS ended WHERE rank>?2 AND updated_at<=?1 AND view_id IS NULL AND NOT EXISTS (SELECT 1 FROM index_runs r WHERE r.overlay_id=ended.id AND r.state IN ('Queued','Running')) ORDER BY updated_at, id LIMIT ?3"
                )))?;
                let rows = db(db(s.query_map(params![ended_before_ms, keep, n], overlay_of))?
                    .collect::<rusqlite::Result<Vec<_>>>())?;
                rows.into_iter().map(overlay_from).collect()
            }
        }
    };
}
index_reads!(Snapshot<'_>);
index_reads!(Unit<'_>);

// ---------------------------------------------------------------------------------------------
// Writes

impl IndexRepository for Unit<'_> {
    fn put_source_config(
        &mut self,
        c: &StoredSourceConfig,
        expected: i64,
    ) -> Result<(), StorageError> {
        if (c.scope_key == WORKSPACE_SCOPE_KEY) != c.project_id.is_none()
            || c.project_id
                .as_ref()
                .is_some_and(|p| p.0.as_str() != c.scope_key)
        {
            return Err(invalid("a section configuration names its own scope"));
        }
        if c.max_file_bytes <= 0
            || c.structural_languages.len() > 128
            || c.semantic_packages.len() > 128
            || c.exclude_patterns.len() > 256
        {
            return Err(invalid("section configuration out of bounds"));
        }
        let current = self.source_config(&c.scope_key)?.map_or(1, |s| s.revision);
        if current != expected {
            let target = c.project_id.as_ref().map_or_else(
                || Id::new(WORKSPACE_SCOPE_KEY).expect("valid"),
                |p| p.0.clone(),
            );
            return Err(StorageError::conflict(
                RevisionDimension::Configuration,
                "index_source_config",
                target,
                rev(expected)?,
                rev(current)?,
            ));
        }
        if c.revision != expected + 1 {
            return Err(invalid("a section configuration advances by one revision"));
        }
        let config = json!({
            "structuralLanguages": c.structural_languages,
            "semanticPackages": c.semantic_packages,
            "excludePatterns": c.exclude_patterns,
            "maxFileBytes": c.max_file_bytes,
        });
        db(db(self.conn.prepare_cached(
            "INSERT INTO index_source_configs(scope_key,project_id,revision,config_json,updated_at) VALUES(?1,?2,?3,?4,?5) \
             ON CONFLICT(scope_key) DO UPDATE SET revision=excluded.revision,config_json=excluded.config_json,updated_at=excluded.updated_at",
        ))?
        .execute(params![
            c.scope_key,
            c.project_id.as_ref().map(|p| p.0.as_str()),
            c.revision,
            text(&config),
            c.updated_at_ms
        ]))?;
        Ok(())
    }
    fn put_repository_source(&mut self, s: &StoredRepositorySource) -> Result<(), StorageError> {
        if ![SOURCE_PRESENT, SOURCE_MISSING, SOURCE_NOT_REPOSITORY].contains(&s.state.as_str())
            || !["Sha1", "Sha256"].contains(&s.object_format.as_str())
            || !oid_ok(&s.head_oid)
            || s.revision < 1
            || s.location.is_empty()
        {
            return Err(invalid("repository source identity"));
        }
        if s.remote_url
            .as_deref()
            .is_some_and(devworks_core::services::projects::carries_credentials)
        {
            return Err(invalid("a recorded remote never carries credentials"));
        }
        if !repository_exists(self.conn, &s.repository_id)? {
            return Err(StorageError::NotFound {
                kind: "repository",
                id: s.repository_id.as_str().to_owned(),
            });
        }
        db(db(self.conn.prepare_cached(&format!(
            "INSERT INTO repository_sources({SOURCE_COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) \
             ON CONFLICT(repository_id) DO UPDATE SET section=excluded.section,section_root=excluded.section_root,location=excluded.location,state=excluded.state,head_oid=excluded.head_oid,head_ref=excluded.head_ref,object_format=excluded.object_format,remote_url=excluded.remote_url,revision=excluded.revision,observed_at=excluded.observed_at"
        )))?
        .execute(params![
            s.repository_id.as_str(),
            s.section,
            s.section_root,
            s.location,
            s.state,
            s.head_oid,
            s.head_ref,
            s.object_format,
            s.remote_url,
            s.revision,
            s.observed_at_ms
        ]))?;
        Ok(())
    }
    fn put_worktrees(&mut self, repository: &Id, w: &[StoredWorktree]) -> Result<(), StorageError> {
        if w.len() > 256
            || w.iter()
                .any(|x| &x.repository_id != repository || !oid_ok(&x.head_oid))
        {
            return Err(invalid(
                "worktrees belong to their repository (at most 256)",
            ));
        }
        db(db(self
            .conn
            .prepare_cached("DELETE FROM repository_worktrees WHERE repository_id=?1"))?
        .execute([repository.as_str()]))?;
        for x in w {
            db(db(self.conn.prepare_cached(&format!(
                "INSERT INTO repository_worktrees({WORKTREE_COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7)"
            )))?
            .execute(params![
                x.repository_id.as_str(),
                x.worktree_id.as_str(),
                x.path,
                x.head_oid,
                x.head_ref,
                i64::from(x.main),
                x.observed_at_ms
            ]))?;
        }
        Ok(())
    }
    fn insert_run(&mut self, run: &StoredIndexRun) -> Result<(), StorageError> {
        check_run(run)?;
        if run.revision != 1 || run_terminal(&run.state) {
            return Err(invalid("a new index run is active at revision 1"));
        }
        if self.run(&run.id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "index_run",
                id: run.id.as_str().to_owned(),
            });
        }
        if IndexRead::active_run(self, &run.repository_id, run.overlay_id.as_ref())?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "active_index_run",
                id: run.repository_id.as_str().to_owned(),
            });
        }
        write_run(self.conn, run, true)
    }
    fn update_run(&mut self, run: &StoredIndexRun, expected: i64) -> Result<(), StorageError> {
        check_run(run)?;
        let Some(current) = self.run(&run.id)? else {
            return Err(StorageError::NotFound {
                kind: "index_run",
                id: run.id.as_str().to_owned(),
            });
        };
        if current.revision != expected {
            return Err(StorageError::conflict(
                RevisionDimension::Operation,
                "index_run",
                run.id.clone(),
                rev(expected)?,
                rev(current.revision)?,
            ));
        }
        if run_terminal(&current.state) {
            return Err(invalid("a finished index run never changes again"));
        }
        if run.revision != expected + 1
            || run.kind != current.kind
            || run.repository_id != current.repository_id
            || run.overlay_id != current.overlay_id
            || run.batch_id != current.batch_id
            || run.attempt < current.attempt
        {
            return Err(invalid(
                "an index run keeps its identity and advances by one revision",
            ));
        }
        write_run(self.conn, run, false)
    }
    fn insert_generation(&mut self, g: &StoredGeneration) -> Result<(), StorageError> {
        if !LAYERS.contains(&g.layer.as_str()) || g.state != GEN_STAGING || g.watermark != 0 {
            return Err(invalid("a new generation is an empty staging layer"));
        }
        if !repository_exists(self.conn, &g.repository_id)? {
            return Err(StorageError::NotFound {
                kind: "repository",
                id: g.repository_id.as_str().to_owned(),
            });
        }
        db(db(self.conn.prepare_cached(&format!(
            "INSERT INTO index_generations({GEN_COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7)"
        )))?
        .execute(params![
            g.id.as_str(),
            g.repository_id.as_str(),
            g.layer,
            g.state,
            g.source_digest,
            g.watermark,
            text(&g.manifest)
        ]))?;
        Ok(())
    }
    fn update_generation(&mut self, g: &StoredGeneration) -> Result<(), StorageError> {
        let Some(current) = self.generation(&g.id)? else {
            return Err(StorageError::NotFound {
                kind: "index_generation",
                id: g.id.as_str().to_owned(),
            });
        };
        if !GEN_STATES.contains(&g.state.as_str())
            || current.layer != g.layer
            || current.repository_id != g.repository_id
        {
            return Err(invalid("a generation keeps its layer and repository"));
        }
        let allowed = match (current.state.as_str(), g.state.as_str()) {
            (GEN_STAGING, GEN_STAGING | GEN_SEALED | GEN_RETIRED) => true,
            // Sealed content is immutable; it only retires.
            (GEN_SEALED, GEN_SEALED) => {
                current.source_digest == g.source_digest
                    && current.watermark == g.watermark
                    && current.manifest == g.manifest
            }
            (GEN_SEALED, GEN_RETIRED) => true,
            (GEN_RETIRED, GEN_RETIRED) => true,
            _ => false,
        };
        if !allowed {
            return Err(invalid(format!(
                "generation transition {} -> {}",
                current.state, g.state
            )));
        }
        if g.state == GEN_SEALED && current.state == GEN_STAGING {
            let units: i64 = db(self.conn.query_row(
                "SELECT count(*) FROM source_units WHERE generation_id=?1",
                [g.id.as_str()],
                |r| r.get(0),
            ))?;
            if units != g.watermark {
                return Err(invalid(format!(
                    "seal of {} expects {} units, {units} are staged",
                    g.id.as_str(),
                    g.watermark
                )));
            }
        }
        db(db(self.conn.prepare_cached(
            "UPDATE index_generations SET state=?1,source_digest=?2,watermark=?3,manifest_json=?4 WHERE id=?5",
        ))?
        .execute(params![
            g.state,
            g.source_digest,
            g.watermark,
            text(&g.manifest),
            g.id.as_str()
        ]))?;
        Ok(())
    }
    fn put_unit(
        &mut self,
        unit: &StoredUnit,
        nodes: &[StoredNode],
        edges: &[StoredEdge],
    ) -> Result<(), StorageError> {
        require_staging(self.conn, &unit.generation_id)?;
        if unit.unit_key.is_empty()
            || unit.unit_key.len() > 4096
            || ![UNIT_REPLACE, UNIT_DELETE].contains(&unit.action.as_str())
            || (unit.action == UNIT_DELETE && (!nodes.is_empty() || !edges.is_empty()))
            || nodes.len() > 20_000
            || edges.len() > 50_000
        {
            return Err(invalid("unit key, action or row bounds"));
        }
        if nodes.iter().any(|n| n.unit_key != unit.unit_key)
            || edges.iter().any(|e| e.unit_key != unit.unit_key)
        {
            return Err(invalid("a unit's rows belong to it"));
        }
        let g = unit.generation_id.as_str();
        let digest = batch_digest(unit, nodes, edges);
        let prior: Option<String> = db(db(self.conn.prepare_cached(
            "SELECT payload_digest FROM index_batches WHERE generation_id=?1 AND unit_key=?2 AND batch_key='rows'",
        ))?
        .query_row(params![g, unit.unit_key], |r| r.get(0))
        .optional())?;
        match prior {
            Some(p) if p == digest => return Ok(()),
            Some(_) => {
                return Err(StorageError::Duplicate {
                    kind: "index_batch",
                    id: format!("{g}:{}", unit.unit_key),
                });
            }
            None => {}
        }
        // A unit staged without its batch record (never: they commit together) is replaced.
        for sql in [
            "DELETE FROM graph_edges WHERE generation_id=?1 AND unit_key=?2",
            "DELETE FROM graph_nodes WHERE generation_id=?1 AND unit_key=?2",
            "DELETE FROM index_batches WHERE generation_id=?1 AND unit_key=?2",
            "DELETE FROM source_units WHERE generation_id=?1 AND unit_key=?2",
        ] {
            db(db(self.conn.prepare_cached(sql))?.execute(params![g, unit.unit_key]))?;
        }
        db(db(self.conn.prepare_cached(
            "INSERT INTO source_units(generation_id,unit_key,content_digest,action,payload_json) VALUES(?1,?2,?3,?4,?5)",
        ))?
        .execute(params![
            g,
            unit.unit_key,
            unit.content_digest,
            unit.action,
            text(&unit.payload)
        ]))?;
        for n in nodes {
            db(db(self.conn.prepare_cached(
                "INSERT INTO graph_nodes(generation_id,unit_key,node_id,kind,name,payload_json) VALUES(?1,?2,?3,?4,?5,?6)",
            ))?
            .execute(params![g, n.unit_key, n.node_id, n.kind, n.name, text(&n.payload)]))?;
        }
        for e in edges {
            db(db(self.conn.prepare_cached(
                "INSERT INTO graph_edges(generation_id,unit_key,edge_id,source_node_id,target_node_id,kind,payload_json) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            ))?
            .execute(params![
                g,
                e.unit_key,
                e.edge_id,
                e.source_node_id,
                e.target_node_id,
                e.kind,
                text(&e.payload)
            ]))?;
        }
        db(db(self.conn.prepare_cached(
            "INSERT INTO index_batches(generation_id,unit_key,batch_key,payload_digest,row_count) VALUES(?1,?2,'rows',?3,?4)",
        ))?
        .execute(params![g, unit.unit_key, digest, (nodes.len() + edges.len()) as i64]))?;
        Ok(())
    }
    fn inherit_units(&mut self, from: &Id, to: &Id, keys: &[String]) -> Result<u64, StorageError> {
        require_staging(self.conn, to)?;
        if generation_state(self.conn, from)?.as_deref() != Some(GEN_SEALED) {
            return Err(invalid("units are inherited from a sealed generation"));
        }
        if keys.len() > 2000 {
            return Err(invalid("at most 2000 units are inherited per batch"));
        }
        let (f, t) = (from.as_str(), to.as_str());
        let mut copied = 0;
        for k in keys {
            if self.unit(to, k)?.is_some() {
                continue;
            }
            let n = db(db(self.conn.prepare_cached(
                "INSERT INTO source_units(generation_id,unit_key,content_digest,action,payload_json) SELECT ?1,unit_key,content_digest,action,payload_json FROM source_units WHERE generation_id=?2 AND unit_key=?3",
            ))?
            .execute(params![t, f, k]))?;
            if n == 0 {
                continue;
            }
            copied += 1;
            db(db(self.conn.prepare_cached(
                "INSERT INTO graph_nodes(generation_id,unit_key,node_id,kind,name,payload_json) SELECT ?1,unit_key,node_id,kind,name,payload_json FROM graph_nodes WHERE generation_id=?2 AND unit_key=?3",
            ))?
            .execute(params![t, f, k]))?;
            db(db(self.conn.prepare_cached(
                "INSERT INTO graph_edges(generation_id,unit_key,edge_id,source_node_id,target_node_id,kind,payload_json) SELECT ?1,unit_key,edge_id,source_node_id,target_node_id,kind,payload_json FROM graph_edges WHERE generation_id=?2 AND unit_key=?3",
            ))?
            .execute(params![t, f, k]))?;
            db(db(self.conn.prepare_cached(
                "INSERT INTO index_batches(generation_id,unit_key,batch_key,payload_digest,row_count) SELECT ?1,unit_key,batch_key,payload_digest,row_count FROM index_batches WHERE generation_id=?2 AND unit_key=?3",
            ))?
            .execute(params![t, f, k]))?;
        }
        Ok(copied)
    }
    fn set_edge_targets(
        &mut self,
        generation: &Id,
        updates: &[EdgeTargetUpdate],
    ) -> Result<(), StorageError> {
        require_staging(self.conn, generation)?;
        if updates.len() > 5000 {
            return Err(invalid("at most 5000 edge targets per batch"));
        }
        for u in updates {
            let n = db(db(self.conn.prepare_cached(
                "UPDATE graph_edges SET source_node_id=?6,target_node_id=?1,payload_json=?2 WHERE generation_id=?3 AND unit_key=?4 AND edge_id=?5",
            ))?
            .execute(params![
                u.target_node_id,
                text(&u.payload),
                generation.as_str(),
                u.unit_key,
                u.edge_id,
                u.source_node_id
            ]))?;
            if n != 1 {
                return Err(StorageError::NotFound {
                    kind: "graph_edge",
                    id: u.edge_id.clone(),
                });
            }
        }
        Ok(())
    }
    fn reclaim_generation(&mut self, generation: &Id, n: u32) -> Result<u64, StorageError> {
        let g = generation.as_str();
        match generation_state(self.conn, generation)?.as_deref() {
            Some(GEN_RETIRED | GEN_STAGING) => {}
            Some(_) => {
                return Err(invalid(
                    "only a retired or abandoned generation is reclaimed",
                ));
            }
            None => return Ok(0),
        }
        let referenced: bool = db(db(self.conn.prepare_cached(
            "SELECT 1 FROM index_views WHERE baseline_id=?1 OR branch_id=?1 OR worktree_id=?1",
        ))?
        .exists([g]))?;
        if referenced {
            return Err(invalid("a generation a view names is not reclaimed"));
        }
        let n = i64::from(n.clamp(1, 50_000));
        let mut removed = 0u64;
        for table in [
            "graph_edges",
            "graph_nodes",
            "search_chunks",
            "index_batches",
            "source_units",
        ] {
            let left = n - removed as i64;
            if left <= 0 {
                break;
            }
            if table == "search_chunks" {
                // Vectors and lexical rows of a chunk go with it.
                db(self.conn.execute(
                    "DELETE FROM chunk_vectors WHERE chunk_id IN (SELECT id FROM search_chunks WHERE generation_id=?1 LIMIT ?2)",
                    params![g, left],
                ))?;
                db(self.conn.execute(
                    "DELETE FROM search_fts WHERE chunk_id IN (SELECT id FROM search_chunks WHERE generation_id=?1 LIMIT ?2)",
                    params![g, left],
                ))?;
            }
            let k = db(self.conn.execute(
                &format!(
                    "DELETE FROM {table} WHERE rowid IN (SELECT rowid FROM {table} WHERE generation_id=?1 LIMIT ?2)"
                ),
                params![g, left],
            ))?;
            removed += k as u64;
        }
        if (removed as i64) < n {
            db(db(self
                .conn
                .prepare_cached("DELETE FROM index_generations WHERE id=?1"))?
            .execute([g]))?;
        }
        Ok(removed)
    }
    fn insert_view(&mut self, v: &StoredView) -> Result<(), StorageError> {
        for layer in [
            Some(&v.baseline_id),
            v.branch_id.as_ref(),
            v.worktree_id.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            match self.generation(layer)? {
                Some(g) if g.state == GEN_SEALED && g.repository_id == v.repository_id => {}
                _ => return Err(invalid("a view names sealed layers of its own repository")),
            }
        }
        if v.view_digest.len() != 64 {
            return Err(invalid("a view has a digest"));
        }
        db(db(self.conn.prepare_cached(&format!(
            "INSERT INTO index_views({VIEW_COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7)"
        )))?
        .execute(params![
            v.view_id.as_str(),
            v.repository_id.as_str(),
            v.view_digest,
            v.baseline_id.as_str(),
            v.branch_id.as_ref().map(Id::as_str),
            v.worktree_id.as_ref().map(Id::as_str),
            text(&v.coverage)
        ]))?;
        Ok(())
    }
    fn publish(&mut self, repository: &Id, view: &Id, expected: i64) -> Result<i64, StorageError> {
        match self.view(view)? {
            Some(v)
                if &v.repository_id == repository
                    && v.branch_id.is_none()
                    && v.worktree_id.is_none() => {}
            _ => {
                return Err(invalid(
                    "a published view is a baseline view of its repository",
                ));
            }
        }
        let current = self.published(repository)?.map_or(0, |p| p.revision);
        if current != expected {
            return Err(StorageError::conflict(
                RevisionDimension::Source,
                "published_view",
                repository.clone(),
                rev(expected.max(1))?,
                rev(current.max(1))?,
            ));
        }
        let next = expected + 1;
        db(db(self.conn.prepare_cached(
            "INSERT INTO published_views(source_key,view_id,revision) VALUES(?1,?2,?3) \
             ON CONFLICT(source_key) DO UPDATE SET view_id=excluded.view_id,revision=excluded.revision",
        ))?
        .execute(params![repository.as_str(), view.as_str(), next]))?;
        Ok(next)
    }
    fn remove_view(&mut self, view: &Id) -> Result<(), StorageError> {
        let named: bool = db(db(self
            .conn
            .prepare_cached("SELECT 1 FROM published_views WHERE view_id=?1"))?
        .exists([view.as_str()]))?;
        if named {
            return Err(invalid("the published view is not removed"));
        }
        for sql in [
            "DELETE FROM repository_texts WHERE view_id=?1",
            "DELETE FROM repository_index_views WHERE view_id=?1",
            "DELETE FROM index_pins WHERE view_id=?1",
            "DELETE FROM index_views WHERE id=?1",
        ] {
            db(db(self.conn.prepare_cached(sql))?.execute([view.as_str()]))?;
        }
        Ok(())
    }
    fn remove_overlay(&mut self, overlay: &Id, expected: i64) -> Result<(), StorageError> {
        let Some(current) = self.overlay(overlay)? else {
            return Err(StorageError::NotFound {
                kind: "index_overlay",
                id: overlay.as_str().to_owned(),
            });
        };
        if current.revision != expected {
            return Err(StorageError::conflict(
                RevisionDimension::Source,
                "index_overlay",
                overlay.clone(),
                rev(expected)?,
                rev(current.revision)?,
            ));
        }
        if current.worktree_id.is_some()
            || !matches!(
                current.state.as_str(),
                OVERLAY_EXPIRED | OVERLAY_UNAVAILABLE | OVERLAY_FAILED
            )
        {
            return Err(invalid("only an ended ref overlay's record is removed"));
        }
        db(db(self
            .conn
            .prepare_cached("DELETE FROM index_overlays WHERE id=?1"))?
        .execute([overlay.as_str()]))?;
        Ok(())
    }
    fn put_overlay(
        &mut self,
        o: &StoredOverlay,
        expected: Option<i64>,
    ) -> Result<(), StorageError> {
        if !OVERLAY_STATES.contains(&o.state.as_str())
            || !oid_ok(&o.base_oid)
            || !oid_ok(&o.head_oid)
            || o.worktree_path.as_ref().is_some_and(String::is_empty)
            || o.worktree_id.is_some() != o.worktree_path.is_some()
        {
            return Err(invalid("overlay state, commits or path"));
        }
        if o.worktree_id.is_none()
            && (o.head_oid.is_none()
                || o.capture_digest.is_some()
                || o.worktree_generation_id.is_some())
        {
            return Err(invalid(
                "a ref overlay names its commit and has no worktree layer",
            ));
        }
        let current = self.overlay(&o.id)?;
        match (expected, &current) {
            (None, None) => {
                if o.revision != 1 {
                    return Err(invalid("a new overlay is revision 1"));
                }
            }
            (None, Some(_)) => {
                return Err(StorageError::Duplicate {
                    kind: "index_overlay",
                    id: o.id.as_str().to_owned(),
                });
            }
            (Some(_), None) => {
                return Err(StorageError::NotFound {
                    kind: "index_overlay",
                    id: o.id.as_str().to_owned(),
                });
            }
            (Some(e), Some(c)) => {
                if c.revision != e {
                    return Err(StorageError::conflict(
                        RevisionDimension::Source,
                        "index_overlay",
                        o.id.clone(),
                        rev(e)?,
                        rev(c.revision)?,
                    ));
                }
                if o.revision != e + 1
                    || o.repository_id != c.repository_id
                    || o.worktree_id != c.worktree_id
                    || (o.worktree_id.is_none() && o.head_oid != c.head_oid)
                {
                    return Err(invalid("an overlay keeps its identity and advances by one"));
                }
            }
        }
        db(db(self.conn.prepare_cached(&format!(
            "INSERT INTO index_overlays({OVERLAY_COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19) \
             ON CONFLICT(id) DO UPDATE SET worktree_path=excluded.worktree_path,state=excluded.state,revision=excluded.revision,base_generation_id=excluded.base_generation_id,base_oid=excluded.base_oid,head_oid=excluded.head_oid,head_ref=excluded.head_ref,capture_digest=excluded.capture_digest,branch_generation_id=excluded.branch_generation_id,worktree_generation_id=excluded.worktree_generation_id,view_id=excluded.view_id,run_id=excluded.run_id,reason=excluded.reason,updated_at=excluded.updated_at,expires_at=excluded.expires_at"
        )))?
        .execute(rusqlite::params_from_iter([
            rusqlite::types::Value::from(o.id.as_str().to_owned()),
            o.repository_id.as_str().to_owned().into(),
            o.worktree_id.as_ref().map(|x| x.as_str().to_owned()).into(),
            o.worktree_path.clone().into(),
            o.state.clone().into(),
            o.revision.into(),
            o.base_generation_id.as_ref().map(|x| x.as_str().to_owned()).into(),
            o.base_oid.clone().into(),
            o.head_oid.clone().into(),
            o.head_ref.clone().into(),
            o.capture_digest.clone().into(),
            o.branch_generation_id.as_ref().map(|x| x.as_str().to_owned()).into(),
            o.worktree_generation_id.as_ref().map(|x| x.as_str().to_owned()).into(),
            o.view_id.as_ref().map(|x| x.as_str().to_owned()).into(),
            o.run_id.as_ref().map(|x| x.as_str().to_owned()).into(),
            o.reason.clone().into(),
            o.created_at_ms.into(),
            o.updated_at_ms.into(),
            o.expires_at_ms.into(),
        ])))?;
        Ok(())
    }
}

fn write_run(conn: &Connection, run: &StoredIndexRun, insert: bool) -> Result<(), StorageError> {
    let values: [rusqlite::types::Value; 30] = [
        run.id.as_str().to_owned().into(),
        run.batch_id.as_str().to_owned().into(),
        run.kind.clone().into(),
        run.repository_id.as_str().to_owned().into(),
        run.project_id
            .as_ref()
            .map(|p| p.0.as_str().to_owned())
            .into(),
        run.overlay_id
            .as_ref()
            .map(|o| o.as_str().to_owned())
            .into(),
        run.state.clone().into(),
        run.stage.clone().into(),
        run.revision.into(),
        run.attempt.into(),
        run.max_attempts.into(),
        run.owner.clone().into(),
        run.lease_until_ms.into(),
        run.cancel_requested_at_ms.into(),
        run.config_revision.into(),
        run.config_digest.clone().into(),
        run.source_oid.clone().into(),
        run.generation_id
            .as_ref()
            .map(|g| g.as_str().to_owned())
            .into(),
        run.completed_units.into(),
        run.total_units.into(),
        run.output_view_id
            .as_ref()
            .map(|v| v.as_str().to_owned())
            .into(),
        run.error_code.clone().into(),
        run.error_message
            .as_ref()
            .map(|m| m.chars().take(4096).collect::<String>())
            .into(),
        i64::from(run.deterministic).into(),
        warnings_json(&run.warnings).into(),
        run.requested_by.0.as_str().to_owned().into(),
        run.created_at_ms.into(),
        run.started_at_ms.into(),
        run.finished_at_ms.into(),
        run.next_attempt_at_ms.into(),
    ];
    let sql = if insert {
        format!(
            "INSERT INTO index_runs({RUN_COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30)"
        )
    } else {
        "UPDATE index_runs SET batch_id=?2,kind=?3,repository_id=?4,project_id=?5,overlay_id=?6,state=?7,stage=?8,revision=?9,attempt=?10,max_attempts=?11,owner=?12,lease_until=?13,cancel_requested_at=?14,config_revision=?15,config_digest=?16,source_oid=?17,generation_id=?18,completed_units=?19,total_units=?20,output_view_id=?21,error_code=?22,error_message=?23,deterministic=?24,warnings_json=?25,requested_by=?26,created_at=?27,started_at=?28,finished_at=?29,next_attempt_at=?30 WHERE id=?1".to_owned()
    };
    let n = db(db(conn.prepare_cached(&sql))?.execute(rusqlite::params_from_iter(values)))?;
    if n != 1 {
        return Err(corrupt("index run changed inside its transaction"));
    }
    Ok(())
}

/// Removes every index view, published pointer, pin and graph generation of `repository` (the
/// `repositories.remove_index` extension of DW-023): the views and pointer go at once so the
/// removed index is never readable again; generations are retired and their payload reclaimed
/// in bounded batches by the index housekeeping. Runs and overlays keep their records.
pub(crate) fn remove_repository_index(
    conn: &Connection,
    repository: &Id,
) -> Result<(), StorageError> {
    let r = repository.as_str();
    for sql in [
        "DELETE FROM published_views WHERE source_key=?1",
        "DELETE FROM index_pins WHERE view_id IN (SELECT id FROM index_views WHERE source_key=?1)",
        "DELETE FROM index_views WHERE source_key=?1",
        "UPDATE index_generations SET state='Retired' WHERE source_key=?1",
    ] {
        db(db(conn.prepare_cached(sql))?.execute([r]))?;
    }
    Ok(())
}
