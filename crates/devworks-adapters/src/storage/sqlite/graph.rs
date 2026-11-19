//! Bounded reads of immutable, explicitly selected composed graph views.
use super::support::{conflict, corrupt, id_of, invalid};
use super::{
    conn::db,
    repos::{Snapshot, Unit},
};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{Id, RevisionDimension};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// SQLite's built-in lower/LIKE fold ASCII only. Preserve the core target predicate's
/// Unicode lowercase and literal semantics before LIMIT/materialization consume a budget.
pub(super) fn register_name_match(conn: &Connection) -> Result<(), StorageError> {
    use rusqlite::functions::FunctionFlags;
    db(conn.create_scalar_function(
        "devworks_graph_name_matches",
        4,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        |ctx| {
            let Some(query) = ctx.get::<Option<String>>(2)? else {
                return Ok(true);
            };
            let name = ctx.get::<String>(0)?.to_lowercase();
            let qualified = ctx.get::<Option<String>>(1)?;
            let folded = query.to_lowercase();
            let matches = if ctx.get::<bool>(3)? {
                name.contains(&folded)
            } else {
                name == folded
            };
            Ok(matches || qualified.as_deref() == Some(query.as_str()))
        },
    ))
}

fn value(value: String) -> Result<Value, StorageError> {
    serde_json::from_str(&value).map_err(|_| corrupt("invalid graph JSON"))
}
fn digest(value: &str) -> bool {
    devworks_core::fingerprint::is_sha256_hex(value)
}
fn scope_json(conn: &Connection, scope: &GraphScope) -> Result<String, StorageError> {
    if scope.views.is_empty() || scope.views.len() > GRAPH_MAX_VIEWS {
        return Err(invalid("graph scope needs 1..=256 exact views"));
    }
    let mut seen = BTreeSet::new();
    for binding in &scope.views {
        if !digest(&binding.view_digest) || !seen.insert(binding.repository_id.as_str()) {
            return Err(invalid(
                "graph scope needs unique repositories and exact view digests",
            ));
        }
        let valid: bool = db(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM index_views v WHERE v.id=?1 AND \
            v.source_key=?2 AND v.view_digest=?3 AND NOT EXISTS(SELECT 1 FROM \
            index_generations g WHERE g.id IN(v.baseline_id,v.branch_id,\
            v.worktree_id) AND (g.state!='Sealed' OR g.source_key!=v.source_key)))",
            params![
                binding.view_id.as_str(),
                binding.repository_id.as_str(),
                binding.view_digest
            ],
            |row| row.get(0),
        ))?;
        if !valid {
            return Err(StorageError::ScopeDenied);
        }
    }
    serde_json::to_string(scope).map_err(|_| invalid("invalid graph scope"))
}
// Highest-layer whole-unit masking precedes ALL row filters and adjacency. Deleted units do
// not expose lower rows. Repository identities remain part of every partition and endpoint.
const COMPOSED: &str = "WITH selected AS (SELECT json_extract(j.value,'$.repository_id') repo,\
    v.id view_id,v.baseline_id,v.branch_id,v.worktree_id FROM json_each(?1,\
    '$.views') j JOIN index_views v ON v.id=json_extract(j.value,\
    '$.view_id')), layers AS (SELECT repo,view_id,baseline_id generation_id,\
    2 priority FROM selected UNION ALL SELECT repo,view_id,branch_id,1 FROM \
    selected WHERE branch_id IS NOT NULL UNION ALL SELECT repo,view_id,\
    worktree_id,0 FROM selected WHERE worktree_id IS NOT NULL), ranked AS \
    (SELECT l.repo,l.view_id,u.*,row_number() OVER(PARTITION BY l.repo,\
    u.unit_key ORDER BY l.priority) rn FROM layers l JOIN source_units u ON \
    u.generation_id=l.generation_id), visible AS (SELECT * FROM ranked \
    WHERE rn=1 AND action='Replace') ";
/// The layers of the selected views (?1 scope), highest (lowest priority number) first.
const SELECTED: &str = "WITH selected AS (SELECT json_extract(j.value,'$.repository_id') repo,\
    v.id view_id,v.baseline_id,v.branch_id,v.worktree_id FROM json_each(?1,\
    '$.views') j JOIN index_views v ON v.id=json_extract(j.value,\
    '$.view_id')), layers AS (SELECT repo,view_id,baseline_id generation_id,\
    2 priority FROM selected UNION ALL SELECT repo,view_id,branch_id,1 FROM \
    selected WHERE branch_id IS NOT NULL UNION ALL SELECT repo,view_id,\
    worktree_id,0 FROM selected WHERE worktree_id IS NOT NULL) ";
/// Whole-unit masking of [`COMPOSED`] checked for one candidate `c` of a layer (its `repo`,
/// `view_id`, `priority`, `generation_id` and `unit_key`) joined to its own unit row `u`: the
/// candidate's layer replaces the unit and no higher layer of its view mentions it.
const VISIBLE_CANDIDATE: &str = "u.action='Replace' AND NOT EXISTS(SELECT 1 FROM layers h JOIN \
    source_units hu ON hu.generation_id=h.generation_id AND \
    hu.unit_key=c.unit_key WHERE h.view_id=c.view_id AND \
    h.priority<c.priority)";
/// How many edges of the one-layer view of generation ?1 name no repository and do not resolve
/// inside that view, by the graph reader's endpoint rules: a non-text unit annotation, or a source
/// or target that is not exactly one visible node (the annotated unit's, or by ID alone). Migration
/// 61 records the same count for the views it finds.
pub(super) const UNRESOLVED_EDGES: &str = "SELECT count(*) FROM source_units u JOIN graph_edges e ON \
    e.generation_id=u.generation_id AND e.unit_key=u.unit_key WHERE \
    u.generation_id=?1 AND u.action='Replace' AND json_type(e.payload_json,\
    '$.sourceRepositoryId') IS NULL AND json_type(e.payload_json,\
    '$.targetRepositoryId') IS NULL AND (coalesce(json_type(e.payload_json,\
    '$.sourceUnit'),'text')!='text' OR coalesce(json_type(e.payload_json,\
    '$.targetUnit'),'text')!='text' OR (SELECT count(*) FROM graph_nodes n \
    INDEXED BY graph_nodes_by_id JOIN source_units nu ON \
    nu.generation_id=n.generation_id AND nu.unit_key=n.unit_key WHERE \
    n.generation_id=u.generation_id AND n.node_id=e.source_node_id AND \
    nu.action='Replace' AND (json_type(e.payload_json,'$.sourceUnit') IS \
    NULL OR n.unit_key=json_extract(e.payload_json,'$.sourceUnit')))!=1 OR \
    (SELECT count(*) FROM graph_nodes n INDEXED BY graph_nodes_by_id JOIN \
    source_units nu ON nu.generation_id=n.generation_id AND \
    nu.unit_key=n.unit_key WHERE n.generation_id=u.generation_id AND \
    n.node_id=e.target_node_id AND nu.action='Replace' AND \
    (json_type(e.payload_json,'$.targetUnit') IS NULL OR \
    n.unit_key=json_extract(e.payload_json,'$.targetUnit')))!=1)";
/// The visible edges naming a source or target repository (`graph_edges_named_repository`) of
/// one-layer views (?1 scope, ?3 after, ?4 limit), in edge order.
fn repository_edges_sql() -> String {
    format!(
        "{SELECTED}, c AS (SELECT l.repo,l.view_id,l.priority,e.generation_id,\
            e.unit_key,e.edge_id,e.source_node_id,e.target_node_id,e.kind,\
            e.payload_json FROM layers l CROSS JOIN graph_edges e INDEXED BY \
            graph_edges_named_repository WHERE e.generation_id=l.generation_id AND \
            (json_type(e.payload_json,'$.sourceRepositoryId') IS NOT NULL OR \
            json_type(e.payload_json,'$.targetRepositoryId') IS NOT NULL)) SELECT \
            c.repo,c.view_id,c.generation_id,u.content_digest,c.unit_key,c.edge_id,\
            c.source_node_id,c.target_node_id,c.kind,c.payload_json FROM c JOIN \
            source_units u ON u.generation_id=c.generation_id AND \
            u.unit_key=c.unit_key WHERE {VISIBLE_CANDIDATE} AND (json_extract(?3,\
            '$.repository_id') IS NULL OR (c.repo,c.unit_key,\
            c.edge_id)>(json_extract(?3,'$.repository_id'),json_extract(?3,\
            '$.unit_key'),json_extract(?3,'$.item_id'))) ORDER BY c.repo,c.unit_key,\
            c.edge_id LIMIT ?4"
    )
}
fn repository_edges(
    conn: &Connection,
    scope: &GraphScope,
    after: Option<&GraphPosition>,
    limit: u16,
) -> Result<Option<GraphPage<GraphEdge>>, StorageError> {
    let scope_text = scope_json(conn, scope)?;
    let overlay: bool = db(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM json_each(?1,'$.views') j JOIN index_views \
            v ON v.id=json_extract(j.value,'$.view_id') WHERE v.branch_id IS NOT \
            NULL OR v.worktree_id IS NOT NULL)",
        [&scope_text],
        |r| r.get(0),
    ))?;
    if overlay {
        return Ok(None);
    }
    probed(
        conn,
        scope,
        false,
        "null",
        &repository_edges_sql(),
        after,
        limit,
    )
}

