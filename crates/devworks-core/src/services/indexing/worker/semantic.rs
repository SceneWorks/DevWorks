//! Adds source-validated package rows to the same generation and writer fence as structural
//! extraction. Packages return proposals. The indexing worker owns publication.
use super::*;
use crate::ports::indexing::{
    EdgeTarget, Enrichment, EnrichmentRequest, EnrichmentSource, PackDescriptor, SemanticEnricher,
};

pub(crate) type UnitRows = (StoredUnit, Vec<StoredNode>, Vec<StoredEdge>);

#[derive(Clone, Copy)]
pub(crate) struct SemanticBatch<'a> {
    pub root: &'a Path,
    pub commit: &'a str,
    pub capture: Option<&'a str>,
    pub files: &'a [EnrichmentSource<'a>],
}

pub(crate) fn select(
    env: &IndexEnv,
    config: &EffectiveConfig,
    repository: &Id,
) -> Result<Vec<Arc<dyn SemanticEnricher>>, StorageError> {
    let candidates = env.enrichers();
    read_snapshot(&*env.store, &ReadOptions::default(), |read| {
        let projects = read.projects().repository_projects(repository)?;
        let mut selected = Vec::new();
        for pack in &candidates {
            let descriptor = pack.descriptor();
            if config.packages.contains(&descriptor.id)
                && descriptor.missing.is_empty()
                && !crate::services::indexing::status::semantic_disabled(
                    read.packages(),
                    &descriptor.id,
                    &projects,
                )?
            {
                selected.push(pack.clone());
            }
        }
        Ok(selected)
    })
}

pub(crate) fn registry_binding(
    registry: &dyn crate::ports::storage::PackageRegistryRead,
    config: &EffectiveConfig,
    projects: &[crate::protocol::ProjectId],
) -> Result<String, StorageError> {
    let mut bindings = Vec::new();
    for id in &config.packages {
        if let Some(record) = registry.package(id)?
            && record.installed
            && record.enabled
            && !projects.iter().any(|project| {
                crate::services::packages::policy::project_disabled(&record, project.0.as_str())
            })
            && let Some(digest) = record.active_digest
        {
            bindings.push(format!("{id}@{digest}"));
        }
    }
    Ok(bindings.join(","))
}

pub(crate) fn incomplete(coverage: &Value) -> bool {
    coverage["gaps"].as_array().is_some_and(|gaps| {
        gaps.iter().any(|gap| {
            gap.as_str()
                .is_some_and(|gap| gap.starts_with("incomplete-semantic-"))
        })
    })
}

pub(crate) fn binding(packs: &[Arc<dyn SemanticEnricher>]) -> String {
    let versions: BTreeSet<_> = packs
        .iter()
        .map(|pack| {
            let descriptor = pack.descriptor();
            format!("{}@{}", descriptor.id, descriptor.version)
        })
        .collect();
    versions.into_iter().collect::<Vec<_>>().join(",")
}

const MAX_REPOSITORY_BYTES: u64 = 4 * 1024 * 1024;
const MAX_REPOSITORY_FILES: usize = 4096;

pub(crate) struct RepositoryResult {
    descriptor: PackDescriptor,
    proposals: Result<BTreeMap<String, Enrichment>, String>,
}

fn supports(descriptor: &PackDescriptor, language: &str) -> bool {
    descriptor.languages.iter().any(|id| id == language)
}

fn source_unit(unit: &PlanUnit) -> bool {
    unit.oid.is_some() && matches!(unit.mode, MODE_STRUCTURAL | MODE_TEXT | MODE_TEXT_FALLBACK)
}

fn cargo_context(unit: &PlanUnit) -> bool {
    matches!(
        unit.key.rsplit('/').next(),
        Some("Cargo.toml" | "Cargo.lock")
    )
}

