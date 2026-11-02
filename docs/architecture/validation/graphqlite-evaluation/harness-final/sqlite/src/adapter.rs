use anyhow::Result;
use eval_common::workload::{Dataset, GraphConstraint, Reader, Scope, Store};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    path::{Path, PathBuf},
    sync::{Mutex, Once},
};
static REGISTER: Once = Once::new();
pub fn open(path: &Path) -> Result<Box<dyn Store>> {
    std::fs::create_dir_all(path)?;
    REGISTER.call_once(|| unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    });
    let file = path.join("store.sqlite");
    let db = connection(&file)?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS nodes(id INTEGER PRIMARY KEY,scope INTEGER,name TEXT,repo INTEGER);CREATE TABLE IF NOT EXISTS edges(src INTEGER,dst INTEGER,kind TEXT);CREATE INDEX IF NOT EXISTS e_src ON edges(src);CREATE INDEX IF NOT EXISTS e_dst ON edges(dst);CREATE VIRTUAL TABLE IF NOT EXISTS vectors USING vec0(embedding float[384] distance_metric=cosine,scope integer partition key);")?;
    Ok(Box::new(Backend {
        writer: Mutex::new(db),
        path: file,
    }))
}
fn connection(path: &Path) -> Result<Connection> {
    let db = Connection::open(path)?;
    db.busy_timeout(std::time::Duration::from_secs(30))?;
    db.execute_batch("PRAGMA journal_mode=WAL;PRAGMA synchronous=FULL;PRAGMA cache_size=-65536;")?;
    Ok(db)
}
fn bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}
struct Backend {
    writer: Mutex<Connection>,
    path: PathBuf,
}
impl Store for Backend {
    fn import(&mut self, data: &Dataset) -> Result<()> {
        let db = self.writer.get_mut().unwrap();
        for batch in data.nodes.chunks(256) {
            let tx = db.transaction()?;
            {
                let mut n = tx.prepare("INSERT INTO nodes VALUES(?,?,?,?)")?;
                let mut v =
                    tx.prepare("INSERT INTO vectors(rowid,embedding,scope) VALUES(?,?,?)")?;
                for node in batch {
                    n.execute(params![
                        node.id as i64,
                        node.scope().code(),
                        node.name,
                        node.repo
                    ])?;
                    v.execute(params![
                        node.id as i64,
                        bytes(&node.vector),
                        node.scope().code()
                    ])?;
                }
            }
            tx.commit()?;
        }
        for batch in data.edges.chunks(256) {
            let tx = db.transaction()?;
            {
                let mut e = tx.prepare("INSERT INTO edges VALUES(?,?,?)")?;
                for edge in batch {
                    e.execute(params![edge.src as i64, edge.dst as i64, edge.kind])?;
                }
            }
            tx.commit()?;
        }
        Ok(())
    }
    fn reader(&self) -> Result<Box<dyn Reader + '_>> {
        Ok(Box::new(Read {
            db: connection(&self.path)?,
        }))
    }
    fn rename(&self, id: u64, name: &str) -> Result<()> {
        self.writer.lock().unwrap().execute(
            "UPDATE nodes SET name=? WHERE id=?",
            params![name, id as i64],
        )?;
        Ok(())
    }
    fn delete(&self, id: u64) -> Result<()> {
        let mut writer = self.writer.lock().unwrap();
        let tx = writer.transaction()?;
        tx.execute("DELETE FROM edges WHERE src=?1 OR dst=?1", [id as i64])?;
        tx.execute("DELETE FROM vectors WHERE rowid=?1", [id as i64])?;
        tx.execute("DELETE FROM nodes WHERE id=?1", [id as i64])?;
        tx.commit()?;
        Ok(())
    }
    fn checkpoint(&self) -> Result<()> {
        self.writer
            .lock()
            .unwrap()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        self.checkpoint()
    }
    fn details(&self) -> serde_json::Value {
        serde_json::json!({"rusqlite":"0.41.1","sqlite_vec":"0.1.7","wal":"FULL","batch":256,"vector_mode":"Exact KNN","cache_kib":65536,"reachability":"Common bounded traversal over indexed adjacency","scope":"Native vec0 partition key and eligible row-ID constraints"})
    }
}
struct Read {
    db: Connection,
}
impl Reader for Read {
    fn lookup(&mut self, id: u64, scope: Scope) -> Result<Option<String>> {
        Ok(self
            .db
            .prepare_cached("SELECT name FROM nodes WHERE id=? AND scope=?")?
            .query_row(params![id as i64, scope.code()], |r| r.get(0))
            .optional()?)
    }
    fn neighbors(&mut self, id: u64, scope: Scope, reverse: bool) -> Result<Vec<u64>> {
        let sql = if reverse {
            "SELECT DISTINCT e.src FROM edges e JOIN nodes n ON n.id=e.src JOIN nodes root ON root.id=e.dst WHERE e.dst=? AND n.scope=? AND root.scope=?"
        } else {
            "SELECT DISTINCT e.dst FROM edges e JOIN nodes n ON n.id=e.dst JOIN nodes root ON root.id=e.src WHERE e.src=? AND n.scope=? AND root.scope=?"
        };
        Ok(self
            .db
            .prepare_cached(sql)?
            .query_map(params![id as i64, scope.code(), scope.code()], |r| {
                r.get::<_, i64>(0).map(|x| x as u64)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
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
            .db
            .prepare(&sql)?
            .query_map(params![bytes(q), scope.code(), k as i64], |r| {
                r.get::<_, i64>(0).map(|x| x as u64)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
