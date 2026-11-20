//! Call and Swift type target resolution for link generation.
use super::*;

/// The retained `SwiftTypeReference` endpoint kinds (`IsSwiftTypeNode`).
pub(super) const SWIFT_TYPE_KINDS: [&str; 4] = ["Class", "Interface", "Struct", "Enum"];

/// The source of an edge a Swift extension of a type declared elsewhere contributes (see
/// [`link_generation`]); any other edge keeps its source.
pub(super) fn swift_extension_source(
    s: &dyn IndexRead,
    generation: &Id,
    lower: &[Id],
    repository: &str,
    unit: &str,
    e: &StoredEdge,
    payload: &mut Value,
) -> Result<String, StorageError> {
    let path = e.payload["properties"]["source_qualified_name"]
        .as_str()
        .and_then(|q| q.strip_prefix(SWIFT_TYPE_REF))
        .filter(|p| !p.is_empty() && is_swift_unit(unit));
    let Some(path) = path else {
        return Ok(e.source_node_id.clone());
    };
    let cut = Cell::new(false);
    let types: Vec<CallNode> = swift_type_nodes(s, generation, lower, path, &cut)?
        .into_iter()
        .filter(|t| SWIFT_TYPE_KINDS.contains(&t.kind.as_str()))
        .collect();
    let Some(m) = payload.as_object_mut() else {
        return Ok(e.source_node_id.clone());
    };
    if let [t] = types.as_slice()
        && !cut.get()
    {
        m.insert("sourceUnit".into(), json!(t.unit));
        m.insert("sourceResolution".into(), json!("Inferred"));
        m.remove("sourceCandidates");
        Ok(t.id.clone())
    } else {
        // None (an external type) or several: the members stay under the extension's file.
        m.remove("sourceUnit");
        m.insert("sourceResolution".into(), json!("Unresolved"));
        m.insert("sourceCandidates".into(), json!(types.len()));
        Ok(file_node_id(repository, unit))
    }
}

/// The confidence of a call bound by its name alone although its receiver was resolved (the
/// retained unresolved-receiver band).
pub(super) const NAME_BOUND_CONFIDENCE: f64 = 0.45;

/// Marks (or clears) a call bound by its name alone, keeping the extracted confidence so a later
/// binding through the receiver restores it.
pub(super) fn mark_name_bound(payload: &mut Value, name_bound: bool) {
    let Some(m) = payload.as_object_mut() else {
        return;
    };
    if name_bound {
        if !m.contains_key("extractedConfidence") {
            let c = m.get("confidence").cloned().unwrap_or(Value::Null);
            m.insert("extractedConfidence".into(), c);
        }
        m.insert("confidence".into(), json!(NAME_BOUND_CONFIDENCE));
        m.insert("resolutionBasis".into(), json!("name"));
    } else {
        if let Some(c) = m.remove("extractedConfidence") {
            m.insert("confidence".into(), c);
        }
        m.remove("resolutionBasis");
    }
}

