//! Cross-unit link generation and view linking.
use super::*;

/// Unit keys already staged in a generation.
pub(crate) fn staged_keys(
    env: &IndexEnv,
    generation: &Id,
) -> Result<BTreeSet<String>, StorageError> {
    let mut out = BTreeSet::new();
    let mut after: Option<String> = None;
    loop {
        let page = read_snapshot(&*env.store, &ReadOptions::default(), |s| {
            s.indexing().units(generation, after.as_deref(), MAX_PAGE)
        })?;
        for u in &page {
            out.insert(u.unit_key.clone());
        }
        if page.len() < MAX_PAGE as usize {
            break;
        }
        after = page.last().map(|u| u.unit_key.clone());
    }
    Ok(out)
}

/// A unit's content digest, mode and language.
pub(crate) type UnitFacts = (String, String, Option<String>, bool);

/// The digest, mode and language of every unit of a sealed generation.
pub(crate) fn unit_index(
    env: &IndexEnv,
    generation: &Id,
) -> Result<BTreeMap<String, UnitFacts>, StorageError> {
    let mut out = BTreeMap::new();
    let mut after: Option<String> = None;
    loop {
        let page = read_snapshot(&*env.store, &ReadOptions::default(), |s| {
            s.indexing().units(generation, after.as_deref(), MAX_PAGE)
        })?;
        for u in &page {
            out.insert(
                u.unit_key.clone(),
                (
                    u.content_digest.clone(),
                    u.payload
                        .get("mode")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    u.payload
                        .get("language")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    u.payload.get("sourceCapture").is_some(),
                ),
            );
        }
        if page.len() < MAX_PAGE as usize {
            break;
        }
        after = page.last().map(|u| u.unit_key.clone());
    }
    Ok(out)
}

/// The edge kinds linked by name, with the node kinds that may be their target.
pub(super) fn target_kinds(edge: &str) -> Option<&'static [&'static str]> {
    Some(match edge {
        // A callable, or a type the call constructs (the retained `CallLanguages.IsCallTarget`).
        "CALLS" => &[
            "Function",
            "Method",
            "Constructor",
            "Delegate",
            "ExternalSymbol",
            "Class",
            "Struct",
            "Record",
            "Enum",
            "Component",
            "Service",
            "Job",
        ],
        "INHERITS" | "IMPLEMENTS" | "USES_TYPE" => &[
            "Class",
            "Interface",
            "Struct",
            "Enum",
            "Record",
            "Trait",
            "Module",
        ],
        "IMPORTS" => &["File", "Module", "Namespace"],
        "HANDLES" => &["Function", "Method"],
        _ => return None,
    })
}

/// The nodes of unit `key` as the layers compose it (`None`: absent or deleted).
pub(super) fn layered_unit_nodes(
    s: &dyn IndexRead,
    layers: &[Id],
    key: &str,
) -> Result<Option<Vec<StoredNode>>, StorageError> {
    for g in layers {
        if let Some(u) = s.unit(g, key)? {
            if u.action != UNIT_REPLACE {
                return Ok(None);
            }
            return Ok(Some(s.nodes(g, key)?));
        }
    }
    Ok(None)
}

/// The package directory a Go import names, from the nearest `go.mod` above `from` (its module
/// path must be the import's prefix): `Err` when that manifest has no unique module identity.
pub(super) fn go_import_dir(
    s: &dyn IndexRead,
    layers: &[Id],
    from: &str,
    import: &str,
) -> Result<Result<Option<String>, String>, StorageError> {
    let mut dir = parent(from).to_owned();
    loop {
        let manifest = if dir.is_empty() {
            "go.mod".to_owned()
        } else {
            format!("{dir}/go.mod")
        };
        if let Some(nodes) = layered_unit_nodes(s, layers, &manifest)? {
            let module = nodes.iter().find(|n| n.kind == "GoModule");
            let Some(module) = module.and_then(|m| {
                m.payload["properties"]["module"]
                    .as_str()
                    .map(str::to_owned)
            }) else {
                return Ok(Err(format!("{manifest} has no unique module identity")));
            };
            if import == module || import.starts_with(&format!("{module}/")) {
                let rest = import[module.len()..].trim_matches('/');
                let joined = [dir.as_str(), rest]
                    .iter()
                    .filter(|p| !p.is_empty())
                    .copied()
                    .collect::<Vec<_>>()
                    .join("/");
                return Ok(Ok(Some(joined)));
            }
            return Ok(Ok(None));
        }
        if dir.is_empty() {
            return Ok(Ok(None));
        }
        dir = parent(&dir).to_owned();
    }
}

