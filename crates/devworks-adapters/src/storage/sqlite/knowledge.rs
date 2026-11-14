//! SQLite unified Knowledge records (migration 18): entities, claims, tags, evidence, basis,
//! relations, history, writes, lesson events, challenges, addresses, deliveries, source heads,
//! the import map and the FTS5 lexical projections. Every listing filter is SQL, applied before
//! `LIMIT`, so no page is filtered after it was cut and no out-of-scope row is read.
use super::conn::db;
use super::repos::{Snapshot, Unit};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, Id, ProjectId, Revision, RevisionDimension};
use rusqlite::types::Value as Sql;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde_json::Value;

fn invalid(m: impl Into<String>) -> StorageError {
    StorageError::InvalidRecord(m.into())
}
fn corrupt(m: impl Into<String>) -> StorageError {
    StorageError::unavailable(Unavailable::Corrupt, m)
}
fn id_of(s: String) -> Result<Id, StorageError> {
    Id::new(s).map_err(|e| corrupt(format!("stored knowledge identifier: {e}")))
}
fn opt_id(s: Option<String>) -> Result<Option<Id>, StorageError> {
    s.map(id_of).transpose()
}
fn json_of(s: &str) -> Result<Value, StorageError> {
    serde_json::from_str(s).map_err(|_| corrupt("stored knowledge JSON"))
}
fn ids_json(ids: &[Id]) -> String {
    serde_json::to_string(&ids.iter().map(Id::as_str).collect::<Vec<_>>()).expect("ids")
}
fn ids_of(s: &str) -> Result<Vec<Id>, StorageError> {
    let v: Vec<String> = serde_json::from_str(s).map_err(|_| corrupt("stored id list"))?;
    v.into_iter().map(id_of).collect()
}
fn conflict(kind: &'static str, id: &Id, expected: i64, current: i64) -> StorageError {
    StorageError::conflict(
        RevisionDimension::Claim,
        kind,
        id.clone(),
        Revision::new(expected.max(1)).expect("positive"),
        Revision::new(current.max(1)).expect("positive"),
    )
}
fn check_limit(limit: u16) -> Result<i64, StorageError> {
    if limit == 0 || limit > KNOWLEDGE_PAGE_MAX {
        return Err(invalid(format!("limit must be 1..={KNOWLEDGE_PAGE_MAX}")));
    }
    Ok(i64::from(limit))
}
fn scope_of(key: &str, project: Option<String>) -> Result<KnowledgeScope, StorageError> {
    let scope = KnowledgeScope::parse(key).map_err(|_| corrupt("stored knowledge scope"))?;
    if scope.project().map(|p| p.0.as_str().to_owned()) != project {
        return Err(corrupt("stored knowledge scope and project disagree"));
    }
    Ok(scope)
}
fn project_col(scope: &KnowledgeScope) -> Option<&str> {
    scope.project().map(|p: &ProjectId| p.0.as_str())
}
/// One quoted FTS5 phrase per word, all required.
fn fts_query(words: &[String]) -> String {
    words
        .iter()
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}
/// One quoted FTS5 phrase per word, any one matching.
fn fts_any(words: &[String]) -> String {
    words
        .iter()
        .map(|w| fts_query(std::slice::from_ref(w)))
        .collect::<Vec<_>>()
        .join(" OR ")
}
fn rows<T>(
    conn: &Connection,
    sql: &str,
    p: Vec<Sql>,
    map: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>, StorageError> {
    let mut st = db(conn.prepare_cached(sql))?;
    db(db(st.query_map(params_from_iter(p), map))?.collect::<rusqlite::Result<Vec<_>>>())
}

// ---------------------------------------------------------------------------------------------
// Scopes

fn scope_revision(conn: &Connection, scope: &KnowledgeScope) -> Result<i64, StorageError> {
    Ok(db(conn
        .query_row(
            "SELECT revision FROM knowledge_scopes WHERE scope_key=?1",
            params![scope.key()],
            |r| r.get::<_, i64>(0),
        )
        .optional())?
    .unwrap_or(1))
}

// ---------------------------------------------------------------------------------------------
// Entities

const ENTITY_COLUMNS: &str = "id,scope_key,project_id,revision,label,entity_type,aliases_json,canonical_key,canonical_name,external_id,source,author_id,created_at,updated_at,deleted_at";

type RawEntity = (
    String,
    String,
    Option<String>,
    i64,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    i64,
    i64,
    Option<i64>,
);
fn raw_entity(r: &Row<'_>) -> rusqlite::Result<RawEntity> {
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
        r.get(12)?,
        r.get(13)?,
        r.get(14)?,
    ))
}
fn entity_of(r: RawEntity) -> Result<StoredEntity, StorageError> {
    Ok(StoredEntity {
        id: id_of(r.0)?,
        scope: scope_of(&r.1, r.2)?,
        revision: r.3,
        label: r.4,
        entity_type: r.5,
        aliases: serde_json::from_str(&r.6).map_err(|_| corrupt("stored aliases"))?,
        canonical_key: r.7,
        canonical_name: r.8,
        external_id: r.9,
        source: r.10,
        author: ActorId(id_of(r.11)?),
        created_at_ms: r.12,
        updated_at_ms: r.13,
        deleted_at_ms: r.14,
    })
}
fn entity(conn: &Connection, id: &Id) -> Result<Option<StoredEntity>, StorageError> {
    db(conn
        .query_row(
            &format!("SELECT {ENTITY_COLUMNS} FROM knowledge_entities WHERE id=?1"),
            params![id.as_str()],
            raw_entity,
        )
        .optional())?
    .map(entity_of)
    .transpose()
}
fn entities_where(
    conn: &Connection,
    filter: &str,
    p: Vec<Sql>,
) -> Result<Vec<StoredEntity>, StorageError> {
    rows(
        conn,
        &format!("SELECT {ENTITY_COLUMNS} FROM knowledge_entities WHERE {filter}"),
        p,
        raw_entity,
    )?
    .into_iter()
    .map(entity_of)
    .collect()
}

// ---------------------------------------------------------------------------------------------
// Claims

const CLAIM_COLUMNS: &str = "c.id,c.scope_key,c.project_id,c.revision,c.kind,c.entry_kind,c.lifecycle,c.subject_id,c.predicate,c.object_id,c.source,c.author_id,c.session_id,c.rule_state,c.lesson_source,c.applicability_json,c.effective_at,c.created_at,c.updated_at,c.deleted_at,c.body_json";

struct RawClaim {
    id: String,
    scope_key: String,
    project_id: Option<String>,
    revision: i64,
    kind: String,
    entry_kind: String,
    lifecycle: String,
    subject_id: String,
    predicate: String,
    object_id: Option<String>,
    source: Option<String>,
    author_id: String,
    session_id: Option<String>,
    rule_state: Option<String>,
    lesson_source: Option<String>,
    applicability: Option<String>,
    effective_at: Option<i64>,
    created_at: i64,
    updated_at: i64,
    deleted_at: Option<i64>,
    body: String,
}
fn raw_claim(r: &Row<'_>) -> rusqlite::Result<RawClaim> {
    Ok(RawClaim {
        id: r.get(0)?,
        scope_key: r.get(1)?,
        project_id: r.get(2)?,
        revision: r.get(3)?,
        kind: r.get(4)?,
        entry_kind: r.get(5)?,
        lifecycle: r.get(6)?,
        subject_id: r.get(7)?,
        predicate: r.get(8)?,
        object_id: r.get(9)?,
        source: r.get(10)?,
        author_id: r.get(11)?,
        session_id: r.get(12)?,
        rule_state: r.get(13)?,
        lesson_source: r.get(14)?,
        applicability: r.get(15)?,
        effective_at: r.get(16)?,
        created_at: r.get(17)?,
        updated_at: r.get(18)?,
        deleted_at: r.get(19)?,
        body: r.get(20)?,
    })
}
fn claim_of(conn: &Connection, r: RawClaim) -> Result<StoredClaim, StorageError> {
    let tags = rows(
        conn,
        "SELECT tag FROM knowledge_claim_tags WHERE claim_id=?1 ORDER BY tag",
        vec![Sql::Text(r.id.clone())],
        |row| row.get::<_, String>(0),
    )?;
    let basis = rows(
        conn,
        "SELECT source_ref,source_revision FROM knowledge_claim_basis WHERE claim_id=?1 \
         ORDER BY source_ref",
        vec![Sql::Text(r.id.clone())],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )?;
    let lesson = match (r.rule_state, r.lesson_source) {
        (Some(rule_state), Some(source)) => Some(StoredLesson {
            rule_state,
            source,
            applicability: match r.applicability.as_deref() {
                Some(a) => json_of(a)?,
                None => serde_json::json!({}),
            },
            effective_at_ms: r.effective_at,
        }),
        (None, None) => None,
        _ => return Err(corrupt("stored lesson fields incomplete")),
    };
    Ok(StoredClaim {
        id: id_of(r.id)?,
        scope: scope_of(&r.scope_key, r.project_id)?,
        revision: r.revision,
        kind: r.kind,
        entry_kind: r.entry_kind,
        lifecycle: r.lifecycle,
        subject_id: id_of(r.subject_id)?,
        predicate: r.predicate,
        object_id: opt_id(r.object_id)?,
        source: r.source,
        author: ActorId(id_of(r.author_id)?),
        session_id: opt_id(r.session_id)?,
        tags,
        basis,
        lesson,
        body: serde_json::from_str(&r.body).map_err(|_| corrupt("stored claim body"))?,
        created_at_ms: r.created_at,
        updated_at_ms: r.updated_at,
        deleted_at_ms: r.deleted_at,
    })
}
fn claims_where(
    conn: &Connection,
    tail: &str,
    p: Vec<Sql>,
) -> Result<Vec<StoredClaim>, StorageError> {
    let raw = rows(
        conn,
        &format!("SELECT {CLAIM_COLUMNS} FROM knowledge_claims c {tail}"),
        p,
        raw_claim,
    )?;
    raw.into_iter().map(|r| claim_of(conn, r)).collect()
}
fn claim(conn: &Connection, id: &Id) -> Result<Option<StoredClaim>, StorageError> {
    Ok(claims_where(conn, "WHERE c.id=?1", vec![Sql::Text(id.as_str().into())])?.pop())
}

/// A subquery listing `values` for `IN (...)`: one bound JSON array, so the cached statement text
/// is the same for any list length.
fn placeholders(p: &mut Vec<Sql>, values: impl IntoIterator<Item = String>) -> String {
    let list = serde_json::Value::from(values.into_iter().collect::<Vec<_>>());
    p.push(Sql::Text(list.to_string()));
    format!("SELECT value FROM json_each(?{})", p.len())
}

