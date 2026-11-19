//! Planning and row building for one index run: the unit plan, file and directory rows,
//! shared-group merging and retirement of staging generations and views.
use super::*;

/// Unit modes recorded in unit payloads.
pub const MODE_STRUCTURAL: &str = "Structural";

pub const MODE_TEXT_FALLBACK: &str = "TextFallback";

pub const MODE_TEXT: &str = "Text";

pub const MODE_NOT_SELECTED: &str = "NotSelected";

pub const MODE_OVERSIZE: &str = "Oversize";

pub const MODE_LINK: &str = "Link";

pub const MODE_SUBMODULE: &str = "Submodule";

pub const MODE_DIRECTORY: &str = "Directory";

pub const MODE_UNREADABLE: &str = "Unreadable";

/// The tree mode a capture gives a worktree file it could not read safely.
pub const UNREADABLE_MODE: &str = "000000";

/// Most warnings a run record keeps (the log keeps every one).
pub(super) const MAX_WARNINGS: usize = 48;

/// Why execution stopped before success.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    Cancelled,
    /// The claim is no longer this worker's (lease lost, reclaimed): write nothing more.
    Lost,
    Failed {
        code: String,
        message: String,
        deterministic: bool,
    },
}

impl Stop {
    pub(crate) fn det(code: &str, message: impl Into<String>) -> Self {
        Self::Failed {
            code: code.into(),
            message: message.into(),
            deterministic: true,
        }
    }
    pub(crate) fn transient(code: &str, message: impl Into<String>) -> Self {
        Self::Failed {
            code: code.into(),
            message: message.into(),
            deterministic: false,
        }
    }
    pub(crate) fn storage(e: StorageError) -> Self {
        match e {
            StorageError::Busy
            | StorageError::StorageUnavailable { .. }
            | StorageError::CommitUnknown
            | StorageError::Cancelled
            | StorageError::RevisionConflict(_) => Self::transient("storage", e.to_string()),
            StorageError::LeaseLost { .. } => Self::Lost,
            other => Self::det("storage_invariant", other.to_string()),
        }
    }
    pub(crate) fn git(e: GitFault) -> Self {
        if e.cancelled() {
            return Self::Cancelled;
        }
        if e.transient() {
            return Self::transient("git", e.to_string());
        }
        Self::det("git", e.to_string())
    }
}

/// One planned unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PlanUnit {
    pub key: String,
    pub digest: String,
    pub mode: &'static str,
    pub language: Option<&'static str>,
    pub size: u64,
    /// The blob to read for extraction or private source capture (within the source limit).
    pub oid: Option<String>,
    pub previous: Option<String>,
    /// Directory units: child keys and whether each is a directory.
    pub children: Vec<(String, bool)>,
}

/// The directory unit key of `dir` (`/` is the repository root): never a valid file path.
pub fn dir_key(dir: &str) -> String {
    format!("/{dir}")
}