/// The keyset continuation after ?3 in node order.
const NODE_AFTER: &str = "(json_extract(?3,'$.repository_id') IS NULL OR (c.repo,c.unit_key,\
    c.node_id)>(json_extract(?3,'$.repository_id'),json_extract(?3,\
    '$.unit_key'),json_extract(?3,'$.item_id')))";

fn page<T>(mut items: Vec<T>, limit: u16) -> GraphPage<T> {
    let has_more = items.len() > usize::from(limit);
    items.truncate(usize::from(limit));
    GraphPage { items, has_more }
}
fn bounded(limit: u16, after: Option<&GraphPosition>) -> Result<(i64, String), StorageError> {
    if limit == 0 || limit > GRAPH_MAX_PAGE {
        return Err(invalid("graph page limit must be 1..=500"));
    }
    Ok((
        i64::from(limit) + 1,
        serde_json::to_string(&after).map_err(|_| invalid("invalid graph position"))?,
    ))
}
fn node_row(r: &Row<'_>) -> rusqlite::Result<(String, String, String, String, StoredNodeRaw)> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        (r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?),
    ))
}
type StoredNodeRaw = (String, String, String, String, String);
fn node_from(
    (repo, view, generation, content, n): (String, String, String, String, StoredNodeRaw),
) -> Result<GraphNode, StorageError> {
    Ok(GraphNode {
        repository_id: id_of(repo)?,
        view_id: id_of(view)?,
        generation_id: id_of(generation)?,
        content_digest: content,
        node: StoredNode {
            unit_key: n.0,
            node_id: n.1,
            kind: n.2,
            name: n.3,
            payload: value(n.4)?,
        },
    })
}
fn node_query(composed: &str, node_predicate: &str, cursor_predicate: &str, order: &str) -> String {
    format!(
        "{composed}SELECT v.repo,v.view_id,v.generation_id,v.content_digest,\
            n.unit_key,n.node_id,n.kind,n.name,n.payload_json FROM visible v JOIN \
            graph_nodes n ON n.generation_id=v.generation_id AND \
            n.unit_key=v.unit_key WHERE (json_extract(?2,'$.name.query') IS NULL OR \
            devworks_graph_name_matches(n.name,CASE WHEN json_type(n.payload_json,\
            '$.qualifiedName')='text' THEN json_extract(n.payload_json,\
            '$.qualifiedName') END,json_extract(?2,'$.name.query'),json_extract(?2,\
            '$.name.contains'))) AND (json_extract(?2,'$.pattern') IS NULL OR \
            n.name LIKE json_extract(?2,'$.pattern') OR json_extract(n.payload_json,\
            '$.qualifiedName') LIKE json_extract(?2,'$.pattern')) AND \
            (json_array_length(?2,'$.kinds')=0 OR n.kind IN(SELECT value FROM \
            json_each(?2,'$.kinds'))) AND (json_extract(?2,'$.unit') IS NULL OR \
            n.unit_key=json_extract(?2,'$.unit')) AND {node_predicate} AND (NOT \
            json_extract(?2,'$.excludeUntrusted') OR NOT EXISTS(SELECT 1 FROM \
            graph_trust t JOIN index_views iv ON iv.id=v.view_id WHERE \
            t.repository_id=v.repo AND t.unit_key=n.unit_key AND \
            t.node_id=n.node_id AND t.view_digest=iv.view_digest AND \
            t.content_digest=v.content_digest AND t.trusted=0)) AND \
            {cursor_predicate} ORDER BY {order} LIMIT ?4"
    )
}

fn nodes(
    conn: &Connection,
    scope: &GraphScope,
    filter: &GraphNodeFilter,
    after: Option<&GraphPosition>,
    limit: u16,
) -> Result<GraphPage<GraphNode>, StorageError> {
    if filter.node_id.is_none() {
        return scan_nodes(conn, scope, filter, after, limit);
    }
    let scope = scope_json(conn, scope)?;
    let (limit, after) = bounded(limit, after)?;
    // The node ID's rows in each layer (`graph_nodes_by_id`), each kept when its unit is visible
    // there: no unit row of the scope is ranked. The unit and every other filter apply below.
    let visible = node_id_visible();
    let node_predicate = "n.node_id=json_extract(?2,'$.node')";
    let filter = json!({
        "pattern": filter.pattern,
        "name": filter
            .name
            .as_ref()
            .map(|n| json!({"query": n.name, "contains": n.contains})),
        "kinds": filter.kinds,
        "unit": filter.unit_key,
        "node": filter.node_id,
        "excludeUntrusted": filter.exclude_untrusted
    })
    .to_string();
    let sql = node_query(
        &visible,
        node_predicate,
        "(json_extract(?3,'$.repository_id') IS NULL OR (v.repo,n.unit_key,\
            n.node_id)>(json_extract(?3,'$.repository_id'),json_extract(?3,\
            '$.unit_key'),json_extract(?3,'$.item_id')))",
        "v.repo,n.unit_key,n.node_id",
    );
    let mut statement = db(conn.prepare_cached(&sql))?;
    let rows = db(
        db(statement.query_map(params![scope, filter, after, limit], node_row))?
            .collect::<rusqlite::Result<Vec<_>>>(),
    )?;
    Ok(page(
        rows.into_iter()
            .map(node_from)
            .collect::<Result<Vec<_>, _>>()?,
        (limit - 1) as u16,
    ))
}
/// The visible units (`visible` of [`COMPOSED`], for [`node_query`]) holding a node of ID
/// `$.node` of ?2.
fn node_id_visible() -> String {
    format!(
        "{SELECTED}, cand AS (SELECT DISTINCT l.repo,l.view_id,l.priority,\
            n.generation_id,n.unit_key FROM layers l CROSS JOIN graph_nodes n \
            INDEXED BY graph_nodes_by_id WHERE n.generation_id=l.generation_id AND \
            n.node_id=json_extract(?2,'$.node')), visible AS (SELECT c.repo,\
            c.view_id,c.generation_id,u.content_digest,c.unit_key FROM cand c JOIN \
            source_units u ON u.generation_id=c.generation_id AND \
            u.unit_key=c.unit_key WHERE {VISIBLE_CANDIDATE}) "
    )
}
/// Visible nodes found by one index probe (?2 its value, ?3 after, ?4 limit), in node order.
fn probed_nodes_sql(index: &str, probe: &str, from: &str) -> String {
    format!(
        "{SELECTED}, cand AS (SELECT l.repo,l.view_id,l.priority,n.generation_id,\
            n.unit_key,n.node_id,n.kind,n.name,n.payload_json FROM {from}layers l \
            CROSS JOIN graph_nodes n INDEXED BY {index} WHERE \
            n.generation_id=l.generation_id AND {probe}) SELECT c.repo,c.view_id,\
            c.generation_id,u.content_digest,c.unit_key,c.node_id,c.kind,c.name,\
            c.payload_json FROM cand c JOIN source_units u ON \
            u.generation_id=c.generation_id AND u.unit_key=c.unit_key WHERE \
            {VISIBLE_CANDIDATE} AND {NODE_AFTER} ORDER BY c.repo,c.unit_key,\
            c.node_id LIMIT ?4"
    )
}
fn kind_nodes_sql() -> String {
    probed_nodes_sql("graph_node_name", "n.kind=?2", "")
}
fn project_nodes_sql() -> String {
    probed_nodes_sql(
        "graph_nodes_project_directory",
        "n.kind='DotnetProject' AND rtrim(n.unit_key,replace(n.unit_key,'/',''))=d.value",
        "json_each(?2) d CROSS JOIN ",
    )
}
fn probed_nodes(
    conn: &Connection,
    scope: &GraphScope,
    sql: &str,
    probe: &str,
    after: Option<&GraphPosition>,
    limit: u16,
) -> Result<GraphPage<GraphNode>, StorageError> {
    let scope = scope_json(conn, scope)?;
    let (count, after) = bounded(limit, after)?;
    let mut statement = db(conn.prepare_cached(sql))?;
    let rows = db(
        db(statement.query_map(params![scope, probe, after, count], node_row))?
            .collect::<rusqlite::Result<Vec<_>>>(),
    )?;
    Ok(page(
        rows.into_iter()
            .map(node_from)
            .collect::<Result<Vec<_>, _>>()?,
        limit,
    ))
}

