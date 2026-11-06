//! Shared query context and bounded wire-page assembly for every graph operator.
use super::{budget::Budget, dataset, lease::Selected, model, modules::Modules, properties};
use crate::context::InvocationContext;
use crate::ports::storage::*;
use crate::protocol::data::DataScope;
use crate::protocol::records::{CodeNode, CodeNodeSelection};
use crate::protocol::{DomainErrorCode, Id, ReadStamp};
use crate::services::sessions::{Failure, Refusal};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const PAGE_BYTES: usize = 512 * 1024;

/// Actual metadata provenance for read-only bootstrap responses that have no graph view.
/// A missing view never gets a synthetic pin or a fabricated source revision.
pub(super) struct Metadata<'a> {
    pub read: &'a dyn ReadSnapshot,
    pub context: &'a InvocationContext,
    pub scope: &'a DataScope,
    pub now_ms: i64,
    pub stamp: ReadStamp,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct Position {
    pub offset: usize,
    pub property: Option<properties::Position>,
    pub source_byte: Option<usize>,
}

pub(super) struct Reader<'a> {
    pub read: &'a dyn ReadSnapshot,
    pub selected: &'a Selected,
    pub budget: Budget,
    pub position: Position,
    pub limit: usize,
    pub next: Option<Position>,
    pub gaps: Vec<&'static str>,
    modules: Option<Modules>,
    encoded_bytes: usize,
}

impl<'a> Reader<'a> {
    pub fn new(
        read: &'a dyn ReadSnapshot,
        selected: &'a Selected,
        budget: Budget,
        position: Position,
        limit: usize,
    ) -> Self {
        Self {
            read,
            selected,
            budget,
            position,
            limit,
            next: None,
            gaps: Vec::new(),
            modules: None,
            encoded_bytes: 0,
        }
    }

    pub fn scope(&self) -> &GraphScope {
        &self.selected.pin.scope
    }

    pub fn scoped_repository(&self, repository: &Id) -> Result<GraphScope, Failure> {
        let views: Vec<_> = self
            .scope()
            .views
            .iter()
            .filter(|v| &v.repository_id == repository)
            .cloned()
            .collect();
        if views.is_empty() {
            return Err(Refusal::new(
                DomainErrorCode::ViewUnavailable,
                "repository_view",
                Some(repository.clone()),
            )
            .into());
        }
        Ok(GraphScope { views })
    }

    pub fn raw_nodes(
        &mut self,
        scope: &GraphScope,
        filter: &GraphNodeFilter,
    ) -> Result<Vec<GraphNode>, Failure> {
        dataset::nodes(self.read, scope, filter, &mut self.budget)
    }

    pub fn raw_edges(&mut self) -> Result<dataset::Edges, Failure> {
        dataset::edges(
            self.read,
            &self.selected.pin.scope,
            &self.selected.layers,
            &mut self.budget,
        )
    }

    pub fn node(
        &mut self,
        raw: &GraphNode,
        property: Option<&properties::Position>,
    ) -> Result<(CodeNode, Option<properties::Position>), Failure> {
        if self.modules.is_none() {
            self.modules = Some(Modules::load(
                self.read,
                &self.selected.pin.scope,
                &mut self.budget,
            )?);
        }
        let (owner, ambiguous) = self.modules.as_ref().expect("loaded modules").owner(raw);
        if ambiguous && !self.gaps.contains(&"ambiguous_internal_module") {
            self.gaps.push("ambiguous_internal_module");
        }
        model::node(
            self.read,
            &self.selected.pin.scope,
            &self.selected.layers,
            raw,
            property,
            owner,
        )
    }

