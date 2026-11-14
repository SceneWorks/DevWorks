//! Domain extractors ported from the retained CodeGraph sources (frozen revision): SQL
//! (`SqlExtractor`/`SqlGraphVisitor`), Terraform (`TerraformExtractor`/`HclParser`), Ansible
//! (`AnsibleExtractor`/`AnsibleParser`) and ColdFusion (`ColdFusionExtractor`/
//! `ColdFusionParser`). None of the retained extractors used Tree-sitter: Terraform and ColdFusion
//! are regex scanners (ported with the same expressions), Ansible reads a YAML representation
//! model (built here from the Tree-sitter YAML tree) and SQL walks a T-SQL syntax tree (ported as
//! a T-SQL statement scanner over the same MySQL-normalized text).
//!
//! Output follows the retained graph: node labels, qualified names (without the retained
//! `{repository}.` prefix: units are repository scoped), property names and values, and edge
//! types. The retained extractors addressed both ends of an edge by qualified name; here a target
//! that names a node of the same file is `Local` (`Inferred`, or `Resolved` for containment) and
//! any other target is a `Symbol` (`Unresolved`). A source the file does not define (a role's task
//! file defining tasks for the role node of `meta/main.yml`) is the file node, with the retained
//! source in the edge's `source_qualified_name` property. Retained `UnresolvedCall`s are `CALLS`
//! symbols with their retained confidence and `receiver_type`. Nodes the retained extractors
//! emitted more than once (the pipeline upserted them by qualified name) are merged per label and
//! qualified name, later properties overwriting earlier ones; identical edges are kept once.
pub mod ansible;
pub mod cfml;
pub mod hcl;
pub mod sql;

use devworks_core::ports::indexing::{
    EdgeTarget, ExtractFault, ExtractedEdge, ExtractedNode, FileExtract, Resolution, Span,
};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

use super::treesitter::{MAX_EDGES, MAX_NODES};

/// The retained confidence bands as edge confidences.
fn band(p: &Map<String, Value>) -> Option<f32> {
    let b = p
        .get("confidence_band")
        .or_else(|| p.get("confidence"))
        .and_then(Value::as_str)?;
    Some(match b {
        "high" => 0.9,
        "medium" => 0.6,
        "low" => 0.3,
        _ => return None,
    })
}

/// Line starts of a source, for spans from byte offsets.
pub(crate) struct Lines {
    starts: Vec<usize>,
    len: usize,
}
impl Lines {
    pub(crate) fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        Self {
            starts,
            len: text.len(),
        }
    }
    fn position(&self, byte: usize) -> (u32, u32) {
        let byte = byte.min(self.len);
        let line = self.starts.partition_point(|s| *s <= byte).max(1) - 1;
        (line as u32 + 1, (byte - self.starts[line]) as u32 + 1)
    }
    pub(crate) fn span(&self, start: usize, end: usize) -> Span {
        let (sl, sc) = self.position(start);
        let (el, ec) = self.position(end.max(start));
        Span {
            start_line: sl,
            start_column: sc,
            end_line: el,
            end_column: ec,
            start_byte: start.min(self.len) as u32,
            end_byte: end.max(start).min(self.len) as u32,
        }
    }
}

struct Pending {
    kind: String,
    source: String,
    target: String,
    /// The target is a plain name (never a node of this file): the retained cross-repository
    /// targets (a container, a queue, a URL).
    name_only: bool,
    contain: bool,
    confidence: Option<f32>,
    properties: Map<String, Value>,
}

