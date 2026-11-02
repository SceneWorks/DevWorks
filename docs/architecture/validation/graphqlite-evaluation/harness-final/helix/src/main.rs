use helix_ast::{batch, graph::NodeRef, query::QueryRequest, traversal, value::PropertyInput};
use helix_core::{HelixDB, HelixDbSource};
mod adapter;

fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--workload") {
        return eval_common::workload::run("helix", adapter::open);
    }
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(smoke())
}

async fn smoke() -> anyhow::Result<()> {
    let path = eval_common::fresh_probe_path()?;
    std::fs::create_dir_all(&path)?;
    let source = HelixDbSource::Disk {
        root: path,
        database: "qualification".into(),
    };
    let db = HelixDB::open(source.clone()).await?;
    db.query(QueryRequest::write(
        batch::write_batch()
            .var_as(
                "created",
                traversal::g().add_n("Probe", vec![("name", PropertyInput::from("persisted"))]),
            )
            .returning(["created"]),
    ))
    .await?;
    let result = db
        .query(QueryRequest::read(
            batch::read_batch()
                .var_as("count", traversal::g().n(NodeRef::all()).count())
                .returning(["count"]),
        ))
        .await?;
    anyhow::ensure!(result["count"] == 1, "Unexpected count: {result}");
    db.close().await?;
    drop(db);
    let db = HelixDB::open(source).await?;
    let result = db
        .query(QueryRequest::read(
            batch::read_batch()
                .var_as("count", traversal::g().n(NodeRef::all()).count())
                .returning(["count"]),
        ))
        .await?;
    anyhow::ensure!(result["count"] == 1, "Unexpected persisted count: {result}");
    db.close().await?;
    println!(
        "{}",
        serde_json::json!({"engine":"helix","revision":"6f2a69653772b60ea099d8d36a02a1d4408147ec","open_write_read_reopen":true,"inference":false})
    );
    Ok(())
}
