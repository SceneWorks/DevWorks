use anyhow::{Context, Result, ensure};
use eval_common::{
    mapping::NativeMap,
    workload::{Dataset, GraphConstraint, Reader, Scope, Store},
};
use helix_ast::{
    batch,
    graph::NodeRef,
    index::VectorDistanceMetric,
    query::QueryRequest,
    traversal::g,
    value::{PropertyInput, PropertyValue},
};
use helix_core::{HelixDB, HelixDbSource};
use serde_json::Value;
use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub fn open(path: &Path) -> Result<Box<dyn Store>> {
    std::fs::create_dir_all(path.join("store"))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;
    let db = runtime.block_on(HelixDB::open(HelixDbSource::Disk {
        root: path.join("store"),
        database: "evaluation".into(),
    }))?;
    let map = NativeMap::load(&path.join("ids.json"))?;
    Ok(Box::new(Backend {
        runtime,
        db,
        map,
        path: path.into(),
    }))
}
struct Backend {
    runtime: tokio::runtime::Runtime,
    db: HelixDB,
    map: NativeMap,
    path: PathBuf,
}
fn ids(value: &Value) -> Result<Vec<u64>> {
    match value {
        Value::Array(a) => {
            let mut v = Vec::new();
            for e in a {
                v.extend(ids(e)?);
            }
            Ok(v)
        }
        Value::Number(n) => Ok(vec![n.as_u64().context("Negative ID")?]),
        Value::String(s) => Ok(vec![s.parse()?]),
        Value::Null => Ok(vec![]),
        other => anyhow::bail!("Unexpected ID terminal {other}"),
    }
}
fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => a.iter().find_map(text),
        Value::Object(a) => a.values().find_map(text),
        _ => None,
    }
}
fn field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(o) => o
            .get(key)
            .or_else(|| o.values().find_map(|v| field(v, key))),
        Value::Array(a) => a.iter().find_map(|v| field(v, key)),
        _ => None,
    }
}
impl Backend {
    fn query(&self, q: QueryRequest) -> Result<Value> {
        Ok(self.runtime.block_on(self.db.query(q))?)
    }
    fn index_ready(&self, receipt: Value) -> Result<()> {
        if field(&receipt, "kind").and_then(Value::as_str) == Some("already_active") {
            return Ok(());
        }
        let id = field(&receipt, "operation_id")
            .and_then(Value::as_str)
            .context("No operation ID in index receipt")?
            .to_owned();
        let start = Instant::now();
        loop {
            let status = self.query(QueryRequest::read(
                batch::read_batch()
                    .var_as("r", g().get_index_operation(&id))
                    .returning(["r"]),
            ))?;
            let state = field(&status, "status")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            match state {
                "succeeded" => return Ok(()),
                "blocked" | "aborted" => anyhow::bail!("Index operation {state}: {status}"),
                _ => {
                    ensure!(
                        start.elapsed() < Duration::from_secs(300),
                        "Index readiness timeout: {status}"
                    );
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
}
impl Store for Backend {
    fn import(&mut self, data: &Dataset) -> Result<()> {
        for chunk in data.nodes.chunks(256) {
            let mut b = batch::write_batch();
            let mut names = Vec::new();
            for n in chunk {
                let name = format!("n{}", n.id);
                let props: Vec<(&str, PropertyInput)> = vec![
                    ("gid", PropertyValue::from(n.id as i64).into()),
                    ("scope", PropertyValue::from(n.scope().code()).into()),
                    ("name", PropertyValue::from(n.name.clone()).into()),
                    ("repo", PropertyValue::from(n.repo as i64).into()),
                    ("embedding", PropertyValue::from(n.vector.clone()).into()),
                ];
                b = b.var_as(&name, g().add_n("Symbol", props).id());
                names.push(name);
            }
            let result = self.query(QueryRequest::write(b.returning(names.clone())))?;
            for (n, name) in chunk.iter().zip(names) {
                let native = ids(&result[&name])?;
                ensure!(native.len() == 1, "Expected one created node ID");
                self.map.insert(n, native[0]);
            }
        }
        for chunk in data.edges.chunks(256) {
            let mut b = batch::write_batch();
            let mut names = Vec::new();
            for (i, e) in chunk.iter().enumerate() {
                let name = format!("e{i}");
                b = b.var_as(
                    &name,
                    g().n(self.map.native(e.src)?)
                        .add_e(
                            &e.kind,
                            self.map.native(e.dst)?,
                            Vec::<(String, PropertyInput)>::new(),
                        )
                        .id(),
                );
                names.push(name);
            }
            self.query(QueryRequest::write(b.returning(names)))?;
        }
        self.map.save(&self.path.join("ids.json"))?;
        let receipt = self.query(QueryRequest::write(
            batch::write_batch()
                .var_as(
                    "r",
                    g().create_vector_index_nodes(
                        "Symbol",
                        "embedding",
                        NonZeroUsize::new(384).unwrap(),
                        VectorDistanceMetric::Cosine,
                        Some("scope"),
                    ),
                )
                .returning(["r"]),
        ))?;
        self.index_ready(receipt)?;
        Ok(())
    }
    fn reader(&self) -> Result<Box<dyn Reader + '_>> {
        Ok(Box::new(Read { backend: self }))
    }
    fn rename(&self, id: u64, name: &str) -> Result<()> {
        self.query(QueryRequest::write(
            batch::write_batch()
                .var_as(
                    "r",
                    g().n(self.map.native(id)?).set_property("name", name).id(),
                )
                .returning(["r"]),
        ))?;
        Ok(())
    }
    fn delete(&self, id: u64) -> Result<()> {
        self.query(QueryRequest::write(
            batch::write_batch()
                .var_as("r", g().n(self.map.native(id)?).drop().id())
                .returning(["r"]),
        ))?;
        Ok(())
    }
    fn checkpoint(&self) -> Result<()> {
        self.runtime.block_on(self.db.flush_writer())?;
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        self.runtime.block_on(self.db.close())?;
        Ok(())
    }
    fn details(&self) -> Value {
        serde_json::json!({"revision":"6f2a69653772b60ea099d8d36a02a1d4408147ec","engine_package":"db","batch":256,"storage":"Disk, embedded defaults with explicit writer flush","index_readiness":"Lifecycle status succeeded before timing queries","workers":4,"scope":"Native vector tenant scope and explicit candidate traversal","reachability":"Common bounded traversal over native adjacency"})
    }
}
struct Read<'a> {
    backend: &'a Backend,
}
impl Reader for Read<'_> {
    fn lookup(&mut self, id: u64, scope: Scope) -> Result<Option<String>> {
        if !self.backend.map.permitted(id, scope) {
            return Ok(None);
        }
        let v = self.backend.query(QueryRequest::read(
            batch::read_batch()
                .var_as(
                    "r",
                    g().n(self.backend.map.native(id)?).values(vec!["name"]),
                )
                .returning(["r"]),
        ))?;
        Ok(text(&v["r"]))
    }
    fn neighbors(&mut self, id: u64, scope: Scope, reverse: bool) -> Result<Vec<u64>> {
        if !self.backend.map.permitted(id, scope) {
            return Ok(vec![]);
        }
        let t = g().n(self.backend.map.native(id)?);
        let t = if reverse {
            t.in_(None::<&str>)
        } else {
            t.out(None::<&str>)
        };
        let v = self.backend.query(QueryRequest::read(
            batch::read_batch().var_as("r", t.id()).returning(["r"]),
        ))?;
        self.backend.map.normalize_neighbors(ids(&v["r"])?, scope)
    }
    fn knn(
        &mut self,
        q: &[f32],
        scope: Scope,
        k: usize,
        allowed: Option<&GraphConstraint>,
    ) -> Result<Vec<u64>> {
        let tenant = Some(PropertyValue::from(scope.code()));
        let t = if let Some(a) = allowed {
            let native = a
                .ids
                .iter()
                .map(|id| self.backend.map.native(*id))
                .collect::<Result<Vec<_>>>()?;
            g().n(NodeRef::ids(native))
                .vector_search("Symbol", "embedding", q.to_vec(), k, tenant)
        } else {
            g().vector_search_nodes("Symbol", "embedding", q.to_vec(), k, tenant)
        };
        let v = self.backend.query(QueryRequest::read(
            batch::read_batch().var_as("r", t.id()).returning(["r"]),
        ))?;
        ids(&v["r"])?
            .into_iter()
            .map(|id| self.backend.map.logical(id))
            .collect()
    }
}