/// Broad keyset pages walk visible units in order, then use each unit's node primary key.
/// No page sorts/materializes the payloads of every node in the repository. Exact-ID
/// lookups retain the sargable composed lookup above (including ambiguity checks).
fn scan_nodes(
    conn: &Connection,
    scope: &GraphScope,
    filter: &GraphNodeFilter,
    after: Option<&GraphPosition>,
    limit: u16,
) -> Result<GraphPage<GraphNode>, StorageError> {
    let scope_json = scope_json(conn, scope)?;
    let (count, after_json) = bounded(limit, after)?;
    let filter_json = json!({
        "pattern": filter.pattern,
        "name": filter
            .name
            .as_ref()
            .map(|n| json!({"query": n.name, "contains": n.contains})),
        "kinds": filter.kinds,
        "unit": filter.unit_key,
        "node": filter.node_id,
        "excludeUntrusted": filter.exclude_untrusted
    })
    .to_string();
    // Unit selection commutes with the per-(repository,unit) layer ranking: apply it before the
    // window, keeping every layer's row of that unit, so one file's nodes rank no other unit.
    let composed = if filter.unit_key.is_some() {
        COMPOSED.replace(
            "u.generation_id=l.generation_id)",
            "u.generation_id=l.generation_id WHERE u.unit_key=json_extract(?2,'$.unit'))",
        )
    } else {
        COMPOSED.to_owned()
    };
    let units_sql = format!(
        "{composed}SELECT v.repo,v.view_id,v.generation_id,v.content_digest,\
            v.unit_key FROM visible v WHERE (json_extract(?2,'$.unit') IS NULL OR \
            v.unit_key=json_extract(?2,'$.unit')) AND (json_extract(?3,\
            '$.repository_id') IS NULL OR (v.repo,v.unit_key)>=(json_extract(?3,\
            '$.repository_id'),json_extract(?3,'$.unit_key'))) ORDER BY v.repo,\
            v.unit_key"
    );
    let mut units = db(conn.prepare_cached(&units_sql))?;
    let mut units = db(units.query(params![scope_json, filter_json, after_json]))?;
    let mut found = Vec::new();
    while let Some(unit) = db(units.next())? {
        let repo: String = db(unit.get(0))?;
        let view: String = db(unit.get(1))?;
        let generation: String = db(unit.get(2))?;
        let content: String = db(unit.get(3))?;
        let unit_key: String = db(unit.get(4))?;
        let same_unit = after.is_some_and(|after| {
            after.repository_id.as_str() == repo && after.unit_key == unit_key
        });
        let cursor = if same_unit {
            "n.node_id>json_extract(?3,'$.item_id')"
        } else {
            "1"
        };
        let header = "WITH visible AS (SELECT json_extract(?1,'$.repo') repo,json_extract(?1,\
            '$.view') view_id,json_extract(?1,'$.generation') generation_id,\
            json_extract(?1,'$.content') content_digest,json_extract(?1,'$.unit') \
            unit_key) ";
        let sql = node_query(header, "1", cursor, "n.node_id");
        let mut nodes = db(conn.prepare_cached(&sql))?;
        let bound=json!({"repo":repo,"view":view,"generation":generation,"content":content,"unit":unit_key}).to_string();
        let remaining =
            count - i64::try_from(found.len()).map_err(|_| invalid("graph page length"))?;
        let rows = db(db(
            nodes.query_map(params![bound, filter_json, after_json, remaining], node_row)
        )?
        .collect::<rusqlite::Result<Vec<_>>>())?;
        for row in rows {
            found.push(node_from(row)?);
        }
        if found.len() >= count as usize {
            break;
        }
    }
    Ok(page(found, limit))
}

fn edges(
    conn: &Connection,
    scope: &GraphScope,
    filter: &GraphEdgeFilter,
    after: Option<&GraphPosition>,
    limit: u16,
) -> Result<GraphPage<GraphEdge>, StorageError> {
    let scope = scope_json(conn, scope)?;
    let (limit, after) = bounded(limit, after)?;
    let filter =
        json!({"unit":filter.unit_key,"kinds":filter.kinds,"sources":filter.source_ids,"targets":filter.target_ids})
            .to_string();
    let sql = format!(
        "{COMPOSED}SELECT v.repo,v.view_id,v.generation_id,v.content_digest,\
            e.unit_key,e.edge_id,e.source_node_id,e.target_node_id,e.kind,\
            e.payload_json FROM visible v JOIN graph_edges e ON \
            e.generation_id=v.generation_id AND e.unit_key=v.unit_key WHERE \
            (json_extract(?2,'$.unit') IS NULL OR e.unit_key=json_extract(?2,\
            '$.unit')) AND (json_array_length(?2,'$.kinds')=0 OR e.kind IN(SELECT \
            value FROM json_each(?2,'$.kinds'))) AND (json_array_length(?2,\
            '$.sources')=0 OR EXISTS(SELECT 1 FROM json_each(?2,'$.sources') s \
            WHERE json_extract(s.value,\
            '$.repository_id')=coalesce(json_extract(e.payload_json,\
            '$.sourceRepositoryId'),v.repo) AND json_extract(s.value,\
            '$.unit_key')=coalesce(json_extract(e.payload_json,'$.sourceUnit'),\
            e.unit_key) AND json_extract(s.value,'$.node_id')=e.source_node_id)) \
            AND (json_array_length(?2,'$.targets')=0 OR EXISTS(SELECT 1 FROM \
            json_each(?2,'$.targets') t WHERE json_extract(t.value,\
            '$.repository_id')=coalesce(json_extract(e.payload_json,\
            '$.targetRepositoryId'),v.repo) AND json_extract(t.value,\
            '$.unit_key')=json_extract(e.payload_json,'$.targetUnit') AND \
            json_extract(t.value,'$.node_id')=e.target_node_id)) AND \
            (json_extract(?3,'$.repository_id') IS NULL OR (v.repo,e.unit_key,\
            e.edge_id)>(json_extract(?3,'$.repository_id'),json_extract(?3,\
            '$.unit_key'),json_extract(?3,'$.item_id'))) ORDER BY v.repo,e.unit_key,\
            e.edge_id LIMIT ?4"
    );
    let mut statement = db(conn.prepare_cached(&sql))?;
    let rows = db(db(
        statement.query_map(params![scope, filter, after, limit], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, String>(9)?,
            ))
        }),
    )?
    .collect::<rusqlite::Result<Vec<_>>>())?;
    let items = rows
        .into_iter()
        .map(|r| {
            Ok(GraphEdge {
                repository_id: id_of(r.0)?,
                view_id: id_of(r.1)?,
                generation_id: id_of(r.2)?,
                content_digest: r.3,
                edge: StoredEdge {
                    unit_key: r.4,
                    edge_id: r.5,
                    source_node_id: r.6,
                    target_node_id: r.7,
                    kind: r.8,
                    payload: value(r.9)?,
                },
            })
        })
        .collect::<Result<Vec<_>, StorageError>>()?;
    Ok(page(items, (limit - 1) as u16))
}
/// The layers of the selected views, highest (lowest priority number) first.
const LAYERS: &str = "WITH selected AS (SELECT json_extract(j.value,'$.repository_id') repo,\
    v.id view_id,v.baseline_id,v.branch_id,v.worktree_id FROM json_each(?1,\
    '$.views') j JOIN index_views v ON v.id=json_extract(j.value,\
    '$.view_id')), layers AS (SELECT repo,view_id,baseline_id generation_id,\
    2 priority FROM selected UNION ALL SELECT repo,view_id,branch_id,1 FROM \
    selected WHERE branch_id IS NOT NULL UNION ALL SELECT repo,view_id,\
    worktree_id,0 FROM selected WHERE worktree_id IS NOT NULL), wanted AS \
    (SELECT json_extract(value,'$.repository_id') repo,json_extract(value,\
    '$.unit_key') unit,json_extract(value,'$.node_id') node FROM \
    json_each(?2,'$.nodes')), probes AS (SELECT DISTINCT node FROM wanted) ";