/// The linked target of one edge (`None`: the edge is not linked by name), with `payload`
/// updated to say how it was established.
#[allow(clippy::too_many_arguments)]
pub(super) fn link_target(
    s: &dyn IndexRead,
    g: &Id,
    low: &[Id],
    repo: &str,
    unit: &str,
    e: &StoredEdge,
    payload: &mut Value,
    cache: &mut NameCache,
    scopes: &mut Option<SwiftScopes>,
) -> Result<Option<String>, StorageError> {
    let resolution = e
        .payload
        .get("resolution")
        .and_then(Value::as_str)
        .unwrap_or("");
    if resolution == "Resolved" || e.payload["properties"]["compiler_unresolved"] == true {
        return Ok(None);
    }
    let Some(name) = e.payload.get("targetName").and_then(Value::as_str) else {
        return Ok(None);
    };
    // A target named by its exact qualified name (any edge kind, any node kind).
    let by_qualified_name =
        e.payload.get("targetMatch").and_then(Value::as_str) == Some("qualifiedName");
    let kinds = target_kinds(&e.kind).unwrap_or(&[]);
    if kinds.is_empty() && !by_qualified_name {
        return Ok(None);
    }
    let mut name_bound = false;
    // Set when a lookup answered at its cap: the candidates are then incomplete.
    let cut = Cell::new(false);
    let mut candidates: Vec<(String, String, String)> = if by_qualified_name {
        visible_qualified(s, g, low, name, &cut)?
    } else if e.kind == "IMPORTS" {
        // A file node's qualified name is its path: exact, never a capped name lookup.
        let mut found = Vec::new();
        for path in import_candidates(unit, name) {
            for n in visible_qualified(s, g, low, &path, &cut)? {
                if n.1 == "File" && n.0 == path {
                    found.push(n.clone());
                }
            }
            if !found.is_empty() {
                break;
            }
        }
        found
    } else if let Some(path) = name.strip_prefix(SWIFT_TYPE_REF).filter(|p| !p.is_empty()) {
        // A Swift type reference names the type at that path, never a name.
        let protocol = e.payload["properties"]["swift_protocol_target"] == json!(true);
        swift_type_nodes(s, g, low, path, &cut)?
            .into_iter()
            .filter(|c| kinds.contains(&c.kind.as_str()) && (!protocol || c.kind == "Interface"))
            .map(CallNode::key)
            .collect()
    } else if e.kind == "CALLS" {
        let (found, by_name) = call_candidates(
            s,
            g,
            low,
            unit,
            name,
            &e.payload["properties"],
            kinds,
            scopes,
            &cut,
        )?;
        name_bound = by_name;
        found
    } else {
        let simple = simple_name(name);
        let all = match cache.get(&simple) {
            Some((c, truncated)) => {
                cut.set(cut.get() || *truncated);
                c.clone()
            }
            None => {
                let own = Cell::new(false);
                let c = visible_named(s, g, low, &simple, &own)?;
                cache.insert(simple.clone(), (c.clone(), own.get()));
                cut.set(cut.get() || own.get());
                c
            }
        };
        all.into_iter()
            .filter(|c| kinds.contains(&c.1.as_str()))
            .collect()
    };
    if e.kind == "HANDLES" {
        let mut layers = vec![g.clone()];
        layers.extend(low.iter().cloned());
        let (scoped, problem) = handler_scope(s, &layers, unit, &e.payload, candidates)?;
        candidates = scoped;
        if let Some(p) = problem {
            payload["resolutionProblem"] = json!(p);
            candidates.clear();
        }
        // One route identity (method and path) registered by several files is ambiguous: none
        // of its registrations proves the handler.
        if let Some(route) = e.payload["properties"]["routeName"].as_str() {
            let mut sources: Vec<String> = visible_named(s, g, low, route, &cut)?
                .into_iter()
                .filter(|n| n.1 == "Route")
                .map(|n| n.0)
                .collect();
            sources.sort();
            sources.dedup();
            if sources.len() > 1 {
                payload["registrationAmbiguous"] = json!(true);
                payload["registrationSources"] = json!(sources);
                candidates.clear();
            }
        }
    }
    // Incomplete candidates prove no unique target.
    let truncated = cut.get();
    if truncated {
        payload["candidatesTruncated"] = json!(true);
    } else if let Some(m) = payload.as_object_mut() {
        m.remove("candidatesTruncated");
    }
    let (target, target_unit) = if candidates.len() == 1 && !truncated {
        payload["resolution"] = json!("Inferred");
        payload["candidates"] = json!(1);
        (candidates[0].2.clone(), Some(candidates[0].0.clone()))
    } else {
        // None or several: the name stays a symbol, never a guessed target.
        payload["resolution"] = json!("Unresolved");
        payload["candidates"] = json!(candidates.len());
        name_bound = false;
        (symbol_id(repo, name), None)
    };
    mark_name_bound(payload, name_bound);
    match target_unit {
        Some(u) => {
            payload["targetUnit"] = json!(u);
        }
        None => {
            if let Some(m) = payload.as_object_mut() {
                m.remove("targetUnit");
            }
        }
    }
    Ok(Some(target))
}

/// The last segment of a dotted or `::` path.
pub(super) fn simple_name(name: &str) -> String {
    name.rsplit(['.', ':'])
        .find(|s| !s.is_empty())
        .unwrap_or(name)
        .to_owned()
}

/// Nodes whose qualified name is exactly `qualified_name`, visible from `generation` (then
/// `lower` layers, skipping units a higher layer mentions): (unit key, kind, node ID).
pub(super) fn visible_qualified(
    s: &dyn IndexRead,
    generation: &Id,
    lower: &[Id],
    qualified_name: &str,
    cut: &Cell<bool>,
) -> Result<Vec<(String, String, String)>, StorageError> {
    Ok(
        visible_nodes(s, generation, lower, qualified_name, true, cut)?
            .into_iter()
            .map(|n| (n.unit_key, n.kind, n.node_id))
            .collect(),
    )
}