/// Link candidates: (unit key, node kind, node ID).
pub(super) type Candidates = Vec<(String, String, String)>;

/// Name lookups of one link page: the candidates and whether the lookup was capped.
pub(super) type NameCache = HashMap<String, (Candidates, bool)>;

/// Narrows a HANDLES edge's name candidates to the scopes its extractor recorded: exact units
/// (each optionally limited to definition lines) and package directories (Go files directly in
/// them, including the one a module-relative import names).
pub(super) fn handler_scope(
    s: &dyn IndexRead,
    layers: &[Id],
    unit: &str,
    payload: &Value,
    all: Candidates,
) -> Result<(Candidates, Option<String>), StorageError> {
    let units: Vec<&str> = payload["candidateUnits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut dirs: Vec<String> = payload["candidateDirs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let mut problem = None;
    if let Some(import) = payload["goImport"].as_str() {
        match go_import_dir(s, layers, unit, import)? {
            Ok(Some(d)) => dirs.push(d),
            Ok(None) => {}
            Err(why) => problem = Some(why),
        }
    }
    let lines = payload["candidateLines"].as_object();
    let mut out = Vec::new();
    for c in all {
        let in_unit = units.contains(&c.0.as_str());
        let in_dir = c.0.ends_with(".go") && dirs.iter().any(|d| parent(&c.0) == d);
        if !in_unit && !in_dir {
            continue;
        }
        if let Some(allowed) = lines.and_then(|l| l.get(&c.0)).and_then(Value::as_array) {
            let nodes = layered_unit_nodes(s, layers, &c.0)?.unwrap_or_default();
            let start = nodes
                .iter()
                .find(|n| n.node_id == c.2)
                .and_then(|n| n.payload["span"]["startLine"].as_i64());
            if !start.is_some_and(|l| allowed.iter().any(|a| a.as_i64() == Some(l))) {
                continue;
            }
        }
        if !out.iter().any(|o: &(String, String, String)| o.2 == c.2) {
            out.push(c);
        }
    }
    Ok((out, problem))
}

/// C and C++ header suffixes an include names.
pub(super) const HEADER_SUFFIXES: [&str; 6] = [".h", ".hh", ".hpp", ".hxx", ".h++", ".inl"];

/// Candidate file paths an import string names, relative to the importing file.
pub(super) fn import_candidates(from: &str, module: &str) -> Vec<String> {
    let dir = parent(from);
    let mut out = Vec::new();
    let join = |base: &str, rel: &str| -> Option<String> {
        let mut parts: Vec<&str> = if base.is_empty() {
            Vec::new()
        } else {
            base.split('/').collect()
        };
        for seg in rel.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    parts.pop()?;
                }
                s => parts.push(s),
            }
        }
        Some(parts.join("/"))
    };
    if from.ends_with(".py") {
        // Python 3 bare/dotted imports are absolute. Leading dots explicitly select
        // the importing package (one dot), then its parents; never guess a sibling.
        let dots = module.bytes().take_while(|b| *b == b'.').count();
        if dots > 0 && (dir.is_empty() || dots > dir.split('/').count()) {
            return out;
        }
        let body = module[dots..].replace('.', "/");
        let path = if dots == 0 {
            Some(body)
        } else {
            join(dir, &format!("{}{}", "../".repeat(dots - 1), body))
        };
        if let Some(path) = path.filter(|p| !p.is_empty()) {
            // A regular package takes precedence over a same-named module file.
            out.push(format!("{path}/__init__.py"));
            if !module.ends_with('.') {
                out.push(format!("{path}.py"));
            }
        }
        return out;
    }
    if module.starts_with("./") || module.starts_with("../") {
        if let Some(p) = join(dir, module) {
            for ext in [
                "",
                ".ts",
                ".tsx",
                ".js",
                ".jsx",
                ".mjs",
                ".cjs",
                "/index.ts",
                "/index.tsx",
                "/index.js",
            ] {
                out.push(format!("{p}{ext}"));
            }
        }
    } else if HEADER_SUFFIXES.iter().any(|s| module.ends_with(s)) {
        // A C/C++ include (checked before dotted modules: `foo.h` is not a module path).
        if let Some(p) = join(dir, module) {
            out.push(p);
        }
        out.push(module.to_owned());
    } else if module.contains('.') && !module.contains('/') && !module.contains(' ') {
        // Python and Java style dotted modules.
        let p = module.replace('.', "/");
        out.push(format!("{p}.py"));
        out.push(format!("{p}/__init__.py"));
        out.push(format!("{p}.java"));
        if let Some(p) = join(dir, &p) {
            out.push(format!("{p}.py"));
        }
    } else {
        if let Some(p) = join(dir, module) {
            out.push(format!("{p}.py"));
        }
        out.push(format!("{module}.py"));
    }
    out
}