/// Composed edges incident to exact endpoints. Each layer's stored edges are probed through its
/// endpoint index (`graph_forward`/`graph_reverse`); an edge its overlay view links differently
/// is read from `graph_view_links` instead, through that table's endpoint index. A candidate
/// counts only when its unit is visible: no higher layer of its view mentions the unit and its
/// own layer replaces it, which is the whole-unit masking of [`COMPOSED`] checked per row.
fn incident(
    conn: &Connection,
    scope: &GraphScope,
    filter: &GraphIncidentFilter,
    after: Option<&GraphPosition>,
    limit: u16,
) -> Result<Option<GraphPage<GraphEdge>>, StorageError> {
    if filter.nodes.len() > usize::from(GRAPH_MAX_PAGE) {
        return Err(invalid("an incident edge read names at most 500 endpoints"));
    }
    let filter_json = json!({"nodes":filter.nodes,"kinds":filter.kinds}).to_string();
    let sql = incident_sql(filter.end, Probe::Exact);
    probed(
        conn,
        scope,
        filter.nodes.is_empty(),
        &filter_json,
        &sql,
        after,
        limit,
    )
}
/// Composed edges whose endpoint node ID, or `targetQualifiedName`, is one of the probed values;
/// the caller resolves which of them end at its node.
fn candidates(
    conn: &Connection,
    scope: &GraphScope,
    filter: &GraphCandidateFilter,
    after: Option<&GraphPosition>,
    limit: u16,
) -> Result<Option<GraphPage<GraphEdge>>, StorageError> {
    if filter.values.len() > usize::from(GRAPH_MAX_PAGE) {
        return Err(invalid("a candidate edge read names at most 500 endpoints"));
    }
    let probe = match (filter.key, filter.end) {
        (GraphEndpointKey::NodeId, _) => Probe::NodeId,
        (GraphEndpointKey::TargetQualifiedName, GraphEdgeEnd::Target) => Probe::Qualified,
        (GraphEndpointKey::TargetQualifiedName, GraphEdgeEnd::Source) => {
            return Err(invalid("a qualified name names only an edge target"));
        }
    };
    let nodes: Vec<Value> = filter
        .values
        .iter()
        .map(|value| json!({"node_id":value}))
        .collect();
    let filter_json = json!({"nodes":nodes,"kinds":filter.kinds}).to_string();
    let sql = incident_sql(filter.end, probe);
    probed(
        conn,
        scope,
        filter.values.is_empty(),
        &filter_json,
        &sql,
        after,
        limit,
    )
}
/// Runs an endpoint-probe query of [`incident_sql`]: `None` while an overlay view in scope has no
/// stored links, whose composed endpoints only the scan can tell.
fn probed(
    conn: &Connection,
    scope: &GraphScope,
    empty: bool,
    filter_json: &str,
    sql: &str,
    after: Option<&GraphPosition>,
    limit: u16,
) -> Result<Option<GraphPage<GraphEdge>>, StorageError> {
    let scope = scope_json(conn, scope)?;
    let (limit, after) = bounded(limit, after)?;
    let unlinked: bool = db(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM json_each(?1,'$.views') j JOIN index_views \
            v ON v.id=json_extract(j.value,'$.view_id') WHERE (v.branch_id IS NOT \
            NULL OR v.worktree_id IS NOT NULL) AND NOT EXISTS(SELECT 1 FROM \
            graph_view_link_sets s WHERE s.view_id=v.id))",
        [&scope],
        |r| r.get(0),
    ))?;
    if unlinked {
        return Ok(None);
    }
    if empty {
        return Ok(Some(GraphPage {
            items: Vec::new(),
            has_more: false,
        }));
    }
    let mut statement = db(conn.prepare_cached(sql))?;
    let rows = db(db(
        statement.query_map(params![scope, filter_json, after, limit], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, String>(9)?,
            ))
        }),
    )?
    .collect::<rusqlite::Result<Vec<_>>>())?;
    let items = rows
        .into_iter()
        .map(|r| {
            Ok(GraphEdge {
                repository_id: id_of(r.0)?,
                view_id: id_of(r.1)?,
                generation_id: id_of(r.2)?,
                content_digest: r.3,
                edge: StoredEdge {
                    unit_key: r.4,
                    edge_id: r.5,
                    source_node_id: r.6,
                    target_node_id: r.7,
                    kind: r.8,
                    payload: value(r.9)?,
                },
            })
        })
        .collect::<Result<Vec<_>, StorageError>>()?;
    Ok(Some(page(items, (limit - 1) as u16)))
}
/// What an endpoint probe of [`incident_sql`] compares.
#[derive(Clone, Copy, Debug)]
enum Probe {
    /// The exact composite endpoint identity of [`GraphEdgeFilter`].
    Exact,
    /// The endpoint node ID only, in any unit and repository.
    NodeId,
    /// The `targetQualifiedName` of a stored edge (`graph_edges_target_qualified`). A view link
    /// never carries one: only an edge naming no other repository is linked.
    Qualified,
}
/// The incident-edge query for one endpoint (?1 scope, ?2 nodes and kinds, ?3 after, ?4 limit).
fn incident_sql(end: GraphEdgeEnd, probe: Probe) -> String {
    let (node, index, linked, identity) = match end {
        GraphEdgeEnd::Target => (
            "target_node_id",
            "graph_reverse",
            "graph_view_links_target",
            "w.repo=coalesce(json_extract(c.payload_json,'$.targetRepositoryId'),\
                c.repo) AND w.unit=json_extract(c.payload_json,'$.targetUnit') AND \
                w.node=c.target_node_id",
        ),
        GraphEdgeEnd::Source => (
            "source_node_id",
            "graph_forward",
            "graph_view_links_source",
            "w.repo=coalesce(json_extract(c.payload_json,'$.sourceRepositoryId'),\
                c.repo) AND w.unit=coalesce(json_extract(c.payload_json,'$.sourceUnit'),\
                c.unit_key) AND w.node=c.source_node_id",
        ),
    };
    let (index, stored_match, identity) = match probe {
        Probe::Exact => (index, format!("e.{node}=p.node"), identity),
        Probe::NodeId => (index, format!("e.{node}=p.node"), "1"),
        Probe::Qualified => (
            "graph_edges_target_qualified",
            "json_extract(e.payload_json,'$.targetQualifiedName')=p.node".to_owned(),
            "1",
        ),
    };
    let candidates = match probe {
        Probe::Qualified => "candidates AS (SELECT * FROM stored)".to_owned(),
        Probe::Exact | Probe::NodeId => format!(
            "linked AS (SELECT l.repo,l.view_id,l.priority,k.generation_id,\
                k.unit_key,k.edge_id,k.source_node_id,k.target_node_id,e.kind,\
                k.payload_json FROM probes p CROSS JOIN layers l CROSS JOIN \
                graph_view_links k INDEXED BY {linked} CROSS JOIN graph_edges e ON \
                k.view_id=l.view_id AND k.{node}=p.node AND \
                k.generation_id=l.generation_id AND e.generation_id=k.generation_id AND \
                e.unit_key=k.unit_key AND e.edge_id=k.edge_id), candidates AS (SELECT * \
                FROM stored UNION ALL SELECT * FROM linked)"
        ),
    };
    format!(
        "{LAYERS}, stored AS (SELECT l.repo,l.view_id,l.priority,e.generation_id,\
            e.unit_key,e.edge_id,e.source_node_id,e.target_node_id,e.kind,\
            e.payload_json FROM probes p CROSS JOIN layers l CROSS JOIN graph_edges \
            e INDEXED BY {index} WHERE e.generation_id=l.generation_id AND \
            {stored_match} AND NOT EXISTS(SELECT 1 FROM graph_view_links k WHERE \
            k.view_id=l.view_id AND k.unit_key=e.unit_key AND \
            k.edge_id=e.edge_id)), {candidates} SELECT c.repo,c.view_id,\
            c.generation_id,u.content_digest,c.unit_key,c.edge_id,c.source_node_id,\
            c.target_node_id,c.kind,c.payload_json FROM candidates c JOIN \
            source_units u ON u.generation_id=c.generation_id AND \
            u.unit_key=c.unit_key WHERE u.action='Replace' AND NOT EXISTS(SELECT 1 \
            FROM layers h JOIN source_units hu ON hu.generation_id=h.generation_id \
            AND hu.unit_key=c.unit_key WHERE h.view_id=c.view_id AND \
            h.priority<c.priority) AND (json_array_length(?2,'$.kinds')=0 OR c.kind \
            IN(SELECT value FROM json_each(?2,'$.kinds'))) AND EXISTS(SELECT 1 FROM \
            wanted w WHERE {identity}) AND (json_extract(?3,'$.repository_id') IS \
            NULL OR (c.repo,c.unit_key,c.edge_id)>(json_extract(?3,\
            '$.repository_id'),json_extract(?3,'$.unit_key'),json_extract(?3,\
            '$.item_id'))) ORDER BY c.repo,c.unit_key,c.edge_id LIMIT ?4"
    )
}
macro_rules! graph_reads {
    ($t:ty) => {
        impl GraphRead for $t {
            fn graph_units(
                &self,
                scope: &GraphScope,
                after: Option<&GraphPosition>,
                limit: u16,
            ) -> Result<GraphPage<GraphUnit>, StorageError> {
                let scope = scope_json(self.conn, scope)?;
                let (n, after) = bounded(limit, after)?;
                let sql = format!(
                    "{COMPOSED}SELECT repo,view_id,generation_id,unit_key,content_digest,\
            action,payload_json FROM visible WHERE json_extract(?2,\
            '$.repository_id') IS NULL OR (repo,unit_key)>(json_extract(?2,\
            '$.repository_id'),json_extract(?2,'$.unit_key')) ORDER BY repo,\
            unit_key LIMIT ?3"
                );
                let mut statement = db(self.conn.prepare_cached(&sql))?;
                let rows = db(db(statement.query_map(params![scope, after, n], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                    ))
                }))?
                .collect::<rusqlite::Result<Vec<_>>>())?;
                Ok(page(
                    rows.into_iter()
                        .map(|r| {
                            Ok(GraphUnit {
                                repository_id: id_of(r.0)?,
                                view_id: id_of(r.1)?,
                                unit: StoredUnit {
                                    generation_id: id_of(r.2)?,
                                    unit_key: r.3,
                                    content_digest: r.4,
                                    action: r.5,
                                    payload: value(r.6)?,
                                },
                            })
                        })
                        .collect::<Result<Vec<_>, StorageError>>()?,
                    limit,
                ))
            }
            fn graph_nodes(
                &self,
                scope: &GraphScope,
                filter: &GraphNodeFilter,
                after: Option<&GraphPosition>,
                limit: u16,
            ) -> Result<GraphPage<GraphNode>, StorageError> {
                nodes(self.conn, scope, filter, after, limit)
            }
            fn graph_node(
                &self,
                scope: &GraphScope,
                key: &GraphNodeIdentity,
            ) -> Result<Option<GraphNode>, StorageError> {
                let scoped = GraphScope {
                    views: scope
                        .views
                        .iter()
                        .filter(|v| v.repository_id == key.repository_id)
                        .cloned()
                        .collect(),
                };
                scope_json(self.conn, scope)?;
                if scoped.views.is_empty() {
                    return Err(StorageError::ScopeDenied);
                }
                Ok(nodes(
                    self.conn,
                    &scoped,
                    &GraphNodeFilter {
                        unit_key: Some(key.unit_key.clone()),
                        node_id: Some(key.node_id.clone()),
                        ..Default::default()
                    },
                    None,
                    1,
                )?
                .items
                .pop())
            }
            fn graph_nodes_of_kind(
                &self,
                scope: &GraphScope,
                kind: &str,
                after: Option<&GraphPosition>,
                limit: u16,
            ) -> Result<GraphPage<GraphNode>, StorageError> {
                probed_nodes(self.conn, scope, &kind_nodes_sql(), kind, after, limit)
            }
            fn graph_view_unresolved(&self, view: &Id) -> Result<Option<u64>, StorageError> {
                let count: Option<i64> = db(self
                    .conn
                    .query_row(
                        "SELECT edge_count FROM graph_view_unresolved WHERE view_id=?1",
                        [view.as_str()],
                        |r| r.get(0),
                    )
                    .optional())?;
                count
                    .map(|c| u64::try_from(c).map_err(|_| corrupt("invalid unresolved edge count")))
                    .transpose()
            }
            fn graph_repository_edges(
                &self,
                scope: &GraphScope,
                after: Option<&GraphPosition>,
                limit: u16,
            ) -> Result<Option<GraphPage<GraphEdge>>, StorageError> {
                repository_edges(self.conn, scope, after, limit)
            }
            fn graph_project_nodes(
                &self,
                scope: &GraphScope,
                directories: &[String],
                after: Option<&GraphPosition>,
                limit: u16,
            ) -> Result<GraphPage<GraphNode>, StorageError> {
                if directories.len() > usize::from(GRAPH_MAX_PAGE) {
                    return Err(invalid("a project node read names at most 500 directories"));
                }
                let directories = serde_json::to_string(directories)
                    .map_err(|_| invalid("invalid graph directories"))?;
                probed_nodes(
                    self.conn,
                    scope,
                    &project_nodes_sql(),
                    &directories,
                    after,
                    limit,
                )
            }
            fn graph_edges(
                &self,
                scope: &GraphScope,
                filter: &GraphEdgeFilter,
                after: Option<&GraphPosition>,
                limit: u16,
            ) -> Result<GraphPage<GraphEdge>, StorageError> {
                edges(self.conn, scope, filter, after, limit)
            }
            fn graph_counts(&self, scope: &GraphScope) -> Result<GraphCounts, StorageError> {
                let scope = scope_json(self.conn, scope)?;
                let sql = format!(
                    "{COMPOSED}SELECT (SELECT count(*) FROM visible),(SELECT count(*) FROM \
            visible v JOIN graph_nodes n ON n.generation_id=v.generation_id AND \
            n.unit_key=v.unit_key),(SELECT count(*) FROM visible v JOIN graph_edges \
            e ON e.generation_id=v.generation_id AND e.unit_key=v.unit_key)"
                );
                let (units, nodes, edges): (i64, i64, i64) =
                    db(self
                        .conn
                        .query_row(&sql, [scope], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))?;
                Ok(GraphCounts {
                    units: u64::try_from(units).map_err(|_| corrupt("invalid graph count"))?,
                    nodes: u64::try_from(nodes).map_err(|_| corrupt("invalid graph count"))?,
                    edges: u64::try_from(edges).map_err(|_| corrupt("invalid graph count"))?,
                })
            }
            fn graph_kinds(
                &self,
                scope: &GraphScope,
                domain: GraphElementKind,
                after: Option<&str>,
                limit: u16,
            ) -> Result<GraphPage<GraphKindCount>, StorageError> {
                let scope = scope_json(self.conn, scope)?;
                let (n, _) = bounded(limit, None)?;
                let table = match domain {
                    GraphElementKind::Node => "graph_nodes",
                    GraphElementKind::Edge => "graph_edges",
                };
                let sql = format!(
                    "{COMPOSED}SELECT r.kind,count(*) FROM visible v JOIN {table} r ON \
            r.generation_id=v.generation_id AND r.unit_key=v.unit_key WHERE (?2 IS \
            NULL OR r.kind>?2) GROUP BY r.kind ORDER BY r.kind LIMIT ?3"
                );
                let mut statement = db(self.conn.prepare_cached(&sql))?;
                let rows = db(db(statement.query_map(params![scope, after, n], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                }))?
                .collect::<rusqlite::Result<Vec<_>>>())?;
                Ok(page(
                    rows.into_iter()
                        .map(|(kind, count)| {
                            Ok(GraphKindCount {
                                kind,
                                count: u64::try_from(count)
                                    .map_err(|_| corrupt("invalid graph count"))?,
                            })
                        })
                        .collect::<Result<Vec<_>, StorageError>>()?,
                    limit,
                ))
            }
            fn graph_trust(
                &self,
                scope: &GraphScope,
                key: &GraphNodeIdentity,
                content: &str,
            ) -> Result<Option<StoredGraphTrust>, StorageError> {
                trust(self.conn, scope, key, content)
            }
            fn graph_trust_revision(&self, scope: &GraphScope) -> Result<i64, StorageError> {
                let scope = scope_json(self.conn, scope)?;
                db(self.conn.query_row(
                    "SELECT COALESCE(max(c.sequence)+1,1) FROM graph_trust_changes c JOIN \
            json_each(?1,'$.views') j ON c.repository_id=json_extract(j.value,\
            '$.repository_id') AND c.view_digest=json_extract(j.value,\
            '$.view_digest')",
                    [scope],
                    |r| r.get(0),
                ))
            }
            fn graph_retirable_views(
                &self,
                now: i64,
                after: Option<&Id>,
                limit: u16,
            ) -> Result<GraphPage<StoredView>, StorageError> {
                let (n, _) = bounded(limit, None)?;
                let mut statement = db(self.conn.prepare_cached(
                    "SELECT v.id FROM index_views v WHERE (json_extract(v.coverage_json,\
            '$.overlayId') IS NOT NULL OR json_extract(v.coverage_json,'$.head') IS \
            NOT NULL) AND NOT EXISTS(SELECT 1 FROM published_views p WHERE \
            p.view_id=v.id) AND NOT EXISTS(SELECT 1 FROM index_overlays o WHERE \
            o.view_id=v.id) AND NOT EXISTS(SELECT 1 FROM index_pins p WHERE \
            p.view_id=v.id AND p.lease_until>?1) AND (?2 IS NULL OR v.id>?2) ORDER \
            BY v.id LIMIT ?3",
                ))?;
                let rows = db(db(statement
                    .query_map(params![now, after.map(Id::as_str), n], |r| {
                        r.get::<_, String>(0)
                    }))?
                .collect::<rusqlite::Result<Vec<_>>>())?;
                let views = rows
                    .into_iter()
                    .map(|v| {
                        IndexRead::view(self, &id_of(v)?)
                            .and_then(|v| v.ok_or_else(|| corrupt("retirable graph view vanished")))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(page(views, limit))
            }
            fn graph_expired_pins(
                &self,
                now: i64,
                after: Option<&Id>,
                limit: u16,
            ) -> Result<GraphPage<StoredGraphReadPin>, StorageError> {
                let (n, _) = bounded(limit, None)?;
                let mut statement = db(self.conn.prepare_cached(
                    "SELECT id FROM graph_read_pins WHERE expires_at<=?1 AND (?2 IS NULL OR \
            id>?2) ORDER BY id LIMIT ?3",
                ))?;
                let ids = db(db(statement
                    .query_map(params![now, after.map(Id::as_str), n], |row| {
                        row.get::<_, String>(0)
                    }))?
                .collect::<rusqlite::Result<Vec<_>>>())?;
                let rows = ids
                    .into_iter()
                    .map(|key| {
                        pin(self.conn, &id_of(key)?).and_then(|row| {
                            row.ok_or_else(|| corrupt("expired graph pin vanished"))
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(page(rows, limit))
            }
            fn graph_pin(&self, key: &Id) -> Result<Option<StoredGraphReadPin>, StorageError> {
                pin(self.conn, key)
            }
            fn graph_source_capture(
                &self,
                key: &Id,
            ) -> Result<Option<StoredGraphSourceCapture>, StorageError> {
                capture(self.conn, key)
            }
            fn graph_incident_edges(
                &self,
                scope: &GraphScope,
                filter: &GraphIncidentFilter,
                after: Option<&GraphPosition>,
                limit: u16,
            ) -> Result<Option<GraphPage<GraphEdge>>, StorageError> {
                incident(self.conn, scope, filter, after, limit)
            }
            fn graph_candidate_edges(
                &self,
                scope: &GraphScope,
                filter: &GraphCandidateFilter,
                after: Option<&GraphPosition>,
                limit: u16,
            ) -> Result<Option<GraphPage<GraphEdge>>, StorageError> {
                candidates(self.conn, scope, filter, after, limit)
            }
            fn graph_unlinked_views(
                &self,
                after: Option<&Id>,
                limit: u16,
            ) -> Result<GraphPage<StoredView>, StorageError> {
                let (n, _) = bounded(limit, None)?;
                let mut statement = db(self.conn.prepare_cached(
                    "SELECT v.id FROM index_views v WHERE (v.branch_id IS NOT NULL OR \
            v.worktree_id IS NOT NULL) AND NOT EXISTS(SELECT 1 FROM \
            graph_view_link_sets s WHERE s.view_id=v.id) AND (?1 IS NULL OR \
            v.id>?1) ORDER BY v.id LIMIT ?2",
                ))?;
                let rows = db(db(statement
                    .query_map(params![after.map(Id::as_str), n], |r| r.get::<_, String>(0)))?
                .collect::<rusqlite::Result<Vec<_>>>())?;
                let views = rows
                    .into_iter()
                    .map(|v| {
                        IndexRead::view(self, &id_of(v)?)
                            .and_then(|v| v.ok_or_else(|| corrupt("unlinked graph view vanished")))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(page(views, limit))
            }
        }
    };
}
graph_reads!(Snapshot<'_>);
graph_reads!(Unit<'_>);

fn trust(
    conn: &Connection,
    scope: &GraphScope,
    key: &GraphNodeIdentity,
    content: &str,
) -> Result<Option<StoredGraphTrust>, StorageError> {
    scope_json(conn, scope)?;
    let binding = scope
        .views
        .iter()
        .find(|v| v.repository_id == key.repository_id)
        .ok_or(StorageError::ScopeDenied)?;
    let raw: Option<(i64, bool, String, i64)> = db(conn
        .query_row(
            "SELECT revision,trusted,updated_by,updated_at FROM graph_trust WHERE \
        repository_id=?1 AND unit_key=?2 AND node_id=?3 AND view_digest=?4 AND \
        content_digest=?5",
            params![
                key.repository_id.as_str(),
                key.unit_key,
                key.node_id,
                binding.view_digest,
                content
            ],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional())?;
    raw.map(|(revision, trusted, actor, updated)| {
        Ok(StoredGraphTrust {
            identity: key.clone(),
            view_id: binding.view_id.clone(),
            view_digest: binding.view_digest.clone(),
            content_digest: content.into(),
            revision,
            trusted,
            updated_by: devworks_core::protocol::ActorId(id_of(actor)?),
            updated_at_ms: updated,
        })
    })
    .transpose()
}
type PinRaw = (
    String,
    String,
    i64,
    String,
    String,
    String,
    String,
    String,
    i64,
    i64,
    i64,
);
pub(super) fn pin(conn: &Connection, key: &Id) -> Result<Option<StoredGraphReadPin>, StorageError> {
    let raw: Option<PinRaw> = db(conn
        .query_row(
            "SELECT owner_namespace,store_id,restore_epoch,scope_digest,\
        intent_digest,scope_json,witness_json,missing_json,revision,created_at,\
        expires_at FROM graph_read_pins WHERE id=?1",
            [key.as_str()],
            |r| {
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
            },
        )
        .optional())?;
    raw.map(|r| {
        let scope: GraphScope =
            serde_json::from_str(&r.5).map_err(|_| corrupt("invalid stored graph pin scope"))?;
        if scope.views.is_empty()
            || scope.views.len() > GRAPH_MAX_VIEWS
            || scope
                .views
                .iter()
                .map(|v| v.repository_id.as_str())
                .collect::<BTreeSet<_>>()
                .len()
                != scope.views.len()
        {
            return Err(corrupt("invalid stored graph pin scope"));
        }
        let unavailable: Vec<GraphUnavailableRepository> = serde_json::from_str(&r.7)
            .map_err(|_| corrupt("invalid graph unavailable repositories"))?;
        validate_unavailable(&scope, &unavailable)
            .map_err(|_| corrupt("invalid graph unavailable repository scope"))?;
        let count: i64 = db(conn.query_row(
            "SELECT count(*) FROM graph_read_pin_views v JOIN index_pins p ON \
            p.id=v.index_pin_id JOIN json_each(?3,'$.views') j ON \
            v.repository_id=json_extract(j.value,'$.repository_id') AND \
            v.view_id=json_extract(j.value,'$.view_id') AND \
            v.view_digest=json_extract(j.value,'$.view_digest') WHERE v.pin_id=?1 \
            AND p.view_id=v.view_id AND p.lease_until=?2",
            params![key.as_str(), r.10, r.5],
            |row| row.get(0),
        ))?;
        if count != scope.views.len() as i64 {
            return Err(corrupt("graph pin lost a child lease"));
        }
        Ok(StoredGraphReadPin {
            id: key.clone(),
            owner_namespace: r.0,
            store_id: devworks_core::protocol::StoreId(id_of(r.1)?),
            restore_epoch: r.2,
            scope_digest: r.3,
            intent_digest: r.4,
            scope,
            view_witnesses: serde_json::from_str(&r.6)
                .map_err(|_| corrupt("invalid graph pin witnesses"))?,
            unavailable_repositories: unavailable,
            revision: r.8,
            created_at_ms: r.9,
            expires_at_ms: r.10,
        })
    })
    .transpose()
}
fn capture(conn: &Connection, key: &Id) -> Result<Option<StoredGraphSourceCapture>, StorageError> {
    let raw: Option<(String, String, String, String, String, i64, i64)> = db(conn
        .query_row(
            "SELECT repository_id,unit_key,source_digest,blob_digest,blob_generation,\
        byte_count,created_at FROM graph_source_captures WHERE id=?1",
            [key.as_str()],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .optional())?;
    raw.map(|r| {
        Ok(StoredGraphSourceCapture {
            id: key.clone(),
            repository_id: id_of(r.0)?,
            unit_key: r.1,
            source_digest: r.2,
            blob_digest: r.3,
            blob_generation: r.4,
            byte_count: r.5,
            created_at_ms: r.6,
        })
    })
    .transpose()
}
impl GraphRepository for Unit<'_> {
    fn put_graph_trust(
        &mut self,
        scope: &GraphScope,
        record: &StoredGraphTrust,
        expected: i64,
    ) -> Result<(), StorageError> {
        let current = self
            .graph_node(scope, &record.identity)?
            .ok_or_else(|| invalid("graph trust target is not visible"))?;
        let binding = scope
            .views
            .iter()
            .find(|v| v.repository_id == record.identity.repository_id)
            .ok_or(StorageError::ScopeDenied)?;
        if current.content_digest != record.content_digest
            || binding.view_id != record.view_id
            || binding.view_digest != record.view_digest
            || record.content_digest.is_empty()
            || record.updated_at_ms < 0
            || record.updated_at_ms > self.now
            || record.revision
                != expected
                    .checked_add(1)
                    .ok_or_else(|| invalid("graph trust revision overflow"))?
        {
            return Err(invalid(
                "graph trust must bind exact visible content and view",
            ));
        }
        let prior = self
            .graph_trust(scope, &record.identity, &record.content_digest)?
            .map_or(1, |v| v.revision);
        if prior != expected {
            return Err(conflict(
                RevisionDimension::Source,
                "graph_trust",
                record.view_id.as_str(),
                expected,
                prior,
            ));
        }
        db(self.conn.execute(
            "INSERT INTO graph_trust(repository_id,unit_key,node_id,view_id,\
            view_digest,content_digest,revision,trusted,updated_by,updated_at) \
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT(repository_id,\
            unit_key,node_id,view_digest,content_digest) DO UPDATE SET \
            revision=excluded.revision,trusted=excluded.trusted,\
            updated_by=excluded.updated_by,updated_at=excluded.updated_at",
            params![
                record.identity.repository_id.as_str(),
                record.identity.unit_key,
                record.identity.node_id,
                record.view_id.as_str(),
                record.view_digest,
                record.content_digest,
                record.revision,
                record.trusted,
                record.updated_by.0.as_str(),
                record.updated_at_ms
            ],
        ))?;
        db(self.conn.execute(
            "INSERT INTO graph_trust_changes(repository_id,view_digest) VALUES(?1,?2)",
            params![record.identity.repository_id.as_str(), record.view_digest],
        ))?;
        Ok(())
    }
    fn acquire_graph_pin(&mut self, record: &StoredGraphReadPin) -> Result<(), StorageError> {
        let scope = scope_json(self.conn, &record.scope)?;
        validate_unavailable(&record.scope, &record.unavailable_repositories)?;
        for missing in &record.unavailable_repositories {
            let registered: bool = db(self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM repositories WHERE id=?1)",
                [missing.repository_id.as_str()],
                |r| r.get(0),
            ))?;
            if !registered {
                return Err(StorageError::ScopeDenied);
            }
        }
        if record.owner_namespace.is_empty()
            || record.owner_namespace.len() > 512
            || record.owner_namespace.contains('\0')
            || record.store_id != self.info.store_id
            || record.restore_epoch != self.info.restore_epoch
            || !digest(&record.scope_digest)
            || !digest(&record.intent_digest)
            || record.revision != 1
            || record.created_at_ms < 0
            || record.created_at_ms > self.now
            || record.expires_at_ms <= self.now
            || record.expires_at_ms.saturating_sub(self.now) > GRAPH_PIN_TTL_MS
        {
            return Err(invalid("invalid graph read pin binding or lease"));
        }
        if let Some(existing) = self.graph_pin(&record.id)? {
            return if existing == *record {
                Ok(())
            } else {
                Err(StorageError::Duplicate {
                    kind: "graph_read_pin",
                    id: record.id.as_str().into(),
                })
            };
        }
        if record.view_witnesses.len() != record.scope.views.len() {
            return Err(invalid("graph pin needs one source witness per view"));
        }
        let mut witnessed = BTreeSet::new();
        for witness in &record.view_witnesses {
            if !witnessed.insert(witness.view_id.as_str()) {
                return Err(invalid("duplicate graph view witness"));
            }
            validate_witness(
                self.conn,
                &record.scope,
                witness,
                self.now,
                record.expires_at_ms,
            )?;
        }
        let boot = self
            .info
            .boot_id
            .as_ref()
            .ok_or_else(|| invalid("graph pin requires a native boot"))?;
        db(self.conn.execute(
            "INSERT INTO graph_read_pins(id,owner_namespace,store_id,restore_epoch,\
            scope_digest,intent_digest,scope_json,witness_json,missing_json,\
            revision,created_at,expires_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,1,?10,\
            ?11)",
            params![
                record.id.as_str(),
                record.owner_namespace,
                record.store_id.0.as_str(),
                record.restore_epoch,
                record.scope_digest,
                record.intent_digest,
                scope,
                serde_json::to_string(&record.view_witnesses)
                    .map_err(|_| invalid("graph witness encoding"))?,
                serde_json::to_string(&record.unavailable_repositories)
                    .map_err(|_| invalid("graph unavailable encoding"))?,
                record.created_at_ms,
                record.expires_at_ms
            ],
        ))?;
        for view in &record.scope.views {
            let child = format!(
                "graphlease_{}",
                devworks_core::fingerprint::sha256_hex(
                    json!([record.id, view.repository_id, view.view_id])
                        .to_string()
                        .as_bytes()
                )
            );
            db(self.conn.execute(
                "INSERT INTO index_pins(id,view_id,boot_id,lease_until) VALUES(?1,?2,?3,?4)",
                params![
                    child,
                    view.view_id.as_str(),
                    boot.as_str(),
                    record.expires_at_ms
                ],
            ))?;
            db(self.conn.execute(
                "INSERT INTO graph_read_pin_views(pin_id,repository_id,view_id,\
                view_digest,index_pin_id) VALUES(?1,?2,?3,?4,?5)",
                params![
                    record.id.as_str(),
                    view.repository_id.as_str(),
                    view.view_id.as_str(),
                    view.view_digest,
                    child
                ],
            ))?;
        }
        Ok(())
    }
    fn renew_graph_pin(
        &mut self,
        key: &Id,
        owner: &str,
        expected: i64,
        expiry: i64,
    ) -> Result<StoredGraphReadPin, StorageError> {
        let mut record = self
            .graph_pin(key)?
            .ok_or_else(|| invalid("graph pin unavailable"))?;
        if record.owner_namespace != owner
            || record.store_id != self.info.store_id
            || record.restore_epoch != self.info.restore_epoch
        {
            return Err(StorageError::ScopeDenied);
        }
        if record.revision != expected {
            return Err(conflict(
                RevisionDimension::Source,
                "graph_read_pin",
                key.as_str(),
                expected,
                record.revision,
            ));
        }
        if record.expires_at_ms <= self.now
            || record
                .view_witnesses
                .iter()
                .any(|witness| witness.expires_at_ms.is_some_and(|until| expiry > until))
            || expiry <= record.expires_at_ms
            || expiry.saturating_sub(self.now) > GRAPH_PIN_TTL_MS
        {
            return Err(invalid("expired or invalid graph pin renewal"));
        }
        scope_json(self.conn, &record.scope)?;
        record.revision = record
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("graph pin revision overflow"))?;
        record.expires_at_ms = expiry;
        db(self.conn.execute(
            "UPDATE graph_read_pins SET revision=?2,expires_at=?3 WHERE id=?1",
            params![key.as_str(), record.revision, expiry],
        ))?;
        db(self.conn.execute(
            "UPDATE index_pins SET lease_until=?2 WHERE id IN(SELECT index_pin_id \
            FROM graph_read_pin_views WHERE pin_id=?1)",
            params![key.as_str(), expiry],
        ))?;
        Ok(record)
    }
    fn retire_graph_pin(
        &mut self,
        key: &Id,
        owner: &str,
        expected: i64,
    ) -> Result<(), StorageError> {
        let Some(record) = self.graph_pin(key)? else {
            return Ok(());
        };
        if record.owner_namespace != owner {
            return Err(StorageError::ScopeDenied);
        }
        if record.revision != expected {
            return Err(conflict(
                RevisionDimension::Source,
                "graph_read_pin",
                key.as_str(),
                expected,
                record.revision,
            ));
        }
        db(self
            .conn
            .execute("DELETE FROM graph_read_pins WHERE id=?1", [key.as_str()]))?;
        Ok(())
    }
    fn invalidate_graph_pins_for_view(&mut self, view: &Id) -> Result<(), StorageError> {
        db(self.conn.execute(
            "DELETE FROM graph_read_pins WHERE id IN(SELECT pin_id FROM \
            graph_read_pin_views WHERE view_id=?1)",
            [view.as_str()],
        ))?;
        Ok(())
    }
    fn insert_graph_source_capture(
        &mut self,
        record: &StoredGraphSourceCapture,
    ) -> Result<(), StorageError> {
        if record.unit_key.is_empty()
            || record.unit_key.len() > 4096
            || record.unit_key.contains('\0')
            || record.source_digest.is_empty()
            || record.source_digest.contains('\0')
            || !digest(&record.blob_digest)
            || record.byte_count < 0
            || record.created_at_ms < 0
            || record.created_at_ms > self.now
        {
            return Err(invalid("invalid private graph source capture"));
        }
        let valid: bool = db(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM blobs WHERE digest=?1 AND generation=?2 AND \
            byte_count=?3 AND state='Present')",
            params![
                record.blob_digest,
                record.blob_generation,
                record.byte_count
            ],
            |r| r.get(0),
        ))?;
        if !valid {
            return Err(invalid("graph source needs exact present blob generation"));
        }
        if let Some(existing) = self.graph_source_capture(&record.id)? {
            return if existing == *record {
                Ok(())
            } else {
                Err(StorageError::Duplicate {
                    kind: "graph_source_capture",
                    id: record.id.as_str().into(),
                })
            };
        }
        db(self.conn.execute(
            "INSERT INTO graph_source_captures(id,repository_id,unit_key,\
            source_digest,blob_digest,blob_generation,byte_count,created_at) \
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                record.id.as_str(),
                record.repository_id.as_str(),
                record.unit_key,
                record.source_digest,
                record.blob_digest,
                record.blob_generation,
                record.byte_count,
                record.created_at_ms
            ],
        ))?;
        Ok(())
    }
}

/// A reclamation write drops the whole expired lease, including its other repositories.
pub(super) fn release_expired_pins_for_view(
    conn: &Connection,
    view: &Id,
    now: i64,
) -> Result<(), StorageError> {
    db(conn.execute(
        "DELETE FROM graph_read_pins WHERE expires_at<=?2 AND id IN(SELECT \
        pin_id FROM graph_read_pin_views WHERE view_id=?1)",
        params![view.as_str(), now],
    ))?;
    Ok(())
}

fn validate_witness(
    conn: &Connection,
    scope: &GraphScope,
    w: &GraphViewWitness,
    now: i64,
    lease_until: i64,
) -> Result<(), StorageError> {
    let member = scope
        .views
        .iter()
        .find(|v| v.view_id == w.view_id)
        .ok_or_else(|| invalid("graph witness outside selected views"))?;
    if w.source_revision <= 0 {
        return Err(invalid(
            "graph source revision must be observed and positive",
        ));
    }
    let valid: bool = match (&w.overlay_id, w.expires_at_ms) {
        (Some(overlay), Some(expiry)) if expiry > now && lease_until <= expiry => db(conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM index_overlays o WHERE o.id=?1 AND \
            o.repository_id=?2 AND o.view_id=?3 AND o.revision=?4 AND \
            o.expires_at=?5 AND o.state IN('Ready','Stale')) OR EXISTS(SELECT 1 \
            FROM index_views v WHERE v.id=?3 AND json_extract(v.coverage_json,\
            '$.sourceRevision')=?4 AND json_extract(v.coverage_json,\
            '$.overlayId')=?1 AND json_extract(v.coverage_json,'$.expiresAtMs')=?5)",
                params![
                    overlay.as_str(),
                    member.repository_id.as_str(),
                    member.view_id.as_str(),
                    w.source_revision,
                    expiry
                ],
                |r| r.get(0),
            ))?,
        (None, None) => db(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM published_views p WHERE p.source_key=?1 AND \
            p.view_id=?2 AND p.revision=?3) OR EXISTS(SELECT 1 FROM index_views v \
            WHERE v.id=?2 AND v.branch_id IS NULL AND v.worktree_id IS NULL AND \
            json_extract(v.coverage_json,'$.head') IS NULL AND \
            json_extract(v.coverage_json,'$.overlayId') IS NULL AND \
            json_extract(v.coverage_json,'$.sourceRevision')=?3) OR EXISTS(SELECT 1 \
            FROM index_views v JOIN index_generations g ON g.id=v.baseline_id JOIN \
            index_runs r ON r.id=json_extract(g.manifest_json,'$.runId') WHERE \
            v.id=?2 AND v.source_key=?1 AND v.branch_id IS NULL AND v.worktree_id \
            IS NULL AND json_extract(v.coverage_json,'$.head') IS NULL AND \
            json_extract(v.coverage_json,'$.overlayId') IS NULL AND \
            g.state='Sealed' AND json_extract(g.manifest_json,\
            '$.parentPointerRevision')+1=?3 AND r.repository_id=?1 AND \
            r.output_view_id=v.id AND r.state='Succeeded')",
            params![
                member.repository_id.as_str(),
                member.view_id.as_str(),
                w.source_revision
            ],
            |r| r.get(0),
        ))?,
        _ => false,
    };
    if !valid {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
}

