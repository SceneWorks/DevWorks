//! Adds source-validated package rows to the same generation and writer fence as structural
//! extraction. Packages return proposals. The indexing worker owns publication.
use super::*;
use crate::ports::indexing::{
    EdgeTarget, Enrichment, EnrichmentRequest, EnrichmentSource, SemanticEnricher,
};

pub(crate) type UnitRows = (StoredUnit, Vec<StoredNode>, Vec<StoredEdge>);

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

pub(crate) fn enrich_batch(
    exec: &mut Exec<'_>,
    packs: &[Arc<dyn SemanticEnricher>],
    batch: SemanticBatch<'_>,
    rows: &mut [UnitRows],
    stop: &(dyn Fn() -> bool + Sync),
) -> Result<(), Stop> {
    let repository = exec.run.repository_id.as_str();
    for pack in packs {
        if stop() {
            return Err(Stop::Cancelled);
        }
        let descriptor = pack.descriptor();
        let files: Vec<_> = batch
            .files
            .iter()
            .filter(|file| {
                descriptor
                    .languages
                    .iter()
                    .any(|language| language == file.language)
            })
            .cloned()
            .collect();
        if files.is_empty() {
            continue;
        }
        let units: Vec<_> = files.iter().map(|file| file.unit.to_owned()).collect();
        let result = pack
            .enrich(
                &EnrichmentRequest {
                    repository_id: repository,
                    root: batch.root,
                    commit: batch.commit,
                    capture_digest: batch.capture,
                    units: &units,
                    files: &files,
                    principal: &exec.ctx,
                },
                stop,
            )
            .and_then(|enrichments| validate(enrichments, &files));
        if stop() {
            return Err(Stop::Cancelled);
        }
        let proposals = match result {
            Ok(proposals) => proposals,
            Err(error) => {
                push_warning(
                    &mut exec.warnings,
                    "semantic_pack_failed",
                    format!("{}: {error}", descriptor.id),
                );
                for (unit, _, _) in rows
                    .iter_mut()
                    .filter(|(unit, _, _)| files.iter().any(|file| file.unit == unit.unit_key))
                {
                    let gap = json!(format!("semantic-{}: {error}", descriptor.id));
                    if let Some(incomplete) = unit.payload["incomplete"].as_array_mut() {
                        incomplete.push(gap);
                    } else {
                        unit.payload["incomplete"] = json!([gap]);
                    }
                }
                continue;
            }
        };
        for (row, nodes, edges) in rows.iter_mut() {
            let Some(proposal) = proposals.get(row.unit_key.as_str()) else {
                continue;
            };
            let file = files
                .iter()
                .find(|file| file.unit == row.unit_key)
                .expect("validated file");
            let (proposed_nodes, proposed_edges) = super::units::rows_of(
                repository,
                &row.unit_key,
                &FileExtract {
                    nodes: proposal.nodes.clone(),
                    edges: proposal.edges.clone(),
                    ..FileExtract::default()
                },
                Some(file.language),
                None,
                &descriptor.id,
            );
            let known_nodes: BTreeSet<_> = nodes.iter().map(|node| node.node_id.clone()).collect();
            let known_edges: BTreeSet<_> = edges.iter().map(|edge| edge.edge_id.clone()).collect();
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
            let applied = row.payload["semanticPacks"].as_array_mut();
            let pack = json!({"id":descriptor.id,"version":descriptor.version,
                "manifestDigest": descriptor.manifest_digest});
            if let Some(applied) = applied {
                applied.push(pack);
            } else {
                row.payload["semanticPacks"] = json!([pack]);
            }
        }
    }
    Ok(())
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