    pub fn module_matches(&mut self, raw: &GraphNode, requested: &str) -> Result<bool, Failure> {
        if self.modules.is_none() {
            self.modules = Some(Modules::load(
                self.read,
                &self.selected.pin.scope,
                &mut self.budget,
            )?);
        }
        let (owner, ambiguous) = self.modules.as_ref().expect("loaded modules").owner(raw);
        if ambiguous && !self.gaps.contains(&"ambiguous_internal_module") {
            self.gaps.push("ambiguous_internal_module");
        }
        Ok(owner == Some(requested))
    }

    pub fn find_id(&mut self, id: &Id) -> Result<Option<GraphNode>, Failure> {
        let scope = self.selected.pin.scope.clone();
        let mut rows = self.raw_nodes(
            &scope,
            &GraphNodeFilter {
                node_id: Some(id.as_str().into()),
                ..Default::default()
            },
        )?;
        if rows.len() > 1 {
            return Err(Refusal::new(
                DomainErrorCode::InvalidArgument,
                "ambiguous_node_identity",
                Some(id.clone()),
            )
            .into());
        }
        if rows.is_empty()
            && (self.budget.exhausted.is_some()
                || !self.selected.pin.unavailable_repositories.is_empty())
        {
            return Err(Refusal::new(
                DomainErrorCode::ViewUnavailable,
                "node_lookup_incomplete",
                Some(id.clone()),
            )
            .into());
        }
        Ok(rows.pop())
    }

    pub fn targets(&mut self, selection: &CodeNodeSelection) -> Result<Vec<GraphNode>, Failure> {
        match selection {
            CodeNodeSelection::Node { node_id } => Ok(self.find_id(node_id)?.into_iter().collect()),
            CodeNodeSelection::Name {
                name,
                repository_id,
            } => {
                let scope = match repository_id {
                    Some(id) => self.scoped_repository(id)?,
                    None => self.selected.pin.scope.clone(),
                };
                let mut rows = self.raw_nodes(&scope, &GraphNodeFilter::default())?;
                rows.retain(|n| {
                    if repository_id.is_some() {
                        n.node.name.to_lowercase() == name.0.to_lowercase()
                            || n.node.payload["qualifiedName"].as_str() == Some(&name.0)
                    } else {
                        n.node.name.to_lowercase().contains(&name.0.to_lowercase())
                            || n.node.payload["qualifiedName"].as_str() == Some(&name.0)
                    }
                });
                rows.sort_by(|a, b| {
                    a.node
                        .name
                        .encode_utf16()
                        .cmp(b.node.name.encode_utf16())
                        .then_with(|| dataset::node_key(a).cmp(&dataset::node_key(b)))
                });
                Ok(rows)
            }
        }
    }

    /// Supplementary endpoint records count toward the byte bound, not a separate hidden
    /// graph limit. A page stops before exceeding its bound and retains a continuation.
    pub fn retain_wire<T: serde::Serialize>(&mut self, value: &T) -> Result<bool, Failure> {
        let bytes = serde_json::to_vec(value)
            .map_err(|_| Refusal::new(DomainErrorCode::ViewUnavailable, "graph_projection", None))?
            .len();
        if self.encoded_bytes.saturating_add(bytes) > PAGE_BYTES && self.encoded_bytes != 0 {
            return Ok(false);
        }
        self.encoded_bytes = self.encoded_bytes.saturating_add(bytes);
        Ok(true)
    }

    pub fn page_nodes(&mut self, rows: &[GraphNode]) -> Result<Vec<CodeNode>, Failure> {
        if self.position.offset > rows.len() {
            return Err(StorageError::CursorExpired.into());
        }
        let mut out = Vec::new();
        for (index, raw) in rows.iter().enumerate().skip(self.position.offset) {
            if out.len() == self.limit {
                self.next = Some(Position {
                    offset: index,
                    ..Default::default()
                });
                break;
            }
            let (node, _) = self.node(raw, None)?;
            if !self.retain_wire(&node)? {
                self.next = Some(Position {
                    offset: index,
                    ..Default::default()
                });
                break;
            }
            out.push(node);
        }
        Ok(out)
    }
}

pub(super) fn optional_string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}