/// Plans the units of `entries` under `cfg`: excluded paths are left out, and every included
/// path, its directories and the root get a unit.
pub(crate) fn plan(
    env: &IndexEnv,
    cfg: &EffectiveConfig,
    entries: &[TreeEntry],
    renamed_from: &BTreeMap<String, String>,
    warnings: &mut Vec<RunWarning>,
    case_insensitive: bool,
) -> (Vec<PlanUnit>, BTreeMap<String, i64>) {
    let present: BTreeSet<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    let mut excluded: BTreeMap<String, i64> = BTreeMap::new();
    let mut units = Vec::new();
    let mut dirs: BTreeMap<String, BTreeSet<(String, bool)>> = BTreeMap::new();
    let mut folded: BTreeMap<String, String> = BTreeMap::new();
    for e in entries {
        if cfg.path_excluded(&e.path) || dependency_directory(&e.path, &|p| present.contains(p)) {
            *excluded.entry("excluded".into()).or_default() += 1;
            continue;
        }
        if case_insensitive && let Some(other) = folded.insert(fold(&e.path), e.path.clone()) {
            push_warning(
                warnings,
                "case_collision",
                format!(
                    "{} and {other} name the same file on this file system",
                    e.path
                ),
            );
        }
        let language = env.parser.language_of(&e.path);
        let (mode, lang) = if e.submodule() {
            (MODE_SUBMODULE, None)
        } else if e.mode == UNREADABLE_MODE {
            (MODE_UNREADABLE, None)
        } else if e.link() {
            (MODE_LINK, None)
        } else {
            match language {
                // Manifests (Cargo.toml, .csproj, go.mod, ...) are read whatever the language
                // selection: package identities are not a language choice.
                Some(l)
                    if !l.capabilities.contains(&"manifest") && !cfg.language_selected(l.id) =>
                {
                    (MODE_NOT_SELECTED, Some(l.id))
                }
                Some(l) if l.mode == LanguageMode::TextFallback => (MODE_TEXT_FALLBACK, Some(l.id)),
                Some(l) if e.size as i64 > cfg.max_bytes_for(&e.path) => {
                    (MODE_OVERSIZE, Some(l.id))
                }
                Some(l) => (MODE_STRUCTURAL, Some(l.id)),
                None => (MODE_TEXT, None),
            }
        };
        units.push(PlanUnit {
            key: e.path.clone(),
            digest: e.oid.clone(),
            mode,
            language: lang,
            size: e.size,
            oid: (matches!(
                mode,
                MODE_STRUCTURAL | MODE_TEXT | MODE_TEXT_FALLBACK | MODE_NOT_SELECTED
            ) && e.size <= cfg.max_bytes_for(&e.path) as u64)
                .then(|| e.oid.clone()),
            previous: renamed_from.get(&e.path).cloned(),
            children: Vec::new(),
        });
        // Every ancestor directory lists its child.
        let mut child = (e.path.clone(), false);
        let mut dir = parent(&e.path).to_owned();
        loop {
            let new = dirs.entry(dir.clone()).or_default().insert(child.clone());
            if dir.is_empty() || !new {
                break;
            }
            child = (dir.clone(), true);
            dir = parent(&dir).to_owned();
        }
    }
    dirs.entry(String::new()).or_default();
    for (dir, children) in dirs {
        let children: Vec<(String, bool)> = children.into_iter().collect();
        let basis: Vec<String> = children.iter().map(|(c, d)| format!("{c}:{d}")).collect();
        units.push(PlanUnit {
            key: dir_key(&dir),
            digest: sha256_hex(basis.join("\u{0}").as_bytes()),
            mode: MODE_DIRECTORY,
            language: None,
            size: 0,
            oid: None,
            previous: None,
            children,
        });
    }
    units.sort_by(|a, b| a.key.cmp(&b.key));
    (units, excluded)
}

pub(crate) fn push_warning(w: &mut Vec<RunWarning>, code: &str, message: String) {
    if w.len() < MAX_WARNINGS {
        w.push(RunWarning {
            code: code.into(),
            message: message.chars().take(1024).collect(),
        });
    }
}

/// The unit payload of a planned unit.
pub(crate) fn unit_payload(
    u: &PlanUnit,
    parse_errors: u32,
    extra: Option<&str>,
    incomplete: &[String],
) -> Value {
    let mut p = json!({
        "mode": u.mode,
        "size": u.size,
        "extractorVersion": EXTRACTOR_VERSION,
    });
    if let Some(l) = u.language {
        p["language"] = json!(l);
    }
    if parse_errors > 0 {
        p["parseErrors"] = json!(parse_errors);
    }
    if let Some(prev) = &u.previous {
        p["previousPath"] = json!(prev);
    }
    if let Some(x) = extra {
        p["note"] = json!(x);
    }
    if !incomplete.is_empty() {
        p["incomplete"] = json!(incomplete);
    }
    p
}

/// The stable ID of a file's node.
pub fn file_node_id(repository: &str, key: &str) -> String {
    node_id(repository, key, "File", key, 0)
}

/// The stable ID of a directory's node (`""`: the repository node).
pub fn dir_node_id(repository: &str, dir: &str) -> String {
    let key = dir_key(dir);
    node_id(
        repository,
        &key,
        if dir.is_empty() {
            "Repository"
        } else {
            "Folder"
        },
        dir,
        0,
    )
}