/// A condition true for a claim row `c` outside `deleted` (its values appended to `p`).
fn outside_of(deleted: &DeletedClaims<'_>, p: &mut Vec<Sql>) -> String {
    outside_as(deleted, p, "c")
}
/// A condition true for the claim row aliased `c` when it is outside `deleted`.
fn outside_as(deleted: &DeletedClaims<'_>, p: &mut Vec<Sql>, c: &str) -> String {
    match deleted {
        DeletedClaims::Ids([]) => "1".into(),
        DeletedClaims::Ids(ids) => {
            let list = placeholders(p, ids.iter().map(|i| i.as_str().to_owned()));
            format!("{c}.id NOT IN ({list})")
        }
        DeletedClaims::Source { scope, source } => {
            p.push(Sql::Text(scope.key()));
            p.push(Sql::Text((*source).to_owned()));
            // `IS`, not `=`: a claim without a source is outside the source, never unknown.
            format!(
                "NOT ({c}.scope_key IS ?{} AND {c}.source IS ?{})",
                p.len() - 1,
                p.len()
            )
        }
    }
}
/// A condition true for the entity row aliased `e` when the set removes it: a delete by
/// source removes its source's entities, a list the entities it names (a scope move's).
fn entity_inside(deleted: &DeletedClaims<'_>, p: &mut Vec<Sql>, e: &str) -> String {
    match deleted {
        DeletedClaims::Ids([]) => "0".into(),
        DeletedClaims::Ids(ids) => {
            let list = placeholders(p, ids.iter().map(|i| i.as_str().to_owned()));
            format!("{e}.id IN ({list})")
        }
        DeletedClaims::Source { scope, source } => {
            p.push(Sql::Text(scope.key()));
            p.push(Sql::Text((*source).to_owned()));
            format!(
                "({e}.scope_key IS ?{} AND {e}.source IS ?{})",
                p.len() - 1,
                p.len()
            )
        }
    }
}
/// A condition true when the node `id_sql` (a claim or entity ID expression) survives the
/// deletion: a live claim outside it, or a live entity it does not remove.
fn survives(deleted: &DeletedClaims<'_>, p: &mut Vec<Sql>, id_sql: &str) -> String {
    let claim = outside_as(deleted, p, "sx");
    let entity = entity_inside(deleted, p, "se");
    format!(
        "(EXISTS(SELECT 1 FROM knowledge_claims sx WHERE sx.id={id_sql} AND \
         sx.lifecycle<>'Deleted' AND {claim}) OR EXISTS(SELECT 1 FROM knowledge_entities se \
         WHERE se.id={id_sql} AND se.deleted_at IS NULL AND NOT {entity}))"
    )
}
/// [`survives`] as the deletion carries it out: an entity of the deleted source that something
/// surviving still uses stays (exactly `knowledge_entity_used_beyond`), and so do its live
/// relations, so what they cite is still cited.
fn kept(deleted: &DeletedClaims<'_>, p: &mut Vec<Sql>, id_sql: &str) -> String {
    let claim = outside_as(deleted, p, "kx");
    let inside = entity_inside(deleted, p, "ke");
    let named = outside_as(deleted, p, "ku");
    let other = survives(
        deleted,
        p,
        "CASE WHEN kr.subject_id=ke.id THEN kr.object_id ELSE kr.subject_id END",
    );
    format!(
        "(EXISTS(SELECT 1 FROM knowledge_claims kx WHERE kx.id={id_sql} AND \
         kx.lifecycle<>'Deleted' AND {claim}) OR EXISTS(SELECT 1 FROM knowledge_entities ke \
         WHERE ke.id={id_sql} AND ke.deleted_at IS NULL AND (NOT {inside} OR \
         EXISTS(SELECT 1 FROM knowledge_claims ku WHERE (ku.subject_id=ke.id OR \
         ku.object_id=ke.id) AND ku.lifecycle<>'Deleted' AND {named}) OR \
         EXISTS(SELECT 1 FROM knowledge_relations kr WHERE (kr.subject_id=ke.id OR \
         kr.object_id=ke.id) AND kr.removed_at IS NULL AND {other}))))"
    )
}

/// The rule state of a lesson row as read: an inferred lesson whose distinct events reach its
/// scope's threshold is `Effective` (the same rule as the service's `effective_rule_state`).
/// The thresholds bind as one JSON array of `[scope, threshold]` pairs; the first pair naming the
/// row's scope applies.
fn effective_state_sql(thresholds: &[(String, i64)], p: &mut Vec<Sql>) -> String {
    let pairs = thresholds
        .iter()
        .map(|(scope, t)| serde_json::json!([scope, t]))
        .collect::<Vec<_>>();
    p.push(Sql::Text(serde_json::Value::from(pairs).to_string()));
    format!(
        "(CASE WHEN c.rule_state IN ('Candidate','Proposed') AND \
         (SELECT count(*) FROM knowledge_lesson_events le WHERE le.claim_id=c.id) >= \
         coalesce((SELECT t.value->>1 FROM json_each(?{}) t WHERE t.value->>0=c.scope_key \
         ORDER BY t.key LIMIT 1), 9223372036854775807) \
         THEN 'Effective' ELSE c.rule_state END)",
        p.len()
    )
}

fn claims_page(
    conn: &Connection,
    q: &ClaimQuery,
    after: Option<(i64, &str)>,
    limit: u16,
) -> Result<Vec<StoredClaim>, StorageError> {
    let limit = check_limit(limit)?;
    if q.scopes.is_empty() {
        return Err(invalid("a claim query reads at least one scope"));
    }
    let mut p: Vec<Sql> = Vec::new();
    let mut w: Vec<String> = Vec::new();
    let scopes = placeholders(&mut p, q.scopes.iter().cloned());
    w.push(format!("c.scope_key IN ({scopes})"));
    for (col, values) in [
        ("c.lifecycle", &q.lifecycles),
        ("c.kind", &q.kinds),
        ("c.entry_kind", &q.entry_kinds),
    ] {
        if !values.is_empty() {
            let list = placeholders(&mut p, values.iter().cloned());
            w.push(format!("{col} IN ({list})"));
        }
    }
    if let Some(s) = &q.source {
        p.push(Sql::Text(s.clone()));
        w.push(format!("c.source=?{}", p.len()));
    }
    if let Some(s) = &q.subject_id {
        p.push(Sql::Text(s.as_str().into()));
        w.push(format!("c.subject_id=?{}", p.len()));
    }
    if !q.tags.is_empty() {
        let list = placeholders(&mut p, q.tags.iter().cloned());
        p.push(Sql::Integer(q.tags.len() as i64));
        w.push(format!(
            "(SELECT count(*) FROM knowledge_claim_tags t WHERE t.claim_id=c.id AND t.tag IN \
             ({list}))=?{}",
            p.len()
        ));
    }
    if !q.words.is_empty() {
        // The matching subjects: visible entities that match any word, limited after their
        // scope filter, never before it.
        p.push(Sql::Text(fts_any(&q.words)));
        let any_match = p.len();
        let entity_scopes = placeholders(&mut p, q.scopes.iter().cloned());
        p.push(Sql::Integer(i64::from(
            q.entity_limit.unwrap_or(KNOWLEDGE_PAGE_MAX),
        )));
        let entity_limit = p.len();
        let subjects = format!(
            "SELECT e.id FROM knowledge_entity_fts f JOIN knowledge_entities e ON \
             e.rowid=f.rowid WHERE knowledge_entity_fts MATCH ?{any_match} AND e.deleted_at IS \
             NULL AND e.scope_key IN ({entity_scopes}) ORDER BY e.id LIMIT ?{entity_limit}"
        );
        // Every word is required, in the claim's own text or its subject's: a query may name
        // the subject and say something about it ("router tiers" finds "tiers rotate" about
        // Router).
        for word in &q.words {
            p.push(Sql::Text(fts_query(std::slice::from_ref(word))));
            let m = p.len();
            w.push(format!(
                "(c.rowid IN (SELECT rowid FROM knowledge_claim_fts WHERE knowledge_claim_fts \
                 MATCH ?{m}) OR c.subject_id IN (SELECT f.entity_id FROM knowledge_entity_fts f \
                 WHERE knowledge_entity_fts MATCH ?{m} AND f.entity_id IN ({subjects})))"
            ));
        }
    }
    if let Some(stale) = q.stale {
        w.push(format!(
            "{} EXISTS(SELECT 1 FROM knowledge_claim_basis b JOIN knowledge_source_heads h ON \
             h.source_ref=b.source_ref WHERE b.claim_id=c.id AND h.source_revision<>b.source_revision)",
            if stale { "" } else { "NOT" }
        ));
    }
    let mut sort = "c.updated_at".to_owned();
    if let Some(l) = &q.lesson {
        w.push("c.kind='Lesson'".into());
        let state = effective_state_sql(&l.thresholds, &mut p);
        if !l.rule_states.is_empty() {
            let list = placeholders(&mut p, l.rule_states.iter().cloned());
            w.push(format!("{state} IN ({list})"));
        }
        if let Some(a) = &l.activity {
            p.push(Sql::Text(a.clone()));
            w.push(format!(
                "EXISTS(SELECT 1 FROM json_each(c.applicability_json,'$.activities') j WHERE \
                 j.value=?{})",
                p.len()
            ));
        }
        match l.view.as_deref() {
            None | Some("All") => {}
            Some("RecentlyLearned") => {
                w.push(format!("{state}='Effective'"));
                sort = "coalesce(c.effective_at,c.updated_at)".into();
            }
            Some("Recurring") => w.push(
                "(SELECT count(*) FROM knowledge_lesson_events le2 WHERE le2.claim_id=c.id)>1"
                    .into(),
            ),
            Some("Unaddressed") => w.push(format!(
                "{state}='Effective' AND NOT EXISTS(SELECT 1 FROM knowledge_addresses a WHERE \
                 a.claim_id=c.id)"
            )),
            Some("Undelivered") => w.push(format!(
                "{state}='Effective' AND NOT EXISTS(SELECT 1 FROM knowledge_deliveries d WHERE \
                 d.claim_id=c.id)"
            )),
            Some(other) => return Err(invalid(format!("unknown lesson view {other}"))),
        }
    }
    if let Some((t, id)) = after {
        p.push(Sql::Integer(t));
        let a = p.len();
        p.push(Sql::Text(id.to_owned()));
        w.push(format!("({sort},c.id) < (?{a},?{})", p.len()));
    }
    p.push(Sql::Integer(limit));
    let tail = format!(
        "WHERE {} ORDER BY {sort} DESC, c.id DESC LIMIT ?{}",
        w.join(" AND "),
        p.len()
    );
    claims_where(conn, &tail, p)
}

// ---------------------------------------------------------------------------------------------
// Evidence, relations, history, writes

const EVIDENCE_COLUMNS: &str = "e.id,e.scope_key,e.project_id,e.kind,e.source_ref,e.artifact_json,e.locator,e.snippet,e.metadata_json,e.source_revision,e.author_id,e.recorded_at,e.deleted_at";
type RawEvidence = (
    String,
    String,
    Option<String>,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    String,
    i64,
    Option<i64>,
);
fn raw_evidence(r: &Row<'_>) -> rusqlite::Result<RawEvidence> {
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
        r.get(12)?,
    ))
}
fn evidence_of(r: RawEvidence) -> Result<StoredEvidence, StorageError> {
    Ok(StoredEvidence {
        id: id_of(r.0)?,
        scope: scope_of(&r.1, r.2)?,
        kind: r.3,
        source_ref: r.4,
        artifact: r.5.as_deref().map(json_of).transpose()?,
        locator: r.6,
        snippet: r.7,
        metadata: r.8.as_deref().map(json_of).transpose()?,
        source_revision: r.9,
        author: ActorId(id_of(r.10)?),
        recorded_at_ms: r.11,
        deleted_at_ms: r.12,
    })
}
fn evidence_where(
    conn: &Connection,
    tail: &str,
    p: Vec<Sql>,
) -> Result<Vec<StoredEvidence>, StorageError> {
    rows(
        conn,
        &format!("SELECT {EVIDENCE_COLUMNS} FROM knowledge_evidence e {tail}"),
        p,
        raw_evidence,
    )?
    .into_iter()
    .map(evidence_of)
    .collect()
}