pub(crate) fn repository_language(packs: &[Arc<dyn SemanticEnricher>], language: &str) -> bool {
    packs.iter().any(|pack| {
        let descriptor = pack.descriptor();
        descriptor.repository_source && supports(&descriptor, language)
    })
}

pub(crate) fn repository_unit(packs: &[Arc<dyn SemanticEnricher>], unit: &PlanUnit) -> bool {
    source_unit(unit)
        && unit
            .language
            .is_some_and(|language| repository_language(packs, language))
}

pub(crate) fn prepare_repository(
    exec: &mut Exec<'_>,
    capture: SemanticBatch<'_>,
    plan: &[PlanUnit],
    local: Option<&BTreeMap<String, String>>,
    stop: &(dyn Fn() -> bool + Sync),
) -> Result<(), Stop> {
    exec.repository_semantics.clear();
    let packs = exec.semantic_packs.clone();
    let mut prepared = Vec::new();
    let env = exec.w.env.clone();
    for pack in packs {
        let descriptor = pack.descriptor();
        if !descriptor.repository_source {
            continue;
        }
        let selected: Vec<_> = plan
            .iter()
            .filter(|unit| {
                source_unit(unit)
                    && (cargo_context(unit)
                        || unit
                            .language
                            .is_some_and(|language| supports(&descriptor, language)))
            })
            .collect();
        if !selected.iter().any(|unit| !cargo_context(unit)) {
            continue;
        }
        let bytes = selected
            .iter()
            .fold(0u64, |sum, unit| sum.saturating_add(unit.size));
        let proposals = if selected.len() > MAX_REPOSITORY_FILES || bytes > MAX_REPOSITORY_BYTES {
            Err("repository source exceeds the semantic input budget".into())
        } else {
            let mut contents = Vec::with_capacity(selected.len());
            let committed: Vec<_> = selected
                .iter()
                .copied()
                .filter(|unit| !local.is_some_and(|local| local.contains_key(&unit.key)))
                .collect();
            let oids: Vec<_> = committed.iter().map(|unit| unit.digest.clone()).collect();
            let buffers = if oids.is_empty() {
                Vec::new()
            } else {
                exec.git(stop)
                    .blobs(capture.root, &oids, bytes as usize)
                    .map_err(Stop::git)?
            };
            for (unit, bytes) in committed.into_iter().zip(buffers) {
                contents.push((unit, bytes));
            }
            for unit in selected {
                if stop() {
                    return Err(Stop::Cancelled);
                }
                let Some(expected) = local.and_then(|local| local.get(&unit.key)) else {
                    continue;
                };
                let bytes = env
                    .source
                    .read_file(capture.root, &unit.key, unit.size.max(1))
                    .map_err(|_| changed(&unit.key))?;
                if expected != &format!("sha256:{}", sha256_hex(&bytes)) {
                    return Err(changed(&unit.key));
                }
                contents.push((unit, bytes));
            }
            contents.sort_by(|(left, _), (right, _)| left.key.cmp(&right.key));
            let files: Vec<_> = contents
                .iter()
                .map(|(unit, bytes)| EnrichmentSource {
                    unit: &unit.key,
                    language: if cargo_context(unit) {
                        "cargo"
                    } else {
                        unit.language.expect("selected language")
                    },
                    bytes,
                })
                .collect();
            collect(
                exec,
                &*pack,
                &descriptor,
                SemanticBatch {
                    files: &files,
                    ..capture
                },
                stop,
            )
        };
        note_failure(exec, &descriptor, &proposals);
        prepared.push(RepositoryResult {
            descriptor,
            proposals,
        });
    }
    exec.repository_semantics = prepared;
    Ok(())
}

fn changed(path: &str) -> Stop {
    Stop::transient("worktree_changed", format!("{path} changed during capture"))
}

