//! Standalone deleted-vector diagnostic. No DevWorks dependencies or adapter.
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
    let mut phases = vec![state(&db, victim, "before_delete")?];
    db.delete_node(victim)?;
    phases.push(state(&db, victim, "after_delete_before_flush")?);
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
fn run() -> Result<(serde_json::Value, bool)> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => {
            ensure!(
                args.len() == 2,
                "Usage: overgraph-delete-repro run FRESH_ROOT"
            );
            let root = std::path::PathBuf::from(&args[1]);
            ensure!(
                !root.exists(),
                "Refusing existing directory {}",
                root.display()
            );
            std::fs::create_dir_all(&root)?;
            let mut cases = Vec::new();
            for flush in [false, true] {
                for rename in [false, true] {
                    cases.push(deletion(
                        &root.join(format!("flush-{flush}-rename-{rename}")),
                        flush,
                        rename,
                    )?);
                }
            }
            let stale = cases.iter().any(|case| {
                case["phases"].as_array().unwrap().iter().any(|phase| {
                    phase["point_exists"] == false && phase["deleted_id_in_vectors"] == true
                })
            });
            Ok((
                serde_json::json!({"engine":"overgraph","version":"0.18.0","mode":"run","direct_api_only":true,"inference":false,"root":root,"deletion_cases":cases,"observed_stale_vector":stale}),
                stale,
            ))
        }
        Some("inspect") => {
            ensure!(
                args.len() == 3,
                "Usage: overgraph-delete-repro inspect EXISTING_CASE VICTIM_ID"
            );
            let path = std::path::PathBuf::from(&args[1]);
            ensure!(
                path.is_dir(),
                "Inspection requires an existing reproduction case directory"
            );
            let id: u64 = args[2].parse()?;
            let db = open(&path, &options())?;
            let observation = state(&db, id, "fresh_process_reopen")?;
            db.close()?;
            let stale = observation["point_exists"] == false
                && observation["deleted_id_in_vectors"] == true;
            Ok((
                serde_json::json!({"engine":"overgraph","version":"0.18.0","mode":"inspect","direct_api_only":true,"inference":false,"observation":observation,"observed_stale_vector":stale}),
                stale,
            ))
        }
        _ => anyhow::bail!(
            "Usage: overgraph-delete-repro run FRESH_ROOT | inspect EXISTING_CASE VICTIM_ID"
        ),
    }
}
fn main() {
    match run() {
        Ok((report, stale)) => {
            println!("{}", serde_json::to_string_pretty(&report).unwrap());
            if stale {
                std::process::exit(1);
            }
        }
        Err(error) => {
            eprintln!("Setup/execution error: {error:#}");
            std::process::exit(2);
        }
    }
}