/// Converts one file's extraction into stored rows (stable IDs, provenance).
pub(crate) fn rows_of(
    repository: &str,
    key: &str,
    extract: &FileExtract,
    language: Option<&str>,
    grammar: Option<&str>,
    extractor: &str,
) -> (Vec<StoredNode>, Vec<StoredEdge>) {
    let mut ids: Vec<String> = Vec::with_capacity(extract.nodes.len());
    let mut seen: HashMap<(String, String), usize> = HashMap::new();
    let mut nodes = Vec::with_capacity(extract.nodes.len());
    for (i, n) in extract.nodes.iter().enumerate() {
        let ordinal = {
            let c = seen
                .entry((n.kind.clone(), n.qualified_name.clone()))
                .or_default();
            let o = *c;
            *c += 1;
            o
        };
        let id = if i == 0 {
            file_node_id(repository, key)
        } else {
            node_id(repository, key, &n.kind, &n.qualified_name, ordinal)
        };
        ids.push(id);
    }
    for (i, n) in extract.nodes.iter().enumerate() {
        let span = json!({
            "startLine": n.span.start_line, "startColumn": n.span.start_column,
            "endLine": n.span.end_line, "endColumn": n.span.end_column,
            "startByte": n.span.start_byte, "endByte": n.span.end_byte,
        });
        let mut payload = json!({
            "qualifiedName": n.qualified_name,
            "span": span,
            "properties": Value::Object(n.properties.clone()),
            "confidence": "high",
            "provenance": {"extractor": extractor, "grammar": grammar, "extractorVersion": EXTRACTOR_VERSION},
        });
        if let Some(l) = language {
            payload["language"] = json!(l);
        }
        if let Some(p) = n.parent.and_then(|p| ids.get(p)) {
            payload["parentId"] = json!(p);
        }
        nodes.push(StoredNode {
            unit_key: key.into(),
            node_id: ids[i].clone(),
            kind: n.kind.clone(),
            name: n.name.chars().take(512).collect(),
            payload,
        });
    }
    let mut edges = Vec::with_capacity(extract.edges.len());
    let mut edge_seen: HashMap<(String, String, String), usize> = HashMap::new();
    for e in &extract.edges {
        let Some(source) = ids.get(e.source) else {
            continue;
        };
        let (target, target_key, mut payload) = match &e.target {
            EdgeTarget::Local(j) => match ids.get(*j) {
                Some(t) => (
                    t.clone(),
                    t.clone(),
                    json!({"resolution": e.resolution.as_str(), "targetUnit": key}),
                ),
                None => continue,
            },
            EdgeTarget::Symbol(name) => {
                let name: String = name.chars().take(1024).collect();
                let mut payload = json!({"resolution": e.resolution.as_str(), "targetName": name});
                // Candidate scopes an extractor already narrowed the name to (a route
                // handler's files, lines or package directory): the linker honours them.
                for k in [
                    "candidateUnits",
                    "candidateLines",
                    "candidateDirs",
                    "goImport",
                    "targetMatch",
                ] {
                    if let Some(v) = e.properties.get(k) {
                        payload[k] = v.clone();
                    }
                }
                (symbol_id(repository, &name), name.clone(), payload)
            }
            EdgeTarget::Node {
                unit,
                kind,
                qualified_name,
            } => {
                let id = if kind == "File" {
                    file_node_id(repository, unit)
                } else {
                    node_id(repository, unit, kind, qualified_name, 0)
                };
                (
                    id.clone(),
                    id,
                    json!({"resolution": e.resolution.as_str(), "targetUnit": unit}),
                )
            }
            EdgeTarget::RepositoryNode {
                repository_id,
                unit,
                kind,
                qualified_name,
                target_name,
            } => {
                let target_repository = repository_id.as_str();
                let id = match kind.as_str() {
                    "File" => file_node_id(target_repository, unit),
                    "Repository" | "Folder" => {
                        dir_node_id(target_repository, unit.trim_start_matches('/'))
                    }
                    _ => node_id(target_repository, unit, kind, qualified_name, 0),
                };
                (id.clone(), id, {
                    let mut payload = json!({
                        "resolution":e.resolution.as_str(), "targetRepositoryId":repository_id,
                        "targetUnit":unit, "targetQualifiedName":qualified_name,"targetKind":kind,
                    });
                    if let Some(name) = target_name {
                        payload["targetName"] = json!(name);
                    }
                    payload
                })
            }
        };
        payload["confidence"] = json!((f64::from(e.confidence) * 100.0).round() / 100.0);
        payload["provenance"] = json!({"extractor": extractor});
        if !e.properties.is_empty() {
            payload["properties"] = Value::Object(e.properties.clone());
        }
        let ordinal = {
            let c = edge_seen
                .entry((source.clone(), e.kind.clone(), target_key.clone()))
                .or_default();
            let o = *c;
            *c += 1;
            o
        };
        edges.push(StoredEdge {
            unit_key: key.into(),
            edge_id: edge_id(source, &e.kind, &target_key, ordinal),
            source_node_id: source.clone(),
            target_node_id: target,
            kind: e.kind.clone(),
            payload,
        });
    }
    (nodes, edges)
}