fn validate_unavailable(
    scope: &GraphScope,
    missing: &[GraphUnavailableRepository],
) -> Result<(), StorageError> {
    if scope.views.len() + missing.len() > GRAPH_MAX_VIEWS {
        return Err(invalid("graph selected repository limit exceeded"));
    }
    let mut seen = scope
        .views
        .iter()
        .map(|v| v.repository_id.as_str())
        .collect::<BTreeSet<_>>();
    for row in missing {
        if !seen.insert(row.repository_id.as_str())
            || row.reason.trim().is_empty()
            || row.reason.chars().count() > 256
            || row.reason.contains('\0')
        {
            return Err(invalid(
                "graph unavailable repositories must be unique, disjoint and qualified",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::migrations::MIGRATIONS;
    use super::*;

    /// A store with every migration and one generation `g` of `callers` functions in each of
    /// seven units calling that unit's leaf, as the large review fixture is shaped.
    fn calling_generation(callers: usize) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        super::super::project_purge::guard::register(&conn, false).unwrap();
        super::register_name_match(&conn).unwrap();
        for migration in MIGRATIONS {
            conn.execute_batch(migration.sql).unwrap();
        }
        conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        for unit in 0..7 {
            let key = format!("src/unit_{unit}.rs");
            conn.execute(
                "INSERT INTO source_units(generation_id,unit_key,content_digest,action,\
                    payload_json) VALUES('g',?1,'d','Replace','{}')",
                [&key],
            )
            .unwrap();
            let leaf = format!("leaf-{unit}");
            conn.execute(
                "INSERT INTO graph_nodes(generation_id,unit_key,node_id,kind,name,\
                    payload_json) VALUES('g',?1,?2,'Function',?2,'{}')",
                [&key, &leaf],
            )
            .unwrap();
            for caller in 0..callers {
                let source = format!("caller-{unit}-{caller}");
                conn.execute(
                    "INSERT INTO graph_nodes(generation_id,unit_key,node_id,kind,name,\
                        payload_json) VALUES('g',?1,?2,'Function',?2,'{}')",
                    [&key, &source],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO graph_edges(generation_id,unit_key,edge_id,source_node_id,\
                        target_node_id,kind,payload_json) VALUES('g',?1,?2,?2,?3,'CALLS',\
                        json_object('targetUnit',?1,'resolution','Inferred'))",
                    [&key, &source, &leaf],
                )
                .unwrap();
            }
        }
        conn
    }

    /// The SQLite steps the unresolved count of a view's insertion takes, and its value.
    fn unresolved_steps(conn: &Connection) -> (i64, i32) {
        let mut statement = conn.prepare(UNRESOLVED_EDGES).unwrap();
        let count: i64 = statement.query_row(["g"], |r| r.get(0)).unwrap();
        (
            count,
            statement.get_status(rusqlite::StatementStatus::VmStep),
        )
    }

    #[test]
    fn a_view_insertion_counts_unresolved_edges_in_store_work_linear_in_its_edges() {
        // Each edge's endpoints are looked up by node ID: never a scan of the layer's nodes,
        // whose work would grow with the edges times the nodes.
        let conn = calling_generation(1);
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {UNRESOLVED_EDGES}"))
            .unwrap()
            .query_map(["g"], |r| r.get::<_, String>(3))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let text = plan.join("\n");
        assert_eq!(
            text.matches("graph_nodes_by_id (generation_id=? AND node_id=?)")
                .count(),
            2,
            "{text}"
        );
        assert!(!text.contains("SCAN"), "{text}");
        // The same bound per edge at two sizes, tenfold apart.
        for callers in [300, 3_000] {
            let conn = calling_generation(callers);
            let (count, steps) = unresolved_steps(&conn);
            let edges = 7 * callers as i64;
            assert_eq!(count, 0);
            eprintln!("{edges} edges: {steps} SQLite steps");
            assert!(
                i64::from(steps) <= 200 * edges,
                "{steps} SQLite steps for {edges} edges"
            );
        }
    }

    #[test]
    fn migration_61_records_the_unresolved_count_insert_view_records() {
        let migration = include_str!("migrations/0061_graph_view_unresolved.sql");
        assert!(migration.contains(&UNRESOLVED_EDGES.replace("?1", "v.baseline_id")));
    }

    #[test]
    fn node_lookups_search_node_indexes_and_rank_no_unit_table() {
        let conn = Connection::open_in_memory().unwrap();
        super::super::project_purge::guard::register(&conn, false).unwrap();
        super::register_name_match(&conn).unwrap();
        for migration in MIGRATIONS {
            conn.execute_batch(migration.sql).unwrap();
        }
        let by_id = node_query(
            &node_id_visible(),
            "n.node_id=json_extract(?2,'$.node')",
            "1",
            "v.repo,n.unit_key,n.node_id",
        );
        for (name, sql, index) in [
            (
                "node id",
                by_id,
                "graph_nodes_by_id (generation_id=? AND node_id=?)",
            ),
            (
                "kind",
                kind_nodes_sql(),
                "graph_node_name (generation_id=? AND kind=?)",
            ),
            (
                "project",
                project_nodes_sql(),
                "graph_nodes_project_directory (generation_id=? AND <expr>=?)",
            ),
            (
                "repository edges",
                repository_edges_sql(),
                "graph_edges_named_repository (generation_id=?)",
            ),
        ] {
            let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let plan: Vec<String> = statement
                .query_map(params!["{}", "{}", "null", 1], |r| r.get::<_, String>(3))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            let text = plan.join("\n");
            assert!(text.contains(&format!("INDEX {index}")), "{name}:\n{text}");
            for line in &plan {
                // Only the request's own JSON lists and the selected views are scanned.
                for table in ["graph_nodes", "source_units", "index_views", "graph_trust"] {
                    assert!(
                        !(line.starts_with("SCAN") && line.contains(table)),
                        "{name} scans {table}:\n{text}"
                    );
                }
            }
        }
    }

    #[test]
    fn incident_edge_reads_search_endpoint_indexes_and_scan_no_edge_or_unit_table() {
        let conn = Connection::open_in_memory().unwrap();
        super::super::project_purge::guard::register(&conn, false).unwrap();
        super::register_name_match(&conn).unwrap();
        for migration in MIGRATIONS {
            conn.execute_batch(migration.sql).unwrap();
        }
        let reverse = "graph_reverse (generation_id=? AND target_node_id=?)";
        let forward = "graph_forward (generation_id=? AND source_node_id=?)";
        let linked_target = Some("graph_view_links_target (view_id=? AND target_node_id=?)");
        let linked_source = Some("graph_view_links_source (view_id=? AND source_node_id=?)");
        let qualified = "graph_edges_target_qualified (generation_id=? AND <expr>=?)";
        for (end, probe, stored, linked) in [
            (GraphEdgeEnd::Target, Probe::Exact, reverse, linked_target),
            (GraphEdgeEnd::Source, Probe::Exact, forward, linked_source),
            (GraphEdgeEnd::Target, Probe::NodeId, reverse, linked_target),
            (GraphEdgeEnd::Source, Probe::NodeId, forward, linked_source),
            (GraphEdgeEnd::Target, Probe::Qualified, qualified, None),
        ] {
            let mut statement = conn
                .prepare(&format!("EXPLAIN QUERY PLAN {}", incident_sql(end, probe)))
                .unwrap();
            let plan: Vec<String> = statement
                .query_map(params!["{}", "{}", "null", 1], |r| r.get::<_, String>(3))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            let text = plan.join("\n");
            assert!(
                text.contains(&format!("INDEX {stored}")),
                "{end:?} {probe:?}:\n{text}"
            );
            if let Some(linked) = linked {
                assert!(
                    text.contains(&format!("INDEX {linked}")),
                    "{end:?} {probe:?}:\n{text}"
                );
            }
            for line in &plan {
                // Only the request's own JSON lists and the selected views are scanned.
                for table in [
                    "graph_edges",
                    "graph_view_links",
                    "source_units",
                    "index_views",
                ] {
                    assert!(
                        !(line.starts_with("SCAN") && line.contains(table)),
                        "{end:?} {probe:?} scans {table}:\n{text}"
                    );
                }
            }
        }
    }
}