/// Nodes named `name` visible from `generation` (then `lower` layers, skipping units a higher
/// layer mentions): (unit key, kind, node ID).
pub(super) fn visible_named(
    s: &dyn IndexRead,
    generation: &Id,
    lower: &[Id],
    name: &str,
    cut: &Cell<bool>,
) -> Result<Vec<(String, String, String)>, StorageError> {
    Ok(visible_nodes(s, generation, lower, name, false, cut)?
        .into_iter()
        .map(|n| (n.unit_key, n.kind, n.node_id))
        .collect())
}

/// The retained `SwiftTypeReference` prefix of a symbolic Swift type endpoint.
pub(super) const SWIFT_TYPE_REF: &str = "swift-type:";

/// Node kinds that own members a call's receiver names (the retained `IsReceiverOwnerLabel`).
pub(super) const RECEIVER_OWNER_KINDS: [&str; 7] = [
    "Namespace",
    "Class",
    "Interface",
    "Enum",
    "Struct",
    "Record",
    "Module",
];

/// A node a call or type reference may bind to, with what the binding rules read.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct CallNode {
    pub(super) unit: String,
    pub(super) kind: String,
    pub(super) id: String,
    pub(super) name: String,
    pub(super) qualified_name: String,
    pub(super) receiver_owner: Option<String>,
    pub(super) argument_labels: Option<String>,
    /// The foreign ABI a function is exported under by name (`abi`).
    pub(super) abi: Option<String>,
}

impl CallNode {
    fn of(n: &StoredNode) -> Self {
        let p = &n.payload["properties"];
        Self {
            unit: n.unit_key.clone(),
            kind: n.kind.clone(),
            id: n.node_id.clone(),
            name: n.name.clone(),
            qualified_name: n.payload["qualifiedName"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            receiver_owner: p["receiver_owner"].as_str().map(str::to_owned),
            argument_labels: p["argument_labels"].as_str().map(str::to_owned),
            abi: p["abi"].as_str().map(str::to_owned),
        }
    }
    fn key(self) -> (String, String, String) {
        (self.unit, self.kind, self.id)
    }
}

/// Most nodes of one name one layer answers a link lookup with. A layer that has more is
/// reported through `cut`: the candidates are then incomplete, so no binding may treat them as
/// unique (the edge stays unresolved, marked `candidatesTruncated`).
pub(super) const CANDIDATE_CAP: u16 = MAX_PAGE;

/// Nodes named `name` (or, with `qualified`, whose qualified name is `name`) visible from
/// `generation` and then `lower` layers, skipping units a higher layer mentions. Sets `cut` when
/// a layer has more than [`CANDIDATE_CAP`] such nodes (the answer is then incomplete).
pub(super) fn visible_nodes(
    s: &dyn IndexRead,
    generation: &Id,
    lower: &[Id],
    name: &str,
    qualified: bool,
    cut: &Cell<bool>,
) -> Result<Vec<StoredNode>, StorageError> {
    let mut out: Vec<StoredNode> = Vec::new();
    let mut layers = vec![generation.clone()];
    layers.extend(lower.iter().cloned());
    for (i, g) in layers.iter().enumerate() {
        let found = if qualified {
            s.nodes_qualified(g, name, CANDIDATE_CAP)?
        } else {
            s.nodes_named(g, name, CANDIDATE_CAP)?
        };
        if found.len() >= usize::from(CANDIDATE_CAP) {
            cut.set(true);
        }
        for n in found {
            let mut masked = false;
            for higher in &layers[..i] {
                if s.unit(higher, &n.unit_key)?.is_some() {
                    masked = true;
                    break;
                }
            }
            if !masked && !out.iter().any(|o| o.node_id == n.node_id) {
                out.push(n);
            }
        }
    }
    Ok(out)
}

/// Whether a call's receiver is present but unproven (the retained `CallReceiverKind`
/// `Unresolved`/`Unknown`): such a call never binds by name.
pub(super) fn call_binding_refused(properties: &Value) -> bool {
    matches!(
        properties.get("receiver_kind").and_then(Value::as_str),
        Some("Unresolved" | "Unknown")
    )
}

/// The qualified names a Swift type at dotted `path` has in its file, as the structural
/// extractor names them: `Outer.Inner` is `#type:Outer#type:Inner`, or a type nested in an
/// extension of a type declared elsewhere (`#extension:Outer#type:Inner`).
pub(super) fn swift_type_qualified_names(path: &str) -> Vec<String> {
    let parts: Vec<&str> = path.split('.').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return Vec::new();
    }
    let mut out = vec![format!("#type:{}", parts.join("#type:"))];
    for split in 1..parts.len() {
        out.push(format!(
            "#extension:{}#type:{}",
            parts[..split].join("."),
            parts[split..].join("#type:")
        ));
    }
    out
}

