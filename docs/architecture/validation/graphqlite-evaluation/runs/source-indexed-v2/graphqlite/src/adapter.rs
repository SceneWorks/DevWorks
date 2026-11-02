//! GraphQLite graph operations use its public bulk API and parameterized Cypher.
//! Vectors use sqlite-vec on the SAME native SQLite connection/file.
use anyhow::{Result, ensure};
use eval_common::{
    mapping::NativeMap,
    workload::{Dataset, GraphConstraint, Reader, Scope, Store},
};
use graphqlite::{Graph, PropertyValue};
use rusqlite::params;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, Once},
};
static REGISTER: Once = Once::new();
pub fn connect(path: &Path) -> Result<Graph> {
    REGISTER.call_once(|| unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    });
    let raw = rusqlite::Connection::open(path)?;
    raw.busy_timeout(std::time::Duration::from_secs(30))?;
    raw.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA cache_size=-65536;")?;
    let graph = Graph::from_connection(graphqlite::Connection::from_rusqlite(raw)?);
    // Initialize the extension schema before public bulk helpers access it.
    graph.query("MATCH (n) RETURN n LIMIT 0")?;
    Ok(graph)
}
pub fn bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|v| v.to_le_bytes()).collect()
}
pub fn open(path: &Path) -> Result<Box<dyn Store>> {
    std::fs::create_dir_all(path)?;
    let file = path.join("store.sqlite");
    let graph = connect(&file)?;
    graph.connection().sqlite_connection().execute_batch("CREATE VIRTUAL TABLE IF NOT EXISTS vectors USING vec0(embedding float[384] distance_metric=cosine,scope integer partition key)")?;
    let map = NativeMap::load(&path.join("ids.json"))?;
    Ok(Box::new(Backend {
        writer: Mutex::new(graph),
        map,
        path: path.into(),
        file,
    }))
}
struct Backend {
    writer: Mutex<Graph>,
    map: NativeMap,
    path: PathBuf,
    file: PathBuf,
}
impl Store for Backend {
    fn import(&mut self, data: &Dataset) -> Result<()> {
        let graph = self.writer.get_mut().unwrap();
        let mut ids = HashMap::new();
        for chunk in data.nodes.chunks(256) {
            let batch = chunk.iter().map(|n| {
                (
                    n.key(),
                    vec![
                        ("gid", PropertyValue::Integer(n.id as i64)),
                        ("scope", PropertyValue::Integer(n.scope().code())),
                        ("name", PropertyValue::Text(n.name.clone())),
                        ("repo", PropertyValue::Integer(n.repo as i64)),
                    ],
                    "Symbol",
                )
            });
            let created = graph.insert_nodes_bulk(batch)?;
            for n in chunk {
                self.map.insert(
                    n,
                    *created
                        .get(&n.key())
                        .ok_or_else(|| anyhow::anyhow!("Missing bulk node ID"))?
                        as u64,
                );
            }
            ids.extend(created);
            let raw = graph.connection().sqlite_connection();
            raw.execute_batch("BEGIN IMMEDIATE")?;
            {
                let mut stmt =
                    raw.prepare("INSERT INTO vectors(rowid,embedding,scope) VALUES(?,?,?)")?;
                for n in chunk {
                    stmt.execute(params![n.id as i64, bytes(&n.vector), n.scope().code()])?;
                }
            }
            raw.execute_batch("COMMIT")?;
        }
        for chunk in data.edges.chunks(256) {
            graph.insert_edges_bulk(
                chunk.iter().map(|e| {
                    (
                        format!("n{}", e.src),
                        format!("n{}", e.dst),
                        Vec::<(String, String)>::new(),
                        e.kind.clone(),
                    )
                }),
                &ids,
            )?;
        }
        self.map.save(&self.path.join("ids.json"))?;
        Ok(())
    }
    fn reader(&self) -> Result<Box<dyn Reader + '_>> {
        Ok(Box::new(Read {
            graph: connect(&self.file)?,
            map: &self.map,
        }))
    }
    fn rename(&self, id: u64, name: &str) -> Result<()> {
        let graph = self.writer.lock().unwrap();
        graph
            .query_builder("MATCH (n {id: $key}) SET n.name = $name")
            .param("key", format!("n{id}"))
            .param("name", name)
            .run()?;
        Ok(())
    }
    fn delete(&self, id: u64) -> Result<()> {
        let graph = self.writer.lock().unwrap();
        let raw = graph.connection().sqlite_connection();
        raw.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            graph.delete_node(&format!("n{id}"))?;
            raw.execute("DELETE FROM vectors WHERE rowid=?", [id as i64])?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                raw.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(e) => {
                raw.execute_batch("ROLLBACK")?;
                Err(e)
            }
        }
    }
    fn checkpoint(&self) -> Result<()> {
        self.writer
            .lock()
            .unwrap()
            .connection()
            .sqlite_connection()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        self.checkpoint()
    }
    fn details(&self) -> serde_json::Value {
        let graph = self.writer.lock().unwrap();
        let capabilities = graph
            .connection()
            .sqlite_connection()
            .query_row("SELECT cypher_capabilities()", [], |r| {
                r.get::<_, String>(0)
            })
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
        let mut result = serde_json::json!({"graphqlite":"0.12.0","rusqlite":"0.41.1","sqlite_vec":"0.1.7","wal":"FULL","batch":256,"graph_import":"Public GraphQLite native bulk API","graph_reads":"Parameterized Cypher external-ID patterns over GraphQLite schema; common per-hop scope checks","vector_mode":"Native vec0 exact KNN on same SQLite connection/file","algorithm_cache":"Not used for common adjacency/BFS; separate native cache freshness experiments","cache_kib":65536});
        result["native_capabilities"] = capabilities.unwrap_or(serde_json::Value::Null);
        result
    }
}
struct Read<'a> {
    graph: Graph,
    map: &'a NativeMap,
}
impl Reader for Read<'_> {
    fn lookup(&mut self, id: u64, scope: Scope) -> Result<Option<String>> {
        if !self.map.permitted(id, scope) {
            return Ok(None);
        }
        let rows = self
            .graph
            .query_builder("MATCH (n {id: $key}) RETURN n.name AS name")
            .param("key", format!("n{id}"))
            .run()?;
        ensure!(rows.len() <= 1, "Duplicate point result");
        rows.get(0)
            .map(|r| r.get::<String>("name").map_err(Into::into))
            .transpose()
    }
    fn neighbors(&mut self, id: u64, scope: Scope, reverse: bool) -> Result<Vec<u64>> {
        if !self.map.permitted(id, scope) {
            return Ok(vec![]);
        }
        let q = if reverse {
            "MATCH (n {id: $key})<-[]-(m) WHERE m.scope = $scope RETURN DISTINCT id(m) AS native"
        } else {
            "MATCH (n {id: $key})-[]->(m) WHERE m.scope = $scope RETURN DISTINCT id(m) AS native"
        };
        let rows = self
            .graph
            .query_builder(q)
            .param("key", format!("n{id}"))
            .param("scope", scope.code())
            .run()?;
        let ids = rows
            .into_iter()
            .map(|r| r.get::<i64>("native").map(|n| n as u64))
            .collect::<graphqlite::Result<Vec<_>>>()?;
        self.map.normalize_neighbors(ids, scope)
    }
    fn knn(
        &mut self,
        q: &[f32],
        scope: Scope,
        k: usize,
        allowed: Option<&GraphConstraint>,
    ) -> Result<Vec<u64>> {
        let restriction = allowed
            .map(|a| {
                format!(
                    " AND rowid IN ({})",
                    a.ids
                        .iter()
                        .map(u64::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                )
            })
            .unwrap_or_default();
        let sql = format!(
            "SELECT rowid FROM vectors WHERE embedding MATCH ? AND scope=? AND k=?{restriction} ORDER BY distance"
        );
        Ok(self
            .graph
            .connection()
            .sqlite_connection()
            .prepare(&sql)?
            .query_map(params![bytes(q), scope.code(), k as i64], |r| {
                r.get::<_, i64>(0).map(|n| n as u64)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