type RawRelation = (
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    String,
    String,
    i64,
    Option<i64>,
    String,
    String,
    String,
);
const RELATION_COLUMNS: &str = "id,scope_key,project_id,subject_id,predicate,object_id,\
     evidence_json,author_id,created_at,removed_at,write_id,added_evidence_json,added_basis_json";
fn relation_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawRelation> {
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
        r.get(12)?,
    ))
}
fn relation_of(r: RawRelation) -> Result<StoredRelation, StorageError> {
    Ok(StoredRelation {
        id: id_of(r.0)?,
        scope: scope_of(&r.1, r.2)?,
        subject_id: id_of(r.3)?,
        predicate: r.4,
        object_id: id_of(r.5)?,
        evidence_ids: ids_of(&r.6)?,
        author: ActorId(id_of(r.7)?),
        created_at_ms: r.8,
        removed_at_ms: r.9,
        write_id: id_of(r.10)?,
        added_evidence_ids: ids_of(&r.11)?,
        added_basis: serde_json::from_str(&r.12).map_err(|_| corrupt("relation basis"))?,
    })
}

type RawHistory = (
    String,
    i64,
    String,
    String,
    String,
    i64,
    String,
    String,
    String,
);
fn history_of(r: RawHistory) -> Result<StoredHistory, StorageError> {
    Ok(StoredHistory {
        target_id: id_of(r.0)?,
        revision: r.1,
        target_kind: r.2,
        change: r.3,
        author: ActorId(id_of(r.4)?),
        at_ms: r.5,
        write_id: id_of(r.6)?,
        lifecycle: r.7,
        snapshot: json_of(&r.8)?,
    })
}
const HISTORY_COLUMNS: &str =
    "target_id,revision,target_kind,change,author_id,at,write_id,lifecycle,snapshot_json";
fn history_row(r: &Row<'_>) -> rusqlite::Result<RawHistory> {
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

type RawWrite = (
    String,
    String,
    String,
    String,
    Option<String>,
    i64,
    i64,
    String,
    String,
    String,
    String,
);
const WRITE_COLUMNS: &str = "write_id,operation,scope_key,actor_id,session_id,at,scope_revision,entity_ids_json,claim_ids_json,changes_json,diagnostics_json";
fn write_row(r: &Row<'_>) -> rusqlite::Result<RawWrite> {
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
    ))
}
fn write_of(r: RawWrite) -> Result<StoredWrite, StorageError> {
    let scope = KnowledgeScope::parse(&r.2).map_err(|_| corrupt("stored write scope"))?;
    Ok(StoredWrite {
        write_id: id_of(r.0)?,
        operation: r.1,
        scope,
        actor: ActorId(id_of(r.3)?),
        session_id: opt_id(r.4)?,
        at_ms: r.5,
        scope_revision: r.6,
        entity_ids: ids_of(&r.7)?,
        claim_ids: ids_of(&r.8)?,
        changes: serde_json::from_str(&r.9).map_err(|_| corrupt("stored write changes"))?,
        diagnostics: serde_json::from_str(&r.10)
            .map_err(|_| corrupt("stored write diagnostics"))?,
    })
}

const DELETION_COLUMNS: &str = "write_id,scope_key,project_id,source,digest,state,actor_id,\
     principal_kind,session_id,invocation_project_id,restore_epoch,policy_revision,\
     planned_claims,planned_entities,planned_evidence,deleted_claims,deleted_entities,\
     purged_evidence,entity_cursor,batches,started_at,finished_at";
fn deletion_row(r: &Row<'_>) -> rusqlite::Result<Vec<Sql>> {
    (0..22).map(|i| r.get::<_, Sql>(i)).collect()
}
fn deletion_of(r: Vec<Sql>) -> Result<StoredSourceDeletion, StorageError> {
    let text = |i: usize| match &r[i] {
        Sql::Text(s) => Ok(s.clone()),
        _ => Err(corrupt("source deletion")),
    };
    let opt = |i: usize| match &r[i] {
        Sql::Null => Ok(None),
        Sql::Text(s) => Ok(Some(s.clone())),
        _ => Err(corrupt("source deletion")),
    };
    let int = |i: usize| match &r[i] {
        Sql::Integer(n) => Ok(*n),
        _ => Err(corrupt("source deletion")),
    };
    Ok(StoredSourceDeletion {
        write_id: id_of(text(0)?)?,
        scope: scope_of(&text(1)?, opt(2)?)?,
        source: text(3)?,
        digest: text(4)?,
        state: text(5)?,
        actor: ActorId(id_of(text(6)?)?),
        principal_kind: text(7)?,
        session_id: opt_id(opt(8)?)?,
        invocation_project: opt(9)?.map(|p| id_of(p).map(ProjectId)).transpose()?,
        restore_epoch: int(10)?,
        policy_revision: int(11)?,
        planned_claims: int(12)?,
        planned_entities: int(13)?,
        planned_evidence: int(14)?,
        deleted_claims: int(15)?,
        deleted_entities: int(16)?,
        purged_evidence: int(17)?,
        entity_cursor: opt(18)?,
        batches: int(19)?,
        started_at_ms: int(20)?,
        finished_at_ms: match &r[21] {
            Sql::Null => None,
            Sql::Integer(n) => Some(*n),
            _ => return Err(corrupt("source deletion")),
        },
    })
}

type RawEvent = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    i64,
    String,
    String,
);
const EVENT_COLUMNS: &str =
    "claim_id,event_key,source,source_ref,session_id,actor_id,at,write_id,merge_path";
fn event_row(r: &Row<'_>) -> rusqlite::Result<RawEvent> {
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
fn event_of(r: RawEvent) -> Result<StoredLessonEvent, StorageError> {
    Ok(StoredLessonEvent {
        claim_id: id_of(r.0)?,
        event_key: r.1,
        source: r.2,
        source_ref: r.3,
        session_id: opt_id(r.4)?,
        actor: ActorId(id_of(r.5)?),
        at_ms: r.6,
        write_id: id_of(r.7)?,
        merge_path: ids_of(&r.8)?,
    })
}

type RawChallenge = (
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
    Option<i64>,
    Option<String>,
    Option<String>,
);
const CHALLENGE_COLUMNS: &str =
    "id,claim_id,state,reason,evidence_json,author_id,opened_at,resolved_at,resolved_by,resolution";
fn challenge_row(r: &Row<'_>) -> rusqlite::Result<RawChallenge> {
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
    ))
}
fn challenge_of(r: RawChallenge) -> Result<StoredChallenge, StorageError> {
    Ok(StoredChallenge {
        id: id_of(r.0)?,
        claim_id: id_of(r.1)?,
        state: r.2,
        reason: r.3,
        evidence_ids: ids_of(&r.4)?,
        author: ActorId(id_of(r.5)?),
        opened_at_ms: r.6,
        resolved_at_ms: r.7,
        resolved_by: r.8.map(id_of).transpose()?.map(ActorId),
        resolution: r.9,
    })
}

const STALE_SQL: &str = "EXISTS(SELECT 1 FROM knowledge_claim_basis b JOIN knowledge_source_heads \
     h ON h.source_ref=b.source_ref WHERE b.claim_id=c.id AND h.source_revision<>b.source_revision)";