pub(super) fn is_swift_unit(unit: &str) -> bool {
    unit.to_ascii_lowercase().ends_with(".swift")
}

/// The Swift types at dotted `path` (exact qualified names in Swift units, never by simple name).
pub(super) fn swift_type_nodes(
    s: &dyn IndexRead,
    generation: &Id,
    lower: &[Id],
    path: &str,
    cut: &Cell<bool>,
) -> Result<Vec<CallNode>, StorageError> {
    let mut out: Vec<CallNode> = Vec::new();
    for qn in swift_type_qualified_names(path) {
        for n in visible_nodes(s, generation, lower, &qn, true, cut)? {
            if is_swift_unit(&n.unit_key) && !out.iter().any(|o| o.id == n.node_id) {
                out.push(CallNode::of(&n));
            }
        }
    }
    Ok(out)
}

/// The retained `IsOwnedBy`: a member of `owner` (same unit, its qualified name continues the
/// owner's with `.` or `#method:`), or, when the owner's name is unique, a member declaring it as
/// its `receiver_owner` (a Rust `impl` in another file).
pub(super) fn owned_by(candidate: &CallNode, owner: &CallNode, symbolic: bool) -> bool {
    let prefixed = candidate.unit == owner.unit
        && candidate
            .qualified_name
            .strip_prefix(owner.qualified_name.as_str())
            .is_some_and(|rest| rest.starts_with('.') || rest.starts_with("#method:"));
    prefixed || (symbolic && candidate.receiver_owner.as_deref() == Some(owner.name.as_str()))
}

/// A method of a Swift extension of the type at `path` declared in another file.
pub(super) fn owned_by_swift_extension(candidate: &CallNode, path: &str) -> bool {
    is_swift_unit(&candidate.unit)
        && candidate
            .qualified_name
            .starts_with(&format!("#extension:{path}#method:"))
}

/// The retained `ArgumentLabels.Accepts`: whether a declaration's argument-label signature
/// (`x=:_...:to:`) accepts a call's (`x:*:`).
pub(super) fn labels_accept(declared: &str, call: &str) -> bool {
    struct Param<'a> {
        label: &'a str,
        default: bool,
        variadic: bool,
    }
    fn split(s: &str) -> Vec<&str> {
        s.split(':')
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .collect()
    }
    let params: Vec<Param<'_>> = split(declared)
        .into_iter()
        .map(|seg| {
            let (seg, variadic) = match seg.strip_suffix("...") {
                Some(r) => (r, true),
                None => (seg, false),
            };
            let (label, default) = match seg.strip_suffix('=') {
                Some(r) => (r, true),
                None => (seg, false),
            };
            Param {
                label,
                default,
                variadic,
            }
        })
        .collect();
    let args = split(call);
    fn matches(ps: &[Param<'_>], p: usize, args: &[&str], a: usize, budget: &mut u32) -> bool {
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        if p == ps.len() {
            return a == args.len();
        }
        let cur = &ps[p];
        if (cur.default || cur.variadic) && matches(ps, p + 1, args, a, budget) {
            return true;
        }
        if a >= args.len() || !(args[a] == "*" || args[a] == cur.label) {
            return false;
        }
        if matches(ps, p + 1, args, a + 1, budget) {
            return true;
        }
        if !cur.variadic {
            return false;
        }
        let mut next = a + 1;
        while next < args.len() && args[next] == "_" {
            if matches(ps, p + 1, args, next + 1, budget) {
                return true;
            }
            next += 1;
        }
        false
    }
    let mut budget = 100_000;
    matches(&params, 0, &args, 0, &mut budget)
}

