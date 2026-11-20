//! Adapts approved package semantic contributions to the existing indexing producer port.
use super::{InvocationKind, InvokeOutcome, InvokeRequest, PackageHost, contributions::scope_of};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::IdKind;
use devworks_core::ports::indexing::{
    EdgeTarget, Enrichment, EnrichmentRequest, EnrichmentSource, ExtractedEdge, ExtractedNode,
    PackDescriptor, Resolution, SemanticCatalog, SemanticEnricher, Span,
};
use devworks_core::ports::packages::{Inspection, dotnet, swift};
use devworks_core::protocol::provider::ArtifactRef;
use devworks_extension_protocol::package::{
    Contribution, ContributionEvent, ContributionMeta, IndexFormat, InvocationOutcome,
};
use protobuf::Message;
use scip::types::{Index, Occurrence, PositionEncoding, SymbolRole};
use serde_json::{Map, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

const MAX_BYTES: u64 = super::artifact_io::MAX_ARTIFACT as u64;
const BUDGET: Duration = Duration::from_secs(60);
const TYPESCRIPT: &str = "devworks-typescript-semantic";

fn node_runtime(
    host: &PackageHost,
) -> Option<(String, devworks_core::ports::packages::RuntimeExecutable)> {
    let (version, executable) = host.runtimes.as_ref()?.cache.executable("node")?;
    version.starts_with("24.").then_some((version, executable))
}

fn dotnet_runtime(host: &PackageHost) -> Option<dotnet::Runtime> {
    let (_, executable) = host.runtimes.as_ref()?.cache.executable("dotnet")?;
    if devworks_core::ports::packages::RuntimeExecutable::inspect(&executable.program).ok()
        != Some(executable.clone())
    {
        return None;
    }
    dotnet::Runtime::detect(&executable).ok()
}

fn swift_runtime(host: &PackageHost) -> Option<swift::Runtime> {
    let cache = &host.runtimes.as_ref()?.cache;
    let runtime = cache.swift()?;
    if runtime.current() {
        Some(runtime)
    } else {
        cache.invalidate_swift(&runtime);
        None
    }
}

pub(crate) struct PackageSemantics(pub Weak<PackageHost>);

struct Pack {
    host: Weak<PackageHost>,
    inspection: Inspection,
    meta: ContributionMeta,
    format: IndexFormat,
    repository_source: bool,
    languages: Vec<String>,
    node: Option<(String, devworks_core::ports::packages::RuntimeExecutable)>,
    dotnet: Option<dotnet::Runtime>,
    swift: Option<swift::Runtime>,
}

impl SemanticCatalog for PackageSemantics {
    fn runtime_binding(&self, package: &str) -> Option<String> {
        let host = self.0.upgrade()?;
        let binding = match package {
            TYPESCRIPT => node_runtime(&host).and_then(|runtime| serde_json::to_vec(&runtime).ok()),
            dotnet::PACKAGE => {
                dotnet_runtime(&host).and_then(|runtime| serde_json::to_vec(&runtime).ok())
            }
            swift::PACKAGE => {
                swift_runtime(&host).and_then(|runtime| serde_json::to_vec(&runtime).ok())
            }
            _ => return None,
        };
        Some(binding.map_or_else(|| "unavailable".into(), |bytes| sha256_hex(&bytes)))
    }

    fn enrichers(&self) -> Vec<Arc<dyn SemanticEnricher>> {
        let Some(host) = self.0.upgrade() else {
            return Vec::new();
        };
        let Ok(installed) = host.active_contributions(&host.host_ctx(), "Semantic") else {
            return Vec::new();
        };
        let mut packs: Vec<Arc<dyn SemanticEnricher>> = Vec::new();
        for inspection in installed {
            if !inspection
                .access
                .iter()
                .any(|access| access == "worktree.read")
            {
                continue;
            }
            for contribution in &inspection.contributions {
                let Some(Contribution::Semantic {
                    meta,
                    format,
                    source_mode,
                    languages,
                }) = contribution
                    .verified_descriptor()
                    .and_then(|value| serde_json::from_value(value.clone()).ok())
                else {
                    continue;
                };
                if host
                    .contribution_schema(&inspection, &meta.input_schema)
                    .is_err()
                    || host
                        .contribution_schema(&inspection, &meta.output_schema)
                        .is_err()
                {
                    continue;
                }
                let node = if inspection.package_id == TYPESCRIPT {
                    let Some(runtime) = node_runtime(&host) else {
                        continue;
                    };
                    Some(runtime)
                } else {
                    None
                };
                let dotnet = if inspection.package_id == dotnet::PACKAGE {
                    let Some(runtime) = dotnet_runtime(&host) else {
                        continue;
                    };
                    Some(runtime)
                } else {
                    None
                };
                let swift = if inspection.package_id == swift::PACKAGE {
                    let Some(runtime) = swift_runtime(&host) else {
                        continue;
                    };
                    Some(runtime)
                } else {
                    None
                };
                packs.push(Arc::new(Pack {
                    host: self.0.clone(),
                    inspection: inspection.clone(),
                    meta,
                    format,
                    repository_source: source_mode.is_some(),
                    languages: languages.iter().map(|id| id.as_str().to_owned()).collect(),
                    node,
                    dotnet,
                    swift,
                }));
            }
        }
        packs
    }
    fn descriptors(
        &self,
        snapshot: &dyn devworks_core::ports::storage::ReadSnapshot,
    ) -> Result<Vec<PackDescriptor>, devworks_core::ports::storage::StorageError> {
        let Some(host) = self.0.upgrade() else {
            return Ok(Vec::new());
        };
        let packages = host.service.registry(snapshot.packages())?;
        let registry = super::policy::Registry {
            packages: &packages,
            runtimes: &**host.service.runtimes(),
            host: host.service.platform(),
        };
        Ok(packages
            .values()
            .filter_map(|(record, active)| {
                let version = active.as_ref()?;
                let inspection = super::policy::inspection(version)?;
                let languages = inspection
                    .contributions
                    .iter()
                    .filter(|entry| entry.kind == "Semantic")
                    .flat_map(|entry| entry.languages.iter().cloned())
                    .collect::<std::collections::BTreeSet<_>>();
                if languages.is_empty()
                    || !inspection
                        .access
                        .iter()
                        .any(|access| access == "worktree.read")
                {
                    return None;
                }
                let (availability, reason) = registry.availability(record, Some(version));
                let ready = matches!(
                    availability,
                    devworks_core::protocol::records::PackageAvailability::Ready
                );
                Some(PackDescriptor {
                    id: inspection.package_id.clone(),
                    name: inspection.package_id,
                    version: inspection.version,
                    repository_source: inspection.contributions.iter().any(|entry| {
                        entry
                            .verified_descriptor()
                            .is_some_and(|value| value["sourceMode"] == "Repository")
                    }),
                    manifest_digest: Some(inspection.manifest_digest),
                    languages: languages.into_iter().collect(),
                    missing: if ready { Vec::new() } else { vec![reason] },
                })
            })
            .collect())
    }
}

impl SemanticEnricher for Pack {
    fn descriptor(&self) -> PackDescriptor {
        PackDescriptor {
            id: self.inspection.package_id.clone(),
            name: self.meta.id.as_str().to_owned(),
            languages: self.languages.clone(),
            version: format!(
                "{}#{}:{}",
                self.inspection.version,
                self.inspection.manifest_digest,
                self.meta.id.as_str()
            ),
            repository_source: self.repository_source,
            manifest_digest: Some(self.inspection.manifest_digest.clone()),
            missing: Vec::new(),
        }
    }

    fn enrich(
        &self,
        request: &EnrichmentRequest<'_>,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<Enrichment>, String> {
        let host = self.host.upgrade().ok_or("the package host stopped")?;
        let mut files = Vec::with_capacity(request.files.len());
        for file in request.files {
            let text = match std::str::from_utf8(file.bytes) {
                Ok(text) => Some(text),
                // The compiler reads a template only when a component needs it. Preserve an
                // unreadable admitted context path without treating it as empty source.
                Err(_) if self.node.is_some() && file.language == "typescript-context" => None,
                Err(_) => return Err("source is not UTF-8".into()),
            };
            files.push(json!({"unit":file.unit,"language":file.language,"sha256":sha256_hex(file.bytes),"text":text}));
        }
        let mut source = json!({"repositoryId":request.repository_id,"commit":request.commit,
            "captureDigest":request.capture_digest,"files":files});
        if self.repository_source {
            source["sourceMode"] = json!("Repository");
        }
        if let Some((version, executable)) = &self.node {
            if node_runtime(&host).as_ref() != self.node.as_ref() {
                return Err("the selected Node runtime changed".into());
            }
            source["runtime"] = json!({"version": version, "executable": executable});
        }
        if let Some(runtime) = &self.dotnet {
            if dotnet_runtime(&host).as_ref() != Some(runtime) || !runtime.current() {
                return Err("dotnet-runtime-changed".into());
            }
            source["runtime"] = serde_json::to_value(runtime).map_err(|error| error.to_string())?;
        }
        if let Some(runtime) = &self.swift {
            if swift_runtime(&host).as_ref() != Some(runtime) {
                return Err("swift-runtime-changed".into());
            }
            source["runtime"] = serde_json::to_value(runtime).map_err(|error| error.to_string())?;
        }
        let bytes = serde_json::to_vec(&source).map_err(|error| error.to_string())?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("the source batch exceeds the semantic artifact bound".into());
        }
        let input_schema = host.contribution_schema(&self.inspection, &self.meta.input_schema)?;
        let output_schema = host.contribution_schema(&self.inspection, &self.meta.output_schema)?;
        host.service
            .validator()
            .validate(&input_schema, &source)
            .map_err(|_| "the source does not match the declared semantic input schema")?;
        let package = &self.inspection.package_id;
        let digest = &self.inspection.manifest_digest;
        let input = host.stage_contribution_input(request.principal, (package, digest), &bytes, json!({
            "kind":"PackageSemanticSource","repositoryId":request.repository_id,"commit":request.commit,
            "captureDigest":request.capture_digest,"contributionId":self.meta.id,"manifestDigest":digest,
        }))?;
        let reference = serde_json::to_value(input).map_err(|error| error.to_string())?;
        let handle = host
            .invoke(InvokeRequest {
                package_id: package.clone(),
                contribution_id: self.meta.id.as_str().to_owned(),
                scope: scope_of(request.principal)?,
                input: serde_json::from_value(reference.clone())
                    .map_err(|error| error.to_string())?,
                operation_id: host.ids.next_id(IdKind::Operation),
                deadline: BUDGET,
                allowed_operations: Vec::new(),
                kind: InvocationKind::Semantic,
                principal: Some(request.principal.clone()),
                expected_digest: Some(digest.clone()),
            })
            .map_err(|error| format!("the semantic helper is unavailable: {error:?}"))?;
        let deadline = Instant::now() + BUDGET;
        loop {
            if stop() || Instant::now() >= deadline {
                handle.cancel();
                return Err("semantic indexing stopped before completion".into());
            }
            if let Some(outcome) = handle.wait(Duration::from_millis(200)) {
                match outcome {
                    InvokeOutcome::Finished(InvocationOutcome::Completed { .. }) => break,
                    InvokeOutcome::Finished(InvocationOutcome::Failed { code }) => {
                        return Err(format!("semantic helper: {}", code.as_str()));
                    }
                    _ => return Err("the semantic helper did not complete".into()),
                }
            }
        }
        let (events, dropped) = handle.events();
        let [
            ContributionEvent::SemanticIndex {
                source_snapshot,
                index,
                format,
            },
        ] = events.as_slice()
        else {
            return Err("the producer must return one semantic index".into());
        };
        if dropped != 0
            || serde_json::to_value(source_snapshot).ok().as_ref() != Some(&reference)
            || std::mem::discriminant(format) != std::mem::discriminant(&self.format)
            || !handle.produced(index)
        {
            return Err("the producer returned an unbound source, format or index".into());
        }
        let artifact: ArtifactRef = serde_json::to_value(index)
            .and_then(serde_json::from_value)
            .map_err(|error| error.to_string())?;
        let ports = host
            .broker
            .get()
            .ok_or("semantic artifacts are unavailable")?;
        let (bytes, _) = ports
            .artifacts
            .read_exact(&*host.store, request.principal, &artifact, MAX_BYTES)
            .map_err(|_| "the exact semantic index is not readable")?;
        let result = match format {
            IndexFormat::Native => {
                let doc =
                    devworks_core::framing::parse(&bytes).map_err(|error| error.to_string())?;
                host.service
                    .validator()
                    .validate(&output_schema, &doc)
                    .map_err(|_| "the native index does not match the declared output schema")?;
                let units = doc
                    .as_object()
                    .filter(|obj| obj.len() == 1)
                    .and_then(|obj| obj.get("units"))
                    .ok_or("the native index must supply units")?;
                serde_json::from_value(units.clone()).map_err(|error| error.to_string())
            }
            IndexFormat::Scip => {
                host.service
                    .validator()
                    .validate(
                        &output_schema,
                        &json!({
                            "format":"Scip", "index":artifact, "sourceSnapshot":source_snapshot,
                        }),
                    )
                    .map_err(
                        |_| "the SCIP index reference does not match the declared output schema",
                    )?;
                let selected: Vec<_> = request
                    .files
                    .iter()
                    .filter(|file| {
                        self.languages
                            .iter()
                            .any(|language| language == file.language)
                    })
                    .cloned()
                    .collect();
                scip_units(&bytes, &selected)
            }
        }?;
        if let Some(runtime) = &self.dotnet
            && (dotnet_runtime(&host).as_ref() != Some(runtime) || !runtime.current())
        {
            return Err("dotnet-runtime-changed".into());
        }
        if let Some(runtime) = &self.swift
            && swift_runtime(&host).as_ref() != Some(runtime)
        {
            return Err("swift-runtime-changed".into());
        }
        if !host.contribution_current(request.principal, "Semantic", (package, digest))? {
            return Err("the producer version changed before the index could be accepted".into());
        }
        Ok(result)
    }
}