pub(crate) fn enrich_batch(
    exec: &mut Exec<'_>,
    packs: &[Arc<dyn SemanticEnricher>],
    batch: SemanticBatch<'_>,
    rows: &mut [UnitRows],
    stop: &(dyn Fn() -> bool + Sync),
) -> Result<(), Stop> {
    for result in &exec.repository_semantics {
        apply(
            exec.run.repository_id.as_str(),
            &result.descriptor,
            &result.proposals,
            rows,
        );
    }
    for pack in packs {
        if stop() {
            return Err(Stop::Cancelled);
        }
        let descriptor = pack.descriptor();
        if descriptor.repository_source {
            continue;
        }
        let files: Vec<_> = batch
            .files
            .iter()
            .filter(|file| supports(&descriptor, file.language))
            .cloned()
            .collect();
        if files.is_empty() {
            continue;
        }
        let proposals = collect(
            exec,
            &**pack,
            &descriptor,
            SemanticBatch {
                files: &files,
                ..batch
            },
            stop,
        );
        if stop() {
            return Err(Stop::Cancelled);
        }
        note_failure(exec, &descriptor, &proposals);
        apply(
            exec.run.repository_id.as_str(),
            &descriptor,
            &proposals,
            rows,
        );
    }
    Ok(())
}

fn collect(
    exec: &Exec<'_>,
    pack: &dyn SemanticEnricher,
    descriptor: &PackDescriptor,
    batch: SemanticBatch<'_>,
    stop: &(dyn Fn() -> bool + Sync),
) -> Result<BTreeMap<String, Enrichment>, String> {
    let selected: Vec<_> = batch
        .files
        .iter()
        .filter(|file| supports(descriptor, file.language))
        .cloned()
        .collect();
    let units: Vec<_> = selected.iter().map(|file| file.unit.to_owned()).collect();
    pack.enrich(
        &EnrichmentRequest {
            repository_id: exec.run.repository_id.as_str(),
            root: batch.root,
            commit: batch.commit,
            capture_digest: batch.capture,
            units: &units,
            files: batch.files,
            principal: &exec.ctx,
        },
        stop,
    )
    .and_then(|proposals| validate(proposals, &selected))
}

fn note_failure(
    exec: &mut Exec<'_>,
    descriptor: &PackDescriptor,
    result: &Result<BTreeMap<String, Enrichment>, String>,
) {
    if let Err(error) = result {
        push_warning(
            &mut exec.warnings,
            "semantic_pack_failed",
            format!("{}: {error}", descriptor.id),
        );
    }
}

fn apply(
    repository: &str,
    descriptor: &PackDescriptor,
    result: &Result<BTreeMap<String, Enrichment>, String>,
    rows: &mut [UnitRows],
) {
    for (row, nodes, edges) in rows {
        let Some(language) = row.payload["language"]
            .as_str()
            .filter(|language| supports(descriptor, language))
            .filter(|_| {
                matches!(
                    row.payload["mode"].as_str(),
                    Some(MODE_STRUCTURAL | MODE_TEXT_FALLBACK)
                )
            })
        else {
            continue;
        };
        let proposals = match result {
            Ok(proposals) => proposals,
            Err(error) => {
                let gap = json!(format!("semantic-{}: {error}", descriptor.id));
                if let Some(incomplete) = row.payload["incomplete"].as_array_mut() {
                    incomplete.push(gap);
                } else {
                    row.payload["incomplete"] = json!([gap]);
                }
                continue;
            }
        };
        let Some(proposal) = proposals.get(row.unit_key.as_str()) else {
            continue;
        };
        let (proposed_nodes, proposed_edges) = super::units::rows_of(
            repository,
            &row.unit_key,
            &FileExtract {
                nodes: proposal.nodes.clone(),
                edges: proposal.edges.clone(),
                ..FileExtract::default()
            },
            Some(language),
            None,
            &descriptor.id,
        );
        let known_nodes: BTreeSet<_> = nodes.iter().map(|n| n.node_id.clone()).collect();
        let known_edges: BTreeSet<_> = edges.iter().map(|e| e.edge_id.clone()).collect();
        nodes.extend(
            proposed_nodes
                .into_iter()
                .skip(1)
                .filter(|node| !known_nodes.contains(&node.node_id)),
        );
        edges.extend(
            proposed_edges
                .into_iter()
                .filter(|edge| !known_edges.contains(&edge.edge_id)),
        );
        let pack = json!({"id":descriptor.id,"version":descriptor.version,
            "manifestDigest": descriptor.manifest_digest});
        if let Some(applied) = row.payload["semanticPacks"].as_array_mut() {
            applied.push(pack);
        } else {
            row.payload["semanticPacks"] = json!([pack]);
        }
    }
}

