//! All driver handles/SQL stay here, including deliberately privileged fixture setup.
use crate::core::*;
use rusqlite::{
    Connection, OptionalExtension, Transaction, TransactionBehavior, ffi::sqlite3_auto_extension,
    params,
};
use std::path::Path;
fn db<T>(r: rusqlite::Result<T>) -> Result<T> {
    r.map_err(|_| StoreError::Unavailable)
}
pub struct SqliteStore {
    connection: Connection,
    pin_serial: u32,
}
struct UnitOfWork<'a> {
    tx: Transaction<'a>,
}
impl SqliteStore {
    pub fn open(path: &Path) -> Result<Self> {
        unsafe {
            sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
        let connection = db(Connection::open(path))?;
        db(connection.execute_batch("PRAGMA foreign_keys=ON;PRAGMA journal_mode=WAL;PRAGMA synchronous=FULL;PRAGMA busy_timeout=1000;"))?;
        Ok(Self {
            connection,
            pin_serial: 0,
        })
    }
    pub fn versions(&self) -> Result<(String, String)> {
        db(self
            .connection
            .query_row("SELECT sqlite_version(),vec_version()", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            }))
    }
    pub fn effective_settings(&self) -> Result<(String, i64, i64)> {
        let journal = db(self
            .connection
            .pragma_query_value(None, "journal_mode", |r| r.get(0)))?;
        let sync = db(self
            .connection
            .pragma_query_value(None, "synchronous", |r| r.get(0)))?;
        let foreign = db(self
            .connection
            .pragma_query_value(None, "foreign_keys", |r| r.get(0)))?;
        Ok((journal, sync, foreign))
    }
    pub fn install_fixture(&mut self) -> Result<()> {
        db(self.connection.execute_batch(include_str!(
            "../../../docs/architecture/persistence-schema.sql"
        )))?;
        db(self.connection.execute_batch("INSERT INTO store_meta VALUES(1,'store-a',1,1,1,0);INSERT INTO app_boots VALUES('boot-a',1,NULL,'Open');INSERT INTO actors VALUES('user','user','Fixture');INSERT INTO projects VALUES('p','Before','Active',1,1,1),('q','Private','Active',1,1,1);INSERT INTO repositories VALUES('repo-a','fixture-a',1),('repo-q','fixture-q',1);INSERT INTO project_repositories VALUES('p','repo-a'),('q','repo-q');"))?;
        for (id, source, kind) in [
            ("base", "repo-a", "Baseline"),
            ("branch", "repo-a", "Branch"),
            ("capture", "repo-a", "Worktree"),
            ("private", "repo-q", "Baseline"),
        ] {
            db(self.connection.execute(
                "INSERT INTO index_generations VALUES(?,?,?,'Sealed','digest',1,'{}')",
                params![id, source, kind],
            ))?;
        }
        for (generation, unit, action) in [
            ("base", "a", "Replace"),
            ("base", "b", "Replace"),
            ("base", "k", "Replace"),
            ("branch", "a", "Replace"),
            ("capture", "a", "Delete"),
            ("capture", "b", "Replace"),
            ("private", "secret", "Replace"),
        ] {
            db(self.connection.execute(
                "INSERT INTO source_units VALUES(?,?,'digest',?)",
                params![generation, unit, action],
            ))?;
        }
        for (generation, unit, node, name) in [
            ("base", "a", "a-old", "outdated-text"),
            ("base", "b", "b-old", "old source"),
            ("base", "k", "keep", "kept source"),
            ("branch", "a", "a-branch", "branch old"),
            ("capture", "b", "b-new", "current source"),
            ("private", "secret", "private-node", "private-text"),
        ] {
            db(self.connection.execute(
                "INSERT INTO graph_nodes VALUES(?,?,?,'function',?,'{}')",
                params![generation, unit, node, name],
            ))?;
            db(self.connection.execute(
                "INSERT INTO search_chunks VALUES(?,?,?,NULL,'{}',?,'digest')",
                params![node, generation, unit, name],
            ))?;
            db(self.connection.execute(
                "INSERT INTO search_fts(chunk_id,text) VALUES(?,?)",
                params![node, name],
            ))?;
        }
        for (generation, unit, eid, from, to) in [
            ("base", "b", "edge-old", "b-old", "a-old"),
            ("base", "k", "edge-dangling", "keep", "a-old"),
            ("capture", "b", "edge-current", "b-new", "keep"),
        ] {
            db(self.connection.execute(
                "INSERT INTO graph_edges VALUES(?,?,?,?,?,'calls')",
                params![generation, unit, eid, from, to],
            ))?;
        }
        db(self.connection.execute_batch("INSERT INTO index_views VALUES('view-base','repo-a','digest-base','base',NULL,NULL,'{}'),('view-capture','repo-a','digest-capture','base','branch','capture','{}'),('view-private','repo-q','digest-private','private',NULL,NULL,'{}'),('view-mixed','repo-a','digest-mixed','private',NULL,NULL,'{}');INSERT INTO published_views VALUES('repo-a','view-base',1);INSERT INTO embedding_spaces VALUES('two','model-two',2,'L2','f32'),('three','model-three',3,'L2','f32');"))?;
        for (chunk, space, values) in [
            ("a-old", "two", vec![0f32, 0.]),
            ("a-branch", "two", vec![0., 0.]),
            ("b-new", "two", vec![1., 0.]),
            ("keep", "two", vec![2., 0.]),
            ("private-node", "two", vec![0., 0.]),
            ("private-node", "three", vec![0., 0., 0.]),
        ] {
            let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
            db(self.connection.execute(
                "INSERT INTO chunk_vectors VALUES(?,?,?,?)",
                params![chunk, space, values.len() as i64, bytes],
            ))?;
        }
        Ok(())
    }
    fn scope(&self, s: &Scope, source: &str) -> Result<()> {
        let allowed:bool=db(self.connection.query_row("SELECT EXISTS(SELECT 1 FROM projects p JOIN project_repositories pr ON pr.project_id=p.id WHERE p.id=?1 AND p.scope_revision=?2 AND pr.repository_id=?3)",params![s.project_id,s.expected_scope_revision,source],|r|r.get(0)))?;
        if !allowed {
            return Err(StoreError::ScopeDenied);
        }
        Ok(())
    }
    fn view_source(&self, view: &str) -> Result<String> {
        let source: Option<String> = db(self
            .connection
            .query_row(
                "SELECT source_key FROM index_views WHERE id=?",
                [view],
                |r| r.get(0),
            )
            .optional())?;
        source.ok_or(StoreError::ViewUnavailable)
    }
    fn valid_view(&self, view: &str) -> Result<()> {
        let good:bool=db(self.connection.query_row("SELECT count(*)=0 FROM index_views v JOIN index_generations g ON g.id IN(v.baseline_id,v.branch_id,v.worktree_id) WHERE v.id=? AND (g.source_key<>v.source_key OR g.state<>'Sealed')",[view],|r|r.get(0)))?;
        if !good {
            return Err(StoreError::ViewUnavailable);
        }
        Ok(())
    }
    fn read_pin(&self, p: &Pin, now: i64) -> Result<()> {
        let source = self.view_source(&p.view_id)?;
        self.scope(&p.scope, &source)?;
        self.valid_view(&p.view_id)?;
        let live:bool=db(self.connection.query_row("SELECT EXISTS(SELECT 1 FROM index_pins WHERE id=?1 AND view_id=?2 AND boot_id='boot-a' AND lease_until>?3)",params![p.id,p.view_id,now],|r|r.get(0)))?;
        if !live {
            return Err(StoreError::ViewUnavailable);
        }
        Ok(())
    }
    pub fn fixture_revoke(&self) -> Result<()> {
        db(self.connection.execute_batch("UPDATE projects SET scope_revision=scope_revision+1 WHERE id='p';DELETE FROM project_repositories WHERE project_id='p'"))
    }
    pub fn fixture_advance_source(&self) -> Result<()> {
        db(self.connection.execute(
            "UPDATE repositories SET revision=revision+1 WHERE id='repo-a'",
            [],
        ))
        .map(|_| ())
    }
    pub fn fixture_counts(&self) -> Result<(i64, i64, i64)> {
        db(self.connection.query_row("SELECT (SELECT count(*) FROM command_receipts),(SELECT count(*) FROM events),(SELECT count(*) FROM queue_entries)",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))))
    }
    pub fn fixture_title(&self) -> Result<String> {
        db(self
            .connection
            .query_row("SELECT title FROM projects WHERE id='p'", [], |r| r.get(0)))
    }
    pub fn fixture_view_reclaimable(&self, view: &str, now: i64) -> Result<bool> {
        db(self.connection.query_row("SELECT NOT EXISTS(SELECT 1 FROM published_views WHERE view_id=?1) AND NOT EXISTS(SELECT 1 FROM index_pins WHERE view_id=?1 AND lease_until>?2)",params![view,now],|r|r.get(0)))
    }
}
const VISIBLE: &str = "WITH layers AS (SELECT baseline_id generation,0 priority FROM index_views WHERE id=?1 UNION ALL SELECT branch_id,1 FROM index_views WHERE id=?1 AND branch_id IS NOT NULL UNION ALL SELECT worktree_id,2 FROM index_views WHERE id=?1 AND worktree_id IS NOT NULL),ranked AS (SELECT u.*,row_number() OVER(PARTITION BY unit_key ORDER BY priority DESC) choice FROM layers l JOIN source_units u ON u.generation_id=l.generation),visible AS MATERIALIZED(SELECT generation_id,unit_key FROM ranked WHERE choice=1 AND action='Replace') ";
impl ProjectRepository for UnitOfWork<'_> {
    fn revision(&self, id: &str) -> Result<i64> {
        db(self
            .tx
            .query_row("SELECT revision FROM projects WHERE id=?", [id], |r| {
                r.get(0)
            }))
    }
    fn rename(&mut self, id: &str, expected: i64, title: &str) -> Result<()> {
        let n = db(self.tx.execute(
            "UPDATE projects SET title=?1,revision=revision+1 WHERE id=?2 AND revision=?3",
            params![title, id, expected],
        ))?;
        if n != 1 {
            return Err(StoreError::RevisionConflict);
        }
        Ok(())
    }
}
impl JournalRepository for UnitOfWork<'_> {
    fn receipt(&self, key: &str) -> Result<Option<String>> {
        db(self.tx.query_row("SELECT payload_digest FROM command_receipts WHERE actor_id='user' AND scope_key='fixture' AND idempotency_key=?",[key],|r|r.get(0)).optional())
    }
    fn record(&mut self, key: &str, project: &str, payload: &str) -> Result<()> {
        db(self.tx.execute(
            "INSERT INTO command_receipts VALUES('user','fixture',?,'FixtureRename',?,NULL,'{}',1)",
            params![key, payload],
        ))?;
        let event = format!("event-{key}");
        db(self.tx.execute(
            "INSERT INTO events VALUES(NULL,?,'user',?,NULL,NULL,'project',?,2,'Changed',1,'{}')",
            params![event, project, project],
        ))?;
        db(self.tx.execute("INSERT INTO queue_entries VALUES(?,'fixture',?,NULL,NULL,1,0,'Ready',NULL,NULL,0,0,0,'{}')",params![format!("queue-{key}"),event]))?;
        Ok(())
    }
}
impl WriteTransaction for UnitOfWork<'_> {
    fn projects(&mut self) -> &mut dyn ProjectRepository {
        self
    }
    fn journal(&mut self) -> &mut dyn JournalRepository {
        self
    }
    fn commit(self: Box<Self>) -> Result<()> {
        db(self.tx.commit())
    }
}
impl ApplicationStore for SqliteStore {
    fn begin_write(&mut self) -> Result<Box<dyn WriteTransaction + '_>> {
        Ok(Box::new(UnitOfWork {
            tx: db(self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate))?,
        }))
    }
    fn pin(&mut self, scope: Scope, view_id: &str, now: i64, expires: i64) -> Result<Pin> {
        if expires <= now {
            return Err(StoreError::ViewUnavailable);
        }
        let source = self.view_source(view_id)?;
        self.scope(&scope, &source)?;
        self.valid_view(view_id)?;
        self.pin_serial += 1;
        let id = format!("pin-{}", self.pin_serial);
        db(self.connection.execute(
            "INSERT INTO index_pins VALUES(?,?,'boot-a',?)",
            params![id, view_id, expires],
        ))?;
        Ok(Pin {
            id,
            view_id: view_id.to_owned(),
            scope,
        })
    }
    fn nodes(&self, p: &Pin, now: i64) -> Result<Vec<String>> {
        self.read_pin(p, now)?;
        let sql = VISIBLE.to_owned()
            + "SELECT n.node_id FROM graph_nodes n JOIN visible v USING(generation_id,unit_key) ORDER BY n.node_id";
        let mut q = db(self.connection.prepare(&sql))?;
        let rows = db(q.query_map([&p.view_id], |r| r.get(0)))?;
        db(rows.collect())
    }
    fn edges(&self, p: &Pin, now: i64) -> Result<Vec<(String, String)>> {
        self.read_pin(p, now)?;
        let sql = VISIBLE.to_owned()
            + ",nodes AS MATERIALIZED(SELECT n.node_id FROM graph_nodes n JOIN visible v USING(generation_id,unit_key)) SELECT e.source_node_id,e.target_node_id FROM graph_edges e JOIN visible v USING(generation_id,unit_key) JOIN nodes a ON a.node_id=e.source_node_id JOIN nodes b ON b.node_id=e.target_node_id ORDER BY e.edge_id";
        let mut q = db(self.connection.prepare(&sql))?;
        let rows = db(q.query_map([&p.view_id], |r| Ok((r.get(0)?, r.get(1)?))))?;
        db(rows.collect())
    }
    fn vector_candidates(
        &self,
        p: &Pin,
        query: &[f32],
        space: &str,
        limit: u16,
        now: i64,
    ) -> Result<Vec<Candidate>> {
        self.read_pin(p, now)?;
        let dims: i64 = db(self.connection.query_row(
            "SELECT dimensions FROM embedding_spaces WHERE id=?",
            [space],
            |r| r.get(0),
        ))?;
        if dims != query.len() as i64
            || query.iter().any(|f| !f.is_finite())
            || limit == 0
            || limit > 500
        {
            return Err(StoreError::Unavailable);
        }
        let metric: String = db(self.connection.query_row(
            "SELECT metric FROM embedding_spaces WHERE id=?",
            [space],
            |r| r.get(0),
        ))?;
        if metric != "L2" {
            return Err(StoreError::Unavailable);
        }
        let bytes: Vec<u8> = query.iter().flat_map(|f| f.to_le_bytes()).collect();
        let sql = VISIBLE.to_owned()
            + ",eligible AS MATERIALIZED(SELECT c.id,v.vector FROM search_chunks c JOIN visible u USING(generation_id,unit_key) JOIN chunk_vectors v ON v.chunk_id=c.id WHERE v.space_id=?2) SELECT id,vec_distance_L2(vector,?3) distance FROM eligible ORDER BY distance,id LIMIT ?4";
        let mut q = db(self.connection.prepare(&sql))?;
        let rows = db(q.query_map(params![p.view_id, space, bytes, limit], |r| {
            Ok(Candidate {
                chunk_id: r.get(0)?,
                distance: r.get(1)?,
            })
        }))?;
        db(rows.collect())
    }
    fn lexical_candidates(&self, p: &Pin, text: &str, now: i64) -> Result<Vec<String>> {
        self.read_pin(p, now)?;
        let sql = VISIBLE.to_owned()
            + "SELECT c.id FROM search_chunks c JOIN visible u USING(generation_id,unit_key) JOIN search_fts f ON f.chunk_id=c.id WHERE search_fts MATCH ?2 ORDER BY c.id";
        let mut q = db(self.connection.prepare(&sql))?;
        let rows = db(q.query_map(
            params![p.view_id, format!("\"{}\"", text.replace('"', "\"\""))],
            |r| r.get(0),
        ))?;
        db(rows.collect())
    }
    fn materialize(&self, p: &Pin, candidates: &[Candidate], now: i64) -> Result<Vec<String>> {
        self.read_pin(p, now)?;
        let sql = VISIBLE.to_owned()
            + "SELECT c.text FROM search_chunks c JOIN visible u USING(generation_id,unit_key) WHERE c.id=?2";
        let mut texts = Vec::new();
        for c in candidates {
            let text: Option<String> = db(self
                .connection
                .query_row(&sql, params![p.view_id, c.chunk_id], |r| r.get(0))
                .optional())?;
            if let Some(t) = text {
                texts.push(t);
            }
        }
        Ok(texts)
    }
    fn publish(
        &mut self,
        source: &str,
        expected_source_revision: i64,
        expected_pointer_revision: i64,
        view: &str,
    ) -> Result<()> {
        self.valid_view(view)?;
        if self.view_source(view)? != source {
            return Err(StoreError::ViewUnavailable);
        }
        let tx = db(self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate))?;
        let rev: i64 = db(tx.query_row(
            "SELECT revision FROM repositories WHERE id=?",
            [source],
            |r| r.get(0),
        ))?;
        if rev != expected_source_revision {
            return Err(StoreError::RevisionConflict);
        }
        let n=db(tx.execute("UPDATE published_views SET view_id=?1,revision=revision+1 WHERE source_key=?2 AND revision=?3",params![view,source,expected_pointer_revision]))?;
        if n != 1 {
            return Err(StoreError::RevisionConflict);
        }
        let event = format!("published-{source}-{}", expected_pointer_revision + 1);
        db(tx.execute("INSERT INTO events VALUES(NULL,?,'user','p',NULL,NULL,'source',?,?, 'ViewPublished',1,'{}')",params![event,source,expected_pointer_revision+1]))?;
        db(tx.execute("INSERT INTO queue_entries VALUES(?,'projection',?,NULL,NULL,1,0,'Ready',NULL,NULL,0,0,0,'{}')",params![format!("delivery-{event}"),event]))?;
        db(tx.commit())
    }
}