fn scip_units(bytes: &[u8], sources: &[EnrichmentSource<'_>]) -> Result<Vec<Enrichment>, String> {
    let index = Index::parse_from_bytes(bytes).map_err(|error| error.to_string())?;
    let mut documents = BTreeMap::new();
    for document in &index.documents {
        let path = document.relative_path.as_str();
        let source = sources
            .iter()
            .find(|source| source.unit == path)
            .ok_or("SCIP document is outside the captured source")?;
        let text = std::str::from_utf8(source.bytes).map_err(|_| "SCIP source is not UTF-8")?;
        if (!document.text.is_empty() && document.text != text)
            || documents.insert(path, (document, text)).is_some()
        {
            return Err("a SCIP document is duplicate or has different source bytes".into());
        }
    }
    let mut found = BTreeMap::new();
    let mut result = Vec::new();
    for (&path, &(document, text)) in &documents {
        let lines = source_lines(text);
        let mut unit = file_unit(path);
        let metadata: BTreeMap<_, _> = document
            .symbols
            .iter()
            .map(|symbol| (symbol.symbol.as_str(), symbol))
            .collect();
        for occurrence in &document.occurrences {
            let span = scip_span(
                occurrence,
                &lines,
                document.position_encoding.enum_value_or_default(),
            )?;
            if occurrence.symbol.is_empty()
                || occurrence.symbol_roles & SymbolRole::Definition as i32 == 0
            {
                continue;
            }
            let symbol = &occurrence.symbol;
            let info = metadata.get(symbol.as_str());
            let label = info
                .filter(|info| !info.display_name.is_empty())
                .map(|info| info.display_name.as_str())
                .unwrap_or(symbol);
            let kind = info
                .map(|info| format!("{:?}", info.kind.enum_value_or_default()))
                .filter(|kind| kind != "UnspecifiedKind")
                .unwrap_or_else(|| "Symbol".into());
            let scoped = (
                scip::symbol::is_local_symbol(symbol).then_some(path),
                symbol.as_str(),
            );
            found
                .entry(scoped)
                .or_insert((path, kind.clone(), symbol.clone()));
            let properties = json!({"symbol":symbol,"roles":occurrence.symbol_roles,
                "documentation":info.map(|info| &info.documentation)})
            .as_object()
            .cloned()
            .expect("object");
            unit.nodes.push(ExtractedNode {
                kind,
                name: label.to_owned(),
                qualified_name: symbol.clone(),
                span,
                parent: Some(0),
                properties,
            });
        }
        result.push(unit);
    }
    for unit in &mut result {
        let (document, text) = documents[unit.unit_key.as_str()];
        let lines = source_lines(text);
        let endpoint = |symbol: &str| {
            let scoped = scip::symbol::is_local_symbol(symbol).then_some(unit.unit_key.as_str());
            match found.get(&(scoped, symbol)) {
                Some((unit, kind, qualified_name)) => (
                    EdgeTarget::Node {
                        unit: (*unit).to_owned(),
                        kind: kind.clone(),
                        qualified_name: qualified_name.clone(),
                    },
                    Resolution::Resolved,
                ),
                None => (
                    EdgeTarget::Symbol(symbol.to_owned()),
                    Resolution::Unresolved,
                ),
            }
        };
        for occurrence in &document.occurrences {
            if occurrence.symbol.is_empty()
                || occurrence.symbol_roles & SymbolRole::Definition as i32 != 0
            {
                continue;
            }
            let (target, resolution) = endpoint(&occurrence.symbol);
            let span = scip_span(
                occurrence,
                &lines,
                document.position_encoding.enum_value_or_default(),
            )?;
            let properties = json!({"span":span,"roles":occurrence.symbol_roles})
                .as_object()
                .cloned()
                .expect("object");
            unit.edges.push(ExtractedEdge {
                kind: "REFERENCES".into(),
                source: 0,
                target,
                resolution,
                confidence: 1.0,
                properties,
            });
        }
        for info in &document.symbols {
            let Some(source) = unit
                .nodes
                .iter()
                .position(|node| node.qualified_name == info.symbol)
            else {
                continue;
            };
            for relation in &info.relationships {
                let kind = if relation.is_implementation {
                    "IMPLEMENTS"
                } else if relation.is_type_definition {
                    "TYPE_DEFINITION"
                } else if relation.is_definition {
                    "DEFINES"
                } else if relation.is_reference {
                    "REFERENCES"
                } else {
                    continue;
                };
                let (target, resolution) = endpoint(&relation.symbol);
                unit.edges.push(ExtractedEdge {
                    kind: kind.into(),
                    source,
                    target,
                    resolution,
                    confidence: 1.0,
                    properties: Map::new(),
                });
            }
        }
    }
    result.extend(
        sources
            .iter()
            .filter(|source| !documents.contains_key(source.unit))
            .map(|source| file_unit(source.unit)),
    );
    Ok(result)
}

fn file_unit(path: &str) -> Enrichment {
    Enrichment {
        unit_key: path.to_owned(),
        nodes: vec![ExtractedNode {
            kind: "File".into(),
            name: path.rsplit('/').next().unwrap_or(path).into(),
            qualified_name: path.into(),
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
        edges: Vec::new(),
    }
}

fn source_lines(source: &str) -> Vec<(usize, &str)> {
    let mut offset = 0;
    source
        .split('\n')
        .map(|line| {
            let start = offset;
            offset += line.len() + 1;
            (start, line.trim_end_matches('\r'))
        })
        .collect()
}

fn scip_span(
    occurrence: &Occurrence,
    lines: &[(usize, &str)],
    encoding: PositionEncoding,
) -> Result<Span, String> {
    let (start_line, start, end_line, end) = if occurrence.has_single_line_range() {
        let range = occurrence.single_line_range();
        (
            range.line,
            range.start_character,
            range.line,
            range.end_character,
        )
    } else if occurrence.has_multi_line_range() {
        let range = occurrence.multi_line_range();
        (
            range.start_line,
            range.start_character,
            range.end_line,
            range.end_character,
        )
    } else {
        match occurrence.range.as_slice() {
            [line, start, end] => (*line, *start, *line, *end),
            [start_line, start, end_line, end] => (*start_line, *start, *end_line, *end),
            _ => return Err("SCIP occurrence range has an invalid shape".into()),
        }
    };
    let position = |line: i32, column: i32| -> Option<(u32, u32)> {
        let line = usize::try_from(line).ok()?;
        let column = usize::try_from(column).ok()?;
        let (offset, row) = lines.get(line).copied()?;
        let byte = match encoding {
            PositionEncoding::UTF16CodeUnitOffsetFromLineStart
            | PositionEncoding::UTF32CodeUnitOffsetFromLineStart => {
                let mut units = 0;
                let mut found = None;
                for (byte, character) in row.char_indices() {
                    if units == column {
                        found = Some(byte);
                        break;
                    }
                    units += if encoding == PositionEncoding::UTF16CodeUnitOffsetFromLineStart {
                        character.len_utf16()
                    } else {
                        1
                    };
                }
                found.or_else(|| (units == column).then_some(row.len()))?
            }
            _ => row.is_char_boundary(column).then_some(column)?,
        };
        Some((
            u32::try_from(offset + byte).ok()?,
            u32::try_from(byte + 1).ok()?,
        ))
    };
    let (start_byte, start_column) =
        position(start_line, start).ok_or("invalid SCIP start position")?;
    let (end_byte, end_column) = position(end_line, end).ok_or("invalid SCIP end position")?;
    if end_byte < start_byte {
        return Err("reversed SCIP span".into());
    }
    Ok(Span {
        start_line: start_line as u32 + 1,
        start_column,
        end_line: end_line as u32 + 1,
        end_column,
        start_byte,
        end_byte,
    })
}
