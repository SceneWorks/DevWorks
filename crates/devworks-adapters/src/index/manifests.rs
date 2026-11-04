//! Package identities from manifests (the anchors cross-repository linking joins on), retained
//! from CodeGraph's `CargoManifestExtractor` (with `CargoPackageKey`),
//! `SwiftPackageManifestExtractor` (with `XcodeProjectReader`), `NuGetReferenceExtractor` and the
//! pipeline's `DotnetProject` nodes, and the Go module identity the backend-registration pass
//! resolves module-relative handler imports with.
//!
//! Cargo and SwiftPM manifests are read as repository-level groups (a member's output depends on
//! the others: workspace inheritance, in-repository path dependencies, the shallowest definition
//! of a package): see [`extract_group`]. A `.csproj` and a `go.mod` are read on their own.
//!
//! Node qualified names, keys and properties are the retained ones (`package:{key}@{version}`,
//! `cargo:registry:crates.io:serde`, `swiftpm:scm:github.com/apple/swift-log`,
//! `nuget:Newtonsoft.Json`, `{repository}:project:{name}`, `{repository}:{manifest}#target:{name}`).
//! A node belongs to the manifest that first defines it (in the retained order) and other
//! manifests' relationships point at that node exactly. The retained repository-level source of
//! `CONTAINS_PROJECT` (and of a manifest without a package) is the manifest's file node here, with
//! `scope: repository`.
use devworks_core::ports::indexing::{
    EdgeTarget, ExtractFault, ExtractedEdge, ExtractedNode, FileExtract, LanguageInfo,
    LanguageMode, Resolution, Span,
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Group of Cargo manifests.
pub const CARGO: &str = "cargo";
/// Group of `Package.swift`, `Package.resolved` and Xcode `project.pbxproj` files.
pub const SWIFTPM: &str = "swiftpm";
/// Group of `.csproj` files (each read on its own; the group supplies the repository name the
/// `DotnetProject` identity carries).
pub const DOTNET: &str = "dotnet";

const MANIFEST: &[&str] = &["manifest", "package-identities"];

macro_rules! manifest {
    ($id:expr, $name:expr, $files:expr, $note:expr) => {
        LanguageInfo {
            id: $id,
            name: $name,
            extensions: $files,
            mode: LanguageMode::Structural,
            grammar: None,
            capabilities: MANIFEST,
            note: Some($note),
        }
    };
}

/// The manifest languages (read whatever the language selection).
pub fn languages() -> Vec<LanguageInfo> {
    vec![
        manifest!(
            "cargo-manifest",
            "Cargo manifest",
            &["Cargo.toml"],
            "package definitions and dependencies (workspace-aware)"
        ),
        manifest!(
            "swiftpm-resolved",
            "SwiftPM resolution",
            &["Package.resolved"],
            "resolved package pins (formats 1-3)"
        ),
        manifest!(
            "xcode-project",
            "Xcode project",
            &["project.pbxproj"],
            "native targets and the Swift modules they build"
        ),
        manifest!(
            "msbuild-project",
            "MSBuild project",
            &[".csproj"],
            "the project and its NuGet package references"
        ),
        manifest!("go-module", "Go module", &["go.mod"], "the module identity"),
    ]
}

/// The group a repository-relative path belongs to.
pub fn group_of(path: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name {
        // Cargo build output under `target/` is never a manifest of the repository.
        "Cargo.toml" if !format!("/{path}").to_ascii_lowercase().contains("/target/") => {
            Some(CARGO)
        }
        "Package.swift" | "Package.resolved" | "project.pbxproj" => Some(SWIFTPM),
        n if n.to_ascii_lowercase().ends_with(".csproj") => Some(DOTNET),
        _ => None,
    }
}

fn whole_span(source: &[u8]) -> Span {
    let lines = source.iter().filter(|b| **b == b'\n').count() as u32 + 1;
    Span {
        start_line: 1,
        start_column: 1,
        end_line: lines,
        end_column: 1,
        start_byte: 0,
        end_byte: source.len() as u32,
    }
}

fn file_node(path: &str, source: &[u8]) -> ExtractedNode {
    ExtractedNode {
        kind: "File".into(),
        name: path.rsplit('/').next().unwrap_or(path).to_owned(),
        qualified_name: path.to_owned(),
        span: whole_span(source),
        parent: None,
        properties: Map::new(),
    }
}

fn node(
    kind: &str,
    name: &str,
    qn: &str,
    span: Span,
    properties: Map<String, Value>,
) -> ExtractedNode {
    ExtractedNode {
        kind: kind.into(),
        name: name.into(),
        qualified_name: qn.into(),
        span,
        parent: Some(0),
        properties,
    }
}

fn edge(
    kind: &str,
    source: usize,
    target: EdgeTarget,
    properties: Map<String, Value>,
) -> ExtractedEdge {
    ExtractedEdge {
        kind: kind.into(),
        source,
        target,
        resolution: Resolution::Resolved,
        confidence: 1.0,
        properties,
    }
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// The per-file extraction of a manifest language (`None`: not a manifest language). Package
/// identities come from the file's group share (see [`extract_group`]); a `go.mod` names its
/// module here.
pub fn extract_file(language: &str, path: &str, source: &[u8]) -> Option<FileExtract> {
    let mut x = FileExtract {
        nodes: vec![file_node(path, source)],
        ..FileExtract::default()
    };
    match language {
        "cargo-manifest" | "swiftpm-resolved" | "xcode-project" | "msbuild-project" => {}
        "go-module" => go_module(&mut x, source),
        _ => return None,
    }
    Some(x)
}

// ---------------------------------------------------------------------------------------------
// .csproj (NuGetReferenceExtractor and the pipeline's DotnetProject node)

/// `PackageReference` elements with no namespace (the retained `XDocument.Descendants`
/// match): each `Include`/`Version` pair. `Err` when the document is not well-formed XML.
pub fn package_references(xml: &str) -> Result<Vec<(String, String)>, String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out = Vec::new();
    let mut depth = 0usize;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if e.name().as_ref() == b"PackageReference" {
                    let mut include = String::new();
                    let mut version = String::new();
                    for a in e.attributes().flatten() {
                        let value = a
                            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                            .map(|v| v.into_owned())
                            .unwrap_or_default();
                        match a.key.as_ref() {
                            b"Include" => include = value,
                            b"Version" => version = value,
                            _ => {}
                        }
                    }
                    if !include.is_empty() {
                        out.push((include, version));
                    }
                }
                depth += 1;
            }
            Ok(Event::End(_)) => depth = depth.saturating_sub(1),
            Ok(Event::Eof) => return Ok(out),
            Err(e) => return Err(e.to_string()),
            _ => {}
        }
    }
}

fn nuget(x: &mut FileExtract, path: &str, source: &[u8], repository: &str) {
    let name = path.rsplit('/').next().unwrap_or(path);
    let project = name.strip_suffix(".csproj").unwrap_or(name);
    let span = x.nodes[0].span;
    let project_qn = format!("{repository}:project:{project}");
    x.nodes.push(node(
        "DotnetProject",
        project,
        &project_qn,
        span,
        obj(json!({"dotnet_project": project})),
    ));
    let p = x.nodes.len() - 1;
    x.edges.push(edge(
        "CONTAINS_PROJECT",
        0,
        EdgeTarget::Local(p),
        obj(json!({"scope": "repository"})),
    ));
    let text = String::from_utf8_lossy(source);
    match package_references(&text) {
        Ok(refs) => {
            for (package, version) in refs {
                let qn = format!("nuget:{package}");
                let existing = x
                    .nodes
                    .iter()
                    .position(|n| n.kind == "NuGetPackage" && n.qualified_name == qn);
                let t = match existing {
                    Some(i) => i,
                    None => {
                        x.nodes.push(node(
                            "NuGetPackage",
                            &package,
                            &qn,
                            span,
                            obj(json!({"version": version, "ecosystem": "nuget"})),
                        ));
                        x.nodes.len() - 1
                    }
                };
                x.edges.push(edge(
                    "REFERENCES_PACKAGE",
                    p,
                    EdgeTarget::Local(t),
                    obj(json!({"version": version})),
                ));
            }
        }
        // The retained extraction logs and skips an unreadable project file.
        Err(e) => x.incomplete.push(format!("nuget-references: {e}")),
    }
}

// ---------------------------------------------------------------------------------------------
// go.mod

/// The module paths a `go.mod` declares (the retained registration-pass pattern: one `module`
/// directive per line, quoted or bare, an optional trailing `//` comment).
pub fn go_modules(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let t = line.trim_start_matches([' ', '\t']);
        let Some(rest) = t.strip_prefix("module") else {
            continue;
        };
        if !rest.starts_with([' ', '\t']) {
            continue;
        }
        let rest = rest.trim_start_matches([' ', '\t']);
        let (path, after) = if let Some(q) = rest.strip_prefix('"') {
            match q.find('"') {
                Some(end) if !q[..end].is_empty() => (&q[..end], &q[end + 1..]),
                _ => continue,
            }
        } else {
            let end = rest.find([' ', '\t', '"']).unwrap_or(rest.len());
            if end == 0 {
                continue;
            }
            (&rest[..end], &rest[end..])
        };
        let after = after.trim_start_matches([' ', '\t']);
        if after.is_empty() || after.starts_with("//") {
            out.push(path.to_owned());
        }
    }
    out
}