/// The rows of a unit that is not parsed: its file node only.
pub(crate) fn file_only(repository: &str, u: &PlanUnit) -> Vec<StoredNode> {
    let name = u.key.rsplit('/').next().unwrap_or(&u.key).to_owned();
    vec![StoredNode {
        unit_key: u.key.clone(),
        node_id: file_node_id(repository, &u.key),
        kind: "File".into(),
        name,
        payload: json!({
            "qualifiedName": u.key, "language": u.language, "mode": u.mode, "size": u.size,
            "confidence": "high",
            "provenance": {"extractor": "inventory", "extractorVersion": EXTRACTOR_VERSION},
        }),
    }]
}

/// The rows of a directory unit: its node and containment of its children.
pub(crate) fn dir_rows(
    repository: &str,
    name: &str,
    u: &PlanUnit,
) -> (Vec<StoredNode>, Vec<StoredEdge>) {
    let dir = &u.key[1..];
    let id = dir_node_id(repository, dir);
    let label = if dir.is_empty() {
        name.to_owned()
    } else {
        dir.rsplit('/').next().unwrap_or(dir).to_owned()
    };
    let node = StoredNode {
        unit_key: u.key.clone(),
        node_id: id.clone(),
        kind: if dir.is_empty() {
            "Repository"
        } else {
            "Folder"
        }
        .into(),
        name: label,
        payload: json!({
            "qualifiedName": dir, "confidence": "high",
            "provenance": {"extractor": "inventory", "extractorVersion": EXTRACTOR_VERSION},
        }),
    };
    let mut edges = Vec::new();
    for (child, is_dir) in &u.children {
        let (kind, target, unit) = if *is_dir {
            (
                "CONTAINS_FOLDER",
                dir_node_id(repository, child),
                dir_key(child),
            )
        } else {
            (
                "CONTAINS_FILE",
                file_node_id(repository, child),
                child.clone(),
            )
        };
        edges.push(StoredEdge {
            unit_key: u.key.clone(),
            edge_id: edge_id(&id, kind, &target, 0),
            source_node_id: id.clone(),
            target_node_id: target,
            kind: kind.into(),
            payload: json!({"resolution": "Resolved", "confidence": 1.0, "targetUnit": unit,
                "provenance": {"extractor": "inventory"}}),
        });
    }
    (vec![node], edges)
}

/// Most bytes one extraction group reads (every member together).
pub const MAX_GROUP_BYTES: usize = 64 * 1024 * 1024;

/// The members of each repository-level extraction group among planned units: readable file
/// units the parser assigns to a group, within the size limit, in path order.
pub(crate) fn group_members<'a>(
    env: &IndexEnv,
    cfg: &EffectiveConfig,
    units: impl IntoIterator<Item = &'a PlanUnit>,
) -> BTreeMap<&'static str, Vec<&'a PlanUnit>> {
    let mut out: BTreeMap<&'static str, Vec<&'a PlanUnit>> = BTreeMap::new();
    for u in units {
        let readable = matches!(
            u.mode,
            MODE_STRUCTURAL | MODE_NOT_SELECTED | MODE_TEXT_FALLBACK | MODE_TEXT
        ) && u.size as i64 <= cfg.max_bytes_for(&u.key);
        if let (true, Some(g)) = (readable, env.parser.group_of(&u.key)) {
            out.entry(g).or_default().push(u);
        }
    }
    for members in out.values_mut() {
        members.sort_by(|a, b| a.key.cmp(&b.key));
    }
    out
}

/// Whether a group's members differ from the parent generation's (a member added, removed or
/// changed): the whole group is then extracted again, since each member's output depends on
/// the others.
pub(crate) fn group_changed(
    env: &IndexEnv,
    group: &str,
    members: &[&PlanUnit],
    parent: &BTreeMap<String, UnitFacts>,
) -> bool {
    let now: BTreeSet<(&str, &str)> = members
        .iter()
        .map(|u| (u.key.as_str(), u.digest.as_str()))
        .collect();
    let before: BTreeSet<(&str, &str)> = parent
        .iter()
        .filter(|(k, _)| env.parser.group_of(k) == Some(group))
        .map(|(k, (d, _, _, _))| (k.as_str(), d.as_str()))
        .collect();
    now != before
}