/// Collects one file's retained output, keyed by qualified name as the retained extractors were.
pub(crate) struct Out<'a> {
    x: &'a mut FileExtract,
    by_key: HashMap<(String, String), usize>,
    by_qn: HashMap<String, usize>,
    edges: Vec<Pending>,
    full: bool,
    stop: &'a (dyn Fn() -> bool + Sync),
    ticks: usize,
}
impl<'a> Out<'a> {
    pub(crate) fn new(x: &'a mut FileExtract, stop: &'a (dyn Fn() -> bool + Sync)) -> Self {
        Self {
            x,
            by_key: HashMap::new(),
            by_qn: HashMap::new(),
            edges: Vec::new(),
            full: false,
            stop,
            ticks: 0,
        }
    }
    /// Polls the stop signal (cooperative cancellation every few hundred steps).
    pub(crate) fn tick(&mut self) -> Result<(), ExtractFault> {
        self.ticks += 1;
        if self.ticks.is_multiple_of(256) && (self.stop)() {
            return Err(ExtractFault::Cancelled);
        }
        Ok(())
    }
    /// The extractor scans the text, not the grammar's tree: the tree's recovered syntax errors
    /// say nothing about this extraction (the retained regex and ScriptDom extractors reported
    /// none).
    pub(crate) fn text_scanner(&mut self) {
        self.x.parse_errors = 0;
    }
    /// Records the file as not (fully) extracted: the retained `ExtractionResult.Failure`.
    pub(crate) fn failed(&mut self, why: String) {
        self.x.incomplete.push(why);
    }
    /// Adds (or merges into) the node `kind`/`qn`; returns its index.
    pub(crate) fn node(
        &mut self,
        kind: &str,
        name: &str,
        qn: &str,
        span: Span,
        parent: Option<usize>,
        properties: Map<String, Value>,
    ) -> Option<usize> {
        if let Some(&i) = self.by_key.get(&(kind.to_owned(), qn.to_owned())) {
            let n = &mut self.x.nodes[i];
            n.properties.extend(properties);
            return Some(i);
        }
        if self.x.nodes.len() >= MAX_NODES {
            self.full = true;
            return None;
        }
        self.x.nodes.push(ExtractedNode {
            kind: kind.into(),
            name: name.chars().take(512).collect(),
            qualified_name: qn.chars().take(2048).collect(),
            span,
            parent: Some(parent.unwrap_or(0)),
            properties,
        });
        let i = self.x.nodes.len() - 1;
        self.by_key.insert((kind.to_owned(), qn.to_owned()), i);
        self.by_qn.entry(qn.to_owned()).or_insert(i);
        Some(i)
    }
    /// Names node `i` by another qualified name (the file node by its retained file name).
    pub(crate) fn alias(&mut self, qn: &str, i: usize) {
        self.by_qn.entry(qn.to_owned()).or_insert(i);
    }
    /// Whether a node with this qualified name exists (any label).
    pub(crate) fn has_qn(&self, qn: &str) -> bool {
        self.by_qn.contains_key(qn)
    }
    /// The index of the first node with this qualified name.
    pub(crate) fn index_of(&self, qn: &str) -> Option<usize> {
        self.by_qn.get(qn).copied()
    }
    /// An edge between qualified names (the retained `PendingEdge`).
    pub(crate) fn edge(&mut self, kind: &str, source: &str, target: &str, p: Map<String, Value>) {
        self.push(kind, source, target, false, false, None, p);
    }
    /// Syntactic containment (`DEFINES`/`DEFINES_METHOD` between nodes of the file).
    pub(crate) fn contain(&mut self, kind: &str, source: &str, target: &str) {
        self.push(kind, source, target, false, true, None, Map::new());
    }
    /// An edge to a plain name the retained extractor targeted (never a node of this file).
    pub(crate) fn name_edge(
        &mut self,
        kind: &str,
        source: &str,
        target: &str,
        p: Map<String, Value>,
    ) {
        self.push(kind, source, target, true, false, None, p);
    }
    /// The retained `UnresolvedCall(caller, callee, receiverType, confidence)`.
    pub(crate) fn unresolved_call(
        &mut self,
        source: &str,
        callee: &str,
        receiver_type: &str,
        confidence: f32,
    ) {
        let p = props(&[("receiver_type", json!(receiver_type))]);
        self.push("CALLS", source, callee, true, false, Some(confidence), p);
    }
    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        kind: &str,
        source: &str,
        target: &str,
        name_only: bool,
        contain: bool,
        confidence: Option<f32>,
        properties: Map<String, Value>,
    ) {
        if self.edges.len() >= MAX_EDGES {
            self.full = true;
            return;
        }
        self.edges.push(Pending {
            kind: kind.into(),
            source: source.into(),
            target: target.into(),
            name_only,
            contain,
            confidence,
            properties,
        });
    }
    /// Resolves the recorded edges against the file's nodes.
    pub(crate) fn finish(self) {
        let mut seen: HashSet<(String, usize, String, String)> = HashSet::new();
        for e in self.edges {
            let mut properties = e.properties;
            let source = match self.by_qn.get(&e.source) {
                Some(i) => *i,
                None => {
                    properties.insert("source_qualified_name".into(), json!(e.source));
                    0
                }
            };
            let local = if e.name_only {
                None
            } else {
                self.by_qn.get(&e.target).copied()
            };
            let (target, resolution) = match local {
                Some(i) if e.contain => (EdgeTarget::Local(i), Resolution::Resolved),
                Some(i) => (EdgeTarget::Local(i), Resolution::Inferred),
                None => {
                    let name = e.target.trim();
                    if name.is_empty() {
                        continue;
                    }
                    // The retained target is a qualified name (`resource.aws_lb.main`,
                    // `role.web`): the indexer links it by exactly that name, never by its last
                    // segment.
                    properties.insert("targetMatch".into(), json!("qualifiedName"));
                    (EdgeTarget::Symbol(name.to_owned()), Resolution::Unresolved)
                }
            };
            let confidence = if e.contain {
                1.0
            } else {
                e.confidence.or_else(|| band(&properties)).unwrap_or(1.0)
            };
            let key = (
                e.kind.clone(),
                source,
                format!("{target:?}"),
                Value::Object(properties.clone()).to_string(),
            );
            if !seen.insert(key) {
                continue;
            }
            self.x.edges.push(ExtractedEdge {
                kind: e.kind,
                source,
                target,
                resolution,
                confidence,
                properties,
            });
        }
        if self.full {
            self.x.parse_errors += 1;
            self.x.nodes[0]
                .properties
                .insert("truncated".into(), json!(true));
        }
    }
}

/// Runs one domain extractor over `x` (whose node 0 is the file node) and resolves its edges.
pub(crate) fn run(
    x: &mut FileExtract,
    stop: &(dyn Fn() -> bool + Sync),
    f: impl FnOnce(&mut Out<'_>) -> Result<(), ExtractFault>,
) -> Result<(), ExtractFault> {
    let mut out = Out::new(x, stop);
    f(&mut out)?;
    out.finish();
    Ok(())
}

pub(crate) fn props(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

/// .NET `Path.GetFileNameWithoutExtension` on a `/`-separated path.
pub(crate) fn file_stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(i) => &name[..i],
        None => name,
    }
}

/// The number of UTF-16 code units of `s` (.NET string lengths).
pub(crate) fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// The first `n` UTF-16 code units of `s` (.NET `s[..n]`, never splitting a character).
pub(crate) fn utf16_prefix(s: &str, n: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        if units + c.len_utf16() > n {
            return &s[..i];
        }
        units += c.len_utf16();
    }
    s
}
