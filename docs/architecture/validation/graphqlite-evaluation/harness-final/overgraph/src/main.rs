use overgraph::*;
mod adapter;

fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--workload") {
        return eval_common::workload::run("overgraph", adapter::open);
    }
    let path = eval_common::fresh_probe_path()?;
    let options = DbOptions {
        wal_sync_mode: WalSyncMode::Immediate,
        ..Default::default()
    };
    let db = DatabaseEngine::open(&path, &options)?;
    let a = db.upsert_node("Probe", "a", Default::default())?;
    let b = db.upsert_node("Probe", "b", Default::default())?;
    db.upsert_edge(a, b, "LINK", Default::default())?;
    anyhow::ensure!(db.neighbors(a, &NeighborOptions::default())?.len() == 1);
    db.close()?;
    drop(db);
    let db = DatabaseEngine::open(&path, &options)?;
    anyhow::ensure!(db.get_node_by_key("Probe", "a")?.is_some());
    anyhow::ensure!(db.neighbors(a, &NeighborOptions::default())?.len() == 1);
    db.close()?;
    println!(
        "{}",
        serde_json::json!({"engine":"overgraph","version":"0.18.0","open_write_read_reopen":true,"graph_edge":true,"inference":false,"durability":"Immediate WAL sync"})
    );
    Ok(())
}