/// The owner a resolved call's `receiver_type` names (the retained `ResolveReceiverOwner`): a
/// type of the caller's unit by its exact qualified name, else the single owner-kind node of
/// that simple name; with whether its name is unique (symbolic ownership allowed).
pub(super) fn receiver_owner(
    s: &dyn IndexRead,
    generation: &Id,
    lower: &[Id],
    unit: &str,
    receiver: &str,
    cut: &Cell<bool>,
) -> Result<Option<(CallNode, bool)>, StorageError> {
    let owner_kind = |n: &StoredNode| RECEIVER_OWNER_KINDS.contains(&n.kind.as_str());
    let exact = visible_nodes(s, generation, lower, receiver, true, cut)?
        .into_iter()
        .find(|n| n.unit_key == unit && owner_kind(n));
    let owner = match exact {
        Some(o) => CallNode::of(&o),
        None => {
            let normalized = receiver.replace("::", ".");
            let normalized = normalized.trim_matches('.');
            if normalized.is_empty() || normalized.contains('.') {
                return Ok(None);
            }
            let matches: Vec<StoredNode> =
                visible_nodes(s, generation, lower, normalized, false, cut)?
                    .into_iter()
                    .filter(owner_kind)
                    .collect();
            if matches.len() != 1 {
                return Ok(None);
            }
            CallNode::of(&matches[0])
        }
    };
    let named: Vec<StoredNode> = visible_nodes(s, generation, lower, &owner.name, false, cut)?
        .into_iter()
        .filter(owner_kind)
        .collect();
    let symbolic = named.len() == 1 && named[0].node_id == owner.id;
    Ok(Some((owner, symbolic)))
}

/// Link candidates of a call, with whether only its name chose them. A call the structural
/// extractor classified (`receiver_kind`) gets none for an unproven receiver; for a resolved
/// receiver whose type is found, only the members of that type (a Swift type reference: the
/// type at that path, the one the caller's module sees when several modules declare it, and
/// extensions of that path). A resolved receiver naming no type of the index (a Rust module
/// path, an external type) keeps the name's candidates, and the binding is name-bound (the
/// retained pipeline left such a call unbound; DevWorks binds it, marked inferred by name). A
/// call the extractor did not classify names its callee by its last segment. Then the call
/// language families ([`call_target_allowed`]), the declarations whose argument labels accept
/// the call's, and, for a Swift caller with several candidates, the one its module sees.
#[allow(clippy::too_many_arguments)]
pub(super) fn call_candidates(
    s: &dyn IndexRead,
    generation: &Id,
    lower: &[Id],
    unit: &str,
    callee: &str,
    properties: &Value,
    kinds: &[&str],
    scopes: &mut Option<SwiftScopes>,
    cut: &Cell<bool>,
) -> Result<(Candidates, bool), StorageError> {
    if call_binding_refused(properties) {
        return Ok((Vec::new(), false));
    }
    let receiver_kind = properties.get("receiver_kind").and_then(Value::as_str);
    let name = match receiver_kind {
        Some(_) => callee.to_owned(),
        None => simple_name(callee),
    };
    let mut all: Vec<CallNode> = visible_nodes(s, generation, lower, &name, false, cut)?
        .iter()
        .filter(|n| kinds.contains(&n.kind.as_str()))
        .map(CallNode::of)
        .collect();
    let swift_caller = is_swift_unit(unit);
    let mut name_bound = false;
    if receiver_kind == Some("Resolved") {
        let receiver = properties
            .get("receiver_type")
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty());
        if let Some(path) = receiver.and_then(|r| r.strip_prefix(SWIFT_TYPE_REF)) {
            let mut types: Vec<CallNode> = swift_type_nodes(s, generation, lower, path, cut)?
                .into_iter()
                .filter(|t| RECEIVER_OWNER_KINDS.contains(&t.kind.as_str()))
                .collect();
            // A type path several modules declare (a vendored copy) names the one the caller
            // can see.
            if types.len() > 1
                && swift_caller
                && let Some(sc) = swift_scopes(scopes, s, generation, lower)?
            {
                let caller_scope = sc.scope_of(unit).map(str::to_owned);
                types.retain(|t| sc.visible(caller_scope.as_deref(), unit, &t.unit));
            }
            let owner = (types.len() == 1).then(|| types[0].clone());
            let extended = all.iter().any(|c| owned_by_swift_extension(c, path));
            if owner.is_some() || extended {
                all.retain(|c| {
                    owner.as_ref().is_some_and(|o| owned_by(c, o, false))
                        || owned_by_swift_extension(c, path)
                });
            } else {
                name_bound = true;
            }
        } else {
            match match receiver {
                Some(r) => receiver_owner(s, generation, lower, unit, r, cut)?,
                None => None,
            } {
                Some((owner, symbolic)) => all.retain(|c| owned_by(c, &owner, symbolic)),
                None => name_bound = true,
            }
        }
    }
    // A bare call: no receiver at the call site (`Bare`, or the previous walk's `none`).
    let bare = match receiver_kind {
        Some(k) => k == "Bare",
        None => properties
            .get("receiver")
            .and_then(Value::as_str)
            .is_none_or(|r| r == "none"),
    };
    all.retain(|c| {
        call_target_allowed(
            unit,
            &CallTarget {
                unit: &c.unit,
                kind: &c.kind,
                abi: c.abi.as_deref(),
            },
            bare,
        )
    });
    if let Some(labels) = properties.get("argument_labels").and_then(Value::as_str) {
        all.retain(|c| {
            c.argument_labels
                .as_deref()
                .is_none_or(|declared| labels_accept(declared, labels))
        });
    }
    if swift_caller
        && all.len() > 1
        && let Some(sc) = swift_scopes(scopes, s, generation, lower)?
    {
        all = sc.prefer(unit, all, |c| c.unit.as_str());
    }
    Ok((all.into_iter().map(CallNode::key).collect(), name_bound))
}