/// Links unresolved references of `generation` by name. Candidates come from `generation` and
/// then from `lower` layers (an overlay's branch and baseline), skipping units a higher layer
/// replaced or deleted. A unique candidate makes the edge `Inferred`; none or several leave it
/// `Unresolved` with the candidate count. A call whose resolved receiver names no type of the
/// index is bound by its name alone: `Inferred` with `resolutionBasis: name` and the retained
/// unresolved-receiver confidence (CODE-02: such a binding is identifiable).
///
/// An edge a Swift extension of a type declared in another file contributes (its members'
/// `DEFINES`/`DEFINES_METHOD`, its `INHERITS`/`IMPLEMENTS`) starts at the extension's file node
/// with the extended type's reference in `source_qualified_name`; once that path names exactly
/// one Swift type, the edge's source is that type (`sourceUnit`, `sourceResolution: Inferred`),
/// otherwise it stays at the file (`sourceResolution: Unresolved`, `sourceCandidates`). The edge
/// keeps its ID and unit: only its endpoints are linked, as its target is.
pub(crate) fn link_generation(
    exec: &mut Exec<'_>,
    generation: &Id,
    lower: &[Id],
    stop: &(dyn Fn() -> bool + Sync),
) -> Result<(), Stop> {
    let env = exec.w.env.clone();
    let mut after: Option<String> = None;
    // Loaded on the first Swift call that needs a module tie-break; the modules and imports it
    // reads are not changed by linking.
    let mut scopes: Option<SwiftScopes> = None;
    loop {
        if stop() {
            return Err(Stop::Cancelled);
        }
        let (g, low) = (generation.clone(), lower.to_vec());
        let repo = exec.run.repository_id.clone();
        let a = after.clone();
        let scopes = &mut scopes;
        let (updates, last, full) = read_snapshot(&*env.store, &ReadOptions::default(), move |s| {
            let units = s.indexing().units(&g, a.as_deref(), 200)?;
            let full = units.len() == 200;
            let last = units.last().map(|u| u.unit_key.clone());
            let mut updates = Vec::new();
            let mut cache: NameCache = HashMap::new();
            for u in &units {
                for e in s.indexing().edges(&g, &u.unit_key)? {
                    let mut payload = e.payload.clone();
                    let source = swift_extension_source(
                        s.indexing(),
                        &g,
                        &low,
                        repo.as_str(),
                        &u.unit_key,
                        &e,
                        &mut payload,
                    )?;
                    let target = link_target(
                        s.indexing(),
                        &g,
                        &low,
                        repo.as_str(),
                        &u.unit_key,
                        &e,
                        &mut payload,
                        &mut cache,
                        scopes,
                    )?
                    .unwrap_or_else(|| e.target_node_id.clone());
                    if payload != e.payload
                        || target != e.target_node_id
                        || source != e.source_node_id
                    {
                        updates.push(EdgeTargetUpdate {
                            unit_key: e.unit_key.clone(),
                            edge_id: e.edge_id.clone(),
                            source_node_id: source,
                            target_node_id: target,
                            payload,
                        });
                    }
                }
            }
            Ok((updates, last, full))
        })
        .map_err(Stop::storage)?;
        for chunk in updates.chunks(2000) {
            let (g, chunk) = (generation.clone(), chunk.to_vec());
            exec.fenced(WritePriority::Bulk, move |tx, _| {
                tx.indexing().set_edge_targets(&g, &chunk)
            })?;
        }
        if !full {
            return Ok(());
        }
        after = last;
    }
}

