//! Raw OverGraph API reproductions: no DevWorks adapter, mapping or oracle.
use anyhow::{Result, ensure};
use overgraph::*;
use std::{
    path::Path,
    time::{Duration, Instant},
};
fn vector(axis: usize) -> Vec<f32> {
    let mut v = vec![0.; 384];
    v[axis] = 1.;
    v
}
fn options() -> DbOptions {
    DbOptions {
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
    }
}
fn open(path: &Path, options: &DbOptions) -> Result<DatabaseEngine> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match DatabaseEngine::open(path, options) {
            Err(EngineError::InvalidOperation(ref m))
                if (m == "candidate directory admission: Busy"
                    || m == "candidate directory admission: ActiveSession")
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(1))
            }
            r => return Ok(r?),
        }
    }
}
fn request(q: Vec<f32>, scope: Option<VectorSearchScope>) -> VectorSearchRequest {
    VectorSearchRequest {
        mode: VectorSearchMode::Dense,
        dense_query: Some(q),
        sparse_query: None,
        k: 10,
        label_filter: Some(NodeLabelFilter {
            labels: vec!["Project0".into()],
            mode: LabelMatchMode::All,
        }),
        ef_search: Some(256),
        scope,
        dense_weight: None,
        sparse_weight: None,
        fusion_mode: None,
    }
}
fn state(db: &DatabaseEngine, victim: u64, phase: &str) -> Result<serde_json::Value> {
    let exists = db.get_node(victim)?.is_some();
    let ids: Vec<_> = db
        .vector_search(&request(vector(0), None))?
        .into_iter()
        .map(|v| v.node_id)
        .collect();
    Ok(
        serde_json::json!({"phase":phase,"point_exists":exists,"vector_ids":ids,"deleted_id_in_vectors":ids.contains(&victim)}),
    )
}
fn deletion(path: &Path, flush_before: bool, rename: bool) -> Result<serde_json::Value> {
    let opts = options();
    let db = open(path, &opts)?;
    let victim = db.upsert_node(
        &["Symbol", "Project0"],
        "victim",
        UpsertNodeOptions {
            dense_vector: Some(vector(0)),
            ..Default::default()
        },
    )?;
    let kept = db.upsert_node(
        &["Symbol", "Project0"],
        "kept",
        UpsertNodeOptions {
            dense_vector: Some(vector(1)),
            ..Default::default()
        },
    )?;
    if flush_before {
        db.flush()?;
    }
    if rename {
        let old = db.get_node(victim)?.unwrap();
        db.upsert_node(
            old.labels,
            &old.key,
            UpsertNodeOptions {
                props: std::collections::BTreeMap::from([(
                    "name".into(),
                    PropValue::String("renamed".into()),
                )]),
                weight: old.weight,
                dense_vector: old.dense_vector,
                sparse_vector: old.sparse_vector,
            },
        )?;
    }
    db.delete_node(victim)?;
    let mut phases = vec![state(&db, victim, "after_delete_before_flush")?];
    db.flush()?;
    phases.push(state(&db, victim, "after_delete_and_flush")?);
    db.close()?;
    drop(db);
    let db = open(path, &opts)?;
    phases.push(state(&db, victim, "after_reopen")?);
    db.close()?;
    Ok(
        serde_json::json!({"flush_before_delete":flush_before,"rename_before_delete":rename,"victim":victim,"kept":kept,"phases":phases}),
    )
}
fn graph(path: &Path) -> Result<serde_json::Value> {
    let opts = options();
    let db = open(path, &opts)?;
    let a = db.upsert_node(
        &["Symbol", "Project0"],
        "a",
        UpsertNodeOptions {
            dense_vector: Some(vector(1)),
            ..Default::default()
        },
    )?;
    let b = db.upsert_node(
        &["Symbol", "Project1"],
        "bridge",
        UpsertNodeOptions {
            dense_vector: Some(vector(2)),
            ..Default::default()
        },
    )?;
    let c = db.upsert_node(
        &["Symbol", "Project0"],
        "c",
        UpsertNodeOptions {
            dense_vector: Some(vector(0)),
            ..Default::default()
        },
    )?;
    db.upsert_edge(a, b, "LINK", Default::default())?;
    db.upsert_edge(b, c, "LINK", Default::default())?;
    db.flush()?;
    let ids: Vec<_> = db
        .vector_search(&request(
            vector(0),
            Some(VectorSearchScope {
                start_node_id: a,
                max_depth: 2,
                direction: Direction::Outgoing,
                edge_label_filter: None,
                at_epoch: None,
            }),
        ))?
        .into_iter()
        .map(|h| h.node_id)
        .collect();
    db.close()?;
    Ok(
        serde_json::json!({"a_project0":a,"b_project1":b,"c_project0":c,"path":"A(project0) -> B(project1) -> C(project0)","returned_ids":ids,"c_returned":ids.contains(&c),"interpretation":"label_filter selects result nodes; no intermediate-project restriction was specified"}),
    )
}
fn main() -> Result<()> {
    let root = std::path::PathBuf::from(
        std::env::args_os()
            .nth(1)
            .expect("Fresh disposable root path required"),
    );
    ensure!(!root.exists(), "Refusing existing directory");
    std::fs::create_dir_all(&root)?;
    let mut cases = Vec::new();
    for flush in [false, true] {
        for rename in [false, true] {
            cases.push(deletion(
                &root.join(format!("delete-{flush}-{rename}")),
                flush,
                rename,
            )?);
        }
    }
    let result = serde_json::json!({"engine":"overgraph","version":"0.18.0","direct_api_only":true,"inference":false,"deletion_cases":cases,"graph_scope":graph(&root.join("graph"))?});
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