/// The Swift module scopes of the layers (loaded once per link pass), or `None` when the index
/// declares no Swift module.
pub(super) fn swift_scopes<'c>(
    cache: &'c mut Option<SwiftScopes>,
    s: &dyn IndexRead,
    generation: &Id,
    lower: &[Id],
) -> Result<Option<&'c SwiftScopes>, StorageError> {
    if cache.is_none() {
        let mut layers = vec![generation.clone()];
        layers.extend(lower.iter().cloned());
        *cache = Some(load_swift_scopes(s, &layers)?);
    }
    Ok(cache.as_ref().filter(|c| !c.is_empty()))
}

/// Reads the SwiftPM and Xcode targets (from `Package.swift` and `project.pbxproj` units) and
/// every Swift file's module imports, as the layers compose them.
pub(super) fn load_swift_scopes(
    s: &dyn IndexRead,
    layers: &[Id],
) -> Result<SwiftScopes, StorageError> {
    // The visible units: the highest layer that mentions a key decides it.
    let mut visible: BTreeMap<String, Option<Id>> = BTreeMap::new();
    for g in layers {
        let mut after: Option<String> = None;
        loop {
            let page = s.units(g, after.as_deref(), 500)?;
            for u in &page {
                visible
                    .entry(u.unit_key.clone())
                    .or_insert_with(|| (u.action == UNIT_REPLACE).then(|| g.clone()));
            }
            if page.len() < 500 {
                break;
            }
            after = page.last().map(|u| u.unit_key.clone());
        }
    }
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let mut modules = Vec::new();
    for (key, g) in &visible {
        let Some(g) = g else { continue };
        let name = key.rsplit('/').next().unwrap_or(key);
        if name != "Package.swift" && name != "project.pbxproj" {
            continue;
        }
        for n in s.nodes(g, key)? {
            let p = &n.payload["properties"];
            let ecosystem = p["ecosystem"].as_str();
            if n.kind != "Module" || !matches!(ecosystem, Some("swiftpm" | "xcode")) {
                continue;
            }
            modules.push(SwiftModule {
                qualified_name: text(&n.payload, "qualifiedName").unwrap_or_default(),
                module_name: text(p, "module_name").unwrap_or_else(|| n.name.clone()),
                xcode: ecosystem == Some("xcode"),
                manifest_path: text(p, "manifest_path").unwrap_or_else(|| key.clone()),
                project_directory: text(p, "project_directory"),
                source_path: text(p, "source_path"),
                source_paths: text(p, "source_paths"),
                package_key: text(p, "package_key"),
            });
        }
    }
    if modules.is_empty() {
        return Ok(SwiftScopes::default());
    }
    let mut imports = Vec::new();
    for (key, g) in &visible {
        let Some(g) = g else { continue };
        if !is_swift_unit(key) {
            continue;
        }
        for e in s.edges(g, key)? {
            let p = &e.payload["properties"];
            let module = e.payload["targetName"].as_str().unwrap_or_default();
            // Module imports only: not a type reference, not the previous walk's written path.
            if e.kind != "IMPORTS"
                || module.is_empty()
                || module.starts_with(SWIFT_TYPE_REF)
                || p["baseline_extension"] == json!(true)
            {
                continue;
            }
            imports.push(SwiftImport {
                file: key.clone(),
                module: module.to_owned(),
                exported: p["exported"] == json!(true),
            });
        }
    }
    Ok(SwiftScopes::new(&modules, &imports))
}