fn validate(
    enrichments: Vec<Enrichment>,
    files: &[EnrichmentSource<'_>],
) -> Result<BTreeMap<String, Enrichment>, String> {
    let mut checked = BTreeMap::new();
    for result in enrichments {
        let Some(file) = files.iter().find(|file| file.unit == result.unit_key) else {
            return Err("output names a unit outside the source capture".into());
        };
        if result.nodes.is_empty()
            || result.nodes[0].kind != "File"
            || result.nodes[0].parent.is_some()
            || result.nodes.len() > 50_000
            || result.edges.len() > 100_000
        {
            return Err("invalid semantic unit shape".into());
        }
        let mut line_offsets = vec![0];
        line_offsets.extend(
            file.bytes
                .iter()
                .enumerate()
                .filter_map(|(i, byte)| (*byte == b'\n').then_some(i + 1)),
        );
        let position_matches = |byte: u32, line: u32, column: u32| {
            let Some(index) = line.checked_sub(1).map(|n| n as usize) else {
                return false;
            };
            let Some(start) = line_offsets.get(index) else {
                return false;
            };
            let offset = byte as usize;
            offset >= *start
                && line_offsets.get(index + 1).is_none_or(|end| offset < *end)
                && offset - *start + 1 == column as usize
        };
        for (position, node) in result.nodes.iter().enumerate() {
            if node.name.is_empty()
                || node.qualified_name.is_empty()
                || node.qualified_name.len() > 4096
                || node.kind.is_empty()
                || node.kind.len() > 128
                || (position > 0
                    && (node.kind == "File" || node.parent.is_none_or(|parent| parent >= position)))
                || node.span.start_byte > node.span.end_byte
                || node.span.end_byte as usize > file.bytes.len()
                || !position_matches(
                    node.span.start_byte,
                    node.span.start_line,
                    node.span.start_column,
                )
                || !position_matches(node.span.end_byte, node.span.end_line, node.span.end_column)
            {
                return Err("a semantic definition has an invalid identity, span or parent".into());
            }
        }
        for edge in &result.edges {
            let result_bound = |unit: &str| files.iter().any(|file| file.unit == unit);
            let target_ok = match &edge.target {
                EdgeTarget::Local(index) => *index < result.nodes.len(),
                EdgeTarget::Symbol(symbol) => !symbol.is_empty() && symbol.len() <= 4096,
                EdgeTarget::Node {
                    unit,
                    kind,
                    qualified_name,
                } => result_bound(unit) && !kind.is_empty() && !qualified_name.is_empty(),
                EdgeTarget::RepositoryNode { .. } => false,
            };
            if edge.source >= result.nodes.len()
                || edge.kind.is_empty()
                || edge.kind.len() > 128
                || !target_ok
                || !edge.confidence.is_finite()
                || !(0.0..=1.0).contains(&edge.confidence)
            {
                return Err("a semantic relationship has an invalid endpoint or confidence".into());
            }
        }
        if checked.insert(result.unit_key.clone(), result).is_some() {
            return Err("duplicate semantic unit".into());
        }
    }
    if checked.len() != files.len() {
        return Err("the pack did not account for every selected source unit".into());
    }
    Ok(checked)
}