/// Appends a group share (its node 0 stands for the unit's file node) to an extraction.
pub(crate) fn merge_share(x: &mut FileExtract, share: &FileExtract) {
    let base = x.nodes.len();
    let map = |i: usize| if i == 0 { 0 } else { base + i - 1 };
    for n in share.nodes.iter().skip(1) {
        let mut n = n.clone();
        n.parent = Some(n.parent.map_or(0, map));
        x.nodes.push(n);
    }
    for e in &share.edges {
        let mut e = e.clone();
        e.source = map(e.source);
        if let EdgeTarget::Local(t) = &mut e.target {
            *t = map(*t);
        }
        x.edges.push(e);
    }
    x.incomplete.extend(share.incomplete.iter().cloned());
}

/// The stored rows of a unit whose extraction is `x` with `share` merged in: rows from the
/// share carry the group extractor's provenance.
pub(super) fn rows_with_share(
    repo: &str,
    key: &str,
    mut x: FileExtract,
    share: Option<&FileExtract>,
    language: Option<&str>,
    grammar: Option<&str>,
) -> (Vec<StoredNode>, Vec<StoredEdge>, Vec<String>) {
    let (node_split, edge_split) = (x.nodes.len(), x.edges.len());
    if let Some(s) = share {
        merge_share(&mut x, s);
    }
    let (mut n, mut e) = rows_of(repo, key, &x, language, grammar, "tree-sitter");
    for row in n.iter_mut().skip(node_split) {
        row.payload["provenance"]["extractor"] = json!("manifest-group");
    }
    for row in e.iter_mut().skip(edge_split) {
        row.payload["provenance"]["extractor"] = json!("manifest-group");
    }
    (n, e, x.incomplete)
}

/// Builds the stored rows of one planned unit (`content`: the blob of a structural unit;
/// `share`: its part of a repository-level group extraction).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_unit(
    env: &IndexEnv,
    repository: &Id,
    name: &str,
    generation: &Id,
    u: &PlanUnit,
    content: Option<&[u8]>,
    share: Option<&FileExtract>,
    stop: &(dyn Fn() -> bool + Sync),
    warnings: &mut Vec<RunWarning>,
) -> Result<(StoredUnit, Vec<StoredNode>, Vec<StoredEdge>), Stop> {
    let repo = repository.as_str();
    let mut incomplete: Vec<String> = Vec::new();
    let (nodes, edges, parse_errors, note) = match u.mode {
        MODE_DIRECTORY => {
            let (n, e) = dir_rows(repo, name, u);
            (n, e, 0, None)
        }
        MODE_STRUCTURAL => {
            let language = u.language.unwrap_or("text");
            let info = env.parser.languages().iter().find(|l| l.id == language);
            let bytes = content.unwrap_or_default();
            match env.parser.extract(
                language,
                &u.key,
                name,
                bytes,
                stop,
                env.settings.file_budget_ms,
            ) {
                Ok(x) => {
                    let errors = x.parse_errors;
                    if errors > 0 {
                        push_warning(
                            warnings,
                            "parse_errors",
                            format!("{}: {errors} syntax errors recovered", u.key),
                        );
                    }
                    let (n, e, inc) = rows_with_share(
                        repo,
                        &u.key,
                        x,
                        share,
                        Some(language),
                        info.and_then(|i| i.grammar),
                    );
                    for why in &inc {
                        push_warning(warnings, "pass_incomplete", format!("{}: {why}", u.key));
                    }
                    incomplete = inc;
                    (n, e, errors, None)
                }
                Err(ExtractFault::Cancelled) => return Err(Stop::Cancelled),
                Err(ExtractFault::TimedOut) => {
                    push_warning(
                        warnings,
                        "parse_timeout",
                        format!("{}: the parse budget passed", u.key),
                    );
                    (
                        file_only(repo, u),
                        Vec::new(),
                        1,
                        Some("parse budget exceeded; recorded without definitions"),
                    )
                }
                Err(ExtractFault::Failed(why)) => {
                    push_warning(warnings, "parse_failed", format!("{}: {why}", u.key));
                    (
                        file_only(repo, u),
                        Vec::new(),
                        1,
                        Some("not parsed; recorded without definitions"),
                    )
                }
            }
        }
        // A group member that is not parsed itself (its language not selected) still carries
        // its group share.
        _ => match share {
            Some(s) => {
                let file = file_only(repo, u);
                let x = FileExtract {
                    nodes: vec![ExtractedNode {
                        kind: "File".into(),
                        name: file[0].name.clone(),
                        qualified_name: u.key.clone(),
                        span: Span {
                            start_line: 1,
                            start_column: 1,
                            end_line: 1,
                            end_column: 1,
                            start_byte: 0,
                            end_byte: 0,
                        },
                        parent: None,
                        properties: Map::new(),
                    }],
                    ..FileExtract::default()
                };
                let (mut n, e, inc) = rows_with_share(repo, &u.key, x, Some(s), u.language, None);
                n[0] = file.into_iter().next().expect("a file node");
                incomplete = inc;
                (n, e, 0, None)
            }
            None => (file_only(repo, u), Vec::new(), 0, None),
        },
    };
    let unit = StoredUnit {
        generation_id: generation.clone(),
        unit_key: u.key.clone(),
        content_digest: u.digest.clone(),
        action: UNIT_REPLACE.into(),
        payload: unit_payload(u, parse_errors, note, &incomplete),
    };
    Ok((unit, nodes, edges))
}