fn go_module(x: &mut FileExtract, source: &[u8]) {
    let text = String::from_utf8_lossy(source);
    let modules = go_modules(&text);
    let span = x.nodes[0].span;
    let mut props = Map::new();
    let name = if modules.len() == 1 {
        props.insert("module".into(), json!(modules[0]));
        modules[0].clone()
    } else {
        // No unique identity: a module-relative import cannot be resolved through it.
        props.insert(
            "moduleError".into(),
            json!(format!("{} module directives", modules.len())),
        );
        String::from("go.mod")
    };
    x.nodes.push(node("GoModule", &name, &name, span, props));
}

// ---------------------------------------------------------------------------------------------
// Groups

/// Every member's share of a group.
struct Shares {
    units: BTreeMap<String, FileExtract>,
    /// The owner (unit, node index) of each qualified name.
    owner: HashMap<String, (String, usize)>,
    edge_keys: BTreeSet<(String, String, String)>,
}
impl Shares {
    fn new(files: &[(String, Vec<u8>)]) -> Self {
        let units = files
            .iter()
            .map(|(p, b)| {
                (
                    p.clone(),
                    FileExtract {
                        nodes: vec![file_node(p, b)],
                        ..FileExtract::default()
                    },
                )
            })
            .collect();
        Self {
            units,
            owner: HashMap::new(),
            edge_keys: BTreeSet::new(),
        }
    }
    /// Adds a node to `unit` unless its qualified name is owned already; returns its owner.
    fn add(
        &mut self,
        unit: &str,
        kind: &str,
        name: &str,
        qn: &str,
        props: Map<String, Value>,
    ) -> (String, usize, bool) {
        if let Some((u, i)) = self.owner.get(qn) {
            return (u.clone(), *i, false);
        }
        let x = self.units.get_mut(unit).expect("a member");
        let span = x.nodes[0].span;
        x.nodes.push(node(kind, name, qn, span, props));
        let i = x.nodes.len() - 1;
        self.owner.insert(qn.to_owned(), (unit.to_owned(), i));
        (unit.to_owned(), i, true)
    }
    fn props_mut(&mut self, unit: &str, i: usize) -> &mut Map<String, Value> {
        &mut self.units.get_mut(unit).expect("a member").nodes[i].properties
    }
    fn qn(&self, unit: &str, i: usize) -> String {
        self.units[unit].nodes[i].qualified_name.clone()
    }
    /// Adds an edge from (`unit`, `source`) to the node (`target_unit`, `target`), once per
    /// (source, target, kind).
    fn link(
        &mut self,
        unit: &str,
        source: usize,
        kind: &str,
        target_unit: &str,
        target: usize,
        props: Map<String, Value>,
    ) {
        let sq = if source == 0 {
            format!("file:{unit}")
        } else {
            self.qn(unit, source)
        };
        let tq = self.qn(target_unit, target);
        if !self.edge_keys.insert((sq, tq.clone(), kind.to_owned())) {
            return;
        }
        let target = if target_unit == unit {
            EdgeTarget::Local(target)
        } else {
            EdgeTarget::Node {
                unit: target_unit.to_owned(),
                kind: self.units[target_unit].nodes[target].kind.clone(),
                qualified_name: tq,
            }
        };
        self.units
            .get_mut(unit)
            .expect("a member")
            .edges
            .push(edge(kind, source, target, props));
    }
}

/// Extracts one group (see [`group_of`]) from its members.
pub fn extract_group(
    group: &str,
    repository: &str,
    files: &[(String, Vec<u8>)],
    stop: &(dyn Fn() -> bool + Sync),
) -> Result<BTreeMap<String, FileExtract>, ExtractFault> {
    if stop() {
        return Err(ExtractFault::Cancelled);
    }
    match group {
        CARGO => cargo::extract(repository, files),
        SWIFTPM => swiftpm::extract(repository, files),
        DOTNET => Ok(files
            .iter()
            .map(|(path, bytes)| {
                let mut x = FileExtract {
                    nodes: vec![file_node(path, bytes)],
                    ..FileExtract::default()
                };
                nuget(&mut x, path, bytes, repository);
                (path.clone(), x)
            })
            .collect()),
        other => Err(ExtractFault::Failed(format!("unknown group {other}"))),
    }
}

/// The directory of a repository-relative path (`""` at the root).
fn dir_of(p: &str) -> &str {
    p.rfind('/').map_or("", |i| &p[..i])
}

/// Resolves `rel` against the repository-relative directory `dir`: the normalized path, and
/// whether it stays inside the repository (`../sibling` otherwise).
fn resolve(dir: &str, rel: &str) -> (String, bool) {
    let rel = rel.replace('\\', "/");
    let mut parts: Vec<&str> = if rel.starts_with('/') || dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    let mut ups = 0usize;
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    ups += 1;
                }
            }
            s => parts.push(s),
        }
    }
    let mut out: Vec<&str> = std::iter::repeat_n("..", ups).collect();
    out.extend(parts);
    (out.join("/"), ups == 0 && !rel.starts_with('/'))
}

/// Whether `candidate` is `parent` or below it (repository-relative directories).
fn within(candidate: &str, parent: &str) -> bool {
    parent.is_empty() || candidate == parent || candidate.starts_with(&format!("{parent}/"))
}

fn trim_slashes(p: &str) -> String {
    p.replace('\\', "/").trim_matches('/').to_owned()
}

fn qualified(key: &str, version: Option<&str>) -> String {
    format!(
        "package:{key}@{}",
        version.filter(|v| !v.trim().is_empty()).unwrap_or("*")
    )
}

mod cargo {
    use super::*;
    use toml::Value as T;

    const SECTIONS: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

    /// `CargoPackageKey`.
    pub fn registry_key(registry: &str, name: &str) -> String {
        format!(
            "cargo:registry:{}:{}",
            registry.to_lowercase(),
            name.to_lowercase()
        )
    }
    pub fn git_key(git: &str, name: &str) -> String {
        format!("cargo:git:{}:{}", git.to_lowercase(), name.to_lowercase())
    }
    pub fn path_key(repository: &str, path: &str, name: &str) -> String {
        format!(
            "cargo:path:{}:{}:{}",
            repository.to_lowercase(),
            path.to_lowercase(),
            name.to_lowercase()
        )
    }
    pub fn workspace_key(repository: &str, name: &str) -> String {
        format!(
            "cargo:workspace:{}:{}",
            repository.to_lowercase(),
            name.to_lowercase()
        )
    }

    struct Manifest {
        path: String,
        dir: String,
        model: toml::Table,
        /// The retained repository scope: the manifest directory's name.
        scope: String,
    }
    impl Manifest {
        fn package(&self) -> Option<&toml::Table> {
            self.model.get("package").and_then(T::as_table)
        }
        fn workspace(&self) -> Option<&toml::Table> {
            self.model.get("workspace").and_then(T::as_table)
        }
    }