/// Re-evaluates inherited edges against the complete immutable composed view. The
/// generation linker uses these same name, receiver, import and Swift-module rules.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn link_view_edges(
    read: &dyn IndexRead,
    repository: &str,
    layers: &[Id],
    unit: &str,
    edges: Vec<StoredEdge>,
) -> Result<Vec<StoredEdge>, StorageError> {
    ViewLinker::default().link(read, repository, layers, unit, edges)
}

/// An index read of one view's layers with its whole-unit masking: a unit's nodes and edges are
/// read only from the highest layer that mentions the unit, and only when that layer replaces
/// it. This is what the graph reader links an inherited edge against.
pub(super) struct Masked<'a> {
    read: &'a dyn IndexRead,
    layers: &'a [Id],
}

impl Masked<'_> {
    fn visible_in(&self, generation: &Id, unit_key: &str) -> Result<bool, StorageError> {
        for layer in self.layers {
            if let Some(unit) = self.read.unit(layer, unit_key)? {
                return Ok(layer == generation && unit.action == UNIT_REPLACE);
            }
        }
        Ok(false)
    }
}

macro_rules! forward_index_reads {
    ($($name:ident($($arg:ident:$ty:ty),*) -> $out:ty;)*) => {$ (
        fn $name(&self, $($arg:$ty),*) -> Result<$out, StorageError> { self.read.$name($($arg),*) }
    )*};
}

impl IndexRead for Masked<'_> {
    forward_index_reads! {
        source_config(scope_key:&str) -> Option<StoredSourceConfig>;
        repository_source(repository:&Id) -> Option<StoredRepositorySource>;
        repository_at(location:&str) -> Option<Id>;
        worktrees(repository:&Id) -> Vec<StoredWorktree>;
        run(id:&Id) -> Option<StoredIndexRun>;
        runs(filter:&IndexRunFilter,after:Option<&RunPosition>,limit:u16) -> Vec<StoredIndexRun>;
        active_run(repository:&Id,overlay:Option<&Id>) -> Option<StoredIndexRun>;
        batch_runs(batch:&Id) -> Vec<StoredIndexRun>;
        latest_run(repository:&Id,overlay:Option<&Id>) -> Option<StoredIndexRun>;
        claimable_runs(now_ms:i64,limit:u16) -> Vec<StoredIndexRun>;
        running_runs(limit:u16) -> Vec<StoredIndexRun>;
        generation(id:&Id) -> Option<StoredGeneration>;
        generations(repository:&Id) -> Vec<StoredGeneration>;
        generation_in_view(generation:&Id) -> bool;
        generations_in_state(state:&str,limit:u16) -> Vec<StoredGeneration>;
        units(generation:&Id,after:Option<&str>,limit:u16) -> Vec<StoredUnit>;
        unit(generation:&Id,unit_key:&str) -> Option<StoredUnit>;
        nodes_named(generation:&Id,name:&str,limit:u16) -> Vec<StoredNode>;
        nodes_qualified(generation:&Id,qualified_name:&str,limit:u16) -> Vec<StoredNode>;
        nodes_qualified_after(generation:&Id,qualified_name:&str,after:Option<(&str,&str)>,limit:u16) -> Vec<StoredNode>;
        published(repository:&Id) -> Option<StoredPublished>;
        view(view:&Id) -> Option<StoredView>;
        live_pins(view:&Id,now_ms:i64) -> i64;
        overlay(id:&Id) -> Option<StoredOverlay>;
        overlay_for(repository:&Id,worktree:&Id) -> Option<StoredOverlay>;
        overlay_at(repository:&Id,commit:&str) -> Option<StoredOverlay>;
        overlays(repository:&Id) -> Vec<StoredOverlay>;
        overlays_due(now_ms:i64,limit:u16) -> Vec<StoredOverlay>;
        prunable_ref_overlays(ended_before_ms:i64,keep:u32,limit:u16) -> Vec<StoredOverlay>;
    }
    fn nodes(&self, generation: &Id, unit_key: &str) -> Result<Vec<StoredNode>, StorageError> {
        if self.visible_in(generation, unit_key)? {
            self.read.nodes(generation, unit_key)
        } else {
            Ok(Vec::new())
        }
    }
    fn edges(&self, generation: &Id, unit_key: &str) -> Result<Vec<StoredEdge>, StorageError> {
        if self.visible_in(generation, unit_key)? {
            self.read.edges(generation, unit_key)
        } else {
            Ok(Vec::new())
        }
    }
}

