//! Truthful wire projections of captured graph facts; no current checkout or catalog lookups.
use super::{properties, scan};
use crate::ports::storage::*;
use crate::protocol::data::{
    Citation, CitationSource, GitOid, Locator, ObjectFormat, RepositorySnapshot,
};
use crate::protocol::records::{CodeEdge, CodeNode, CodeSourceLocation};
use crate::protocol::support::{BoundedText, GraphUnitKey};
use crate::protocol::{Counter, DomainErrorCode, Id};
use crate::services::indexing::views::{self, ViewLayers};
use crate::services::sessions::{Failure, Refusal, rev};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub(super) fn wire<T: DeserializeOwned>(value: Value) -> Result<T, Failure> {
    serde_json::from_value(value).map_err(|_| {
        Refusal::new(
            DomainErrorCode::ViewUnavailable,
            "graph_value_unrepresentable",
            None,
        )
        .into()
    })
}

pub(super) fn count(value: u64) -> Result<Counter, Failure> {
    wire(json!(value.to_string()))
}

fn oid(text: &str) -> Result<GitOid, Failure> {
    let oid = GitOid {
        format: if text.len() == 64 {
            ObjectFormat::Sha256
        } else {
            ObjectFormat::Sha1
        },
        hex: text.into(),
    };
    oid.validate()
        .map_err(|_| Refusal::new(DomainErrorCode::ViewUnavailable, "graph_snapshot", None))?;
    Ok(oid)
}

pub(super) fn snapshot(view: &StoredView) -> Result<RepositorySnapshot, Failure> {
    let base = view
        .coverage
        .get("base")
        .or_else(|| view.coverage.get("commit"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Refusal::new(
                DomainErrorCode::ViewUnavailable,
                "graph_snapshot",
                Some(view.view_id.clone()),
            )
        })?;
    let head = view
        .coverage
        .get("head")
        .and_then(Value::as_str)
        .unwrap_or(base);
    Ok(RepositorySnapshot {
        repository_id: view.repository_id.clone(),
        base: oid(base)?,
        head: oid(head)?,
        worktree_id: view
            .coverage
            .get("worktreeId")
            .filter(|v| !v.is_null())
            .map(|v| wire(v.clone()))
            .transpose()?,
        capture_digest: view
            .coverage
            .get("captureDigest")
            .filter(|v| !v.is_null())
            .map(|v| wire(v.clone()))
            .transpose()?,
    })
}

pub(super) fn capture(
    read: &dyn ReadSnapshot,
    view: &ViewLayers,
    key: &str,
) -> Result<Option<(StoredUnit, StoredGraphSourceCapture)>, Failure> {
    let Some((_, unit)) = views::resolve_unit(read.indexing(), view, key)? else {
        return Ok(None);
    };
    let Some(value) = unit.payload.get("sourceCapture").and_then(|v| v.get("id")) else {
        return Ok(None);
    };
    let id: Id = wire(value.clone())?;
    let Some(capture) = read.graph().graph_source_capture(&id)? else {
        return Ok(None);
    };
    if capture.repository_id != view.view.repository_id
        || capture.unit_key != key
        || capture.source_digest != unit.content_digest
        || unit.payload["sourceCapture"]["sha256"].as_str() != Some(capture.blob_digest.as_str())
    {
        return Err(Refusal::new(
            DomainErrorCode::ViewUnavailable,
            "graph_source_binding",
            Some(id),
        )
        .into());
    }
    Ok(Some((unit, capture)))
}

pub(super) fn location(
    view: &StoredView,
    unit: &StoredUnit,
    capture: &StoredGraphSourceCapture,
    payload: &Value,
) -> Result<Option<CodeSourceLocation>, Failure> {
    if unit.unit_key.starts_with('/')
        || unit.payload["sourceCapture"]["textEncoding"].as_str() != Some("Utf8")
    {
        return Ok(None);
    }
    let Some(lines) = unit.payload["sourceCapture"]["lineCount"].as_u64() else {
        return Ok(None);
    };
    let start = payload["span"]["startLine"]
        .as_u64()
        .unwrap_or(u64::from(lines > 0));
    let end = payload["span"]["endLine"].as_u64().unwrap_or(lines);
    let captured_snapshot = snapshot(view)?;
    let digest: crate::protocol::provider::Digest = wire(json!(capture.blob_digest))?;
    // Legacy keys cannot express real paths containing '/' or '.'. Omit the old citation
    // rather than transliterating an identity. This capture is the real immutable byte block.
    let legacy_citation = match (
        Id::new(unit.unit_key.clone()),
        u32::try_from(start),
        u32::try_from(end),
    ) {
        (Ok(unit_key), Ok(first_line), Ok(last_line))
            if first_line > 0 && last_line >= first_line =>
        {
            Some(Citation {
                source: CitationSource::Repository {
                    snapshot: captured_snapshot.clone(),
                    unit_key: unit_key.clone(),
                    source_digest: digest.clone(),
                },
                block_id: capture.id.clone(),
                content_digest: digest.clone(),
                locator: Locator::Code {
                    snapshot: captured_snapshot.clone(),
                    unit_key,
                    first_line,
                    last_line,
                },
            })
        }
        _ => None,
    };
    Ok(Some(CodeSourceLocation {
        snapshot: captured_snapshot,
        unit_key: wire(json!(unit.unit_key))?,
        source_digest: digest,
        path: BoundedText(unit.unit_key.clone()),
        start_line: count(start)?,
        end_line: count(end)?,
        start_byte: payload["span"]["startByte"]
            .as_u64()
            .map(count)
            .transpose()?,
        end_byte: payload["span"]["endByte"].as_u64().map(count).transpose()?,
        language: payload
            .get("language")
            .filter(|v| v.is_string())
            .map(|v| wire(v.clone()))
            .transpose()?,
        legacy_citation,
    }))
}

