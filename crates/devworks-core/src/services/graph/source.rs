//! Exact captured UTF-8 source, prepared outside the gateway's publication snapshot.
use super::{
    model,
    read::{Position, Reader},
};
use crate::ports::storage::*;
use crate::protocol::{DomainErrorCode, data::GraphResult, records::*};
use crate::services::{
    artifacts::ArtifactService,
    sessions::{Failure, Refusal},
};
use serde_json::json;

pub(super) struct Source {
    pub capture: StoredGraphSourceCapture,
    pub text: String,
    pub unit: StoredUnit,
    pub repository: crate::protocol::Id,
    pub view: crate::protocol::Id,
    pub node: Option<GraphNode>,
    pub first: usize,
    pub end: usize,
}
fn unavailable() -> Failure {
    Refusal::new(DomainErrorCode::ViewUnavailable, "captured_source", None).into()
}
fn invalid() -> Failure {
    Refusal::new(DomainErrorCode::InvalidArgument, "source_line_range", None).into()
}

pub(super) fn prepare(
    reader: &mut Reader<'_>,
    artifacts: &ArtifactService,
    selection: &CodeSourceSelection,
) -> Result<Source, Failure> {
    let (repository, path, node, range) = match selection {
        CodeSourceSelection::Node { node_id, mode } => {
            let node = reader
                .find_id(node_id)?
                .ok_or_else(|| crate::services::sessions::not_found("node", node_id))?;
            let range = match mode {
                CodeSourceMode::WholeFile => None,
                CodeSourceMode::NodeWindow => Some((
                    node.node.payload["span"]["startLine"].as_u64().unwrap_or(1) as usize,
                    node.node.payload["span"]["endLine"].as_u64().unwrap_or(1) as usize,
                )),
            };
            (
                node.repository_id.clone(),
                node.node.unit_key.clone(),
                Some(node),
                range,
            )
        }
        CodeSourceSelection::File {
            repository_id,
            path,
            start_line,
            end_line,
        } => (
            repository_id.clone(),
            path.0.clone(),
            None,
            Some((start_line.get() as usize, end_line.get() as usize)),
        ),
    };
    reader.scoped_repository(&repository)?;
    let view = reader
        .selected
        .layers
        .iter()
        .find(|v| v.view.repository_id == repository)
        .ok_or_else(unavailable)?;
    let (unit, capture) = model::capture(reader.read, view, &path)?.ok_or_else(unavailable)?;
    if !reader
        .budget
        .retain(usize::try_from(capture.byte_count).map_err(|_| unavailable())?)
    {
        return Err(unavailable());
    }
    let bytes = artifacts
        .read_index_source(reader.read, &capture, 16 * 1024 * 1024)
        .map_err(|_| unavailable())?;
    if !reader.budget.visit() {
        return Err(unavailable());
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| Refusal::new(DomainErrorCode::ViewUnavailable, "source_not_utf8", None))?;
    let lines: Vec<_> = text.split_inclusive('\n').collect();
    let count = lines.len();
    let (start, end) = match (selection, range) {
        (
            CodeSourceSelection::Node {
                mode: CodeSourceMode::NodeWindow,
                ..
            },
            Some((a, b)),
        ) if count > 200 => (a.saturating_sub(5).max(1), b.saturating_add(5).min(count)),
        (CodeSourceSelection::File { .. }, Some((a, b))) => {
            let a = a.max(1);
            let b = if b == 0 { count } else { b.min(count) };
            if (count > 0 && a > count) || b < a && count > 0 {
                return Err(invalid());
            }
            (a, b)
        }
        _ => (1, count),
    };
    let first = lines
        .iter()
        .take(start.saturating_sub(1))
        .map(|l| l.len())
        .sum();
    let end = lines.iter().take(end).map(|l| l.len()).sum();
    Ok(Source {
        capture,
        text,
        unit,
        repository,
        view: view.view.view_id.clone(),
        node,
        first,
        end,
    })
}

pub(super) fn query(reader: &mut Reader<'_>, source: &Source) -> Result<GraphResult, Failure> {
    let view = reader
        .selected
        .layers
        .iter()
        .find(|v| v.view.view_id == source.view && v.view.repository_id == source.repository)
        .ok_or_else(unavailable)?;
    let current =
        model::capture(reader.read, view, &source.unit.unit_key)?.ok_or_else(unavailable)?;
    if current.1 != source.capture || current.0 != source.unit {
        return Err(unavailable());
    }
    let start = reader.position.source_byte.unwrap_or(source.first);
    if start < source.first || start > source.end || !source.text.is_char_boundary(start) {
        return Err(StorageError::CursorExpired.into());
    }
    let mut end = start.saturating_add(64 * 1024).min(source.end);
    while !source.text.is_char_boundary(end) {
        end -= 1;
    }
    let line_at = |offset: usize| {
        if source.text.is_empty() {
            0
        } else {
            1 + source.text.as_bytes()[..offset]
                .iter()
                .filter(|b| **b == b'\n')
                .count()
        }
    };
    let first_line = line_at(start);
    let last_line = if start == end {
        first_line
    } else {
        line_at(end - 1)
    };
    let mut location = model::location(
        &view.view,
        &source.unit,
        &source.capture,
        &serde_json::Value::Null,
    )?
    .ok_or_else(unavailable)?;
    location.start_line = model::count(first_line as u64)?;
    location.end_line = model::count(last_line as u64)?;
    location.start_byte = Some(model::count(start as u64)?);
    location.end_byte = Some(model::count(end as u64)?);
    if end < source.end {
        reader.next = Some(Position {
            source_byte: Some(end),
            ..Default::default()
        });
    }
    let mut value = json!({"source":location,"text":&source.text[start..end],"startByte":start.to_string(),"endByte":end.to_string(),"totalBytes":source.text.len().to_string(),"firstLine":first_line.to_string(),"lastLine":last_line.to_string()});
    if let Some(node) = &source.node {
        value["nodeId"] = json!(node.node.node_id);
        for (key, field) in [("nodeStartLine", "startLine"), ("nodeEndLine", "endLine")] {
            if let Some(line) = node.node.payload["span"][field].as_u64() {
                value[key] = json!(line.to_string());
            }
        }
    }
    Ok(GraphResult::SourceText {
        value: model::wire(value)?,
    })
}