    fn table<'a>(t: Option<&'a toml::Table>, name: &str) -> Option<&'a toml::Table> {
        t?.get(name)?.as_table()
    }
    fn string(t: Option<&toml::Table>, name: &str) -> Option<String> {
        t?.get(name)?.as_str().map(str::to_owned)
    }
    fn boolean(t: Option<&toml::Table>, name: &str) -> Option<bool> {
        t?.get(name)?.as_bool()
    }
    fn inherited<'a>(
        package: &'a toml::Table,
        workspace: Option<&'a toml::Table>,
        name: &str,
    ) -> Option<&'a T> {
        let local = package.get(name)?;
        if let Some(t) = local.as_table()
            && t.get("workspace").and_then(T::as_bool) == Some(true)
        {
            return workspace?.get(name);
        }
        Some(local)
    }

    #[derive(Clone)]
    struct Spec {
        package_name: String,
        version: Option<String>,
        key: String,
        source_kind: &'static str,
        source: String,
        workspace_inherited: bool,
        optional: bool,
        git: Option<String>,
        registry: Option<String>,
        path: Option<String>,
        branch: Option<String>,
        tag: Option<String>,
        rev: Option<String>,
        git_reference: Option<String>,
        /// The dependency's directory (repository-relative) and whether it is inside.
        resolved: Option<(String, bool)>,
    }

    fn normalize_git(v: &str) -> String {
        let t = v.trim().trim_end_matches('/');
        if t.to_ascii_lowercase().ends_with(".git") {
            t[..t.len() - 4].to_owned()
        } else {
            t.to_owned()
        }
    }

    fn spec(
        local: &str,
        raw: &T,
        manifest: &Manifest,
        workspace: Option<&Manifest>,
        workspace_deps: Option<&toml::Table>,
    ) -> Spec {
        let t = raw.as_table();
        let ws_inherited = boolean(t, "workspace") == Some(true);
        let inherited = match (ws_inherited, workspace_deps.and_then(|d| d.get(local))) {
            (true, Some(v)) => Some(spec(local, v, workspace.unwrap_or(manifest), None, None)),
            _ => None,
        };
        let i = inherited.as_ref();
        let package_name = string(t, "package")
            .or_else(|| i.map(|s| s.package_name.clone()))
            .unwrap_or_else(|| local.to_owned());
        let version = raw
            .as_str()
            .map(str::to_owned)
            .or_else(|| string(t, "version"))
            .or_else(|| i.and_then(|s| s.version.clone()));
        let git = string(t, "git").or_else(|| i.and_then(|s| s.git.clone()));
        let registry = string(t, "registry").or_else(|| i.and_then(|s| s.registry.clone()));
        let path = string(t, "path").or_else(|| i.and_then(|s| s.path.clone()));
        let branch = string(t, "branch").or_else(|| i.and_then(|s| s.branch.clone()));
        let tag = string(t, "tag").or_else(|| i.and_then(|s| s.tag.clone()));
        let rev = string(t, "rev").or_else(|| i.and_then(|s| s.rev.clone()));
        let optional = boolean(t, "optional")
            .or_else(|| i.map(|s| s.optional))
            .unwrap_or(false);
        let (source_kind, source, key, resolved) =
            match path.as_deref().filter(|p| !p.trim().is_empty()) {
                Some(p) => {
                    // A path inherited from the workspace is relative to the workspace manifest.
                    let declaring = match (i, workspace) {
                        (Some(_), Some(w)) => w.dir.as_str(),
                        _ => manifest.dir.as_str(),
                    };
                    let source = trim_slashes(p);
                    (
                        "path",
                        source.clone(),
                        path_key(&manifest.scope, &source, &package_name),
                        Some(resolve(declaring, p)),
                    )
                }
                None => match git.as_deref().filter(|g| !g.trim().is_empty()) {
                    Some(g) => {
                        let source = normalize_git(g);
                        ("git", source.clone(), git_key(&source, &package_name), None)
                    }
                    None => {
                        let source = registry
                            .clone()
                            .filter(|r| !r.trim().is_empty())
                            .unwrap_or_else(|| "crates.io".into());
                        (
                            "registry",
                            source.clone(),
                            registry_key(&source, &package_name),
                            None,
                        )
                    }
                },
            };
        let git_reference = rev
            .as_ref()
            .map(|r| format!("rev:{r}"))
            .or_else(|| tag.as_ref().map(|t| format!("tag:{t}")))
            .or_else(|| branch.as_ref().map(|b| format!("branch:{b}")));
        Spec {
            package_name,
            version,
            key,
            source_kind,
            source,
            workspace_inherited: ws_inherited,
            optional,
            git,
            registry,
            path,
            branch,
            tag,
            rev,
            git_reference,
            resolved,
        }
    }

    pub fn extract(
        repository: &str,
        files: &[(String, Vec<u8>)],
    ) -> Result<BTreeMap<String, FileExtract>, ExtractFault> {
        let mut manifests: Vec<Manifest> = Vec::new();
        for (path, bytes) in files {
            let text = std::str::from_utf8(bytes)
                .map_err(|_| ExtractFault::Failed(format!("Cargo manifest {path} is not UTF-8")))?;
            // The retained extraction fails as a whole on one unreadable manifest.
            let model: toml::Table = text.parse().map_err(|e: toml::de::Error| {
                ExtractFault::Failed(format!("Cargo manifest {path}: {}", e.message()))
            })?;
            let dir = dir_of(path).to_owned();
            let scope = if dir.is_empty() {
                repository.to_owned()
            } else {
                dir.rsplit('/').next().unwrap_or(&dir).to_owned()
            };
            manifests.push(Manifest {
                path: path.clone(),
                dir,
                model,
                scope,
            });
        }
        manifests.sort_by(|a, b| a.path.cmp(&b.path));
        let mut shares = Shares::new(files);
        let workspace_of = |m: &Manifest, all: &[Manifest]| -> Option<usize> {
            all.iter()
                .enumerate()
                .filter(|(_, c)| c.workspace().is_some() && within(&m.dir, &c.dir))
                .max_by_key(|(_, c)| c.dir.len())
                .map(|(i, _)| i)
        };
        // Pass 1: package definitions.
        let mut by_dir: HashMap<String, (String, usize)> = HashMap::new();
        for m in &manifests {
            let Some(package) = m.package() else { continue };
            let Some(name) = package
                .get("name")
                .and_then(T::as_str)
                .filter(|n| !n.trim().is_empty())
            else {
                continue;
            };
            let ws = workspace_of(m, &manifests).map(|i| &manifests[i]);
            let ws_package = table(ws.and_then(Manifest::workspace), "package");
            let version = inherited(package, ws_package, "version")
                .and_then(T::as_str)
                .unwrap_or("")
                .to_owned();
            let publish = inherited(package, ws_package, "publish");
            let registry_source = !matches!(publish, Some(T::Boolean(false)));
            let key = if registry_source {
                registry_key("crates.io", name)
            } else {
                workspace_key(repository, name)
            };
            let qn = qualified(&key, Some(&version));
            let props = obj(json!({
                "ecosystem": "cargo", "package_name": name, "version": version,
                "package_key": key,
                "source_kind": if registry_source { "registry" } else { "workspace" },
                "source": if registry_source { "crates.io" } else { "workspace" },
                "is_definition": true, "manifest_path": m.path, "confidence": "high",
            }));
            let (u, i, _) = shares.add(&m.path, "Package", name, &qn, props);
            by_dir.insert(m.dir.clone(), (u.clone(), i));
            shares.link(
                &m.path,
                0,
                "CONTAINS_PROJECT",
                &u,
                i,
                obj(json!({"ecosystem": "cargo", "source": "Cargo.toml", "scope": "repository"})),
            );
        }
        // Pass 2: dependencies.
        for m in &manifests {
            let ws = workspace_of(m, &manifests).map(|i| &manifests[i]);
            let ws_deps = table(ws.and_then(Manifest::workspace), "dependencies");
            let owner = by_dir.get(&m.dir).cloned();
            let (src_unit, src) = match &owner {
                Some((u, i)) if *u == m.path => (u.clone(), *i),
                // The repository is the source of a manifest without a package.
                _ => (m.path.clone(), 0),
            };
            let mut sections: Vec<(&str, Option<String>, &toml::Table)> = Vec::new();
            for s in SECTIONS {
                if let Some(t) = table(Some(&m.model), s) {
                    sections.push((s, None, t));
                }
            }
            if let Some(targets) = table(Some(&m.model), "target") {
                for (target, value) in targets {
                    let Some(tt) = value.as_table() else { continue };
                    for s in SECTIONS {
                        if let Some(t) = table(Some(tt), s) {
                            sections.push((s, Some(target.clone()), t));
                        }
                    }
                }
            }
            for (section, target, deps) in sections {
                for (local, raw) in deps {
                    let mut sp = spec(local, raw, m, ws, ws_deps);
                    let definition = sp
                        .resolved
                        .as_ref()
                        .and_then(|(d, _)| by_dir.get(d))
                        .cloned();
                    let mut target_node = None;
                    if let Some((du, di)) = &definition {
                        let def = &shares.units[du].nodes[*di];
                        sp.package_name = def.name.clone();
                        sp.key = def.properties["package_key"]
                            .as_str()
                            .unwrap_or("")
                            .to_owned();
                        sp.version = def.properties["version"]
                            .as_str()
                            .map(str::to_owned)
                            .or(sp.version);
                        target_node = Some((du.clone(), *di));
                    }
                    let mut props = obj(json!({
                        "ecosystem": "cargo", "package_name": sp.package_name, "local_name": local,
                        "version": sp.version.clone().unwrap_or_default(), "package_key": sp.key,
                        "source_kind": sp.source_kind, "source": sp.source, "is_definition": false,
                        "manifest_path": m.path, "dependency_scope": section,
                        "workspace_inherited": sp.workspace_inherited, "optional": sp.optional,
                        "confidence": "high",
                    }));
                    if let Some(t) = &target {
                        props.insert("target".into(), json!(t));
                    }
                    if let Some(g) = &sp.git_reference {
                        props.insert("git_reference".into(), json!(g));
                    }
                    // A path dependency that leaves the repository (a sibling checkout): the
                    // cross-repository linker resolves it to the repository that defines it.
                    if definition.is_none()
                        && let Some((resolved, false)) = &sp.resolved
                    {
                        props.insert("external_path".into(), json!(resolved));
                    }
                    let (tu, ti) = match target_node {
                        Some(t) => t,
                        None => {
                            let qn = match &definition {
                                Some((du, di)) => shares.qn(du, *di),
                                None => qualified(&sp.key, sp.version.as_deref()),
                            };
                            let (u, i, _) = shares.add(
                                &m.path,
                                "Package",
                                &sp.package_name,
                                &qn,
                                props.clone(),
                            );
                            (u, i)
                        }
                    };
                    let mut edge_props = props;
                    edge_props.insert("canonical_package_name".into(), json!(sp.package_name));
                    shares.link(&src_unit, src, "REFERENCES_PACKAGE", &tu, ti, edge_props);
                }
            }
        }
        Ok(shares.units)
    }
}

mod swiftpm {
    use super::*;

    pub const PRODUCTS: &str = "swift_products";