pub(super) fn node(
    read: &dyn ReadSnapshot,
    scope: &GraphScope,
    layers: &[ViewLayers],
    raw: &GraphNode,
    property_position: Option<&properties::Position>,
    internal_module: Option<&str>,
) -> Result<(CodeNode, Option<properties::Position>), Failure> {
    let view = layers
        .iter()
        .find(|v| v.view.view_id == raw.view_id)
        .ok_or_else(|| {
            Refusal::new(
                DomainErrorCode::ViewUnavailable,
                "graph_view",
                Some(raw.view_id.clone()),
            )
        })?;
    let identity = GraphNodeIdentity {
        repository_id: raw.repository_id.clone(),
        unit_key: raw.node.unit_key.clone(),
        node_id: raw.node.node_id.clone(),
    };
    let trust = read
        .graph()
        .graph_trust(scope, &identity, &raw.content_digest)?;
    let captured = capture(read, view, &raw.node.unit_key)?;
    let source = captured
        .as_ref()
        .map(|(unit, captured)| location(&view.view, unit, captured, &raw.node.payload))
        .transpose()?
        .flatten();
    let properties = properties::page(&raw.node.payload["properties"], property_position)
        .map_err(|_| Refusal::new(DomainErrorCode::ViewUnavailable, "graph_properties", None))?;
    let optional = |key: &str| {
        raw.node
            .payload
            .get(key)
            .or_else(|| raw.node.payload["properties"].get(key))
            .filter(|v| !v.is_null())
            .cloned()
    };
    let projected = CodeNode {
        node_id: wire(json!(raw.node.node_id))?,
        repository_id: raw.repository_id.clone(),
        view_id: raw.view_id.clone(),
        unit_key: GraphUnitKey(raw.node.unit_key.clone()),
        node_kind: wire(json!(raw.node.kind))?,
        name: wire(json!(raw.node.name))?,
        qualified_name: wire(optional("qualifiedName").unwrap_or_else(|| json!("")))?,
        internal_module: optional("internalModule")
            .or_else(|| internal_module.map(|m| json!(m)))
            .map(wire)
            .transpose()?,
        parent_id: optional("parentId").map(wire).transpose()?,
        source,
        properties: properties.items,
        properties_complete: properties.next.is_none(),
        do_not_trust: trust.as_ref().is_some_and(|t| !t.trusted),
        trust_score: optional("trustScore").map(wire).transpose()?,
        description: optional("description").map(wire).transpose()?,
        analysis_confidence: optional("analysisConfidence")
            .or_else(|| optional("analysis_confidence"))
            .map(wire)
            .transpose()?,
        confidence: optional("confidence").map(wire).transpose()?,
        extraction_source: raw.node.payload["provenance"]
            .get("extractor")
            .filter(|v| v.is_string())
            .cloned()
            .map(wire)
            .transpose()?,
        fallback_reason: optional("fallbackReason")
            .or_else(|| optional("fallback_reason"))
            .map(wire)
            .transpose()?,
        trust_revision: rev(trust.map_or(1, |t| t.revision)),
        content_digest: captured
            .as_ref()
            .map(|(_, c)| wire(json!(c.blob_digest)))
            .transpose()?,
        view_digest: wire(json!(view.view.view_digest))?,
        content_identity: wire(json!(raw.content_digest))?,
    };
    Ok((projected, properties.next))
}

pub(super) fn edge_page(
    raw: &scan::Edge,
    position: Option<&properties::Position>,
) -> Result<(CodeEdge, Option<properties::Position>), Failure> {
    let properties = properties::page(&raw.raw.edge.payload["properties"], position)
        .map_err(|_| Refusal::new(DomainErrorCode::ViewUnavailable, "graph_properties", None))?;
    let payload = &raw.raw.edge.payload;
    let next = properties.next;
    Ok((
        CodeEdge {
            edge_id: wire(json!(raw.raw.edge.edge_id))?,
            source_id: wire(json!(raw.source.node_id))?,
            target_id: wire(json!(raw.target.node_id))?,
            source_repository_id: raw.source.repository_id.clone(),
            target_repository_id: raw.target.repository_id.clone(),
            producing_unit: Some(GraphUnitKey(raw.raw.edge.unit_key.clone())),
            edge_kind: wire(json!(raw.raw.edge.kind))?,
            resolution: payload
                .get("resolution")
                .filter(|v| v.is_string())
                .cloned()
                .map(wire)
                .transpose()?,
            confidence: payload
                .get("confidence")
                .filter(|v| v.is_number())
                .cloned()
                .map(wire)
                .transpose()?,
            confidence_band: payload
                .get("confidenceBand")
                .or_else(|| payload["properties"].get("confidence_band"))
                .filter(|v| v.is_string())
                .cloned()
                .map(wire)
                .transpose()?,
            properties: properties.items,
            properties_complete: next.is_none(),
            source_unit_key: GraphUnitKey(raw.source.unit_key.clone()),
            target_unit_key: GraphUnitKey(raw.target.unit_key.clone()),
        },
        next,
    ))
}