macro_rules! knowledge_reads {
    ($t:ty) => {
        impl KnowledgeRead for $t {
            fn knowledge_review_candidates(&self,scopes:&[String],kind:KnowledgeReviewKind,limit:u16)->Result<Vec<StoredClaim>,StorageError>{
                let limit=check_limit(limit)?;if scopes.is_empty(){return Ok(Vec::new());}for scope in scopes{KnowledgeScope::parse(scope)?;}
                let mut p=Vec::new();let scopes=placeholders(&mut p,scopes.iter().cloned());
                let condition=match kind{KnowledgeReviewKind::Challenge=>"EXISTS(SELECT 1 FROM knowledge_challenges h WHERE h.claim_id=c.id AND h.state='Open')",KnowledgeReviewKind::ProposedRule=>"c.kind='Lesson' AND c.rule_state='Proposed'"};
                p.push(Sql::Integer(limit));let tail=format!("WHERE c.scope_key IN ({scopes}) AND c.lifecycle='Active' AND {condition} ORDER BY c.id LIMIT ?{}",p.len());claims_where(self.conn,&tail,p)
            }
            fn knowledge_review_contradictions(&self,scopes:&[String],limit:u16)->Result<Vec<StoredRelation>,StorageError>{
                let limit=check_limit(limit)?;if scopes.is_empty(){return Ok(Vec::new());}for scope in scopes{KnowledgeScope::parse(scope)?;}
                let mut p=Vec::new();let scopes=placeholders(&mut p,scopes.iter().cloned());p.push(Sql::Integer(limit));
                rows(self.conn,&format!("SELECT {RELATION_COLUMNS} FROM (SELECT r.*,ROW_NUMBER() OVER (PARTITION BY r.scope_key,min(r.subject_id,r.object_id),max(r.subject_id,r.object_id) ORDER BY r.created_at,r.id) AS pair_rank FROM knowledge_relations r JOIN knowledge_claims a ON a.id=r.subject_id JOIN knowledge_claims b ON b.id=r.object_id WHERE r.scope_key IN ({scopes}) AND r.predicate='contradicts' AND r.removed_at IS NULL AND r.subject_id<>r.object_id AND a.lifecycle='Active' AND b.lifecycle='Active' AND a.scope_key=b.scope_key AND r.scope_key=a.scope_key) WHERE pair_rank=1 ORDER BY created_at,id LIMIT ?{}",p.len()),p,relation_row)?.into_iter().map(relation_of).collect()
            }
            fn knowledge_scope_revision(
                &self,
                scope: &KnowledgeScope,
            ) -> Result<i64, StorageError> {
                scope_revision(self.conn, scope)
            }
            fn knowledge_entity(&self, id: &Id) -> Result<Option<StoredEntity>, StorageError> {
                entity(self.conn, id)
            }
            fn knowledge_entity_by_key(
                &self,
                scope: &KnowledgeScope,
                key: &str,
            ) -> Result<Option<StoredEntity>, StorageError> {
                Ok(entities_where(
                    self.conn,
                    "scope_key=?1 AND canonical_key=?2 AND deleted_at IS NULL",
                    vec![Sql::Text(scope.key()), Sql::Text(key.into())],
                )?
                .pop())
            }
            fn knowledge_claim(&self, id: &Id) -> Result<Option<StoredClaim>, StorageError> {
                claim(self.conn, id)
            }
            fn knowledge_claims(
                &self,
                query: &ClaimQuery,
                after: Option<(i64, &str)>,
                limit: u16,
            ) -> Result<Vec<StoredClaim>, StorageError> {
                claims_page(self.conn, query, after, limit)
            }
            fn knowledge_claims_about(
                &self,
                scopes: &[String],
                subject: &Id,
                predicate: &str,
                limit: u16,
            ) -> Result<Vec<StoredClaim>, StorageError> {
                let limit = check_limit(limit)?;
                if scopes.is_empty() {
                    return Ok(Vec::new());
                }
                let mut p = vec![
                    Sql::Text(subject.as_str().into()),
                    Sql::Text(predicate.into()),
                ];
                let list = placeholders(&mut p, scopes.iter().cloned());
                p.push(Sql::Integer(limit));
                claims_where(
                    self.conn,
                    &format!(
                        "WHERE c.subject_id=?1 AND c.predicate=?2 AND c.lifecycle='Active' AND \
                         c.scope_key IN ({list}) ORDER BY c.updated_at DESC, c.id DESC LIMIT ?{}",
                        p.len()
                    ),
                    p,
                )
            }
            fn knowledge_entity_users(
                &self,
                entity: &Id,
                limit: u16,
            ) -> Result<Vec<StoredClaim>, StorageError> {
                let limit = check_limit(limit)?;
                claims_where(
                    self.conn,
                    "WHERE (c.subject_id=?1 OR c.object_id=?1) AND c.lifecycle<>'Deleted' \
                     ORDER BY c.id LIMIT ?2",
                    vec![Sql::Text(entity.as_str().into()), Sql::Integer(limit)],
                )
            }
            fn knowledge_entity_claims(
                &self,
                entity: &Id,
                scopes: &[String],
                limit: u16,
            ) -> Result<Vec<StoredClaim>, StorageError> {
                let limit = check_limit(limit)?;
                claims_where(
                    self.conn,
                    "WHERE (c.subject_id=?1 OR c.object_id=?1) AND c.lifecycle<>'Deleted' \
                     AND c.scope_key IN (SELECT value FROM json_each(?2)) \
                     ORDER BY c.updated_at DESC, c.id DESC LIMIT ?3",
                    vec![
                        Sql::Text(entity.as_str().into()),
                        Sql::Text(serde_json::to_string(scopes).expect("scope keys")),
                        Sql::Integer(limit),
                    ],
                )
            }
            fn knowledge_entity_edges(
                &self,
                entity: &Id,
                scopes: &[String],
                limit: u16,
            ) -> Result<Vec<StoredClaim>, StorageError> {
                let limit = check_limit(limit)?;
                claims_where(
                    self.conn,
                    // An edge's object is a live entity in a readable scope; a claim whose
                    // object is another claim is a fact, not an edge, and spends no edge budget.
                    "WHERE (c.subject_id=?1 OR c.object_id=?1) AND c.lifecycle='Active' \
                     AND EXISTS (SELECT 1 FROM knowledge_entities e WHERE e.id=c.object_id \
                       AND e.deleted_at IS NULL \
                       AND e.scope_key IN (SELECT value FROM json_each(?2))) \
                     AND c.scope_key IN (SELECT value FROM json_each(?2)) \
                     ORDER BY c.updated_at DESC, c.id DESC LIMIT ?3",
                    vec![
                        Sql::Text(entity.as_str().into()),
                        Sql::Text(serde_json::to_string(scopes).expect("scope keys")),
                        Sql::Integer(limit),
                    ],
                )
            }
            fn knowledge_entities(
                &self,
                scopes: &[String],
                after: Option<(i64, &str)>,
                limit: u16,
            ) -> Result<Vec<StoredEntity>, StorageError> {
                let limit = check_limit(limit)?;
                let (at, id) = after.map_or((i64::MAX, String::new()), |(t, i)| (t, i.to_owned()));
                entities_where(
                    self.conn,
                    "deleted_at IS NULL AND scope_key IN (SELECT value FROM json_each(?1)) \
                     AND (?2=9223372036854775807 OR updated_at<?2 OR (updated_at=?2 AND id<?3)) \
                     ORDER BY updated_at DESC, id DESC LIMIT ?4",
                    vec![
                        Sql::Text(serde_json::to_string(scopes).expect("scope keys")),
                        Sql::Integer(at),
                        Sql::Text(id),
                        Sql::Integer(limit),
                    ],
                )
            }
            fn knowledge_evidence(
                &self,
                id: &Id,
            ) -> Result<Option<StoredEvidence>, StorageError> {
                Ok(evidence_where(
                    self.conn,
                    "WHERE e.id=?1",
                    vec![Sql::Text(id.as_str().into())],
                )?
                .pop())
            }
            fn knowledge_evidence_users(
                &self,
                evidence: &Id,
                limit: u16,
            ) -> Result<Vec<Id>, StorageError> {
                let limit = check_limit(limit)?;
                rows(
                    self.conn,
                    "SELECT ce.claim_id FROM knowledge_claim_evidence ce JOIN knowledge_claims c \
                     ON c.id=ce.claim_id WHERE ce.evidence_id=?1 AND c.lifecycle<>'Deleted' \
                     ORDER BY ce.claim_id LIMIT ?2",
                    vec![Sql::Text(evidence.as_str().into()), Sql::Integer(limit)],
                    |r| r.get::<_, String>(0),
                )?
                .into_iter()
                .map(id_of)
                .collect()
            }
            fn knowledge_relations(
                &self,
                id: &Id,
                scopes: &[String],
                limit: u16,
            ) -> Result<Vec<StoredRelation>, StorageError> {
                let limit = check_limit(limit)?;
                if scopes.is_empty() {
                    return Ok(Vec::new());
                }
                // Scope applies before LIMIT: another project's relations never use the budget.
                let mut p = vec![Sql::Text(id.as_str().into())];
                let list = placeholders(&mut p, scopes.iter().cloned());
                p.push(Sql::Integer(limit));
                rows(
                    self.conn,
                    &format!(
                        "SELECT {RELATION_COLUMNS} FROM knowledge_relations WHERE \
                         (subject_id=?1 OR object_id=?1) AND removed_at IS NULL AND \
                         scope_key IN ({list}) ORDER BY created_at, id LIMIT ?{}",
                        p.len()
                    ),
                    p,
                    relation_row,
                )?
                .into_iter()
                .map(relation_of)
                .collect()
            }
            fn knowledge_history(
                &self,
                target: &Id,
                limit: u16,
            ) -> Result<Vec<StoredHistory>, StorageError> {
                let limit = check_limit(limit)?;
                rows(
                    self.conn,
                    &format!(
                        "SELECT {HISTORY_COLUMNS} FROM knowledge_history WHERE target_id=?1 \
                         ORDER BY revision DESC LIMIT ?2"
                    ),
                    vec![Sql::Text(target.as_str().into()), Sql::Integer(limit)],
                    history_row,
                )?
                .into_iter()
                .map(history_of)
                .collect()
            }
            fn knowledge_history_at(
                &self,
                target: &Id,
                revision: i64,
            ) -> Result<Option<StoredHistory>, StorageError> {
                db(self
                    .conn
                    .query_row(
                        &format!(
                            "SELECT {HISTORY_COLUMNS} FROM knowledge_history WHERE target_id=?1 \
                             AND revision=?2"
                        ),
                        params![target.as_str(), revision],
                        history_row,
                    )
                    .optional())?
                .map(history_of)
                .transpose()
            }
            fn knowledge_write(&self, id: &Id) -> Result<Option<StoredWrite>, StorageError> {
                let Some(mut w) = db(self
                    .conn
                    .query_row(
                        &format!("SELECT {WRITE_COLUMNS} FROM knowledge_writes WHERE write_id=?1"),
                        params![id.as_str()],
                        write_row,
                    )
                    .optional())?
                .map(write_of)
                .transpose()?
                else {
                    return Ok(None);
                };
                // A batched write's later IDs, after those of its first transaction.
                for (kind, out) in [("claim", &mut w.claim_ids), ("entity", &mut w.entity_ids)] {
                    let more = rows(
                        self.conn,
                        "SELECT target_id FROM knowledge_write_targets WHERE write_id=?1 AND \
                         kind=?2 ORDER BY seq",
                        vec![Sql::Text(id.as_str().into()), Sql::Text(kind.into())],
                        |r| r.get::<_, String>(0),
                    )?;
                    for t in more {
                        out.push(id_of(t)?);
                    }
                }
                Ok(Some(w))
            }
            fn knowledge_source_deletion(
                &self,
                write_id: &Id,
            ) -> Result<Option<StoredSourceDeletion>, StorageError> {
                db(self
                    .conn
                    .query_row(
                        &format!(
                            "SELECT {DELETION_COLUMNS} FROM knowledge_source_deletions \
                             WHERE write_id=?1"
                        ),
                        params![write_id.as_str()],
                        deletion_row,
                    )
                    .optional())?
                .map(deletion_of)
                .transpose()
            }
            fn knowledge_relations_touching(
                &self,
                endpoints: &[Id],
            ) -> Result<Vec<StoredRelation>, StorageError> {
                if endpoints.is_empty() {
                    return Ok(Vec::new());
                }
                let mut p = Vec::new();
                let list = placeholders(&mut p,
                    endpoints.iter().map(|i| i.as_str().to_owned()),
                );
                rows(
                    self.conn,
                    &format!(
                        "SELECT {RELATION_COLUMNS} FROM knowledge_relations WHERE \
                         removed_at IS NULL AND (subject_id IN ({list}) OR object_id IN ({list})) \
                         ORDER BY id"
                    ),
                    p,
                    relation_row,
                )?
                .into_iter()
                .map(relation_of)
                .collect()
            }
            fn knowledge_running_source_deletion(
                &self,
                scope: &KnowledgeScope,
                source: Option<&str>,
            ) -> Result<Option<Id>, StorageError> {
                db(self
                    .conn
                    .query_row(
                        "SELECT write_id FROM knowledge_source_deletions WHERE scope_key=?1 AND \
                         (?2 IS NULL OR source=?2) AND state='Running' ORDER BY write_id LIMIT 1",
                        params![scope.key(), source],
                        |r| r.get::<_, String>(0),
                    )
                    .optional())?
                .map(id_of)
                .transpose()
            }
            fn knowledge_recent_writes(
                &self,
                scope: &KnowledgeScope,
                limit: u16,
            ) -> Result<Vec<StoredWrite>, StorageError> {
                let limit = check_limit(limit)?;
                rows(
                    self.conn,
                    &format!(
                        "SELECT {WRITE_COLUMNS} FROM knowledge_writes WHERE scope_key=?1 \
                         ORDER BY at DESC, write_id DESC LIMIT ?2"
                    ),
                    vec![Sql::Text(scope.key()), Sql::Integer(limit)],
                    write_row,
                )?
                .into_iter()
                .map(write_of)
                .collect()
            }
            fn knowledge_lesson_event_count(&self, claim: &Id) -> Result<i64, StorageError> {
                Ok(self.knowledge_claim_tally(claim)?.lesson_events)
            }
            fn knowledge_lesson_events(
                &self,
                claim: &Id,
                limit: u16,
            ) -> Result<Vec<StoredLessonEvent>, StorageError> {
                let limit = check_limit(limit)?;
                rows(
                    self.conn,
                    &format!(
                        "SELECT {EVENT_COLUMNS} FROM knowledge_lesson_events WHERE claim_id=?1 \
                         ORDER BY at, event_key LIMIT ?2"
                    ),
                    vec![Sql::Text(claim.as_str().into()), Sql::Integer(limit)],
                    event_row,
                )?
                .into_iter()
                .map(event_of)
                .collect()
            }
            fn knowledge_merged_events(
                &self,
                source: &Id,
                limit: u16,
            ) -> Result<Vec<StoredLessonEvent>, StorageError> {
                let limit = check_limit(limit)?;
                rows(
                    self.conn,
                    &format!(
                        "SELECT {EVENT_COLUMNS} FROM knowledge_lesson_events WHERE \
                         json_extract(merge_path,'$[#-1]')=?1 AND merge_path<>'[]' \
                         ORDER BY at, event_key LIMIT ?2"
                    ),
                    vec![Sql::Text(source.as_str().into()), Sql::Integer(limit)],
                    event_row,
                )?
                .into_iter()
                .map(event_of)
                .collect()
            }
            fn knowledge_challenges(
                &self,
                claim: &Id,
                limit: u16,
            ) -> Result<Vec<StoredChallenge>, StorageError> {
                let limit = check_limit(limit)?;
                rows(
                    self.conn,
                    &format!(
                        "SELECT {CHALLENGE_COLUMNS} FROM knowledge_challenges WHERE claim_id=?1 \
                         ORDER BY opened_at, id LIMIT ?2"
                    ),
                    vec![Sql::Text(claim.as_str().into()), Sql::Integer(limit)],
                    challenge_row,
                )?
                .into_iter()
                .map(challenge_of)
                .collect()
            }
            fn knowledge_open_challenge_records(&self,claim:&Id,limit:u16)->Result<Vec<StoredChallenge>,StorageError>{
                let limit=check_limit(limit)?;
                rows(self.conn,&format!("SELECT {CHALLENGE_COLUMNS} FROM knowledge_challenges WHERE claim_id=?1 AND state='Open' ORDER BY opened_at,id LIMIT ?2"),vec![Sql::Text(claim.as_str().into()),Sql::Integer(limit)],challenge_row)?.into_iter().map(challenge_of).collect()
            }
            fn knowledge_challenge(
                &self,
                id: &Id,
            ) -> Result<Option<StoredChallenge>, StorageError> {
                db(self
                    .conn
                    .query_row(
                        &format!(
                            "SELECT {CHALLENGE_COLUMNS} FROM knowledge_challenges WHERE id=?1"
                        ),
                        params![id.as_str()],
                        challenge_row,
                    )
                    .optional())?
                .map(challenge_of)
                .transpose()
            }
            fn knowledge_open_challenges(&self, claim: &Id) -> Result<i64, StorageError> {
                Ok(self.knowledge_claim_tally(claim)?.open_challenges)
            }
            fn knowledge_addresses(&self, claim: &Id) -> Result<Vec<StoredAddress>, StorageError> {
                rows(
                    self.conn,
                    "SELECT claim_id,kind,name,reference,actor_id,at FROM knowledge_addresses \
                     WHERE claim_id=?1 ORDER BY kind, name",
                    vec![Sql::Text(claim.as_str().into())],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, String>(4)?,
                            r.get::<_, i64>(5)?,
                        ))
                    },
                )?
                .into_iter()
                .map(|(c, kind, name, reference, actor, at)| {
                    Ok(StoredAddress {
                        claim_id: id_of(c)?,
                        kind,
                        name,
                        reference,
                        actor: ActorId(id_of(actor)?),
                        at_ms: at,
                    })
                })
                .collect()
            }
            fn knowledge_deliveries(&self, claim: &Id) -> Result<DeliverySummary, StorageError> {
                Ok(self.knowledge_claim_tally(claim)?.deliveries)
            }
            fn knowledge_claim_tally(&self, claim: &Id) -> Result<ClaimTally, StorageError> {
                // Migration 54's triggers maintain the tallies; an absent claim has none.
                Ok(db(self
                    .conn
                    .query_row(
                        &format!(
                            "SELECT {STALE_SQL}, c.open_challenges, c.lesson_events, \
                             c.delivered_sessions, c.last_delivered_at FROM knowledge_claims c \
                             WHERE c.id=?1"
                        ),
                        params![claim.as_str()],
                        |r| {
                            Ok(ClaimTally {
                                stale: r.get(0)?,
                                open_challenges: r.get(1)?,
                                lesson_events: r.get(2)?,
                                deliveries: DeliverySummary {
                                    sessions: r.get(3)?,
                                    last_at_ms: r.get(4)?,
                                },
                            })
                        },
                    )
                    .optional())?
                .unwrap_or_default())
            }
            fn knowledge_claim_stale(&self, claim: &Id) -> Result<bool, StorageError> {
                db(self.conn.query_row(
                    &format!("SELECT {STALE_SQL} FROM (SELECT ?1 AS id) c"),
                    params![claim.as_str()],
                    |r| r.get(0),
                ))
            }
            fn knowledge_source_head(
                &self,
                source_ref: &str,
            ) -> Result<Option<String>, StorageError> {
                db(self
                    .conn
                    .query_row(
                        "SELECT source_revision FROM knowledge_source_heads WHERE source_ref=?1",
                        params![source_ref],
                        |r| r.get(0),
                    )
                    .optional())
            }
            fn knowledge_source_claims(
                &self,
                scope: &KnowledgeScope,
                source: &str,
                after: Option<&str>,
                limit: u16,
            ) -> Result<Vec<StoredClaim>, StorageError> {
                let limit = check_limit(limit)?;
                claims_where(
                    self.conn,
                    "WHERE c.scope_key=?1 AND c.source=?2 AND c.lifecycle<>'Deleted' AND c.id>?3 \
                     ORDER BY c.id LIMIT ?4",
                    vec![
                        Sql::Text(scope.key()),
                        Sql::Text(source.into()),
                        Sql::Text(after.unwrap_or("").into()),
                        Sql::Integer(limit),
                    ],
                )
            }
            fn knowledge_source_entities(
                &self,
                scope: &KnowledgeScope,
                source: &str,
                after: Option<&str>,
                limit: u16,
            ) -> Result<Vec<StoredEntity>, StorageError> {
                let limit = check_limit(limit)?;
                entities_where(
                    self.conn,
                    "scope_key=?1 AND source=?2 AND deleted_at IS NULL AND id>?3 ORDER BY id \
                     LIMIT ?4",
                    vec![
                        Sql::Text(scope.key()),
                        Sql::Text(source.into()),
                        Sql::Text(after.unwrap_or("").into()),
                        Sql::Integer(limit),
                    ],
                )
            }
            fn knowledge_entity_used_beyond(
                &self,
                entity: &Id,
                deleted: &DeletedClaims<'_>,
            ) -> Result<bool, StorageError> {
                // Every reference to an entity: a surviving claim naming it, or a live relation
                // between it and a surviving node (a relation to a removed node goes with it).
                let mut p = vec![Sql::Text(entity.as_str().into())];
                let outside = outside_of(deleted, &mut p);
                let other = survives(
                    deleted,
                    &mut p,
                    "CASE WHEN r.subject_id=?1 THEN r.object_id ELSE r.subject_id END",
                );
                db(self.conn.query_row(
                    &format!(
                        "SELECT EXISTS(SELECT 1 FROM knowledge_claims c WHERE \
                         (c.subject_id=?1 OR c.object_id=?1) AND c.lifecycle<>'Deleted' AND \
                         {outside}) OR EXISTS(SELECT 1 FROM knowledge_relations r WHERE \
                         (r.subject_id=?1 OR r.object_id=?1) AND r.removed_at IS NULL AND {other})"
                    ),
                    params_from_iter(p),
                    |r| r.get(0),
                ))
            }
            fn knowledge_claim_referrer_outside(
                &self,
                claim: &Id,
                except: &[Id],
                scope: &KnowledgeScope,
            ) -> Result<Option<Id>, StorageError> {
                let found: Option<String> = db(self
                    .conn
                    .query_row(
                        "SELECT c.id FROM knowledge_claims c WHERE c.object_id=?1 AND \
                         c.lifecycle<>'Deleted' AND c.scope_key<>?2 AND c.id NOT IN \
                         (SELECT value FROM json_each(?3)) ORDER BY c.id LIMIT 1",
                        params![claim.as_str(), scope.key(), ids_json(except)],
                        |r| r.get(0),
                    )
                    .optional())?;
                found.map(id_of).transpose()
            }
            fn knowledge_evidence_cited_beyond(
                &self,
                evidence: &Id,
                deleted: &DeletedClaims<'_>,
            ) -> Result<bool, StorageError> {
                // Every reference to evidence: a surviving claim's citation, a challenge of a
                // surviving claim, and a live relation's evidence or the citations its merge
                // added, while both its endpoints survive (addresses cite no evidence; a kept
                // entity of the deleted source survives with its relations).
                let mut p = vec![Sql::Text(evidence.as_str().into())];
                let cited = outside_of(deleted, &mut p);
                let challenged = outside_of(deleted, &mut p);
                let subject = kept(deleted, &mut p, "r.subject_id");
                let object = kept(deleted, &mut p, "r.object_id");
                db(self.conn.query_row(
                    &format!(
                        "SELECT EXISTS(SELECT 1 FROM knowledge_claim_evidence ce JOIN \
                         knowledge_claims c ON c.id=ce.claim_id WHERE ce.evidence_id=?1 AND \
                         c.lifecycle<>'Deleted' AND {cited}) \
                         OR EXISTS(SELECT 1 FROM knowledge_challenges ch JOIN knowledge_claims c \
                         ON c.id=ch.claim_id WHERE c.lifecycle<>'Deleted' AND {challenged} AND \
                         EXISTS(SELECT 1 FROM json_each(ch.evidence_json) j WHERE j.value=?1)) \
                         OR EXISTS(SELECT 1 FROM knowledge_relations r WHERE r.removed_at IS NULL \
                         AND (EXISTS(SELECT 1 FROM json_each(r.evidence_json) j WHERE j.value=?1) \
                         OR EXISTS(SELECT 1 FROM json_each(r.added_evidence_json) j \
                         WHERE j.value=?1)) AND {subject} AND {object})"
                    ),
                    params_from_iter(p),
                    |r| r.get(0),
                ))
            }
            fn knowledge_counts(&self, scopes: &[String]) -> Result<KnowledgeCounts, StorageError> {
                let mut out = KnowledgeCounts::default();
                if scopes.is_empty() {
                    return Ok(out);
                }
                let mut p = Vec::new();
                let list = placeholders(&mut p, scopes.iter().cloned());
                out.claims = rows(
                    self.conn,
                    &format!(
                        "SELECT lifecycle, count(*) FROM knowledge_claims WHERE scope_key IN \
                         ({list}) GROUP BY lifecycle ORDER BY lifecycle"
                    ),
                    p.clone(),
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
                )?;
                out.lessons = rows(
                    self.conn,
                    &format!(
                        "SELECT rule_state, count(*) FROM knowledge_claims WHERE scope_key IN \
                         ({list}) AND kind='Lesson' AND lifecycle='Active' GROUP BY rule_state \
                         ORDER BY rule_state"
                    ),
                    p.clone(),
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
                )?;
                let one = |sql: String| -> Result<i64, StorageError> {
                    db(self
                        .conn
                        .query_row(&sql, params_from_iter(p.clone()), |r| r.get(0)))
                };
                out.entities = one(format!(
                    "SELECT count(*) FROM knowledge_entities WHERE scope_key IN ({list}) AND \
                     deleted_at IS NULL"
                ))?;
                out.evidence = one(format!(
                    "SELECT count(*) FROM knowledge_evidence WHERE scope_key IN ({list}) AND \
                     deleted_at IS NULL"
                ))?;
                out.stale = one(format!(
                    "SELECT count(*) FROM knowledge_claims c WHERE c.scope_key IN ({list}) AND \
                     c.lifecycle='Active' AND {STALE_SQL}"
                ))?;
                out.open_challenges = one(format!(
                    "SELECT count(*) FROM knowledge_challenges ch JOIN knowledge_claims c ON \
                     c.id=ch.claim_id WHERE c.scope_key IN ({list}) AND ch.state='Open'"
                ))?;
                Ok(out)
            }
            fn knowledge_imported(
                &self,
                version: &Id,
                candidate: i64,
            ) -> Result<Option<Id>, StorageError> {
                db(self
                    .conn
                    .query_row(
                        "SELECT claim_id FROM knowledge_import_map WHERE artifact_version_id=?1 \
                         AND candidate=?2",
                        params![version.as_str(), candidate],
                        |r| r.get::<_, String>(0),
                    )
                    .optional())?
                .map(id_of)
                .transpose()
            }
        }
    };
}
knowledge_reads!(Snapshot<'_>);
knowledge_reads!(Unit<'_>);

// ---------------------------------------------------------------------------------------------
// Writes

/// The compare-and-set revision of an insert (`expected` `None`) or replacement.
fn next_revision(
    kind: &'static str,
    id: &Id,
    expected: Option<i64>,
    current: Option<i64>,
) -> Result<i64, StorageError> {
    match (expected, current) {
        (None, None) => Ok(1),
        (None, Some(_)) => Err(StorageError::Duplicate {
            kind,
            id: id.as_str().to_owned(),
        }),
        (Some(_), None) => Err(StorageError::NotFound {
            kind,
            id: id.as_str().to_owned(),
        }),
        (Some(e), Some(c)) if e != c => Err(conflict(kind, id, e, c)),
        (Some(e), Some(_)) => e
            .checked_add(1)
            .ok_or_else(|| invalid("knowledge revision overflow")),
    }
}

fn entity_text(e: &StoredEntity) -> String {
    let mut parts = vec![e.label.clone()];
    parts.extend(e.aliases.iter().cloned());
    parts.extend(e.canonical_name.iter().cloned());
    parts.join(" ")
}
fn claim_text(c: &StoredClaim) -> String {
    let mut parts = vec![c.predicate.clone()];
    parts.extend(c.body.value.iter().cloned());
    parts.extend(c.body.normalized_text.iter().cloned());
    parts.join(" ")
}

impl KnowledgeRepository for Unit<'_> {
    fn bump_scope(&mut self, scope: &KnowledgeScope) -> Result<i64, StorageError> {
        db(self.conn.execute(
            "INSERT INTO knowledge_scopes(scope_key,revision) VALUES(?1,2) \
             ON CONFLICT(scope_key) DO UPDATE SET revision=revision+1",
            params![scope.key()],
        ))?;
        scope_revision(self.conn, scope)
    }

    fn put_knowledge_entity(
        &mut self,
        e: &StoredEntity,
        expected: Option<i64>,
    ) -> Result<StoredEntity, StorageError> {
        let current = entity(self.conn, &e.id)?;
        let next = next_revision(
            "knowledge_entity",
            &e.id,
            expected,
            current.map(|c| c.revision),
        )?;
        let aliases = serde_json::to_string(&e.aliases).map_err(|x| invalid(x.to_string()))?;
        db(self.conn.execute(
            &format!(
                "INSERT INTO knowledge_entities({ENTITY_COLUMNS}) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15) \
                 ON CONFLICT(id) DO UPDATE SET scope_key=excluded.scope_key, \
                 project_id=excluded.project_id, revision=excluded.revision, label=excluded.label, \
                 entity_type=excluded.entity_type, aliases_json=excluded.aliases_json, \
                 canonical_key=excluded.canonical_key, canonical_name=excluded.canonical_name, \
                 external_id=excluded.external_id, source=excluded.source, \
                 updated_at=excluded.updated_at, deleted_at=excluded.deleted_at \
                 WHERE knowledge_entities.revision=?16"
            ),
            params![
                e.id.as_str(),
                e.scope.key(),
                project_col(&e.scope),
                next,
                e.label,
                e.entity_type,
                aliases,
                e.canonical_key,
                e.canonical_name,
                e.external_id,
                e.source,
                e.author.0.as_str(),
                e.created_at_ms,
                e.updated_at_ms,
                e.deleted_at_ms,
                expected.unwrap_or(0),
            ],
        ))?;
        let stored = entity(self.conn, &e.id)?
            .ok_or_else(|| corrupt("entity vanished inside its transaction"))?;
        if stored.revision != next {
            return Err(corrupt("entity changed inside its transaction"));
        }
        let rowid: i64 = db(self.conn.query_row(
            "SELECT rowid FROM knowledge_entities WHERE id=?1",
            params![e.id.as_str()],
            |r| r.get(0),
        ))?;
        db(self.conn.execute(
            "DELETE FROM knowledge_entity_fts WHERE rowid=?1",
            params![rowid],
        ))?;
        if stored.deleted_at_ms.is_none() {
            db(self.conn.execute(
                "INSERT INTO knowledge_entity_fts(rowid,entity_id,text) VALUES(?1,?2,?3)",
                params![rowid, e.id.as_str(), entity_text(&stored)],
            ))?;
        }
        Ok(stored)
    }

    fn put_knowledge_claim(
        &mut self,
        c: &StoredClaim,
        expected: Option<i64>,
    ) -> Result<StoredClaim, StorageError> {
        let current = db(self
            .conn
            .query_row(
                "SELECT revision FROM knowledge_claims WHERE id=?1",
                params![c.id.as_str()],
                |r| r.get::<_, i64>(0),
            )
            .optional())?;
        let next = next_revision("knowledge_claim", &c.id, expected, current)?;
        if !(0.0..=1.0).contains(&c.body.confidence) || !c.body.confidence.is_finite() {
            return Err(invalid("confidence outside 0..=1"));
        }
        let body = serde_json::to_string(&c.body).map_err(|x| invalid(x.to_string()))?;
        let applicability = c
            .lesson
            .as_ref()
            .map(|l| serde_json::to_string(&l.applicability))
            .transpose()
            .map_err(|x| invalid(x.to_string()))?;
        let n = db(self.conn.execute(
            "INSERT INTO knowledge_claims(id,scope_key,project_id,revision,kind,entry_kind,\
             lifecycle,subject_id,predicate,object_id,source,author_id,session_id,rule_state,\
             lesson_source,applicability_json,effective_at,created_at,updated_at,deleted_at,\
             body_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,\
             ?19,?20,?21) ON CONFLICT(id) DO UPDATE SET scope_key=excluded.scope_key, \
             project_id=excluded.project_id, revision=excluded.revision, kind=excluded.kind, \
             entry_kind=excluded.entry_kind, lifecycle=excluded.lifecycle, \
             subject_id=excluded.subject_id, predicate=excluded.predicate, \
             object_id=excluded.object_id, source=excluded.source, \
             rule_state=excluded.rule_state, lesson_source=excluded.lesson_source, \
             applicability_json=excluded.applicability_json, effective_at=excluded.effective_at, \
             updated_at=excluded.updated_at, deleted_at=excluded.deleted_at, \
             body_json=excluded.body_json WHERE knowledge_claims.revision=?22",
            params![
                c.id.as_str(),
                c.scope.key(),
                project_col(&c.scope),
                next,
                c.kind,
                c.entry_kind,
                c.lifecycle,
                c.subject_id.as_str(),
                c.predicate,
                c.object_id.as_ref().map(Id::as_str),
                c.source,
                c.author.0.as_str(),
                c.session_id.as_ref().map(Id::as_str),
                c.lesson.as_ref().map(|l| l.rule_state.as_str()),
                c.lesson.as_ref().map(|l| l.source.as_str()),
                applicability,
                c.lesson.as_ref().and_then(|l| l.effective_at_ms),
                c.created_at_ms,
                c.updated_at_ms,
                c.deleted_at_ms,
                body,
                expected.unwrap_or(0),
            ],
        ))?;
        if n != 1 {
            return Err(corrupt("claim changed inside its transaction"));
        }
        let id = c.id.as_str();
        db(self.conn.execute(
            "DELETE FROM knowledge_claim_tags WHERE claim_id=?1",
            params![id],
        ))?;
        for t in &c.tags {
            if t.is_empty() || t.chars().count() > 256 {
                return Err(invalid("tag must be 1 to 256 characters"));
            }
            db(self.conn.execute(
                "INSERT OR IGNORE INTO knowledge_claim_tags(claim_id,tag) VALUES(?1,?2)",
                params![id, t],
            ))?;
        }
        db(self.conn.execute(
            "DELETE FROM knowledge_claim_basis WHERE claim_id=?1",
            params![id],
        ))?;
        for (source, revision) in &c.basis {
            db(self.conn.execute(
                "INSERT OR REPLACE INTO knowledge_claim_basis(claim_id,source_ref,source_revision) \
                 VALUES(?1,?2,?3)",
                params![id, source, revision],
            ))?;
        }
        // Evidence links follow the claim's cited evidence (kept after deletion for provenance).
        let cited = ids_json(&c.body.evidence_ids);
        db(self.conn.execute(
            "DELETE FROM knowledge_claim_evidence WHERE claim_id=?1 AND evidence_id NOT IN \
             (SELECT value FROM json_each(?2))",
            params![id, cited],
        ))?;
        for e in &c.body.evidence_ids {
            db(self.conn.execute(
                "INSERT OR IGNORE INTO knowledge_claim_evidence(claim_id,evidence_id) VALUES(?1,?2)",
                params![id, e.as_str()],
            ))?;
        }
        let rowid: i64 = db(self.conn.query_row(
            "SELECT rowid FROM knowledge_claims WHERE id=?1",
            params![id],
            |r| r.get(0),
        ))?;
        db(self.conn.execute(
            "DELETE FROM knowledge_claim_fts WHERE rowid=?1",
            params![rowid],
        ))?;
        if c.lifecycle != "Deleted" {
            db(self.conn.execute(
                "INSERT INTO knowledge_claim_fts(rowid,claim_id,text) VALUES(?1,?2,?3)",
                params![rowid, id, claim_text(c)],
            ))?;
        }
        claim(self.conn, &c.id)?.ok_or_else(|| corrupt("claim vanished inside its transaction"))
    }

    fn put_knowledge_evidence(
        &mut self,
        e: &StoredEvidence,
        replace: bool,
    ) -> Result<(), StorageError> {
        let exists: bool = db(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM knowledge_evidence WHERE id=?1)",
            params![e.id.as_str()],
            |r| r.get(0),
        ))?;
        match (replace, exists) {
            (false, true) => {
                return Err(StorageError::Duplicate {
                    kind: "knowledge_evidence",
                    id: e.id.as_str().to_owned(),
                });
            }
            (true, false) => {
                return Err(StorageError::NotFound {
                    kind: "knowledge_evidence",
                    id: e.id.as_str().to_owned(),
                });
            }
            _ => {}
        }
        let artifact = e
            .artifact
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|x| invalid(x.to_string()))?;
        let metadata = e
            .metadata
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|x| invalid(x.to_string()))?;
        db(self.conn.execute(
            "INSERT INTO knowledge_evidence(id,scope_key,project_id,kind,source_ref,artifact_json,\
             locator,snippet,metadata_json,source_revision,author_id,recorded_at,deleted_at) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13) ON CONFLICT(id) DO UPDATE SET \
             scope_key=excluded.scope_key, project_id=excluded.project_id, \
             source_ref=excluded.source_ref, artifact_json=excluded.artifact_json, \
             locator=excluded.locator, \
             snippet=excluded.snippet, metadata_json=excluded.metadata_json, \
             deleted_at=excluded.deleted_at",
            params![
                e.id.as_str(),
                e.scope.key(),
                project_col(&e.scope),
                e.kind,
                e.source_ref,
                artifact,
                e.locator,
                e.snippet,
                metadata,
                e.source_revision,
                e.author.0.as_str(),
                e.recorded_at_ms,
                e.deleted_at_ms,
            ],
        ))?;
        Ok(())
    }

    fn put_knowledge_relation(&mut self, r: &StoredRelation) -> Result<(), StorageError> {
        let live: bool = db(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM knowledge_relations WHERE scope_key=?4 AND subject_id=?1 \
             AND predicate=?2 AND object_id=?3 AND removed_at IS NULL)",
            params![
                r.subject_id.as_str(),
                r.predicate,
                r.object_id.as_str(),
                r.scope.key()
            ],
            |x| x.get(0),
        ))?;
        if live {
            return Err(StorageError::Duplicate {
                kind: "knowledge_relation",
                id: r.id.as_str().to_owned(),
            });
        }
        db(self.conn.execute(
            &format!(
                "INSERT INTO knowledge_relations({RELATION_COLUMNS}) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)"
            ),
            params![
                r.id.as_str(),
                r.scope.key(),
                project_col(&r.scope),
                r.subject_id.as_str(),
                r.predicate,
                r.object_id.as_str(),
                ids_json(&r.evidence_ids),
                r.author.0.as_str(),
                r.created_at_ms,
                r.removed_at_ms,
                r.write_id.as_str(),
                ids_json(&r.added_evidence_ids),
                serde_json::to_string(&r.added_basis).map_err(|x| invalid(x.to_string()))?,
            ],
        ))?;
        Ok(())
    }

    fn remove_knowledge_relation(&mut self, id: &Id, at_ms: i64) -> Result<bool, StorageError> {
        Ok(db(self.conn.execute(
            "UPDATE knowledge_relations SET removed_at=?2 WHERE id=?1 AND removed_at IS NULL",
            params![id.as_str(), at_ms],
        ))? == 1)
    }

    fn set_knowledge_relation_ownership(
        &mut self,
        id: &Id,
        added_evidence_ids: &[Id],
        added_basis: &[(String, String)],
    ) -> Result<bool, StorageError> {
        let basis = serde_json::to_string(added_basis).map_err(|x| invalid(x.to_string()))?;
        Ok(db(self.conn.execute(
            "UPDATE knowledge_relations SET added_evidence_json=?2, added_basis_json=?3 \
             WHERE id=?1 AND removed_at IS NULL",
            params![id.as_str(), ids_json(added_evidence_ids), basis],
        ))? == 1)
    }

    fn put_knowledge_history(&mut self, h: &StoredHistory) -> Result<(), StorageError> {
        let snapshot = serde_json::to_string(&h.snapshot).map_err(|x| invalid(x.to_string()))?;
        let n = db(self.conn.execute(
            &format!(
                "INSERT OR IGNORE INTO knowledge_history({HISTORY_COLUMNS}) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)"
            ),
            params![
                h.target_id.as_str(),
                h.revision,
                h.target_kind,
                h.change,
                h.author.0.as_str(),
                h.at_ms,
                h.write_id.as_str(),
                h.lifecycle,
                snapshot,
            ],
        ))?;
        if n != 1 {
            return Err(StorageError::Duplicate {
                kind: "knowledge_history",
                id: h.target_id.as_str().to_owned(),
            });
        }
        Ok(())
    }

    fn purge_knowledge_history(
        &mut self,
        target: &Id,
        retained: &dyn Fn(&Value) -> Value,
    ) -> Result<(), StorageError> {
        let all = rows(
            self.conn,
            "SELECT revision, snapshot_json FROM knowledge_history WHERE target_id=?1",
            vec![Sql::Text(target.as_str().into())],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )?;
        for (revision, snapshot) in all {
            let kept = retained(&json_of(&snapshot)?);
            db(self.conn.execute(
                "UPDATE knowledge_history SET snapshot_json=?3 WHERE target_id=?1 AND revision=?2",
                params![
                    target.as_str(),
                    revision,
                    serde_json::to_string(&kept).map_err(|x| invalid(x.to_string()))?
                ],
            ))?;
        }
        Ok(())
    }

    fn extend_knowledge_write(
        &mut self,
        write_id: &Id,
        claim_ids: &[Id],
        entity_ids: &[Id],
        scope_revision: i64,
        diagnostics: &[Value],
    ) -> Result<(), StorageError> {
        let diagnostics = serde_json::to_string(diagnostics).map_err(|x| invalid(x.to_string()))?;
        let updated = db(self.conn.execute(
            "UPDATE knowledge_writes SET scope_revision=?2, diagnostics_json=?3 WHERE write_id=?1",
            params![write_id.as_str(), scope_revision, diagnostics],
        ))?;
        if updated != 1 {
            return Err(StorageError::NotFound {
                kind: "knowledge_write",
                id: write_id.as_str().to_owned(),
            });
        }
        for (kind, ids) in [("claim", claim_ids), ("entity", entity_ids)] {
            // After the IDs it already names: its row's, then earlier appended batches'.
            let column = if kind == "claim" {
                "claim_ids_json"
            } else {
                "entity_ids_json"
            };
            let first: i64 = db(self.conn.query_row(
                &format!(
                    "SELECT json_array_length({column}) + (SELECT count(*) FROM \
                     knowledge_write_targets WHERE write_id=?1 AND kind=?2) FROM knowledge_writes \
                     WHERE write_id=?1"
                ),
                params![write_id.as_str(), kind],
                |r| r.get(0),
            ))?;
            for (seq, id) in (first..).zip(ids) {
                db(self.conn.execute(
                    "INSERT INTO knowledge_write_targets(write_id,kind,seq,target_id) \
                     VALUES(?1,?2,?3,?4)",
                    params![write_id.as_str(), kind, seq, id.as_str()],
                ))?;
            }
        }
        Ok(())
    }

    fn put_knowledge_source_deletion(
        &mut self,
        j: &StoredSourceDeletion,
    ) -> Result<(), StorageError> {
        db(self.conn.execute(
            &format!(
                "INSERT OR REPLACE INTO knowledge_source_deletions({DELETION_COLUMNS}) VALUES(\
                 ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)"
            ),
            params![
                j.write_id.as_str(),
                j.scope.key(),
                project_col(&j.scope),
                j.source,
                j.digest,
                j.state,
                j.actor.0.as_str(),
                j.principal_kind,
                j.session_id.as_ref().map(Id::as_str),
                j.invocation_project.as_ref().map(|p| p.0.as_str()),
                j.restore_epoch,
                j.policy_revision,
                j.planned_claims,
                j.planned_entities,
                j.planned_evidence,
                j.deleted_claims,
                j.deleted_entities,
                j.purged_evidence,
                j.entity_cursor,
                j.batches,
                j.started_at_ms,
                j.finished_at_ms,
            ],
        ))?;
        Ok(())
    }

    fn rescope_knowledge_relation(
        &mut self,
        id: &Id,
        to: &KnowledgeScope,
    ) -> Result<(), StorageError> {
        let taken: bool = db(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM knowledge_relations o JOIN knowledge_relations r ON \
             r.id=?1 WHERE o.scope_key=?2 AND o.subject_id=r.subject_id AND \
             o.predicate=r.predicate AND o.object_id=r.object_id AND o.removed_at IS NULL AND \
             o.id<>r.id)",
            params![id.as_str(), to.key()],
            |r| r.get(0),
        ))?;
        if taken {
            return Err(StorageError::Duplicate {
                kind: "knowledge_relation",
                id: id.as_str().to_owned(),
            });
        }
        db(self.conn.execute(
            "UPDATE knowledge_relations SET scope_key=?2, project_id=?3 WHERE id=?1 AND \
             removed_at IS NULL",
            params![id.as_str(), to.key(), project_col(to)],
        ))?;
        Ok(())
    }

    fn put_knowledge_write(&mut self, w: &StoredWrite) -> Result<(), StorageError> {
        let changes = serde_json::to_string(&w.changes).map_err(|x| invalid(x.to_string()))?;
        let diagnostics =
            serde_json::to_string(&w.diagnostics).map_err(|x| invalid(x.to_string()))?;
        db(self.conn.execute(
            &format!(
                "INSERT INTO knowledge_writes({WRITE_COLUMNS}) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)"
            ),
            params![
                w.write_id.as_str(),
                w.operation,
                w.scope.key(),
                w.actor.0.as_str(),
                w.session_id.as_ref().map(Id::as_str),
                w.at_ms,
                w.scope_revision,
                ids_json(&w.entity_ids),
                ids_json(&w.claim_ids),
                changes,
                diagnostics,
            ],
        ))?;
        Ok(())
    }

    fn add_knowledge_lesson_event(&mut self, e: &StoredLessonEvent) -> Result<bool, StorageError> {
        Ok(db(self.conn.execute(
            &format!(
                "INSERT OR IGNORE INTO knowledge_lesson_events({EVENT_COLUMNS}) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)"
            ),
            params![
                e.claim_id.as_str(),
                e.event_key,
                e.source,
                e.source_ref,
                e.session_id.as_ref().map(Id::as_str),
                e.actor.0.as_str(),
                e.at_ms,
                e.write_id.as_str(),
                ids_json(&e.merge_path),
            ],
        ))? == 1)
    }

    fn merge_knowledge_lesson_events(&mut self, from: &Id, to: &Id) -> Result<i64, StorageError> {
        // An event the target already has stays with the source (one event counts once).
        // Each moved event pushes `from` onto its merge path, keeping earlier merges below it.
        Ok(db(self.conn.execute(
            "UPDATE knowledge_lesson_events SET claim_id=?2, \
             merge_path=json_insert(merge_path,'$[#]',?1) WHERE claim_id=?1 AND \
             event_key NOT IN (SELECT event_key FROM knowledge_lesson_events WHERE claim_id=?2)",
            params![from.as_str(), to.as_str()],
        ))? as i64)
    }

    fn unmerge_knowledge_lesson_events(&mut self, from: &Id) -> Result<i64, StorageError> {
        // Only events whose latest merge left `from` return, one level popped.
        Ok(db(self.conn.execute(
            "UPDATE knowledge_lesson_events SET claim_id=?1, \
             merge_path=json_remove(merge_path,'$[#-1]') WHERE merge_path<>'[]' AND \
             json_extract(merge_path,'$[#-1]')=?1 AND event_key NOT IN \
             (SELECT event_key FROM knowledge_lesson_events WHERE claim_id=?1)",
            params![from.as_str()],
        ))? as i64)
    }

    fn settle_knowledge_lesson_events(
        &mut self,
        deleted: &Id,
        into: &Id,
        live: &[Id],
    ) -> Result<i64, StorageError> {
        let held = rows(
            self.conn,
            "SELECT event_key, merge_path FROM knowledge_lesson_events WHERE claim_id=?1",
            vec![Sql::Text(deleted.as_str().into())],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )?;
        let path_of = |raw: &str| -> Result<Vec<String>, StorageError> {
            serde_json::from_str(raw).map_err(|_| corrupt("stored lesson event merge path"))
        };
        let mut settled = 0;
        for (key, own_path) in held {
            // The copy `into` holds (or held, before merging on) through a live merge: its
            // chain of holders passes through `into` right after that merge's lesson.
            let copies = rows(
                self.conn,
                "SELECT claim_id, merge_path FROM knowledge_lesson_events WHERE event_key=?1 \
                 AND claim_id<>?2",
                vec![Sql::Text(key.clone()), Sql::Text(deleted.as_str().into())],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )?;
            for (holder, path) in copies {
                let mut chain = path_of(&path)?;
                chain.push(holder.clone());
                let Some(at) = chain.iter().position(|c| c == into.as_str()) else {
                    continue;
                };
                let Some(owner) = at.checked_sub(1).map(|i| chain[i].clone()) else {
                    continue;
                };
                if !live.iter().any(|l| l.as_str() == owner) {
                    continue;
                }
                let owner_has: bool = db(self.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM knowledge_lesson_events WHERE claim_id=?1 AND \
                     event_key=?2)",
                    params![owner, key],
                    |r| r.get(0),
                ))?;
                if owner_has {
                    continue;
                }
                // That merge's copy returns to its lesson (as its unmerge would move it) ...
                let below =
                    serde_json::to_string(&chain[..at - 1]).map_err(|x| invalid(x.to_string()))?;
                db(self.conn.execute(
                    "UPDATE knowledge_lesson_events SET claim_id=?3, merge_path=?4 WHERE \
                     claim_id=?1 AND event_key=?2",
                    params![holder, key, owner, below],
                ))?;
                // ... and the deleted lesson's copy takes its place, merged in for good.
                let mut moved = path_of(&own_path)?;
                moved.push(deleted.as_str().to_owned());
                moved.extend(chain[at..chain.len() - 1].iter().cloned());
                let moved = serde_json::to_string(&moved).map_err(|x| invalid(x.to_string()))?;
                db(self.conn.execute(
                    "UPDATE knowledge_lesson_events SET claim_id=?3, merge_path=?4 WHERE \
                     claim_id=?1 AND event_key=?2",
                    params![deleted.as_str(), key, holder, moved],
                ))?;
                settled += 1;
                break;
            }
        }
        Ok(settled)
    }

    fn restore_merged_lesson_events(&mut self, from: &Id, into: &Id) -> Result<i64, StorageError> {
        // Candidates: every event whose merge path names `from` (it is on the way below
        // `into`, or `into` holds it directly).
        let held = rows(
            self.conn,
            "SELECT claim_id, event_key, merge_path FROM knowledge_lesson_events e WHERE \
             EXISTS(SELECT 1 FROM json_each(e.merge_path) j WHERE j.value=?1)",
            vec![Sql::Text(from.as_str().into())],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )?;
        let mut moved = 0;
        for (holder, key, path) in held {
            let mut chain: Vec<String> = serde_json::from_str(&path)
                .map_err(|_| corrupt("stored lesson event merge path"))?;
            chain.push(holder.clone());
            let Some(at) = chain
                .windows(2)
                .position(|p| p[0] == from.as_str() && p[1] == into.as_str())
            else {
                continue;
            };
            let below = serde_json::to_string(&chain[..at]).map_err(|x| invalid(x.to_string()))?;
            // One event counts once: a copy `from` holds already stays as it is.
            moved += db(self.conn.execute(
                "UPDATE knowledge_lesson_events SET claim_id=?3, merge_path=?4 WHERE \
                 claim_id=?1 AND event_key=?2 AND NOT EXISTS(SELECT 1 FROM \
                 knowledge_lesson_events WHERE claim_id=?3 AND event_key=?2)",
                params![holder, key, from.as_str(), below],
            ))? as i64;
        }
        Ok(moved)
    }

    fn refill_knowledge_lesson_events(
        &mut self,
        into: &Id,
        below: &[Vec<Id>],
    ) -> Result<i64, StorageError> {
        let mut moved = 0;
        for chain in below {
            let Some(holder) = chain.first() else {
                continue;
            };
            let via: Vec<&str> = chain.iter().map(Id::as_str).collect();
            let held = rows(
                self.conn,
                "SELECT event_key, merge_path FROM knowledge_lesson_events WHERE claim_id=?1 \
                 AND event_key NOT IN (SELECT event_key FROM knowledge_lesson_events WHERE \
                 claim_id=?2)",
                vec![
                    Sql::Text(holder.as_str().into()),
                    Sql::Text(into.as_str().into()),
                ],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )?;
            for (key, path) in held {
                let mut next: Vec<String> = serde_json::from_str(&path)
                    .map_err(|_| corrupt("stored lesson event merge path"))?;
                next.extend(via.iter().map(|s| (*s).to_owned()));
                let next = serde_json::to_string(&next).map_err(|x| invalid(x.to_string()))?;
                moved += db(self.conn.execute(
                    "UPDATE knowledge_lesson_events SET claim_id=?3, merge_path=?4 WHERE \
                     claim_id=?1 AND event_key=?2",
                    params![holder.as_str(), key, into.as_str(), next],
                ))? as i64;
            }
        }
        Ok(moved)
    }

    fn remove_live_knowledge_relation(
        &mut self,
        scope: &KnowledgeScope,
        subject: &Id,
        predicate: &str,
        object: &Id,
        at_ms: i64,
    ) -> Result<Option<StoredRelation>, StorageError> {
        let raw = db(self
            .conn
            .query_row(
                &format!(
                    "SELECT {RELATION_COLUMNS} FROM knowledge_relations WHERE scope_key=?1 AND \
                     subject_id=?2 AND predicate=?3 AND object_id=?4 AND removed_at IS NULL"
                ),
                params![scope.key(), subject.as_str(), predicate, object.as_str()],
                relation_row,
            )
            .optional())?;
        let Some(raw) = raw else { return Ok(None) };
        let mut relation = relation_of(raw)?;
        self.remove_knowledge_relation(&relation.id, at_ms)?;
        relation.removed_at_ms = Some(at_ms);
        Ok(Some(relation))
    }

    fn purge_knowledge_lesson_records(&mut self, claim: &Id) -> Result<(), StorageError> {
        let id = claim.as_str();
        db(self.conn.execute(
            "UPDATE knowledge_challenges SET reason='', evidence_json='[]', \
             resolution=CASE WHEN resolution IS NULL THEN NULL ELSE '' END WHERE claim_id=?1",
            params![id],
        ))?;
        db(self.conn.execute(
            "DELETE FROM knowledge_addresses WHERE claim_id=?1",
            params![id],
        ))?;
        // Event references can be content (a review URL, a message): keep each distinct event
        // as an opaque digest so the count and attribution stay, the text does not.
        let events = rows(
            self.conn,
            "SELECT event_key FROM knowledge_lesson_events WHERE claim_id=?1",
            vec![Sql::Text(id.into())],
            |r| r.get::<_, String>(0),
        )?;
        for key in events {
            let digest = format!(
                "purged|{}",
                devworks_core::fingerprint::sha256_hex(key.as_bytes())
            );
            if !key.starts_with("purged|") {
                db(self.conn.execute(
                    "UPDATE knowledge_lesson_events SET event_key=?3, source_ref=NULL \
                     WHERE claim_id=?1 AND event_key=?2",
                    params![id, key, digest],
                ))?;
            }
        }
        Ok(())
    }

    fn put_knowledge_challenge(&mut self, c: &StoredChallenge) -> Result<(), StorageError> {
        let current: Option<String> = db(self
            .conn
            .query_row(
                "SELECT state FROM knowledge_challenges WHERE id=?1",
                params![c.id.as_str()],
                |r| r.get(0),
            )
            .optional())?;
        if current.as_deref().is_some_and(|s| s != "Open") {
            return Err(invalid("a resolved challenge cannot change"));
        }
        db(self.conn.execute(
            &format!(
                "INSERT INTO knowledge_challenges({CHALLENGE_COLUMNS}) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT(id) DO UPDATE SET \
                 state=excluded.state, evidence_json=excluded.evidence_json, \
                 resolved_at=excluded.resolved_at, resolved_by=excluded.resolved_by, \
                 resolution=excluded.resolution WHERE knowledge_challenges.state='Open'"
            ),
            params![
                c.id.as_str(),
                c.claim_id.as_str(),
                c.state,
                c.reason,
                ids_json(&c.evidence_ids),
                c.author.0.as_str(),
                c.opened_at_ms,
                c.resolved_at_ms,
                c.resolved_by.as_ref().map(|a| a.0.as_str()),
                c.resolution,
            ],
        ))?;
        Ok(())
    }

    fn put_knowledge_address(&mut self, a: &StoredAddress) -> Result<(), StorageError> {
        db(self.conn.execute(
            "INSERT INTO knowledge_addresses(claim_id,kind,name,reference,actor_id,at) \
             VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(claim_id,kind,name) DO UPDATE SET \
             reference=excluded.reference, actor_id=excluded.actor_id, at=excluded.at",
            params![
                a.claim_id.as_str(),
                a.kind,
                a.name,
                a.reference,
                a.actor.0.as_str(),
                a.at_ms,
            ],
        ))?;
        Ok(())
    }

    fn record_knowledge_delivery(
        &mut self,
        session: &Id,
        claim: &Id,
        revision: i64,
        at_ms: i64,
    ) -> Result<bool, StorageError> {
        Ok(db(self.conn.execute(
            "INSERT OR IGNORE INTO knowledge_deliveries(session_id,claim_id,revision,delivered_at) \
             VALUES(?1,?2,?3,?4)",
            params![session.as_str(), claim.as_str(), revision, at_ms],
        ))? == 1)
    }

    fn remove_knowledge_relations_of(&mut self, id: &Id, at_ms: i64) -> Result<i64, StorageError> {
        Ok(db(self.conn.execute(
            "UPDATE knowledge_relations SET removed_at=?2, predicate='(deleted)' WHERE \
             (subject_id=?1 OR object_id=?1) AND removed_at IS NULL",
            params![id.as_str(), at_ms],
        ))? as i64)
    }

    fn observe_knowledge_source(
        &mut self,
        source_ref: &str,
        revision: &str,
        at_ms: i64,
    ) -> Result<Vec<(Id, KnowledgeScope)>, StorageError> {
        // The stale status of every claim with a basis on this source, before and after.
        let status = |conn: &Connection| {
            rows(
                conn,
                &format!(
                    "SELECT c.id, c.scope_key, c.project_id, {STALE_SQL} FROM knowledge_claims c \
                     WHERE c.id IN (SELECT claim_id FROM knowledge_claim_basis WHERE source_ref=?1) \
                     ORDER BY c.id"
                ),
                vec![Sql::Text(source_ref.into())],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, bool>(3)?,
                    ))
                },
            )
        };
        let before = status(self.conn)?;
        db(self.conn.execute(
            "INSERT INTO knowledge_source_heads(source_ref,source_revision,observed_at) \
             VALUES(?1,?2,?3) ON CONFLICT(source_ref) DO UPDATE SET \
             source_revision=excluded.source_revision, observed_at=excluded.observed_at",
            params![source_ref, revision, at_ms],
        ))?;
        let after = status(self.conn)?;
        let mut changed = Vec::new();
        for ((id, key, project, was), (_, _, _, now)) in before.into_iter().zip(after) {
            if was != now {
                changed.push((id_of(id)?, scope_of(&key, project)?));
            }
        }
        Ok(changed)
    }

    fn map_knowledge_import(
        &mut self,
        version: &Id,
        candidate: i64,
        claim: &Id,
        write: &Id,
    ) -> Result<(), StorageError> {
        db(self.conn.execute(
            "INSERT INTO knowledge_import_map(artifact_version_id,candidate,claim_id,write_id) \
             VALUES(?1,?2,?3,?4)",
            params![version.as_str(), candidate, claim.as_str(), write.as_str()],
        ))?;
        Ok(())
    }
}
