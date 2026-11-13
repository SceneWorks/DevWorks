//! Bounded reads of immutable, explicitly selected composed graph views.
use super::{
    conn::db,
    repos::{Snapshot, Unit},
};
use devworks_core::ports::storage::*;
use devworks_core::protocol::Id;
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

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
fn corrupt(message: &str) -> StorageError {
    StorageError::unavailable(Unavailable::Corrupt, message)
}
fn id(value: String) -> Result<Id, StorageError> {
    Id::new(value).map_err(|_| corrupt("invalid graph identifier"))
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
        let valid: bool = db(conn.query_row("SELECT EXISTS(SELECT 1 FROM index_views v WHERE v.id=?1 AND v.source_key=?2 AND v.view_digest=?3 AND NOT EXISTS(SELECT 1 FROM index_generations g WHERE g.id IN(v.baseline_id,v.branch_id,v.worktree_id) AND (g.state!='Sealed' OR g.source_key!=v.source_key)))",params![binding.view_id.as_str(),binding.repository_id.as_str(),binding.view_digest],|row|row.get(0)))?;
        if !valid {
            return Err(StorageError::ScopeDenied);
        }
    }
    serde_json::to_string(scope).map_err(|_| invalid("invalid graph scope"))
}
// Highest-layer whole-unit masking precedes ALL row filters and adjacency. Deleted units do
// not expose lower rows. Repository identities remain part of every partition and endpoint.
const COMPOSED: &str = "WITH selected AS (SELECT json_extract(j.value,'$.repository_id') repo,v.id view_id,v.baseline_id,v.branch_id,v.worktree_id FROM json_each(?1,'$.views') j JOIN index_views v ON v.id=json_extract(j.value,'$.view_id')), layers AS (SELECT repo,view_id,baseline_id generation_id,2 priority FROM selected UNION ALL SELECT repo,view_id,branch_id,1 FROM selected WHERE branch_id IS NOT NULL UNION ALL SELECT repo,view_id,worktree_id,0 FROM selected WHERE worktree_id IS NOT NULL), ranked AS (SELECT l.repo,l.view_id,u.*,row_number() OVER(PARTITION BY l.repo,u.unit_key ORDER BY l.priority) rn FROM layers l JOIN source_units u ON u.generation_id=l.generation_id), visible AS (SELECT * FROM ranked WHERE rn=1 AND action='Replace') ";
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
        repository_id: id(repo)?,
        view_id: id(view)?,
        generation_id: id(generation)?,
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
        "{composed}SELECT v.repo,v.view_id,v.generation_id,v.content_digest,n.unit_key,n.node_id,n.kind,n.name,n.payload_json FROM visible v JOIN graph_nodes n ON n.generation_id=v.generation_id AND n.unit_key=v.unit_key WHERE (json_extract(?2,'$.name.query') IS NULL OR devworks_graph_name_matches(n.name,CASE WHEN json_type(n.payload_json,'$.qualifiedName')='text' THEN json_extract(n.payload_json,'$.qualifiedName') END,json_extract(?2,'$.name.query'),json_extract(?2,'$.name.contains'))) AND (json_extract(?2,'$.pattern') IS NULL OR n.name LIKE json_extract(?2,'$.pattern') OR json_extract(n.payload_json,'$.qualifiedName') LIKE json_extract(?2,'$.pattern')) AND (json_array_length(?2,'$.kinds')=0 OR n.kind IN(SELECT value FROM json_each(?2,'$.kinds'))) AND (json_extract(?2,'$.unit') IS NULL OR n.unit_key=json_extract(?2,'$.unit')) AND {node_predicate} AND (NOT json_extract(?2,'$.excludeUntrusted') OR NOT EXISTS(SELECT 1 FROM graph_trust t JOIN index_views iv ON iv.id=v.view_id WHERE t.repository_id=v.repo AND t.unit_key=n.unit_key AND t.node_id=n.node_id AND t.view_digest=iv.view_digest AND t.content_digest=v.content_digest AND t.trusted=0)) AND {cursor_predicate} ORDER BY {order} LIMIT ?4"
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
    // Unit selection commutes with the per-(repository,unit) layer ranking. Apply it
    // before the window, keeping every selected layer of that unit so Delete/Replace
    // masking is unchanged. An exact node predicate must remain indexable: an optional
    // OR otherwise scans every node in every visible unit for each endpoint lookup.
    let composed = if filter.unit_key.is_some() {
        COMPOSED.replace(
            "u.generation_id=l.generation_id)",
            "u.generation_id=l.generation_id WHERE u.unit_key=json_extract(?2,'$.unit'))",
        )
    } else {
        COMPOSED.to_owned()
    };
    let node_predicate = if filter.node_id.is_some() {
        "n.node_id=json_extract(?2,'$.node')"
    } else {
        "1"
    };
    let filter=json!({"pattern":filter.pattern,"name":filter.name.as_ref().map(|n| json!({"query":n.name,"contains":n.contains})),"kinds":filter.kinds,"unit":filter.unit_key,"node":filter.node_id,"excludeUntrusted":filter.exclude_untrusted}).to_string();
    let sql = node_query(
        &composed,
        node_predicate,
        "(json_extract(?3,'$.repository_id') IS NULL OR (v.repo,n.unit_key,n.node_id)>(json_extract(?3,'$.repository_id'),json_extract(?3,'$.unit_key'),json_extract(?3,'$.item_id')))",
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
    let filter_json=json!({"pattern":filter.pattern,"name":filter.name.as_ref().map(|n|json!({"query":n.name,"contains":n.contains})),"kinds":filter.kinds,"unit":filter.unit_key,"node":filter.node_id,"excludeUntrusted":filter.exclude_untrusted}).to_string();
    let units_sql = format!(
        "{COMPOSED}SELECT v.repo,v.view_id,v.generation_id,v.content_digest,v.unit_key FROM visible v WHERE (json_extract(?2,'$.unit') IS NULL OR v.unit_key=json_extract(?2,'$.unit')) AND (json_extract(?3,'$.repository_id') IS NULL OR (v.repo,v.unit_key)>=(json_extract(?3,'$.repository_id'),json_extract(?3,'$.unit_key'))) ORDER BY v.repo,v.unit_key"
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
        let header = "WITH visible AS (SELECT json_extract(?1,'$.repo') repo,json_extract(?1,'$.view') view_id,json_extract(?1,'$.generation') generation_id,json_extract(?1,'$.content') content_digest,json_extract(?1,'$.unit') unit_key) ";
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
        "{COMPOSED}SELECT v.repo,v.view_id,v.generation_id,v.content_digest,e.unit_key,e.edge_id,e.source_node_id,e.target_node_id,e.kind,e.payload_json FROM visible v JOIN graph_edges e ON e.generation_id=v.generation_id AND e.unit_key=v.unit_key WHERE (json_extract(?2,'$.unit') IS NULL OR e.unit_key=json_extract(?2,'$.unit')) AND (json_array_length(?2,'$.kinds')=0 OR e.kind IN(SELECT value FROM json_each(?2,'$.kinds'))) AND (json_array_length(?2,'$.sources')=0 OR EXISTS(SELECT 1 FROM json_each(?2,'$.sources') s WHERE json_extract(s.value,'$.repository_id')=coalesce(json_extract(e.payload_json,'$.sourceRepositoryId'),v.repo) AND json_extract(s.value,'$.unit_key')=coalesce(json_extract(e.payload_json,'$.sourceUnit'),e.unit_key) AND json_extract(s.value,'$.node_id')=e.source_node_id)) AND (json_array_length(?2,'$.targets')=0 OR EXISTS(SELECT 1 FROM json_each(?2,'$.targets') t WHERE json_extract(t.value,'$.repository_id')=coalesce(json_extract(e.payload_json,'$.targetRepositoryId'),v.repo) AND json_extract(t.value,'$.unit_key')=json_extract(e.payload_json,'$.targetUnit') AND json_extract(t.value,'$.node_id')=e.target_node_id)) AND (json_extract(?3,'$.repository_id') IS NULL OR (v.repo,e.unit_key,e.edge_id)>(json_extract(?3,'$.repository_id'),json_extract(?3,'$.unit_key'),json_extract(?3,'$.item_id'))) ORDER BY v.repo,e.unit_key,e.edge_id LIMIT ?4"
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
                repository_id: id(r.0)?,
                view_id: id(r.1)?,
                generation_id: id(r.2)?,
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
macro_rules! graph_reads {($t:ty)=>{impl GraphRead for $t {
    fn graph_units(&self,scope:&GraphScope,after:Option<&GraphPosition>,limit:u16)->Result<GraphPage<GraphUnit>,StorageError>{
        let scope=scope_json(self.conn,scope)?;let (n,after)=bounded(limit,after)?;
        let sql=format!("{COMPOSED}SELECT repo,view_id,generation_id,unit_key,content_digest,action,payload_json FROM visible WHERE json_extract(?2,'$.repository_id') IS NULL OR (repo,unit_key)>(json_extract(?2,'$.repository_id'),json_extract(?2,'$.unit_key')) ORDER BY repo,unit_key LIMIT ?3");
        let mut statement=db(self.conn.prepare_cached(&sql))?;let rows=db(db(statement.query_map(params![scope,after,n],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?))))?.collect::<rusqlite::Result<Vec<_>>>())?;
        Ok(page(rows.into_iter().map(|r|Ok(GraphUnit{repository_id:id(r.0)?,view_id:id(r.1)?,unit:StoredUnit{generation_id:id(r.2)?,unit_key:r.3,content_digest:r.4,action:r.5,payload:value(r.6)?}})).collect::<Result<Vec<_>,StorageError>>()?,limit))
    }
    fn graph_nodes(&self,scope:&GraphScope,filter:&GraphNodeFilter,after:Option<&GraphPosition>,limit:u16)->Result<GraphPage<GraphNode>,StorageError>{nodes(self.conn,scope,filter,after,limit)}
    fn graph_node(&self,scope:&GraphScope,key:&GraphNodeIdentity)->Result<Option<GraphNode>,StorageError>{
        let scoped=GraphScope{views:scope.views.iter().filter(|v|v.repository_id==key.repository_id).cloned().collect()};
        scope_json(self.conn,scope)?;
        if scoped.views.is_empty(){return Err(StorageError::ScopeDenied);}
        Ok(nodes(self.conn,&scoped,&GraphNodeFilter{unit_key:Some(key.unit_key.clone()),node_id:Some(key.node_id.clone()),..Default::default()},None,1)?.items.pop())
    }
    fn graph_edges(&self,scope:&GraphScope,filter:&GraphEdgeFilter,after:Option<&GraphPosition>,limit:u16)->Result<GraphPage<GraphEdge>,StorageError>{edges(self.conn,scope,filter,after,limit)}
    fn graph_counts(&self,scope:&GraphScope)->Result<GraphCounts,StorageError>{
        let scope=scope_json(self.conn,scope)?;
        let sql=format!("{COMPOSED}SELECT (SELECT count(*) FROM visible),(SELECT count(*) FROM visible v JOIN graph_nodes n ON n.generation_id=v.generation_id AND n.unit_key=v.unit_key),(SELECT count(*) FROM visible v JOIN graph_edges e ON e.generation_id=v.generation_id AND e.unit_key=v.unit_key)");
        let (units,nodes,edges):(i64,i64,i64)=db(self.conn.query_row(&sql,[scope],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))))?;
        Ok(GraphCounts{units:u64::try_from(units).map_err(|_|corrupt("invalid graph count"))?,nodes:u64::try_from(nodes).map_err(|_|corrupt("invalid graph count"))?,edges:u64::try_from(edges).map_err(|_|corrupt("invalid graph count"))?})
    }
    fn graph_kinds(&self,scope:&GraphScope,domain:GraphElementKind,after:Option<&str>,limit:u16)->Result<GraphPage<GraphKindCount>,StorageError>{
        let scope=scope_json(self.conn,scope)?;let(n,_)=bounded(limit,None)?;let table=match domain{GraphElementKind::Node=>"graph_nodes",GraphElementKind::Edge=>"graph_edges"};
        let sql=format!("{COMPOSED}SELECT r.kind,count(*) FROM visible v JOIN {table} r ON r.generation_id=v.generation_id AND r.unit_key=v.unit_key WHERE (?2 IS NULL OR r.kind>?2) GROUP BY r.kind ORDER BY r.kind LIMIT ?3");
        let mut statement=db(self.conn.prepare_cached(&sql))?;let rows=db(db(statement.query_map(params![scope,after,n],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?))))?.collect::<rusqlite::Result<Vec<_>>>())?;
        Ok(page(rows.into_iter().map(|(kind,count)|Ok(GraphKindCount{kind,count:u64::try_from(count).map_err(|_|corrupt("invalid graph count"))?})).collect::<Result<Vec<_>,StorageError>>()?,limit))
    }
    fn graph_trust(&self,scope:&GraphScope,key:&GraphNodeIdentity,content:&str)->Result<Option<StoredGraphTrust>,StorageError>{trust(self.conn,scope,key,content)}
    fn graph_trust_revision(&self,scope:&GraphScope)->Result<i64,StorageError>{
        let scope=scope_json(self.conn,scope)?;
        db(self.conn.query_row("SELECT COALESCE(max(c.sequence)+1,1) FROM graph_trust_changes c JOIN json_each(?1,'$.views') j ON c.repository_id=json_extract(j.value,'$.repository_id') AND c.view_digest=json_extract(j.value,'$.view_digest')",[scope],|r|r.get(0)))
    }
    fn graph_retirable_views(&self,now:i64,after:Option<&Id>,limit:u16)->Result<GraphPage<StoredView>,StorageError>{
        let(n,_)=bounded(limit,None)?;
        let mut statement=db(self.conn.prepare_cached("SELECT v.id FROM index_views v WHERE (json_extract(v.coverage_json,'$.overlayId') IS NOT NULL OR json_extract(v.coverage_json,'$.head') IS NOT NULL) AND NOT EXISTS(SELECT 1 FROM published_views p WHERE p.view_id=v.id) AND NOT EXISTS(SELECT 1 FROM index_overlays o WHERE o.view_id=v.id) AND NOT EXISTS(SELECT 1 FROM index_pins p WHERE p.view_id=v.id AND p.lease_until>?1) AND (?2 IS NULL OR v.id>?2) ORDER BY v.id LIMIT ?3"))?;
        let rows=db(db(statement.query_map(params![now,after.map(Id::as_str),n],|r|r.get::<_,String>(0)))?.collect::<rusqlite::Result<Vec<_>>>())?;
        let views=rows.into_iter().map(|v|IndexRead::view(self,&id(v)?).and_then(|v|v.ok_or_else(||corrupt("retirable graph view vanished")))).collect::<Result<Vec<_>,_>>()?;
        Ok(page(views,limit))
    }
    fn graph_expired_pins(&self,now:i64,after:Option<&Id>,limit:u16)->Result<GraphPage<StoredGraphReadPin>,StorageError>{
        let(n,_)=bounded(limit,None)?;
        let mut statement=db(self.conn.prepare_cached("SELECT id FROM graph_read_pins WHERE expires_at<=?1 AND (?2 IS NULL OR id>?2) ORDER BY id LIMIT ?3"))?;
        let ids=db(db(statement.query_map(params![now,after.map(Id::as_str),n],|row|row.get::<_,String>(0)))?.collect::<rusqlite::Result<Vec<_>>>())?;
        let rows=ids.into_iter().map(|key|pin(self.conn,&id(key)?).and_then(|row|row.ok_or_else(||corrupt("expired graph pin vanished")))).collect::<Result<Vec<_>,_>>()?;
        Ok(page(rows,limit))
    }
    fn graph_pin(&self,key:&Id)->Result<Option<StoredGraphReadPin>,StorageError>{pin(self.conn,key)}
    fn graph_source_capture(&self,key:&Id)->Result<Option<StoredGraphSourceCapture>,StorageError>{capture(self.conn,key)}

}}}
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
    let raw:Option<(i64,bool,String,i64)>=db(conn.query_row("SELECT revision,trusted,updated_by,updated_at FROM graph_trust WHERE repository_id=?1 AND unit_key=?2 AND node_id=?3 AND view_digest=?4 AND content_digest=?5",params![key.repository_id.as_str(),key.unit_key,key.node_id,binding.view_digest,content],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional())?;
    raw.map(|(revision, trusted, actor, updated)| {
        Ok(StoredGraphTrust {
            identity: key.clone(),
            view_id: binding.view_id.clone(),
            view_digest: binding.view_digest.clone(),
            content_digest: content.into(),
            revision,
            trusted,
            updated_by: devworks_core::protocol::ActorId(id(actor)?),
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
    let raw:Option<PinRaw>=db(conn.query_row("SELECT owner_namespace,store_id,restore_epoch,scope_digest,intent_digest,scope_json,witness_json,missing_json,revision,created_at,expires_at FROM graph_read_pins WHERE id=?1",[key.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?,r.get(10)?))).optional())?;
    raw.map(|r|{
        let scope:GraphScope=serde_json::from_str(&r.5).map_err(|_|corrupt("invalid stored graph pin scope"))?;
        if scope.views.is_empty() || scope.views.len()>GRAPH_MAX_VIEWS || scope.views.iter().map(|v|v.repository_id.as_str()).collect::<BTreeSet<_>>().len()!=scope.views.len(){return Err(corrupt("invalid stored graph pin scope"));}
        let unavailable:Vec<GraphUnavailableRepository>=serde_json::from_str(&r.7).map_err(|_|corrupt("invalid graph unavailable repositories"))?;
        validate_unavailable(&scope,&unavailable).map_err(|_|corrupt("invalid graph unavailable repository scope"))?;
        let count:i64=db(conn.query_row("SELECT count(*) FROM graph_read_pin_views v JOIN index_pins p ON p.id=v.index_pin_id JOIN json_each(?3,'$.views') j ON v.repository_id=json_extract(j.value,'$.repository_id') AND v.view_id=json_extract(j.value,'$.view_id') AND v.view_digest=json_extract(j.value,'$.view_digest') WHERE v.pin_id=?1 AND p.view_id=v.view_id AND p.lease_until=?2",params![key.as_str(),r.10,r.5],|row|row.get(0)))?;
        if count!=scope.views.len() as i64{return Err(corrupt("graph pin lost a child lease"));}
        Ok(StoredGraphReadPin{id:key.clone(),owner_namespace:r.0,store_id:devworks_core::protocol::StoreId(id(r.1)?),restore_epoch:r.2,scope_digest:r.3,intent_digest:r.4,scope,view_witnesses:serde_json::from_str(&r.6).map_err(|_|corrupt("invalid graph pin witnesses"))?,unavailable_repositories:unavailable,revision:r.8,created_at_ms:r.9,expires_at_ms:r.10})
    }).transpose()
}
fn capture(conn: &Connection, key: &Id) -> Result<Option<StoredGraphSourceCapture>, StorageError> {
    let raw:Option<(String,String,String,String,String,i64,i64)>=db(conn.query_row("SELECT repository_id,unit_key,source_digest,blob_digest,blob_generation,byte_count,created_at FROM graph_source_captures WHERE id=?1",[key.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional())?;
    raw.map(|r| {
        Ok(StoredGraphSourceCapture {
            id: key.clone(),
            repository_id: id(r.0)?,
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
fn conflict(kind: &'static str, key: &Id, expected: i64, current: i64) -> StorageError {
    use devworks_core::protocol::{Revision, RevisionDimension};
    StorageError::conflict(
        RevisionDimension::Source,
        kind,
        key.clone(),
        Revision::new(expected.max(1)).expect("positive"),
        Revision::new(current.max(1)).expect("positive"),
    )
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
            return Err(conflict("graph_trust", &record.view_id, expected, prior));
        }
        db(self.conn.execute("INSERT INTO graph_trust(repository_id,unit_key,node_id,view_id,view_digest,content_digest,revision,trusted,updated_by,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT(repository_id,unit_key,node_id,view_digest,content_digest) DO UPDATE SET revision=excluded.revision,trusted=excluded.trusted,updated_by=excluded.updated_by,updated_at=excluded.updated_at",params![record.identity.repository_id.as_str(),record.identity.unit_key,record.identity.node_id,record.view_id.as_str(),record.view_digest,record.content_digest,record.revision,record.trusted,record.updated_by.0.as_str(),record.updated_at_ms]))?;
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
        db(self.conn.execute("INSERT INTO graph_read_pins(id,owner_namespace,store_id,restore_epoch,scope_digest,intent_digest,scope_json,witness_json,missing_json,revision,created_at,expires_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,1,?10,?11)",params![record.id.as_str(),record.owner_namespace,record.store_id.0.as_str(),record.restore_epoch,record.scope_digest,record.intent_digest,scope,serde_json::to_string(&record.view_witnesses).map_err(|_|invalid("graph witness encoding"))?,serde_json::to_string(&record.unavailable_repositories).map_err(|_|invalid("graph unavailable encoding"))?,record.created_at_ms,record.expires_at_ms]))?;
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
            db(self.conn.execute("INSERT INTO graph_read_pin_views(pin_id,repository_id,view_id,view_digest,index_pin_id) VALUES(?1,?2,?3,?4,?5)",params![record.id.as_str(),view.repository_id.as_str(),view.view_id.as_str(),view.view_digest,child]))?;
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
            return Err(conflict("graph_read_pin", key, expected, record.revision));
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
        db(self.conn.execute("UPDATE index_pins SET lease_until=?2 WHERE id IN(SELECT index_pin_id FROM graph_read_pin_views WHERE pin_id=?1)",params![key.as_str(),expiry]))?;
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
            return Err(conflict("graph_read_pin", key, expected, record.revision));
        }
        db(self
            .conn
            .execute("DELETE FROM graph_read_pins WHERE id=?1", [key.as_str()]))?;
        Ok(())
    }
    fn invalidate_graph_pins_for_view(&mut self, view: &Id) -> Result<(), StorageError> {
        db(self.conn.execute("DELETE FROM graph_read_pins WHERE id IN(SELECT pin_id FROM graph_read_pin_views WHERE view_id=?1)",[view.as_str()]))?;
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
        let valid:bool=db(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM blobs WHERE digest=?1 AND generation=?2 AND byte_count=?3 AND state='Present')",params![record.blob_digest,record.blob_generation,record.byte_count],|r|r.get(0)))?;
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
        db(self.conn.execute("INSERT INTO graph_source_captures(id,repository_id,unit_key,source_digest,blob_digest,blob_generation,byte_count,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![record.id.as_str(),record.repository_id.as_str(),record.unit_key,record.source_digest,record.blob_digest,record.blob_generation,record.byte_count,record.created_at_ms]))?;
        Ok(())
    }
}

/// A reclamation write drops the whole expired lease, including its other repositories.
pub(super) fn release_expired_pins_for_view(
    conn: &Connection,
    view: &Id,
    now: i64,
) -> Result<(), StorageError> {
    db(conn.execute("DELETE FROM graph_read_pins WHERE expires_at<=?2 AND id IN(SELECT pin_id FROM graph_read_pin_views WHERE view_id=?1)",params![view.as_str(),now]))?;
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
    let valid:bool=match (&w.overlay_id,w.expires_at_ms){
        (Some(overlay),Some(expiry)) if expiry>now && lease_until<=expiry=>db(conn.query_row("SELECT EXISTS(SELECT 1 FROM index_overlays o WHERE o.id=?1 AND o.repository_id=?2 AND o.view_id=?3 AND o.revision=?4 AND o.expires_at=?5 AND o.state IN('Ready','Stale')) OR EXISTS(SELECT 1 FROM index_views v WHERE v.id=?3 AND json_extract(v.coverage_json,'$.sourceRevision')=?4 AND json_extract(v.coverage_json,'$.overlayId')=?1 AND json_extract(v.coverage_json,'$.expiresAtMs')=?5)",params![overlay.as_str(),member.repository_id.as_str(),member.view_id.as_str(),w.source_revision,expiry],|r|r.get(0)))?,
        (None,None)=>db(conn.query_row("SELECT EXISTS(SELECT 1 FROM published_views p WHERE p.source_key=?1 AND p.view_id=?2 AND p.revision=?3) OR EXISTS(SELECT 1 FROM index_views v WHERE v.id=?2 AND v.branch_id IS NULL AND v.worktree_id IS NULL AND json_extract(v.coverage_json,'$.head') IS NULL AND json_extract(v.coverage_json,'$.overlayId') IS NULL AND json_extract(v.coverage_json,'$.sourceRevision')=?3) OR EXISTS(SELECT 1 FROM index_views v JOIN index_generations g ON g.id=v.baseline_id JOIN index_runs r ON r.id=json_extract(g.manifest_json,'$.runId') WHERE v.id=?2 AND v.source_key=?1 AND v.branch_id IS NULL AND v.worktree_id IS NULL AND json_extract(v.coverage_json,'$.head') IS NULL AND json_extract(v.coverage_json,'$.overlayId') IS NULL AND g.state='Sealed' AND json_extract(g.manifest_json,'$.parentPointerRevision')+1=?3 AND r.repository_id=?1 AND r.output_view_id=v.id AND r.state='Succeeded')",params![member.repository_id.as_str(),member.view_id.as_str(),w.source_revision],|r|r.get(0)))?,
        _=>false,
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
