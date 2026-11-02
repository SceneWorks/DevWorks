use rusqlite::{Connection, ffi::sqlite3_auto_extension};
mod adapter;

fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--workload") {
        return eval_common::workload::run("sqlite", adapter::open);
    }
    let path = eval_common::fresh_probe_path()?;
    // Register the statically linked extension using its documented C entry point.
    unsafe {
        sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    }
    {
        let db = Connection::open(&path)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE nodes(id INTEGER PRIMARY KEY, name TEXT); CREATE TABLE edges(src INTEGER, dst INTEGER); CREATE INDEX edge_src ON edges(src); CREATE VIRTUAL TABLE vectors USING vec0(embedding float[4]); INSERT INTO nodes VALUES(1, 'a'), (2, 'b'); INSERT INTO edges VALUES(1,2); INSERT INTO vectors(rowid,embedding) VALUES(1,'[1,0,0,0]');")?;
        let n: i64 = db.query_row(
            "SELECT count(*) FROM edges JOIN nodes ON edges.dst=nodes.id WHERE src=1",
            [],
            |r| r.get(0),
        )?;
        anyhow::ensure!(n == 1);
    }
    let db = Connection::open(&path)?;
    let n: i64 = db.query_row(
        "SELECT count(*) FROM edges JOIN nodes ON edges.dst=nodes.id WHERE src=1",
        [],
        |r| r.get(0),
    )?;
    anyhow::ensure!(n == 1);
    let (id, distance): (i64, f64) = db.query_row(
        "SELECT rowid,distance FROM vectors WHERE embedding MATCH '[1,0,0,0]' AND k=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    anyhow::ensure!(id == 1 && distance.abs() < 1e-6);
    let version: String = db.query_row("SELECT sqlite_version()", [], |r| r.get(0))?;
    println!(
        "{}",
        serde_json::json!({"engine":"sqlite","sqlite_version":version,"sqlite_vec":"0.1.7","open_write_read_reopen":true,"graph_edge":true,"vector_search":true,"inference":false,"durability":"WAL synchronous FULL"})
    );
    Ok(())
}
