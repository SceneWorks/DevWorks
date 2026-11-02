//! Shared workload, independent result oracle and raw timing capture.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Node {
    pub id: u64,
    pub project: u32,
    pub repo: u32,
    #[serde(default)]
    pub view: u32,
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub vector: Vec<f32>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Edge {
    pub src: u64,
    pub dst: u64,
    pub kind: String,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct Dataset {
    pub manifest: serde_json::Value,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Scope {
    pub project: u32,
    pub view: u32,
}
impl Scope {
    pub fn code(self) -> i64 {
        i64::from(self.project) * 1000 + i64::from(self.view)
    }
    pub fn label(self) -> String {
        format!("Scope_{}_{}", self.project, self.view)
    }
}
impl Node {
    pub fn scope(&self) -> Scope {
        Scope {
            project: self.project,
            view: self.view,
        }
    }
    pub fn key(&self) -> String {
        format!("n{}", self.id)
    }
}

/// Native reader operations. Reachability has a common correct baseline; adapters
/// may override it with equivalent native traversal. Query preparation is outside
/// individual samples and materialized results are consumed before checking.
pub trait Reader {
    fn lookup(&mut self, id: u64, scope: Scope) -> Result<Option<String>>;
    fn neighbors(&mut self, id: u64, scope: Scope, reverse: bool) -> Result<Vec<u64>>;
    fn knn(
        &mut self,
        query: &[f32],
        scope: Scope,
        k: usize,
        allowed: Option<&GraphConstraint>,
    ) -> Result<Vec<u64>>;
    fn reach(&mut self, id: u64, scope: Scope, depth: usize, reverse: bool) -> Result<Vec<u64>> {
        let mut seen = HashSet::from([id]);
        let mut frontier = vec![id];
        for _ in 0..depth {
            let mut next = Vec::new();
            for n in frontier {
                for t in self.neighbors(n, scope, reverse)? {
                    if seen.insert(t) {
                        next.push(t);
                    }
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }
        seen.remove(&id);
        let mut out: Vec<_> = seen.into_iter().collect();
        out.sort_unstable();
        Ok(out)
    }
}
pub struct GraphConstraint {
    pub root: u64,
    pub depth: u32,
    pub ids: Vec<u64>,
}
pub trait Store: Sync {
    fn import(&mut self, data: &Dataset) -> Result<()>;
    fn reader(&self) -> Result<Box<dyn Reader + '_>>;
    fn rename(&self, id: u64, name: &str) -> Result<()>;
    fn delete(&self, id: u64) -> Result<()>;
    fn checkpoint(&self) -> Result<()>;
    fn close(&mut self) -> Result<()>;
    fn details(&self) -> serde_json::Value;
}
pub type Factory = fn(&Path) -> Result<Box<dyn Store>>;

#[derive(Deserialize)]
pub struct RunConfig {
    pub fixture: PathBuf,
    pub database: PathBuf,
    pub output: PathBuf,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default = "default_samples")]
    pub samples: usize,
    #[serde(default = "default_vectors")]
    pub vector_samples: usize,
    #[serde(default)]
    pub overlay: bool,
    #[serde(default)]
    pub action: String,
    pub signal: Option<PathBuf>,
}
fn default_limit() -> usize {
    usize::MAX
}
fn default_samples() -> usize {
    2000
}
fn default_vectors() -> usize {
    80
}

pub struct Oracle {
    pub nodes: HashMap<u64, Node>,
    pub out: HashMap<u64, Vec<u64>>,
    pub incoming: HashMap<u64, Vec<u64>>,
}
impl Oracle {
    pub fn new(data: &Dataset) -> Self {
        let mut o = Self {
            nodes: data.nodes.iter().cloned().map(|n| (n.id, n)).collect(),
            out: HashMap::new(),
            incoming: HashMap::new(),
        };
        for e in &data.edges {
            o.out.entry(e.src).or_default().push(e.dst);
            o.incoming.entry(e.dst).or_default().push(e.src);
        }
        o
    }
    pub fn neighbors(&self, id: u64, scope: Scope, reverse: bool) -> Vec<u64> {
        let map = if reverse { &self.incoming } else { &self.out };
        let mut out: Vec<_> = map
            .get(&id)
            .into_iter()
            .flatten()
            .filter(|x| self.nodes.get(x).is_some_and(|n| n.scope() == scope))
            .copied()
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }
    pub fn reach(&self, id: u64, scope: Scope, depth: usize, reverse: bool) -> Vec<u64> {
        let mut visited = HashSet::from([id]);
        let mut q = VecDeque::from([(id, 0)]);
        while let Some((at, d)) = q.pop_front() {
            if d == depth {
                continue;
            }
            for n in self.neighbors(at, scope, reverse) {
                if visited.insert(n) {
                    q.push_back((n, d + 1));
                }
            }
        }
        visited.remove(&id);
        let mut out: Vec<_> = visited.into_iter().collect();
        out.sort_unstable();
        out
    }
    pub fn knn(&self, query: &[f32], scope: Scope, k: usize, allowed: Option<&[u64]>) -> Vec<u64> {
        let allowed = allowed.map(|v| v.iter().copied().collect::<HashSet<_>>());
        let mut candidates: Vec<_> = self
            .nodes
            .values()
            .filter(|n| n.scope() == scope && allowed.as_ref().is_none_or(|s| s.contains(&n.id)))
            .map(|n| (n.id, cosine_distance(query, &n.vector)))
            .collect();
        candidates.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        candidates.into_iter().take(k).map(|x| x.0).collect()
    }
}
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f64 {
    let mut dot = 0.;
    let mut aa = 0.;
    let mut bb = 0.;
    for (x, y) in a.iter().zip(b) {
        dot += f64::from(*x) * f64::from(*y);
        aa += f64::from(*x).powi(2);
        bb += f64::from(*y).powi(2);
    }
    1. - dot / (aa.sqrt() * bb.sqrt())
}
pub fn sorted(mut v: Vec<u64>) -> Vec<u64> {
    v.sort_unstable();
    v.dedup();
    v
}
fn rss_kib() -> Option<u64> {
    let o = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    String::from_utf8(o.stdout).ok()?.trim().parse().ok()
}
fn disk_bytes(path: &Path) -> u64 {
    if path.is_file() {
        return path.metadata().map(|m| m.len()).unwrap_or(0);
    }
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| disk_bytes(&e.path()))
        .sum()
}
fn measure<F, T>(samples: &mut Vec<u64>, f: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    let t = Instant::now();
    let out = f()?;
    samples.push(t.elapsed().as_nanos().min(u64::MAX as u128) as u64);
    Ok(out)
}
fn summary(samples: &[u64]) -> serde_json::Value {
    if samples.is_empty() {
        return serde_json::json!({"samples":0});
    }
    let mut s = samples.to_vec();
    s.sort_unstable();
    let p = |q: f64| s[((s.len() - 1) as f64 * q).round() as usize];
    serde_json::json!({"samples":s.len(),"median_ns":p(0.5),"p95_ns":p(0.95),"p99_ns":if s.len()>=1000{Some(p(0.99))}else{None},"mean_ns":s.iter().map(|v|*v as f64).sum::<f64>()/s.len()as f64})
}
pub fn subset(mut data: Dataset, limit: usize, overlay: bool) -> Dataset {
    if data.nodes.len() > limit {
        let mut groups: HashMap<u32, Vec<Node>> = HashMap::new();
        for n in data.nodes {
            groups.entry(n.project).or_default().push(n);
        }
        let per = limit / groups.len().max(1);
        data.nodes = groups
            .into_values()
            .flat_map(|v| v.into_iter().take(per))
            .collect();
        data.nodes.sort_by_key(|n| n.id);
        let keep: HashSet<_> = data.nodes.iter().map(|n| n.id).collect();
        data.edges
            .retain(|e| keep.contains(&e.src) && keep.contains(&e.dst));
    }
    if overlay {
        let offset = 1_u64 << 40;
        let mut clones: Vec<_> = data
            .nodes
            .iter()
            .filter(|n| n.project == 0 && n.id % 97 != 0)
            .cloned()
            .collect();
        for n in &mut clones {
            n.id += offset;
            n.view = 1;
            if n.id % 61 == 0 {
                n.name.push_str("_changed");
            }
        }
        let keep: HashSet<_> = clones.iter().map(|n| n.id - offset).collect();
        let edges: Vec<_> = data
            .edges
            .iter()
            .filter(|e| keep.contains(&e.src) && keep.contains(&e.dst))
            .map(|e| Edge {
                src: e.src + offset,
                dst: e.dst + offset,
                kind: e.kind.clone(),
            })
            .collect();
        data.nodes.extend(clones);
        data.edges.extend(edges);
        data.manifest["overlay_fixture"] = serde_json::json!(
            "Materialized second view with deletions/renames; measures snapshot isolation and full-view storage cost, not a delta-optimized implementation"
        );
    }
    // One adversarial edge crosses project scope. Reads/traversals must exclude it.
    if let (Some(a), Some(b)) = (
        data.nodes.iter().find(|n| n.project == 0),
        data.nodes.iter().find(|n| n.project == 1),
    ) {
        data.edges.push(Edge {
            src: a.id,
            dst: b.id,
            kind: "ScopeBoundaryControl".into(),
        });
    }
    if let (Some(bridge), Some(target)) = (
        data.nodes.iter().find(|n| n.project == 1),
        data.nodes
            .iter()
            .rev()
            .find(|n| n.project == 0 && n.view == 0),
    ) {
        data.edges.push(Edge {
            src: bridge.id,
            dst: target.id,
            kind: "ScopeBoundaryControl".into(),
        });
    }
    data
}

pub fn deletion_targets(data: &Dataset) -> Vec<Node> {
    data.nodes
        .iter()
        .step_by((data.nodes.len() / 12).max(1))
        .take(12)
        .cloned()
        .collect()
}

pub fn run(name: &str, factory: Factory) -> Result<()> {
    let config_path = std::env::args()
        .nth(2)
        .context("Use --workload CONFIG.json")?;
    let cfg: RunConfig = serde_json::from_reader(std::fs::File::open(config_path)?)?;
    if cfg.action == "recovery_check" || cfg.action == "interrupted_open" {
        let t = Instant::now();
        let mut db = factory(&cfg.database)?;
        let reopen_ns = t.elapsed().as_nanos();
        let mut checks = 0;
        if cfg.action == "recovery_check" {
            let data: Dataset = serde_json::from_reader(std::io::BufReader::new(
                std::fs::File::open(&cfg.fixture)?,
            ))?;
            let data = subset(data, cfg.limit, false);
            let mut r = db.reader()?;
            for (i, n) in data.nodes.iter().enumerate() {
                let expected = if i == 0 {
                    "acknowledged_before_sigkill"
                } else {
                    &n.name
                };
                ensure!(
                    r.lookup(n.id, n.scope())?.as_deref() == Some(expected),
                    "Crash recovery lost acknowledged value at {}",
                    n.id
                );
                checks += 1;
            }
            for n in data.nodes.iter().step_by(37) {
                let actual = r.knn(&n.vector, n.scope(), 10, None)?;
                ensure!(
                    actual.iter().all(|id| data
                        .nodes
                        .iter()
                        .any(|v| v.id == *id && v.scope() == n.scope())),
                    "Recovered vector scope mismatch"
                );
                checks += 1;
            }
        }
        db.checkpoint()?;
        db.close()?;
        serde_json::to_writer_pretty(
            std::fs::File::create(&cfg.output)?,
            &serde_json::json!({"engine":name,"action":cfg.action,"reopen_ns":reopen_ns,"checks":checks,"real_inference":false}),
        )?;
        return Ok(());
    }
    ensure!(
        !cfg.database.exists(),
        "Refusing existing benchmark database {}",
        cfg.database.display()
    );
    if let Some(p) = cfg.database.parent() {
        std::fs::create_dir_all(p)?;
    }
    if let Some(p) = cfg.output.parent() {
        std::fs::create_dir_all(p)?;
    }
    let data: Dataset =
        serde_json::from_reader(std::io::BufReader::new(std::fs::File::open(&cfg.fixture)?))?;
    let mut data = subset(data, cfg.limit, cfg.overlay);
    let imported_nodes = data.nodes.len();
    let imported_edges = data.edges.len();
    let oracle = Oracle::new(&data);
    let baseline_rss = rss_kib();
    let t = Instant::now();
    let mut db = factory(&cfg.database)?;
    let open_ns = t.elapsed().as_nanos();
    if cfg.action == "interrupted_writer" {
        std::fs::write(
            cfg.signal.as_ref().context("Missing signal path")?,
            b"opened; import/index not acknowledged",
        )?;
        db.import(&data)?;
        // Keep the process alive even if an unusually fast import completes.
        // The controller records whether it killed before or after this marker.
        std::fs::write(
            cfg.signal.as_ref().unwrap().with_extension("finished"),
            b"import finished",
        )?;
        loop {
            std::thread::park();
        }
    }
    let t = Instant::now();
    db.import(&data).context("Import/index readiness")?;
    db.checkpoint()?;
    let import_ns = t.elapsed().as_nanos();
    if cfg.action == "recovery_writer" {
        db.rename(data.nodes[0].id, "acknowledged_before_sigkill")?;
        // The acknowledgment marker is synced after the native write returns,
        // with no explicit engine checkpoint after this final mutation.
        let mut f = std::fs::File::create(cfg.signal.as_ref().context("Missing signal path")?)?;
        std::io::Write::write_all(
            &mut f,
            b"native mutation acknowledged; no subsequent checkpoint",
        )?;
        f.sync_all()?;
        loop {
            std::thread::park();
        }
    }
    let imported_rss = rss_kib();
    let details = db.details();
    let mut raw: HashMap<String, Vec<u64>> = HashMap::new();
    let mut checks = 0_usize;
    let mut failures = Vec::<String>::new();
    let mut vector_quality = Vec::new();
    {
        let mut r = db.reader()?;
        for i in 0..cfg.samples {
            let node = &data.nodes[(i.wrapping_mul(7919) + 17) % data.nodes.len()];
            let actual = measure(raw.entry("lookup".into()).or_default(), || {
                r.lookup(node.id, node.scope())
            })?;
            ensure!(
                actual.as_deref() == Some(node.name.as_str()),
                "Lookup mismatch id {}",
                node.id
            );
            checks += 1;
            if i < cfg.samples.min(500) {
                for reverse in [false, true] {
                    let key = if reverse {
                        "reverse_neighbors"
                    } else {
                        "neighbors"
                    };
                    let actual = measure(raw.entry(key.into()).or_default(), || {
                        r.neighbors(node.id, node.scope(), reverse)
                    })?;
                    ensure!(
                        sorted(actual) == oracle.neighbors(node.id, node.scope(), reverse),
                        "Neighborhood mismatch id {} reverse {reverse}",
                        node.id
                    );
                    checks += 1;
                }
                let actual = measure(raw.entry("reach_depth_3".into()).or_default(), || {
                    r.reach(node.id, node.scope(), 3, false)
                })?;
                ensure!(
                    sorted(actual) == oracle.reach(node.id, node.scope(), 3, false),
                    "Reachability mismatch id {}",
                    node.id
                );
                checks += 1;
            }
        }
        for i in 0..cfg.vector_samples {
            let node = &data.nodes[(i.wrapping_mul(3571) + 31) % data.nodes.len()];
            for scoped in [false, true] {
                let allow = if scoped {
                    let mut a = oracle.reach(node.id, node.scope(), 3, false);
                    a.push(node.id);
                    Some(GraphConstraint {
                        root: node.id,
                        depth: 3,
                        ids: a,
                    })
                } else {
                    None
                };
                let expected = oracle.knn(
                    &node.vector,
                    node.scope(),
                    10,
                    allow.as_ref().map(|a| a.ids.as_slice()),
                );
                let key = if scoped {
                    "knn_graph_scope"
                } else {
                    "knn_project_view_scope"
                };
                let result = measure(raw.entry(key.into()).or_default(), || {
                    r.knn(&node.vector, node.scope(), 10, allow.as_ref())
                });
                match result {
                    Ok(actual) => {
                        ensure!(
                            actual.iter().all(|id| oracle
                                .nodes
                                .get(id)
                                .is_some_and(|n| n.scope() == node.scope())),
                            "Vector result leaked project/view"
                        );
                        ensure!(
                            allow
                                .as_ref()
                                .is_none_or(|a| actual.iter().all(|id| a.ids.contains(id))),
                            "Vector result leaked graph scope"
                        );
                        ensure!(
                            sorted(actual.clone()).len() == actual.len(),
                            "Vector results contain duplicate IDs"
                        );
                        let hit = actual.iter().filter(|id| expected.contains(id)).count();
                        vector_quality.push(serde_json::json!({"case":key,"query":node.id,"expected":expected.len(),"returned":actual.len(),"recall":hit as f64/expected.len().max(1)as f64}));
                        checks += 1;
                    }
                    Err(e) => {
                        failures.push(format!("{key}: {e:#}"));
                        break;
                    }
                }
            }
        }
    }
    {
        let root = data
            .nodes
            .iter()
            .find(|n| n.project == 0 && n.view == 0)
            .unwrap();
        let query = data
            .nodes
            .iter()
            .rev()
            .find(|n| n.project == 0 && n.view == 0)
            .unwrap();
        let mut permitted = oracle.reach(root.id, root.scope(), 3, false);
        permitted.push(root.id);
        let allow = GraphConstraint {
            root: root.id,
            depth: 3,
            ids: permitted,
        };
        let got = db
            .reader()?
            .knn(&query.vector, root.scope(), 10, Some(&allow))?;
        if got.iter().any(|id| !allow.ids.contains(id)) {
            failures.push(format!(
                "Graph KNN traversed a foreign-project bridge: root={} query={} results={got:?}",
                root.id, query.id
            ));
        }
        checks += 1;
    }
    // The same auxiliary SQLite FTS5 file and RRF implementation are used for
    // all candidates. Text is fixture symbol names, not semantic RAG evidence.
    let mut lexical = rusqlite::Connection::open(cfg.database.join("shared-lexical.sqlite"))?;
    lexical.execute_batch("PRAGMA journal_mode=WAL;PRAGMA synchronous=FULL;CREATE VIRTUAL TABLE texts USING fts5(gid UNINDEXED,scope UNINDEXED,text);")?;
    {
        let tx = lexical.transaction()?;
        {
            let mut statement = tx.prepare("INSERT INTO texts(gid,scope,text) VALUES(?,?,?)")?;
            for n in &data.nodes {
                statement.execute(rusqlite::params![n.id as i64, n.scope().code(), n.name])?;
            }
        }
        tx.commit()?;
    }
    let mut hybrid_quality = Vec::new();
    {
        let mut r = db.reader()?;
        for i in 0..cfg.vector_samples {
            let n = &data.nodes[(i * 3571 + 31) % data.nodes.len()];
            let token = n
                .name
                .split(|c: char| !c.is_alphanumeric())
                .find(|s| !s.is_empty())
                .unwrap_or("symbol");
            let query = format!("\"{}\"", token.replace('"', "\"\""));
            let t = Instant::now();
            let ids:Vec<u64>=lexical.prepare_cached("SELECT gid FROM texts WHERE texts MATCH ? AND scope=? ORDER BY rank,gid LIMIT 20")?.query_map(rusqlite::params![query,n.scope().code()],|r|r.get::<_,i64>(0).map(|x|x as u64))?.collect::<rusqlite::Result<_>>()?;
            let vectors = r.knn(&n.vector, n.scope(), 20, None)?;
            let mut scores = HashMap::<u64, f64>::new();
            for list in [&ids, &vectors] {
                for (rank, id) in list.iter().enumerate() {
                    *scores.entry(*id).or_default() += 1. / (60. + rank as f64 + 1.);
                }
            }
            let mut ranked: Vec<_> = scores.into_iter().collect();
            ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            ranked.truncate(10);
            raw.entry("hybrid_fts_vector_rrf".into())
                .or_default()
                .push(t.elapsed().as_nanos() as u64);
            ensure!(
                ranked
                    .iter()
                    .all(|(id, _)| oracle.nodes.get(id).is_some_and(|x| x.scope() == n.scope())),
                "Hybrid scope leak"
            );
            // Provenance comes from the frozen source manifest, independently
            // of native engine IDs; every result must resolve to its locator.
            ensure!(
                ranked.iter().all(|(id, _)| oracle
                    .nodes
                    .get(id)
                    .is_some_and(|x| !x.file.is_empty() || x.kind == "Repository")),
                "Hybrid missing source locator"
            );
            hybrid_quality.push(serde_json::json!({"query":n.id,"lexical_candidates":ids.len(),"vector_candidates":vectors.len(),"results":ranked.len()}));
            checks += 2;
        }
    }
    drop(lexical);
    // Independent feature caps: ten readers and twenty readers, not a global cap.
    for workers in [10_usize, 20] {
        let t = Instant::now();
        let results = std::thread::scope(|s| {
            let handles: Vec<_> = (0..workers)
                .map(|worker| {
                    let db = &*db;
                    let data = &data;
                    s.spawn(move || -> Result<Vec<u64>> {
                        let mut r = db.reader()?;
                        let mut samples = Vec::new();
                        for j in 0..100 {
                            let n = &data.nodes[(worker * 137 + j * 7919) % data.nodes.len()];
                            let found = measure(&mut samples, || r.lookup(n.id, n.scope()))?;
                            ensure!(
                                found.as_deref() == Some(n.name.as_str()),
                                "Concurrent lookup mismatch"
                            );
                        }
                        Ok(samples)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().map_err(|_| anyhow::anyhow!("Reader panicked"))?)
                .collect::<Result<Vec<_>>>()
        })?;
        let elapsed = t.elapsed().as_nanos();
        let samples: Vec<_> = results.into_iter().flatten().collect();
        checks += samples.len();
        raw.insert(
            format!("concurrent_{workers}_wall_ns"),
            vec![elapsed as u64],
        );
        raw.insert(format!("concurrent_{workers}_lookup"), samples);
    }
    // An aggregate/health baseline: scan native adjacency to compute a scoped
    // degree histogram. This is application-owned analysis, not a native OLAP claim.
    {
        let mut r = db.reader()?;
        let t = Instant::now();
        let mut histogram = std::collections::BTreeMap::<usize, usize>::new();
        for n in &data.nodes {
            *histogram
                .entry(r.neighbors(n.id, n.scope(), false)?.len())
                .or_default() += 1;
        }
        raw.insert(
            "health_degree_histogram_wall_ns".into(),
            vec![t.elapsed().as_nanos() as u64],
        );
        let mut expected = std::collections::BTreeMap::<usize, usize>::new();
        for n in &data.nodes {
            *expected
                .entry(oracle.neighbors(n.id, n.scope(), false).len())
                .or_default() += 1;
        }
        ensure!(histogram == expected, "Health aggregate mismatch");
        checks += 1;
    }
    // Write to a reserved node while readers query other nodes. A barrier starts
    // all workers together. Each rename is acknowledged individually; no sleeps.
    for workers in [10_usize, 20] {
        let target = &data.nodes[0];
        let barrier = std::sync::Barrier::new(workers + 1);
        let t = Instant::now();
        let (reads, writes) = std::thread::scope(|s| -> Result<_> {
            let db = &*db;
            let data = &data;
            let barrier = &barrier;
            let writer = s.spawn(move || -> Result<Vec<u64>> {
                barrier.wait();
                let mut samples = Vec::new();
                for j in 0..100 {
                    measure(&mut samples, || {
                        db.rename(target.id, &format!("mutation_{j}"))
                    })?;
                }
                Ok(samples)
            });
            let handles: Vec<_> = (0..workers)
                .map(|worker| {
                    s.spawn(move || -> Result<Vec<u64>> {
                        let mut reader = db.reader()?;
                        let mut samples = Vec::new();
                        barrier.wait();
                        for j in 0..100 {
                            let n =
                                &data.nodes[1 + (worker * 137 + j * 7919) % (data.nodes.len() - 1)];
                            ensure!(
                                measure(&mut samples, || reader.lookup(n.id, n.scope()))?
                                    .as_deref()
                                    == Some(n.name.as_str()),
                                "Reader/writer visibility mismatch"
                            );
                        }
                        Ok(samples)
                    })
                })
                .collect();
            let reads = handles
                .into_iter()
                .map(|h| h.join().map_err(|_| anyhow::anyhow!("Reader panic"))?)
                .collect::<Result<Vec<_>>>()?;
            Ok((
                reads.into_iter().flatten().collect::<Vec<_>>(),
                writer
                    .join()
                    .map_err(|_| anyhow::anyhow!("Writer panic"))??,
            ))
        })?;
        raw.insert(
            format!("reader_writer_{workers}_wall_ns"),
            vec![t.elapsed().as_nanos() as u64],
        );
        checks += reads.len() + writes.len();
        raw.insert(format!("reader_writer_{workers}_lookup"), reads);
        raw.insert(format!("reader_writer_{workers}_rename"), writes);
        ensure!(
            db.reader()?.lookup(target.id, target.scope())?.as_deref() == Some("mutation_99"),
            "Acknowledged rename not visible"
        );
        db.rename(target.id, &target.name)?;
    }
    // Delete records, including vector rows and incident edges; verify queries
    // against the independently edited fixture before and after durable reopen.
    let deleted = deletion_targets(&data);
    for n in &deleted {
        measure(raw.entry("incremental_delete".into()).or_default(), || {
            db.delete(n.id)
        })?;
    }
    let deleted_ids: HashSet<_> = deleted.iter().map(|n| n.id).collect();
    data.nodes.retain(|n| !deleted_ids.contains(&n.id));
    data.edges
        .retain(|e| !deleted_ids.contains(&e.src) && !deleted_ids.contains(&e.dst));
    let edited = Oracle::new(&data);
    db.checkpoint()?;
    {
        let mut r = db.reader()?;
        for n in &deleted {
            ensure!(
                r.lookup(n.id, n.scope())?.is_none(),
                "Deleted point remained visible"
            );
            checks += 1;
        }
        for n in &data.nodes {
            ensure!(
                sorted(r.neighbors(n.id, n.scope(), false)?)
                    == edited.neighbors(n.id, n.scope(), false),
                "Deleted incident edge remained visible"
            );
            checks += 1;
        }
        for n in deleted.iter().take(3) {
            let got = r.knn(&n.vector, n.scope(), 10, None)?;
            let stale: Vec<_> = got
                .iter()
                .filter(|id| deleted_ids.contains(id))
                .copied()
                .collect();
            if !stale.is_empty() {
                failures.push(format!(
                    "Deleted vector remained visible: query={} stale={stale:?}",
                    n.id
                ));
            }
            checks += 1;
        }
    }
    let post_query_rss = rss_kib();
    let t = Instant::now();
    db.close()?;
    let close_ns = t.elapsed().as_nanos();
    drop(db);
    let t = Instant::now();
    let mut reopened = factory(&cfg.database).context("Graceful reopen")?;
    let reopen_ns = t.elapsed().as_nanos();
    {
        let mut r = reopened.reader()?;
        for n in data.nodes.iter().step_by((data.nodes.len() / 100).max(1)) {
            ensure!(
                r.lookup(n.id, n.scope())?.as_deref() == Some(n.name.as_str()),
                "Reopen mismatch"
            );
            checks += 1;
        }
    }
    {
        let mut r = reopened.reader()?;
        for n in deleted.iter().take(3) {
            let got = r.knn(&n.vector, n.scope(), 10, None)?;
            let stale: Vec<_> = got
                .iter()
                .filter(|id| deleted_ids.contains(id))
                .copied()
                .collect();
            if !stale.is_empty() {
                failures.push(format!(
                    "Deleted vector remained visible after reopen: query={} stale={stale:?}",
                    n.id
                ));
            }
            checks += 1;
        }
    }
    reopened.close()?;
    let summaries: HashMap<_, _> = raw.iter().map(|(k, v)| (k.clone(), summary(v))).collect();
    let report = serde_json::json!({"engine":name,"manifest":data.manifest,"nodes":imported_nodes,"edges":imported_edges,"post_delete_nodes":data.nodes.len(),"vector_dimensions":384,"open_ns":open_ns,"import_and_index_ready_ns":import_ns,"close_ns":close_ns,"reopen_ns":reopen_ns,"baseline_rss_kib":baseline_rss,"imported_rss_kib":imported_rss,"post_query_rss_kib":post_query_rss,"database_logical_bytes":disk_bytes(&cfg.database),"details":details,"correctness_checks":checks,"case_errors":failures,"vector_quality":vector_quality,"hybrid_quality":hybrid_quality,"summary":summaries,"raw_ns":raw,"real_inference":false});
    serde_json::to_writer_pretty(std::fs::File::create(&cfg.output)?, &report)?;
    println!(
        "{}",
        serde_json::json!({"engine":name,"output":cfg.output,"nodes":data.nodes.len(),"checks":checks,"errors":report["case_errors"]})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn node(id: u64, p: u32, v: Vec<f32>) -> Node {
        Node {
            id,
            project: p,
            repo: 0,
            view: 0,
            name: id.to_string(),
            kind: "fixture".into(),
            file: "".into(),
            line: 0,
            vector: v,
        }
    }
    #[test]
    fn scopes_exclude_cross_project_paths_and_vector_ties() {
        let d = Dataset {
            manifest: serde_json::json!({}),
            nodes: vec![
                node(1, 0, vec![1., 0.]),
                node(2, 1, vec![1., 0.]),
                node(3, 0, vec![0., 1.]),
            ],
            edges: vec![
                Edge {
                    src: 1,
                    dst: 2,
                    kind: "x".into(),
                },
                Edge {
                    src: 2,
                    dst: 3,
                    kind: "x".into(),
                },
            ],
        };
        let o = Oracle::new(&d);
        let s = Scope {
            project: 0,
            view: 0,
        };
        assert!(o.reach(1, s, 5, false).is_empty());
        assert_eq!(o.knn(&[1., 0.], s, 2, None), vec![1, 3]);
        assert_eq!(o.knn(&[1., 0.], s, 2, Some(&[3])), vec![3]);
    }
    #[test]
    fn cyclic_reachability_is_finite_and_deduplicated() {
        let d = Dataset {
            manifest: serde_json::json!({}),
            nodes: vec![node(1, 0, vec![1.]), node(2, 0, vec![1.])],
            edges: vec![
                Edge {
                    src: 1,
                    dst: 2,
                    kind: "x".into(),
                },
                Edge {
                    src: 2,
                    dst: 1,
                    kind: "x".into(),
                },
            ],
        };
        assert_eq!(
            Oracle::new(&d).reach(
                1,
                Scope {
                    project: 0,
                    view: 0
                },
                20,
                false
            ),
            vec![2]
        );
    }
    #[test]
    fn sparse_tail_samples_do_not_claim_p99() {
        assert!(summary(&[10, 20, 30])["p99_ns"].is_null());
    }
}
