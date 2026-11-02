use anyhow::Result;
use eval_common::{
    mapping::NativeMap,
    workload::{Dataset, GraphConstraint, Reader, Scope, Store},
};
use overgraph::*;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub fn open(path: &Path) -> Result<Box<dyn Store>> {
    std::fs::create_dir_all(path)?;
    let options = DbOptions {
        wal_sync_mode: WalSyncMode::Immediate,
        dense_vector: Some(DenseVectorConfig {
            dimension: 384,
            metric: DenseMetric::Cosine,
            hnsw: HnswConfig {
                m: 16,
                ef_construction: 200,
            },
        }),
        ..Default::default()
    };
    // Upstream production-concurrency lifecycle tests use this exact bounded
    // admission retry: close completion can precede native directory release.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let db = loop {
        match DatabaseEngine::open(&path.join("store"), &options) {
            Err(EngineError::InvalidOperation(ref message))
                if (message == "candidate directory admission: Busy"
                    || message == "candidate directory admission: ActiveSession")
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            result => break result?,
        }
    };
    let map = NativeMap::load(&path.join("ids.json"))?;
    Ok(Box::new(Backend {
        db,
        map,
        path: path.into(),
    }))
}
struct Backend {
    db: DatabaseEngine,
    map: NativeMap,
    path: PathBuf,
}
fn props(n: &eval_common::workload::Node) -> BTreeMap<String, PropValue> {
    BTreeMap::from([
        ("gid".into(), PropValue::UInt(n.id)),
        ("project".into(), PropValue::Int(n.project.into())),
        ("view".into(), PropValue::Int(n.view.into())),
        ("name".into(), PropValue::String(n.name.clone())),
        ("repo".into(), PropValue::Int(n.repo.into())),
    ])
}
impl Store for Backend {
    fn import(&mut self, data: &Dataset) -> Result<()> {
        for batch in data.nodes.chunks(256) {
            let inputs = batch
                .iter()
                .map(|n| NodeInput {
                    labels: vec!["Symbol".into(), n.scope().label()],
                    key: n.key(),
                    props: props(n),
                    weight: 1.,
                    dense_vector: Some(n.vector.clone()),
                    sparse_vector: None,
                })
                .collect();
            let ids = self.db.batch_upsert_nodes(inputs)?;
            for (n, id) in batch.iter().zip(ids) {
                self.map.insert(n, id);
            }
        }
        for batch in data.edges.chunks(256) {
            let mut inputs = Vec::new();
            for e in batch {
                inputs.push(EdgeInput {
                    from: self.map.native(e.src)?,
                    to: self.map.native(e.dst)?,
                    label: e.kind.clone(),
                    props: BTreeMap::new(),
                    weight: 1.,
                    valid_from: None,
                    valid_to: None,
                });
            }
            self.db.batch_upsert_edges(inputs)?;
        }
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
        let native = self.map.native(id)?;
        let old = self
            .db
            .get_node(native)?
            .ok_or_else(|| anyhow::anyhow!("Missing rename target"))?;
        let mut p = old.props;
        p.insert("name".into(), PropValue::String(name.into()));
        self.db.upsert_node(
            old.labels,
            &old.key,
            UpsertNodeOptions {
                props: p,
                weight: old.weight,
                dense_vector: old.dense_vector,
                sparse_vector: old.sparse_vector,
            },
        )?;
        Ok(())
    }
    fn delete(&self, id: u64) -> Result<()> {
        self.db.delete_node(self.map.native(id)?)?;
        Ok(())
    }
    fn checkpoint(&self) -> Result<()> {
        self.db.flush()?;
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        self.db.close()?;
        Ok(())
    }
    fn details(&self) -> serde_json::Value {
        serde_json::json!({"version":"0.18.0","wal":"Immediate","batch":256,"hnsw_m":16,"hnsw_ef_construction":200,"hnsw_ef_search":256,"reachability":"Common bounded traversal over native adjacency","scope":"Native label and traversal vector filters; projection membership guards for adjacency","retention":"No prune policy configured"})
    }
}
struct Read<'a> {
    db: &'a DatabaseEngine,
    map: &'a NativeMap,
}
impl Reader for Read<'_> {
    fn lookup(&mut self, id: u64, scope: Scope) -> Result<Option<String>> {
        if !self.map.permitted(id, scope) {
            return Ok(None);
        }
        Ok(self
            .db
            .get_node(self.map.native(id)?)?
            .and_then(|n| match n.props.get("name") {
                Some(PropValue::String(s)) => Some(s.clone()),
                _ => None,
            }))
    }
    fn neighbors(&mut self, id: u64, scope: Scope, reverse: bool) -> Result<Vec<u64>> {
        if !self.map.permitted(id, scope) {
            return Ok(vec![]);
        }
        let entries = self.db.neighbors(
            self.map.native(id)?,
            &NeighborOptions {
                direction: if reverse {
                    Direction::Incoming
                } else {
                    Direction::Outgoing
                },
                limit: None,
                ..Default::default()
            },
        )?;
        self.map
            .normalize_neighbors(entries.into_iter().map(|e| e.node_id), scope)
    }
    fn knn(
        &mut self,
        q: &[f32],
        scope: Scope,
        k: usize,
        allowed: Option<&GraphConstraint>,
    ) -> Result<Vec<u64>> {
        let traversal = allowed
            .map(|a| -> Result<_> {
                Ok(VectorSearchScope {
                    start_node_id: self.map.native(a.root)?,
                    max_depth: a.depth,
                    direction: Direction::Outgoing,
                    edge_label_filter: None,
                    at_epoch: None,
                })
            })
            .transpose()?;
        let hits = self.db.vector_search(&VectorSearchRequest {
            mode: VectorSearchMode::Dense,
            dense_query: Some(q.to_vec()),
            sparse_query: None,
            k,
            label_filter: Some(NodeLabelFilter {
                labels: vec![scope.label()],
                mode: LabelMatchMode::All,
            }),
            ef_search: Some(256),
            scope: traversal,
            dense_weight: None,
            sparse_weight: None,
            fusion_mode: None,
        })?;
        hits.into_iter()
            .map(|h| self.map.logical(h.node_id))
            .collect()
    }
}
