use anyhow::Result;
use eval_common::{
    mapping::NativeMap,
    workload::{Dataset, GraphConstraint, Reader, Scope, Store},
};
use grafeo::{Config, Direction, DurabilityMode, GrafeoDB, NodeId, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

pub fn open(path: &Path) -> Result<Box<dyn Store>> {
    std::fs::create_dir_all(path)?;
    let db = GrafeoDB::with_config(
        Config::persistent(path.join("store.grafeo")).with_wal_durability(DurabilityMode::Sync),
    )?;
    let map = NativeMap::load(&path.join("ids.json"))?;
    Ok(Box::new(Backend {
        db,
        map,
        path: path.into(),
    }))
}
struct Backend {
    db: GrafeoDB,
    map: NativeMap,
    path: PathBuf,
}
impl Store for Backend {
    fn import(&mut self, data: &Dataset) -> Result<()> {
        for batch in data.nodes.chunks(256) {
            let props: Vec<HashMap<grafeo::store::PropertyKey, Value>> = batch
                .iter()
                .map(|n| {
                    HashMap::from([
                        ("gid".into(), Value::Int64(n.id as i64)),
                        ("scope".into(), Value::Int64(n.scope().code())),
                        ("name".into(), Value::from(n.name.as_str())),
                        ("repo".into(), Value::Int64(n.repo.into())),
                        (
                            "embedding".into(),
                            Value::Vector(Arc::from(n.vector.clone())),
                        ),
                    ])
                })
                .collect();
            let ids = self.db.batch_create_nodes_with_props("Symbol", props)?;
            for (n, id) in batch.iter().zip(ids) {
                self.map.insert(n, id.0);
            }
        }
        for batch in data.edges.chunks(256) {
            let mut edges = Vec::new();
            for e in batch {
                edges.push(grafeo::database::BatchEdge::new(
                    NodeId(self.map.native(e.src)?),
                    NodeId(self.map.native(e.dst)?),
                    e.kind.as_str(),
                ));
            }
            self.db.batch_create_edges(edges)?;
        }
        self.db.create_property_index("gid")?;
        self.db.create_property_index("scope")?;
        self.db.create_vector_index(
            "Symbol",
            "embedding",
            Some(384),
            Some("cosine"),
            Some(16),
            Some(200),
            None,
        )?;
        self.map.save(&self.path.join("ids.json"))?;
        Ok(())
    }
    fn reader(&self) -> Result<Box<dyn Reader + '_>> {
        Ok(Box::new(Read {
            db: &self.db,
            map: &self.map,
        }))
    }
    fn rename(&self, id: u64, name: &str) -> Result<()> {
        self.db
            .set_node_property(NodeId(self.map.native(id)?), "name", Value::from(name))?;
        Ok(())
    }
    fn delete(&self, id: u64) -> Result<()> {
        anyhow::ensure!(
            self.db.detach_delete_node(NodeId(self.map.native(id)?))?,
            "Delete target missing"
        );
        Ok(())
    }
    fn checkpoint(&self) -> Result<()> {
        self.db.wal_checkpoint()?;
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        self.db.close()?;
        Ok(())
    }
    fn details(&self) -> serde_json::Value {
        serde_json::json!({"version":"0.6.0","wal":"Sync","batch":256,"hnsw_m":16,"hnsw_ef_construction":200,"hnsw_ef_search":256,"model_generation":false,"reachability":"Common bounded traversal over native adjacency","graph_knn":"Native Cypher predicate with explicit eligible IDs and cosine ordering"})
    }
}
struct Read<'a> {
    db: &'a GrafeoDB,
    map: &'a NativeMap,
}
impl Reader for Read<'_> {
    fn lookup(&mut self, id: u64, scope: Scope) -> Result<Option<String>> {
        if !self.map.permitted(id, scope) {
            return Ok(None);
        }
        Ok(self
            .db
            .get_node(NodeId(self.map.native(id)?))?
            .and_then(|n| {
                n.get_property("name")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            }))
    }
    fn neighbors(&mut self, id: u64, scope: Scope, reverse: bool) -> Result<Vec<u64>> {
        if !self.map.permitted(id, scope) {
            return Ok(vec![]);
        }
        let edges = self.db.get_node_edges(
            NodeId(self.map.native(id)?),
            if reverse {
                Direction::Incoming
            } else {
                Direction::Outgoing
            },
            &[],
        )?;
        self.map
            .normalize_neighbors(edges.into_iter().map(|x| x.0.0), scope)
    }
    fn knn(
        &mut self,
        q: &[f32],
        scope: Scope,
        k: usize,
        allowed: Option<&GraphConstraint>,
    ) -> Result<Vec<u64>> {
        if let Some(allowed) = allowed {
            let params = HashMap::from([
                ("q".into(), Value::Vector(Arc::from(q.to_vec()))),
                ("scope".into(), Value::Int64(scope.code())),
                (
                    "ids".into(),
                    Value::List(Arc::from(
                        allowed
                            .ids
                            .iter()
                            .map(|id| Value::Int64(*id as i64))
                            .collect::<Vec<_>>(),
                    )),
                ),
            ]);
            let query = format!(
                "MATCH (n:Symbol) WHERE n.scope = $scope AND n.gid IN $ids RETURN n.gid ORDER BY cosine_similarity(n.embedding, $q) DESC LIMIT {k}"
            );
            let result = self
                .db
                .session()
                .execute_cypher_with_params(&query, params)?;
            return result
                .rows()
                .iter()
                .map(|r| match r.first() {
                    Some(Value::Int64(id)) => Ok(*id as u64),
                    other => anyhow::bail!("Unexpected graph KNN row {other:?}"),
                })
                .collect();
        }
        let filters = HashMap::from([("scope".into(), Value::Int64(scope.code()))]);
        let hits = self
            .db
            .vector_search("Symbol", "embedding", q, k, Some(256), Some(&filters))?;
        hits.into_iter().map(|h| self.map.logical(h.0.0)).collect()
    }
}