/// The links of an overlay view of `layers` (highest first): every edge of a visible unit that
/// the graph reader relinks against the composed view (one of this repository, or naming no
/// other one) and whose endpoints or payload that changes, with the layer that stores it.
/// Empty for a single-layer view, whose stored edges are already its links.
pub fn view_links(
    read: &dyn IndexRead,
    repository: &Id,
    layers: &[Id],
) -> Result<Vec<ViewLink>, StorageError> {
    if layers.len() < 2 {
        return Ok(Vec::new());
    }
    // The highest layer that mentions each unit decides it.
    let mut units = BTreeMap::<String, Option<Id>>::new();
    for layer in layers {
        let mut after: Option<String> = None;
        loop {
            let page = read.units(layer, after.as_deref(), MAX_PAGE)?;
            for unit in &page {
                units
                    .entry(unit.unit_key.clone())
                    .or_insert_with(|| (unit.action == UNIT_REPLACE).then(|| layer.clone()));
            }
            if page.len() < usize::from(MAX_PAGE) {
                break;
            }
            after = page.last().map(|u| u.unit_key.clone());
        }
    }
    let masked = Masked { read, layers };
    let mut linker = ViewLinker::default();
    let mut links = Vec::new();
    for (unit, layer) in units {
        let Some(layer) = layer else {
            continue;
        };
        let stored: Vec<StoredEdge> = read
            .edges(&layer, &unit)?
            .into_iter()
            .filter(|e| {
                e.payload
                    .get("targetRepositoryId")
                    .is_none_or(|id| id.as_str() == Some(repository.as_str()))
            })
            .collect();
        if stored.is_empty() {
            continue;
        }
        let linked = linker.link(&masked, repository.as_str(), layers, &unit, stored.clone())?;
        for (before, after) in stored.into_iter().zip(linked) {
            if before.source_node_id != after.source_node_id
                || before.target_node_id != after.target_node_id
                || before.payload != after.payload
            {
                links.push(ViewLink {
                    generation_id: layer.clone(),
                    link: EdgeTargetUpdate {
                        unit_key: after.unit_key,
                        edge_id: after.edge_id,
                        source_node_id: after.source_node_id,
                        target_node_id: after.target_node_id,
                        payload: after.payload,
                    },
                });
            }
        }
    }
    Ok(links)
}

/// A reader's per-immutable-view cache. Reusing this across bounded pages avoids rescanning
/// the complete Swift module/import composition for every inherited edge.
#[derive(Default)]
pub(crate) struct ViewLinker {
    cache: NameCache,
    scopes: Option<SwiftScopes>,
}

impl ViewLinker {
    pub(crate) fn link(
        &mut self,
        read: &dyn IndexRead,
        repository: &str,
        layers: &[Id],
        unit: &str,
        mut edges: Vec<StoredEdge>,
    ) -> Result<Vec<StoredEdge>, StorageError> {
        let Some((top, lower)) = layers.split_first() else {
            return Ok(edges);
        };
        for edge in &mut edges {
            let mut payload = edge.payload.clone();
            let source =
                swift_extension_source(read, top, lower, repository, unit, edge, &mut payload)?;
            let target = link_target(
                read,
                top,
                lower,
                repository,
                unit,
                edge,
                &mut payload,
                &mut self.cache,
                &mut self.scopes,
            )?;
            edge.source_node_id = source;
            if let Some(target) = target {
                edge.target_node_id = target;
            }
            edge.payload = payload;
        }
        Ok(edges)
    }
}