/// Retires the generations of a run that will not publish them: its staging generation, and a
/// sealed one no view names (a publication that did not commit). Overlay runs also name their
/// branch and worktree layers in the overlay record (see `overlay::settle_overlay`).
pub(crate) fn retire_staging(
    tx: &mut dyn WriteTransaction,
    run: &StoredIndexRun,
) -> Result<(), StorageError> {
    let mut layers: Vec<Id> = run.generation_id.iter().cloned().collect();
    if let Some(o) = &run.overlay_id
        && let Some(o) = tx.indexing().overlay(o)?
        && o.run_id.as_ref() == Some(&run.id)
    {
        layers.extend(o.branch_generation_id.iter().cloned());
        layers.extend(o.worktree_generation_id.iter().cloned());
    }
    for g in layers {
        retire_unused(tx, &g)?;
    }
    Ok(())
}

/// Retires `g` when it is staging, or sealed and named by no view.
pub(crate) fn retire_unused(tx: &mut dyn WriteTransaction, g: &Id) -> Result<(), StorageError> {
    if let Some(mut stored) = tx.indexing().generation(g)? {
        let unused = match stored.state.as_str() {
            GEN_STAGING => true,
            GEN_SEALED => !tx.indexing().generation_in_view(g)?,
            _ => false,
        };
        if unused {
            stored.state = GEN_RETIRED.into();
            tx.indexing().update_generation(&stored)?;
        }
    }
    Ok(())
}

/// Retires one unpublished view: removes it (and its registry row and texts) and retires every
/// layer no other view, overlay or active run still uses.
pub(crate) fn retire_view(
    tx: &mut dyn WriteTransaction,
    v: &StoredView,
) -> Result<(), StorageError> {
    tx.indexing().remove_view(&v.view_id)?;
    let overlays = tx.indexing().overlays(&v.repository_id)?;
    let published = match tx.indexing().published(&v.repository_id)? {
        Some(p) => tx.indexing().view(&p.view_id)?,
        None => None,
    };
    for g in [
        Some(&v.baseline_id),
        v.branch_id.as_ref(),
        v.worktree_id.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        let in_use = tx.indexing().generation_in_view(g)?
            || published.as_ref().is_some_and(|p| &p.baseline_id == g)
            || overlays.iter().any(|o| {
                !matches!(o.state.as_str(), OVERLAY_EXPIRED | OVERLAY_UNAVAILABLE)
                    && o.view_id.as_ref() != Some(&v.view_id)
                    && [
                        o.base_generation_id.as_ref(),
                        o.branch_generation_id.as_ref(),
                        o.worktree_generation_id.as_ref(),
                    ]
                    .contains(&Some(g))
            });
        if in_use {
            continue;
        }
        if let Some(mut stored) = tx.indexing().generation(g)?
            && stored.state == GEN_SEALED
        {
            stored.state = GEN_RETIRED.into();
            tx.indexing().update_generation(&stored)?;
        }
    }
    Ok(())
}