    /// Comments blanked in `code`; string contents also blanked in `masked` (byte-aligned).
    pub fn scan(content: &str) -> (Vec<u8>, Vec<u8>) {
        let b = content.as_bytes();
        let mut code = Vec::with_capacity(b.len());
        let mut masked = Vec::with_capacity(b.len());
        let blank = |code: &mut Vec<u8>, masked: &mut Vec<u8>, c: u8| {
            let x = if c == b'\n' { b'\n' } else { b' ' };
            code.push(x);
            masked.push(x);
        };
        let raw_start = |i: usize| -> usize {
            let mut j = i;
            while j < b.len() && b[j] == b'#' {
                j += 1;
            }
            if j > i && j < b.len() && b[j] == b'"' {
                j - i
            } else {
                0
            }
        };
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if c == b'/' && b.get(i + 1) == Some(&b'/') {
                while i < b.len() && b[i] != b'\n' {
                    blank(&mut code, &mut masked, b[i]);
                    i += 1;
                }
            } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
                // Swift block comments nest.
                let mut depth = 0i32;
                loop {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        blank(&mut code, &mut masked, b[i]);
                        blank(&mut code, &mut masked, b[i + 1]);
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        blank(&mut code, &mut masked, b[i]);
                        blank(&mut code, &mut masked, b[i + 1]);
                        i += 2;
                    } else {
                        blank(&mut code, &mut masked, b[i]);
                        i += 1;
                    }
                    if depth <= 0 || i >= b.len() {
                        break;
                    }
                }
            } else if c == b'"' || raw_start(i) > 0 {
                let hashes = raw_start(i);
                let quote_at = i + hashes;
                let multiline =
                    b.get(quote_at + 1) == Some(&b'"') && b.get(quote_at + 2) == Some(&b'"');
                let quotes: &[u8] = if multiline { b"\"\"\"" } else { b"\"" };
                let mut opening = vec![b'#'; hashes];
                opening.extend_from_slice(quotes);
                let mut delimiter = quotes.to_vec();
                delimiter.extend(std::iter::repeat_n(b'#', hashes));
                code.extend_from_slice(&opening);
                masked.extend_from_slice(&opening);
                i += opening.len();
                while i < b.len() {
                    if hashes == 0 && b[i] == b'\\' && i + 1 < b.len() {
                        code.extend_from_slice(&b[i..i + 2]);
                        masked.extend_from_slice(b"__");
                        i += 2;
                        continue;
                    }
                    if b[i..].starts_with(&delimiter) {
                        code.extend_from_slice(&delimiter);
                        masked.extend_from_slice(&delimiter);
                        i += delimiter.len();
                        break;
                    }
                    if !multiline && b[i] == b'\n' {
                        break;
                    }
                    code.push(b[i]);
                    masked.push(if b[i] == b'\n' { b'\n' } else { b'_' });
                    i += 1;
                }
            } else {
                code.push(c);
                masked.push(c);
                i += 1;
            }
        }
        (code, masked)
    }

    pub fn closing(masked: &[u8], open: usize) -> Option<usize> {
        let mut depth = 0i64;
        for (i, c) in masked.iter().enumerate().skip(open) {
            match c {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        None
    }

    pub fn split_top(masked: &[u8], start: usize, end: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut depth = 0i64;
        let mut seg = start;
        for (i, c) in masked.iter().enumerate().take(end).skip(start) {
            match c {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth -= 1,
                b',' if depth == 0 => {
                    out.push((seg, i));
                    seg = i + 1;
                }
                _ => {}
            }
        }
        out.push((seg, end));
        out
    }

    fn word(c: u8) -> bool {
        c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
    }

    /// Matches of `.{name}\s*(` that are calls (not `foo.name(`), or follow `{prefix}`:
    /// (match start, opening parenthesis).
    pub fn calls(masked: &[u8], names: &[&str], prefix: &str) -> Vec<(usize, usize, String)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < masked.len() {
            if masked[i] != b'.' {
                i += 1;
                continue;
            }
            let rest = &masked[i + 1..];
            let Some(name) = names.iter().find(|n| {
                rest.starts_with(n.as_bytes()) && rest.get(n.len()).is_none_or(|c| !word(*c))
            }) else {
                i += 1;
                continue;
            };
            let before_ok =
                i == 0 || !(word(masked[i - 1]) || masked[i - 1] == b']' || masked[i - 1] == b')');
            let after_prefix = masked[..i].ends_with(prefix.as_bytes())
                && (i == prefix.len() || !word(masked[i - prefix.len() - 1]));
            if !(before_ok || after_prefix) {
                i += 1;
                continue;
            }
            let mut j = i + 1 + name.len();
            while j < masked.len() && (masked[j] as char).is_whitespace() {
                j += 1;
            }
            if masked.get(j) == Some(&b'(') {
                out.push((i, j, (*name).to_owned()));
            }
            i += 1;
        }
        out
    }

    /// `^\s*(label)\s*:(?!:)`: the label and the byte length matched.
    pub fn label(s: &[u8]) -> Option<(String, usize)> {
        let mut i = 0;
        while i < s.len() && (s[i] as char).is_whitespace() {
            i += 1;
        }
        let start = i;
        if i >= s.len() || !(s[i].is_ascii_alphabetic() || s[i] == b'_') {
            return None;
        }
        while i < s.len() && word(s[i]) {
            i += 1;
        }
        let name = String::from_utf8_lossy(&s[start..i]).into_owned();
        while i < s.len() && (s[i] as char).is_whitespace() {
            i += 1;
        }
        if s.get(i) != Some(&b':') || s.get(i + 1) == Some(&b':') {
            return None;
        }
        Some((name, i + 1))
    }

    /// A single-line string literal (plain or raw), `None` when not static.
    pub fn literal(value: &str) -> Option<String> {
        let t = value.trim();
        let hashes = t.bytes().take_while(|b| *b == b'#').count();
        if hashes > 0 {
            let h = "#".repeat(hashes);
            let inner = t
                .strip_prefix(&format!("{h}\""))?
                .strip_suffix(&format!("\"{h}"))?;
            if inner.contains('\n') || inner.contains(&format!("\\{h}(")) {
                return None;
            }
            return Some(inner.to_owned());
        }
        let inner = t.strip_prefix('"')?.strip_suffix('"')?;
        // Escapes stay inside: an unescaped quote ends a literal.
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    chars.next()?;
                }
                '"' => return None,
                _ => {}
            }
        }
        if inner.contains("\\(") {
            return None;
        }
        Some(inner.replace("\\\"", "\"").replace("\\\\", "\\"))
    }

    fn version_init(v: &str) -> Option<String> {
        let t = v
            .trim()
            .strip_prefix("Version")?
            .trim_start()
            .strip_prefix('(')?;
        let t = t.trim_end().strip_suffix(')')?;
        let parts: Vec<&str> = t.split(',').map(str::trim).collect();
        if parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        {
            Some(parts.join("."))
        } else {
            None
        }
    }

    fn read_version(value: &str) -> Option<String> {
        let mut v = value.trim().to_owned();
        if let Some((l, n)) = label(v.as_bytes())
            && l == "from"
        {
            v = v[n..].trim().to_owned();
        }
        literal(&v).or_else(|| version_init(&v))
    }

    #[derive(Clone, Debug)]
    pub struct Requirement {
        pub text: String,
        pub version: Option<String>,
        pub git_reference: Option<String>,
    }
    fn version_req(kind: &str, value: &str) -> Option<Requirement> {
        let v = read_version(value)?;
        Some(Requirement {
            text: format!("{kind}:{v}"),
            version: Some(v),
            git_reference: None,
        })
    }
    fn reference_req(kind: &str, value: &str) -> Option<Requirement> {
        let r = literal(value)?;
        let git = if kind == "revision" {
            format!("rev:{r}")
        } else {
            format!("branch:{r}")
        };
        Some(Requirement {
            text: format!("{kind}:{r}"),
            version: None,
            git_reference: Some(git),
        })
    }
    fn unlabeled(value: &str) -> Option<Requirement> {
        let t = value.trim();
        // "1.0.0"..<"2.0.0" or "1.0.0"..."2.0.0"
        for op in ["..<", "..."] {
            if let Some((l, u)) = t.split_once(op) {
                let (l, u) = (l.trim(), u.trim());
                let q = |s: &str| {
                    s.strip_prefix('"')
                        .and_then(|s| s.strip_suffix('"'))
                        .filter(|s| !s.is_empty() && !s.contains('"'))
                        .map(str::to_owned)
                };
                if let (Some(lo), Some(up)) = (q(l), q(u)) {
                    return Some(Requirement {
                        text: format!("range:{lo}{op}{up}"),
                        version: Some(lo),
                        git_reference: None,
                    });
                }
            }
        }
        let t = t
            .strip_prefix("Package.Dependency.Requirement")
            .unwrap_or(t)
            .strip_prefix('.')?;
        for kind in [
            "upToNextMajor",
            "upToNextMinor",
            "exact",
            "branch",
            "revision",
        ] {
            if let Some(rest) = t.strip_prefix(kind) {
                let rest = rest.trim_start();
                let arg = rest.strip_prefix('(')?.trim_end().strip_suffix(')')?;
                return match kind {
                    "branch" | "revision" => reference_req(kind, arg),
                    k => version_req(k, arg),
                };
            }
        }
        None
    }

    #[derive(Clone, Debug)]
    pub struct Declaration {
        pub name: Option<String>,
        pub url: Option<String>,
        pub path: Option<String>,
        pub id: Option<String>,
        pub requirement: Option<Requirement>,
    }

    fn declaration(code: &str, masked: &[u8], start: usize, end: usize) -> Option<Declaration> {
        let (mut url, mut path, mut id, mut name, mut req) = (None, None, None, None, None);
        for (a, b) in split_top(masked, start, end) {
            let arg = code[a..b].trim();
            if arg.is_empty() {
                continue;
            }
            let (l, value) = match label(&masked[a..b]) {
                Some((l, n)) => (Some(l), code[a + n..b].trim().to_owned()),
                None => (None, arg.to_owned()),
            };
            match l.as_deref() {
                Some("url") => url = literal(&value),
                Some("path") => path = literal(&value),
                Some("id") => id = literal(&value),
                Some("name") => name = literal(&value),
                Some("from") => req = version_req("upToNextMajor", &value),
                Some("exact") => req = version_req("exact", &value),
                Some("branch") => req = reference_req("branch", &value),
                Some("revision") => req = reference_req("revision", &value),
                None | Some("_") => req = unlabeled(&value).or(req),
                _ => {}
            }
        }
        if url.is_none() && path.is_none() && id.is_none() {
            return None;
        }
        Some(Declaration {
            name,
            url,
            path,
            id,
            requirement: req,
        })
    }

    #[derive(Clone, Debug)]
    pub struct Target {
        pub kind: String,
        pub name: String,
        pub path: Option<String>,
        pub products: Vec<(String, String)>,
        pub sources: Option<Vec<String>>,
    }

    fn string_array(code: &str, masked: &[u8], start: usize, end: usize) -> Option<Vec<String>> {
        let open = masked[start..end].iter().position(|c| *c == b'[')? + start;
        let close = closing(masked, open)?;
        let mut out = Vec::new();
        for (a, b) in split_top(masked, open + 1, close) {
            if code[a..b].trim().is_empty() {
                continue;
            }
            out.push(literal(&code[a..b])?.trim_matches('/').to_owned());
        }
        Some(out)
    }

    fn products(code: &str, masked: &[u8], start: usize, end: usize) -> Vec<(String, String)> {
        let Some(open) = masked[start..end]
            .iter()
            .position(|c| *c == b'[')
            .map(|p| p + start)
        else {
            return Vec::new();
        };
        let Some(close) = closing(masked, open) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (a, b) in split_top(masked, open + 1, close) {
            let el = &masked[a..b];
            let lead = el
                .iter()
                .take_while(|c| (**c as char).is_whitespace())
                .count();
            let mut rest = &el[lead..];
            let mut off = a + lead;
            if rest.starts_with(b"Target.Dependency") {
                rest = &rest[17..];
                off += 17;
            }
            let Some(after) = rest.strip_prefix(b".product") else {
                continue;
            };
            let ws = after
                .iter()
                .take_while(|c| (**c as char).is_whitespace())
                .count();
            if after.get(ws) != Some(&b'(') {
                continue;
            }
            let call_open = off + 8 + ws;
            let Some(call_close) = closing(masked, call_open) else {
                continue;
            };
            let (mut name, mut package) = (None, None);
            for (x, y) in split_top(masked, call_open + 1, call_close) {
                let Some((l, n)) = label(&masked[x..y]) else {
                    continue;
                };
                let v = &code[x + n..y];
                match l.as_str() {
                    "name" => name = literal(v),
                    "package" => package = literal(v),
                    _ => {}
                }
            }
            if let (Some(n), Some(p)) = (name, package) {
                out.push((n, p));
            }
        }
        out
    }

    const TARGET_KINDS: [&str; 7] = [
        "target",
        "executableTarget",
        "testTarget",
        "macro",
        "plugin",
        "binaryTarget",
        "systemLibrary",
    ];

    fn targets(code: &str, masked: &[u8]) -> Vec<Target> {
        let mut out = Vec::new();
        let mut consumed = 0usize;
        let mut first = true;
        for (at, open, kind) in calls(masked, &TARGET_KINDS, "Target") {
            // A `.target(name:)` dependency inside an already-read target is not a declaration.
            if !first && at < consumed {
                continue;
            }
            let Some(close) = closing(masked, open) else {
                continue;
            };
            first = false;
            consumed = close;
            let (mut name, mut path, mut sources, mut prods) = (None, None, None, Vec::new());
            for (a, b) in split_top(masked, open + 1, close) {
                let Some((l, n)) = label(&masked[a..b]) else {
                    continue;
                };
                let v = &code[a + n..b];
                match l.as_str() {
                    "name" => name = literal(v),
                    "path" => path = literal(v),
                    "dependencies" => prods.extend(products(code, masked, a + n, b)),
                    "sources" => sources = string_array(code, masked, a + n, b),
                    _ => {}
                }
            }
            if let Some(name) = name {
                out.push(Target {
                    kind,
                    name,
                    path,
                    products: prods,
                    sources,
                });
            }
        }
        out
    }

    /// `Package(name: "...")`: the package name when it is a static literal.
    fn package_name(code: &str, masked: &[u8]) -> Option<String> {
        let m = std::str::from_utf8(masked).ok()?;
        let mut from = 0;
        while let Some(p) = m[from..].find("Package") {
            let at = from + p;
            from = at + 7;
            if at > 0 && word(masked[at - 1]) {
                continue;
            }
            let rest = m[at + 7..].trim_start();
            let Some(rest) = rest.strip_prefix('(') else {
                continue;
            };
            let rest = rest.trim_start();
            let Some(rest) = rest.strip_prefix("name") else {
                continue;
            };
            let rest = rest.trim_start();
            let Some(rest) = rest.strip_prefix(':') else {
                continue;
            };
            let value = rest.trim_start();
            let vstart = m.len() - value.len();
            let hashes = value.bytes().take_while(|b| *b == b'#').count();
            let body = &value[hashes..];
            let Some(body) = body.strip_prefix('"') else {
                continue;
            };
            let underscores = body.bytes().take_while(|b| *b == b'_').count();
            let close = format!("\"{}", "#".repeat(hashes));
            if !body[underscores..].starts_with(&close) {
                continue;
            }
            let len = hashes + 1 + underscores + close.len();
            return literal(&code[vstart..vstart + len]);
        }
        None
    }

    pub struct Parsed {
        pub name: Option<String>,
        pub dependencies: Vec<Declaration>,
        pub targets: Vec<Target>,
    }

    pub fn parse(content: &str) -> Parsed {
        let (code, masked) = scan(content);
        let code = String::from_utf8_lossy(&code).into_owned();
        let mut deps = Vec::new();
        for (_, open, _) in calls(&masked, &["package"], "Dependency") {
            let Some(close) = closing(&masked, open) else {
                continue;
            };
            if let Some(d) = declaration(&code, &masked, open + 1, close) {
                deps.push(d);
            }
        }
        Parsed {
            name: package_name(&code, &masked),
            dependencies: deps,
            targets: targets(&code, &masked),
        }
    }

    /// SwiftPM's canonical package location.
    pub fn canonical_location(location: &str) -> String {
        let mut v = location.trim().to_owned();
        if let Some(s) = v.find("://") {
            v = v[s + 3..].to_owned();
            let at = v.find('@');
            let slash = v.find('/');
            if let Some(a) = at
                && slash.is_none_or(|s| a < s)
            {
                v = v[a + 1..].to_owned();
            }
        } else if let Some((host, path)) = scp(&v) {
            v = format!("{host}/{path}");
        }
        let mut v = v.trim_end_matches('/').to_owned();
        if v.to_ascii_lowercase().ends_with(".git") {
            v.truncate(v.len() - 4);
        }
        v.trim_end_matches('/').to_lowercase()
    }

    /// `^(?:[^@/\s]+@)?(host[^:/\s]{2,}):(path[^/].*)$`
    fn scp(v: &str) -> Option<(String, String)> {
        let rest = match v.find('@') {
            Some(a) if !v[..a].is_empty() && !v[..a].contains(['/', ' ', '\t', '\n']) => {
                &v[a + 1..]
            }
            _ => v,
        };
        let colon = rest.find(':')?;
        let host = &rest[..colon];
        let path = &rest[colon + 1..];
        if host.chars().count() >= 2
            && !host.contains(['/', ' ', '\t', '\n', '@'])
            && !path.is_empty()
            && !path.starts_with('/')
        {
            Some((host.to_owned(), path.to_owned()))
        } else {
            None
        }
    }

    fn identity_of(canonical: &str) -> String {
        canonical.rsplit('/').next().unwrap_or(canonical).to_owned()
    }

    fn is_local(location: &str) -> bool {
        let l = location.to_ascii_lowercase();
        let b = location.as_bytes();
        l.starts_with("file://")
            || location.starts_with(['/', '~', '.'])
            || (b.len() > 2 && b[1] == b':' && (b[0] as char).is_alphabetic())
    }

    /// JSON with comments and trailing commas removed (the retained reader allows both).
    fn lenient_json(text: &str) -> String {
        let b = text.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        let mut in_string = false;
        while i < b.len() {
            let c = b[i];
            if in_string {
                out.push(c);
                if c == b'\\' && i + 1 < b.len() {
                    out.push(b[i + 1]);
                    i += 2;
                    continue;
                }
                if c == b'"' {
                    in_string = false;
                }
                i += 1;
                continue;
            }
            if c == b'"' {
                in_string = true;
                out.push(c);
                i += 1;
            } else if c == b'/' && b.get(i + 1) == Some(&b'/') {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            } else if c == b',' {
                let mut j = i + 1;
                while j < b.len() && (b[j] as char).is_whitespace() {
                    j += 1;
                }
                if !matches!(b.get(j), Some(b'}') | Some(b']')) {
                    out.push(c);
                }
                i += 1;
            } else {
                out.push(c);
                i += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    pub struct Pin {
        pub identity: String,
        pub key: String,
        pub version_part: Option<String>,
        pub props: Map<String, Value>,
    }

    pub fn resolved(text: &str, path: &str) -> Result<Vec<Pin>, String> {
        let root: Value = serde_json::from_str(&lenient_json(text)).map_err(|e| e.to_string())?;
        let mut format = root.get("version").and_then(Value::as_i64).unwrap_or(0);
        let pins = if let Some(p) = root.get("object").and_then(|o| o.get("pins")) {
            if format == 0 {
                format = 1;
            }
            p
        } else if let Some(p) = root.get("pins") {
            p
        } else {
            return Err(format!("Package.resolved {path} has no pins"));
        };
        let Some(pins) = pins.as_array() else {
            return Err(format!("Package.resolved {path} pins are not an array"));
        };
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        let mut out = Vec::new();
        for pin in pins {
            if !pin.is_object() {
                continue;
            }
            let location = s(pin, "location")
                .or_else(|| s(pin, "repositoryURL"))
                .unwrap_or_default();
            let kind = s(pin, "kind");
            let state = pin
                .get("state")
                .filter(|v| v.is_object())
                .cloned()
                .unwrap_or(Value::Null);
            let version = s(&state, "version");
            let revision = s(&state, "revision");
            let branch = s(&state, "branch");
            let (identity, key, source_kind, source) = if kind.as_deref() == Some("registry") {
                let identity = s(pin, "identity")
                    .unwrap_or_else(|| location.clone())
                    .to_lowercase();
                if identity.is_empty() {
                    continue;
                }
                (
                    identity.clone(),
                    format!("swiftpm:registry:{identity}"),
                    "registry",
                    identity,
                )
            } else {
                if location.is_empty() {
                    continue;
                }
                let source = canonical_location(&location);
                let identity = s(pin, "identity")
                    .map(|i| i.to_lowercase())
                    .unwrap_or_else(|| identity_of(&source));
                let local = kind.as_deref() == Some("localSourceControl")
                    || (kind.is_none() && is_local(&location));
                (
                    identity,
                    format!("swiftpm:scm:{source}"),
                    if local { "local" } else { "remote" },
                    source,
                )
            };
            let mut props = obj(json!({
                "ecosystem": "swiftpm",
                "package_name": s(pin, "package").unwrap_or_else(|| identity.clone()),
                "identity": identity, "version": version.clone().unwrap_or_default(),
                "package_key": key, "source_kind": source_kind, "source": source, "location": location,
                "is_definition": false, "dependency_kind": "pinned", "resolved_format_version": format,
                "manifest_path": path, "confidence": "high",
            }));
            if let Some(k) = &kind {
                props.insert("pin_kind".into(), json!(k));
            }
            if let Some(r) = &revision {
                props.insert("revision".into(), json!(r));
            }
            if let Some(b) = &branch {
                props.insert("branch".into(), json!(b));
            }
            out.push(Pin {
                identity,
                key,
                version_part: version.or(revision).or(branch),
                props,
            });
        }
        Ok(out)
    }

    fn default_dir(t: &Target) -> Option<String> {
        match t.kind.as_str() {
            "testTarget" => Some(format!("Tests/{}", t.name)),
            "plugin" => Some(format!("Plugins/{}", t.name)),
            "binaryTarget" => None,
            _ => Some(format!("Sources/{}", t.name)),
        }
    }

    pub fn extract(
        repository: &str,
        files: &[(String, Vec<u8>)],
    ) -> Result<BTreeMap<String, FileExtract>, ExtractFault> {
        let mut shares = Shares::new(files);
        // Shallowest first, so the repository's own manifest wins over nested copies.
        let mut ordered: Vec<&(String, Vec<u8>)> = files.iter().collect();
        ordered.sort_by(|a, b| {
            (a.0.matches('/').count(), &a.0).cmp(&(b.0.matches('/').count(), &b.0))
        });
        let named = |p: &str, n: &str| p.rsplit('/').next() == Some(n);
        let repo_lc = repository.to_lowercase();
        // Pass 1: Package.swift definitions and targets.
        let mut parsed: Vec<(String, Parsed)> = Vec::new();
        let mut definition_by_dir: HashMap<String, (String, usize)> = HashMap::new();
        for (path, bytes) in ordered.iter().filter(|(p, _)| named(p, "Package.swift")) {
            let Ok(text) = std::str::from_utf8(bytes) else {
                shares
                    .units
                    .get_mut(path.as_str())
                    .expect("member")
                    .incomplete
                    .push("swiftpm: Package.swift is not UTF-8".into());
                continue;
            };
            let p = parse(text);
            let dir = dir_of(path).to_owned();
            let owner = match &p.name {
                Some(name) => {
                    let key = format!("swiftpm:local:{repo_lc}:{}", name.to_lowercase());
                    let qn = qualified(&key, None);
                    let props = obj(json!({
                        "ecosystem": "swiftpm", "package_name": name, "identity": name.to_lowercase(),
                        "version": "", "package_key": key, "source_kind": "local",
                        "source": trim_slashes(&dir), "is_definition": true, "manifest_path": path,
                        "confidence": "high",
                    }));
                    let (u, i, added) = shares.add(path, "Package", name, &qn, props);
                    definition_by_dir.insert(dir.clone(), (u.clone(), i));
                    if added {
                        shares.link(
                            path,
                            0,
                            "CONTAINS_PROJECT",
                            &u,
                            i,
                            obj(json!({"ecosystem": "swiftpm", "source": "Package.swift", "scope": "repository"})),
                        );
                    }
                    Some((u, i))
                }
                None => None,
            };
            for t in &p.targets {
                let mut props = obj(json!({
                    "ecosystem": "swiftpm", "language": "Swift", "module_name": t.name,
                    "target_kind": t.kind, "manifest_path": path, "confidence": "high",
                }));
                if let Some((u, i)) = &owner {
                    props.insert("package_name".into(), json!(shares.units[u].nodes[*i].name));
                }
                if let Some(src) = t.path.clone().or_else(|| default_dir(t)) {
                    let source_path = trim_slashes(&if dir.is_empty() {
                        src
                    } else {
                        format!("{dir}/{src}")
                    });
                    props.insert("source_path".into(), json!(source_path));
                    if let Some(sources) = t.sources.as_ref().filter(|s| !s.is_empty()) {
                        let mut seen = Vec::new();
                        for s in sources {
                            let p = trim_slashes(&if source_path.is_empty() {
                                s.clone()
                            } else {
                                format!("{source_path}/{s}")
                            });
                            if !seen.contains(&p) {
                                seen.push(p);
                            }
                        }
                        props.insert("source_paths".into(), json!(seen.join(",")));
                    }
                }
                let qn = format!("{repository}:{path}#target:{}", t.name);
                let (u, i, added) = shares.add(path, "Module", &t.name, &qn, props);
                if added {
                    let (su, si) = owner.clone().unwrap_or_else(|| (path.clone(), 0));
                    shares.link(
                        &su,
                        si,
                        "DEFINES",
                        &u,
                        i,
                        obj(json!({"ecosystem": "swiftpm", "target_kind": t.kind})),
                    );
                }
            }
            parsed.push((path.clone(), p));
        }
        // Pass 2: declared dependencies.
        for (path, p) in &parsed {
            let dir = dir_of(path).to_owned();
            let owner = p
                .name
                .as_ref()
                .and_then(|_| definition_by_dir.get(&dir))
                .cloned();
            // A source outside this manifest's unit is the repository (its file node here).
            let (su, si) = match &owner {
                Some((u, i)) if u == path => (u.clone(), *i),
                _ => (path.clone(), 0),
            };
            let mut declared: Vec<(Declaration, String, usize)> = Vec::new();
            for d in &p.dependencies {
                let (identity, key, source_kind, source, location, resolved_dir, external) =
                    if let Some(rel) = &d.path {
                        let (resolved, inside) = resolve(&dir, rel);
                        let identity = resolved
                            .rsplit('/')
                            .next()
                            .unwrap_or(&resolved)
                            .to_lowercase();
                        let key = format!("swiftpm:path:{repo_lc}:{}", resolved.to_lowercase());
                        (
                            identity,
                            key,
                            "path",
                            resolved.clone(),
                            rel.clone(),
                            Some(resolved.clone()),
                            (!inside).then_some(resolved),
                        )
                    } else if let Some(url) = &d.url {
                        let source = canonical_location(url);
                        let identity = identity_of(&source);
                        let kind = if is_local(url) { "local" } else { "remote" };
                        (
                            identity,
                            format!("swiftpm:scm:{source}"),
                            kind,
                            source,
                            url.clone(),
                            None,
                            None,
                        )
                    } else {
                        let id = d.id.clone().unwrap_or_default();
                        let source = id.to_lowercase();
                        (
                            source.clone(),
                            format!("swiftpm:registry:{source}"),
                            "registry",
                            source,
                            id,
                            None,
                            None,
                        )
                    };
                let mut props = obj(json!({
                    "ecosystem": "swiftpm", "package_name": d.name.clone().unwrap_or_else(|| identity.clone()),
                    "identity": identity,
                    "version": d.requirement.as_ref().and_then(|r| r.version.clone()).unwrap_or_default(),
                    "package_key": key, "source_kind": source_kind, "source": source, "location": location,
                    "is_definition": false, "dependency_kind": "declared", "manifest_path": path,
                    "confidence": "high",
                }));
                if let Some(r) = &d.requirement {
                    props.insert("version_requirement".into(), json!(r.text));
                    if let Some(g) = &r.git_reference {
                        props.insert("git_reference".into(), json!(g));
                    }
                }
                if let Some(e) = &external {
                    props.insert("external_path".into(), json!(e));
                }
                if d.path.is_some()
                    && let Some((lu, li)) = resolved_dir
                        .as_ref()
                        .and_then(|r| definition_by_dir.get(r))
                        .cloned()
                {
                    let mut ep = props.clone();
                    ep.insert(
                        "canonical_package_name".into(),
                        json!(shares.units[&lu].nodes[li].name),
                    );
                    shares.link(&su, si, "REFERENCES_PACKAGE", &lu, li, ep);
                    continue;
                }
                let qn = qualified(&key, d.requirement.as_ref().map(|r| r.text.as_str()));
                let (u, i, _) = shares.add(path, "Package", &identity, &qn, props);
                let mut ep = shares.units[&u].nodes[i].properties.clone();
                let canonical = ep.get("package_name").cloned().unwrap_or(Value::Null);
                ep.insert("canonical_package_name".into(), canonical);
                shares.link(&su, si, "REFERENCES_PACKAGE", &u, i, ep);
                declared.push((d.clone(), u, i));
            }
            // The products the manifest's targets use from each declared dependency.
            for (product, package) in p.targets.iter().flat_map(|t| t.products.iter()) {
                let found = declared.iter().find(|(d, u, i)| {
                    shares.units[u].nodes[*i].properties["identity"]
                        .as_str()
                        .is_some_and(|id| id.eq_ignore_ascii_case(package))
                        || d.name
                            .as_deref()
                            .is_some_and(|n| n.eq_ignore_ascii_case(package))
                });
                let Some((_, u, i)) = found.cloned() else {
                    continue;
                };
                let props = shares.props_mut(&u, i);
                let mut set: BTreeSet<String> = props
                    .get(PRODUCTS)
                    .and_then(Value::as_str)
                    .map(|s| s.split(',').map(str::to_owned).collect())
                    .unwrap_or_default();
                set.insert(product.clone());
                props.insert(
                    PRODUCTS.into(),
                    json!(set.into_iter().collect::<Vec<_>>().join(",")),
                );
            }
        }
        // Pass 3: resolved pins.
        for (path, bytes) in ordered.iter().filter(|(p, _)| named(p, "Package.resolved")) {
            let text = String::from_utf8_lossy(bytes);
            match resolved(&text, path) {
                Ok(pins) => {
                    for pin in pins {
                        let qn = qualified(&pin.key, pin.version_part.as_deref());
                        let (u, i, _) = shares.add(path, "Package", &pin.identity, &qn, pin.props);
                        let mut ep = shares.units[&u].nodes[i].properties.clone();
                        let canonical = ep.get("package_name").cloned().unwrap_or(Value::Null);
                        ep.insert("canonical_package_name".into(), canonical);
                        shares.link(path, 0, "REFERENCES_PACKAGE", &u, i, ep);
                    }
                }
                Err(e) => shares
                    .units
                    .get_mut(path.as_str())
                    .expect("member")
                    .incomplete
                    .push(format!("swiftpm: {e}")),
            }
        }
        // Pass 4: native targets of every Xcode project.
        for (path, bytes) in ordered.iter().filter(|(p, _)| named(p, "project.pbxproj")) {
            let text = String::from_utf8_lossy(bytes);
            match super::xcode::native_targets(&text) {
                Ok(targets) => {
                    let project_dir = trim_slashes(dir_of(dir_of(path)));
                    for t in targets {
                        let mut props = obj(json!({
                            "ecosystem": "xcode", "language": "Swift", "module_name": t.module,
                            "target_name": t.name, "manifest_path": path, "project_directory": project_dir,
                            "confidence": "high",
                        }));
                        if let Some(k) = t.product_type.filter(|k| !k.is_empty()) {
                            props.insert("target_kind".into(), json!(k));
                        }
                        let qn = format!("{repository}:{path}#target:{}", t.name);
                        let (u, i, added) = shares.add(path, "Module", &t.module, &qn, props);
                        if added {
                            shares.link(
                                path,
                                0,
                                "DEFINES",
                                &u,
                                i,
                                obj(json!({"ecosystem": "xcode", "source": "project.pbxproj", "scope": "repository"})),
                            );
                        }
                    }
                }
                Err(e) => shares
                    .units
                    .get_mut(path.as_str())
                    .expect("member")
                    .incomplete
                    .push(format!("swiftpm: {e}")),
            }
        }
        Ok(shares.units)
    }
}

mod xcode {
    //! `XcodeProjectReader`: native targets of an OpenStep property list and their modules.

    /// A property-list value; dictionaries keep file order.
    #[derive(Clone, Debug)]
    pub enum P {
        S(String),
        A(Vec<P>),
        D(Vec<(String, P)>),
    }
    impl P {
        fn get(&self, k: &str) -> Option<&P> {
            match self {
                P::D(d) => d.iter().rev().find(|(x, _)| x == k).map(|(_, v)| v),
                _ => None,
            }
        }
        fn text(&self, k: &str) -> Option<&str> {
            match self.get(k) {
                Some(P::S(s)) => Some(s),
                _ => None,
            }
        }
    }

    struct Parser<'a> {
        t: &'a [u8],
        s: &'a str,
        i: usize,
    }
    impl Parser<'_> {
        fn err(&self, m: &str) -> String {
            format!("{m} at offset {} of the Xcode project", self.i)
        }
        fn trivia(&mut self) {
            while self.i < self.t.len() {
                let c = self.t[self.i];
                if (c as char).is_whitespace() {
                    self.i += 1;
                } else if self.t[self.i..].starts_with(b"//") {
                    self.i = self.s[self.i..]
                        .find('\n')
                        .map_or(self.t.len(), |e| self.i + e + 1);
                } else if self.t[self.i..].starts_with(b"/*") {
                    self.i = self.s[self.i + 2..]
                        .find("*/")
                        .map_or(self.t.len(), |e| self.i + 2 + e + 2);
                } else {
                    return;
                }
            }
        }
        fn value(&mut self, depth: usize) -> Result<P, String> {
            if depth > 256 {
                return Err(self.err("Nesting too deep"));
            }
            self.trivia();
            match self.t.get(self.i) {
                None => Err(self.err("Unexpected end of file")),
                Some(b'{') => {
                    self.i += 1;
                    let mut d = Vec::new();
                    loop {
                        self.trivia();
                        if self.t.get(self.i) == Some(&b'}') {
                            self.i += 1;
                            return Ok(P::D(d));
                        }
                        let P::S(k) = self.value(depth + 1)? else {
                            return Err(self.err("Dictionary key is not a string"));
                        };
                        self.expect(b'=')?;
                        let v = self.value(depth + 1)?;
                        self.expect(b';')?;
                        d.push((k, v));
                    }
                }
                Some(b'(') => {
                    self.i += 1;
                    let mut a = Vec::new();
                    loop {
                        self.trivia();
                        if self.t.get(self.i) == Some(&b')') {
                            self.i += 1;
                            return Ok(P::A(a));
                        }
                        a.push(self.value(depth + 1)?);
                        self.trivia();
                        match self.t.get(self.i) {
                            Some(b',') => self.i += 1,
                            Some(b')') => {}
                            _ => return Err(self.err("Expected ',' or ')'")),
                        }
                    }
                }
                Some(q @ (b'"' | b'\'')) => {
                    let q = *q as char;
                    self.i += 1;
                    let mut out = String::new();
                    let mut chars = self.s[self.i..].char_indices();
                    loop {
                        let Some((o, c)) = chars.next() else {
                            return Err(self.err("Unterminated string"));
                        };
                        if c == q {
                            self.i += o + 1;
                            return Ok(P::S(out));
                        }
                        if c != '\\' {
                            out.push(c);
                            continue;
                        }
                        let Some((_, e)) = chars.next() else {
                            return Err(self.err("Unterminated string"));
                        };
                        match e {
                            'n' => out.push('\n'),
                            't' => out.push('\t'),
                            'r' => out.push('\r'),
                            'U' => {
                                let rest = chars.as_str();
                                match rest.get(..4).and_then(|h| u32::from_str_radix(h, 16).ok()) {
                                    Some(code) => {
                                        out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                                        for _ in 0..4 {
                                            chars.next();
                                        }
                                    }
                                    None => out.push('U'),
                                }
                            }
                            other => out.push(other),
                        }
                    }
                }
                Some(b'<') => {
                    let end = self.s[self.i..]
                        .find('>')
                        .ok_or_else(|| self.err("Unterminated data"))?;
                    let data = self.s[self.i + 1..self.i + end].to_owned();
                    self.i += end + 1;
                    Ok(P::S(data))
                }
                Some(_) => {
                    let start = self.i;
                    while self.i < self.t.len() {
                        let c = self.t[self.i];
                        if c.is_ascii_alphanumeric() || c >= 0x80 || b"_$/:.-+~".contains(&c) {
                            self.i += 1;
                        } else {
                            break;
                        }
                    }
                    if self.i == start {
                        return Err(self.err("Unexpected character"));
                    }
                    Ok(P::S(self.s[start..self.i].to_owned()))
                }
            }
        }
        fn expect(&mut self, c: u8) -> Result<(), String> {
            self.trivia();
            if self.t.get(self.i) != Some(&c) {
                return Err(self.err(&format!("Expected '{}'", c as char)));
            }
            self.i += 1;
            Ok(())
        }
    }

    pub fn parse(text: &str) -> Result<P, String> {
        let mut p = Parser {
            t: text.as_bytes(),
            s: text,
            i: 0,
        };
        let v = p.value(0)?;
        p.trivia();
        if p.i < p.t.len() {
            return Err(p.err("Unexpected content after the root value"));
        }
        Ok(v)
    }

    pub struct Native {
        pub name: String,
        pub module: String,
        pub product_type: Option<String>,
    }

    /// Xcode's `c99extidentifier`.
    pub fn c99(value: &str) -> String {
        let mut out: String = value
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        if out.chars().next().is_some_and(|c| c.is_numeric()) {
            out.insert(0, '_');
        }
        out
    }

    type Settings = Vec<(String, P)>;

    fn configurations(objects: &P, owner: &P) -> Vec<(String, Settings)> {
        let mut out: Vec<(String, Settings)> = Vec::new();
        let Some(list) = owner
            .text("buildConfigurationList")
            .and_then(|id| objects.get(id))
        else {
            return out;
        };
        let Some(P::A(ids)) = list.get("buildConfigurations") else {
            return out;
        };
        for id in ids {
            let P::S(id) = id else { continue };
            if let Some(c) = objects.get(id)
                && let Some(name) = c.text("name")
                && !out.iter().any(|(n, _)| n == name)
            {
                let settings = match c.get("buildSettings") {
                    Some(P::D(d)) => d.clone(),
                    _ => Vec::new(),
                };
                out.push((name.to_owned(), settings));
            }
        }
        out
    }

    struct Eval<'a> {
        target: &'a str,
        product: Option<&'a str>,
        settings: &'a Settings,
        evaluating: Vec<String>,
    }
    impl Eval<'_> {
        fn raw(&self, k: &str) -> Option<&P> {
            self.settings
                .iter()
                .rev()
                .find(|(x, _)| x == k)
                .map(|(_, v)| v)
        }
        fn evaluate(&mut self, setting: &str) -> Option<String> {
            let Some(P::S(raw)) = self.raw(setting).cloned() else {
                return None;
            };
            if self.evaluating.iter().any(|s| s == setting) {
                return None;
            }
            self.evaluating.push(setting.to_owned());
            let out = self.expand(&raw);
            self.evaluating.retain(|s| s != setting);
            out
        }
        fn product_name(&mut self) -> Option<String> {
            if self.raw("PRODUCT_NAME").is_some() {
                return self.evaluate("PRODUCT_NAME");
            }
            Some(
                self.product
                    .filter(|p| !p.trim().is_empty())
                    .unwrap_or(self.target)
                    .to_owned(),
            )
        }
        /// `$(NAME)`, `${NAME}`, `$(NAME:mod1:mod2)`.
        fn expand(&mut self, raw: &str) -> Option<String> {
            let mut out = String::new();
            let mut failed = false;
            let mut rest = raw;
            while let Some(at) = rest.find('$') {
                out.push_str(&rest[..at]);
                let after = &rest[at + 1..];
                let close = match after.chars().next() {
                    Some('(') => ')',
                    Some('{') => '}',
                    _ => {
                        out.push('$');
                        rest = after;
                        continue;
                    }
                };
                let Some(end) = after.find(close) else {
                    out.push('$');
                    rest = after;
                    continue;
                };
                let inner = &after[1..end];
                let mut parts = inner.split(':');
                let name = parts.next().unwrap_or("");
                let valid = |s: &str| {
                    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                };
                let name_ok = valid(name) && !name.starts_with(|c: char| c.is_ascii_digit());
                let modifiers: Vec<&str> = parts.collect();
                if !name_ok || !modifiers.iter().all(|m| valid(m)) {
                    out.push('$');
                    rest = after;
                    continue;
                }
                let mut value = match name {
                    "inherited" => Some(String::new()),
                    "TARGET_NAME" => Some(self.target.to_owned()),
                    "PRODUCT_NAME" => self.product_name(),
                    "PRODUCT_MODULE_NAME" => self.evaluate(name),
                    _ => None,
                };
                if value.is_none() {
                    failed = true;
                }
                for m in modifiers {
                    let Some(v) = value.as_mut() else { break };
                    match m {
                        "c99extidentifier" | "identifier" => *v = c99(v),
                        "rfc1034identifier" => {
                            *v = v
                                .chars()
                                .map(|c| {
                                    if c.is_alphanumeric() || c == '-' || c == '.' {
                                        c
                                    } else {
                                        '-'
                                    }
                                })
                                .collect();
                        }
                        "lower" => *v = v.to_lowercase(),
                        "upper" => *v = v.to_uppercase(),
                        _ => failed = true,
                    }
                }
                out.push_str(&value.unwrap_or_default());
                rest = &after[end + 1..];
            }
            out.push_str(rest);
            let out = out.trim().to_owned();
            if failed || out.is_empty() || out.contains('$') {
                None
            } else {
                Some(out)
            }
        }
    }

    fn module_name(target: &str, product: Option<&str>, settings: &Settings) -> String {
        let mut e = Eval {
            target,
            product,
            settings,
            evaluating: Vec::new(),
        };
        if let Some(m) = e.evaluate("PRODUCT_MODULE_NAME").filter(|m| !m.is_empty()) {
            return m;
        }
        if let Some(p) = e.product_name().filter(|p| !p.is_empty()) {
            return c99(&p);
        }
        c99(target)
    }

    pub fn native_targets(text: &str) -> Result<Vec<Native>, String> {
        let root = parse(text)?;
        let Some(objects @ P::D(entries)) = root.get("objects") else {
            return Err("The Xcode project has no objects table.".into());
        };
        let project = entries
            .iter()
            .map(|(_, v)| v)
            .find(|v| v.text("isa") == Some("PBXProject"))
            .map(|p| configurations(objects, p))
            .unwrap_or_default();
        let mut out = Vec::new();
        for (_, t) in entries {
            if t.text("isa") != Some("PBXNativeTarget") {
                continue;
            }
            let Some(name) = t.text("name").filter(|n| !n.trim().is_empty()) else {
                continue;
            };
            let target = configurations(objects, t);
            let mut names: Vec<String> = target.iter().map(|(n, _)| n.clone()).collect();
            for (n, _) in &project {
                if !names.contains(n) {
                    names.push(n.clone());
                }
            }
            let product = t.text("productName");
            let modules: Vec<(String, String)> = names
                .iter()
                .map(|c| {
                    let mut merged: Settings = project
                        .iter()
                        .find(|(n, _)| n == c)
                        .map(|(_, s)| s.clone())
                        .unwrap_or_default();
                    if let Some((_, s)) = target.iter().find(|(n, _)| n == c) {
                        merged.extend(s.iter().cloned());
                    }
                    (c.clone(), module_name(name, product, &merged))
                })
                .collect();
            let distinct: BTreeSet<&String> = modules.iter().map(|(_, m)| m).collect();
            let module = if distinct.len() > 1 {
                modules
                    .iter()
                    .find(|(c, _)| c == "Release")
                    .map(|(_, m)| m.clone())
                    .unwrap_or_else(|| modules[0].1.clone())
            } else {
                modules
                    .first()
                    .map(|(_, m)| m.clone())
                    .unwrap_or_else(|| module_name(name, product, &Vec::new()))
            };
            out.push(Native {
                name: name.to_owned(),
                module,
                product_type: t.text("productType").map(str::to_owned),
            });
        }
        Ok(out)
    }
    use std::collections::BTreeSet;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_modules_follow_the_retained_pattern() {
        assert_eq!(
            go_modules("module example.com/app\n\ngo 1.22\n"),
            ["example.com/app"]
        );
        assert_eq!(
            go_modules("module \"example.com/q\" // quoted\n"),
            ["example.com/q"]
        );
        assert!(go_modules("// module x\n").is_empty());
        assert_eq!(go_modules("module a\nmodule b\n").len(), 2);
    }

    #[test]
    fn package_references_match_unqualified_elements_only() {
        let sdk = r#"<Project Sdk="Microsoft.NET.Sdk"><ItemGroup>
            <PackageReference Include="Newtonsoft.Json" Version="13.0.3" />
            <PackageReference Include="Serilog"><Version>3.1.1</Version></PackageReference>
            <PackageReference Version="1.0" />
        </ItemGroup></Project>"#;
        assert_eq!(
            package_references(sdk).unwrap(),
            [
                ("Newtonsoft.Json".to_owned(), "13.0.3".to_owned()),
                ("Serilog".to_owned(), String::new())
            ]
        );
        let legacy = r#"<Project xmlns="http://schemas.microsoft.com/developer/msbuild/2003"><ItemGroup><PackageReference Include="X" Version="1"/></ItemGroup></Project>"#;
        // A namespaced element is a different XName: the retained extraction does not see it.
        let _ = legacy;
        assert!(package_references("<Project><unclosed></Project>").is_err());
    }

    #[test]
    fn swift_locations_are_canonical() {
        for (raw, want) in [
            (
                "https://github.com/apple/swift-log.git",
                "github.com/apple/swift-log",
            ),
            (
                "git@github.com:Apple/Swift-Log.git",
                "github.com/apple/swift-log",
            ),
            ("ssh://git@host.com/a/b/", "host.com/a/b"),
        ] {
            assert_eq!(swiftpm::canonical_location(raw), want, "{raw}");
        }
        assert_eq!(swiftpm::literal(r#""a\"b""#).as_deref(), Some("a\"b"));
        assert_eq!(swiftpm::literal(r##"#"raw"#"##).as_deref(), Some("raw"));
        assert_eq!(swiftpm::literal(r#""x\(y)""#), None);
    }

    #[test]
    fn xcode_module_names_follow_build_settings() {
        assert_eq!(xcode::c99("My App-2"), "My_App_2");
        assert_eq!(xcode::c99("2Fast"), "_2Fast");
    }
}
