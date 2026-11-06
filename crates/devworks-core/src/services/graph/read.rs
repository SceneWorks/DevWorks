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
use std::cell::{Cell, RefCell};

const PAGE_BYTES: usize = 512 * 1024;

/// Actual metadata provenance for read-only bootstrap responses that have no graph view.
/// A missing view never gets a synthetic pin or a fabricated source revision.
pub(super) struct Metadata<'a> {
    pub read: &'a dyn ReadSnapshot,
    pub context: &'a InvocationContext,
    pub scope: &'a DataScope,
    pub stamp: ReadStamp,
    pub position: Position,
    pub limit: usize,
    pub next: RefCell<Option<Position>>,
    pub budget: RefCell<Budget>,
    pub encoded_bytes: Cell<usize>,
}

impl Metadata<'_> {
    /// Scope and current repository authority precede every metadata row, including the
    /// unavailable/no-view path. The visitor decides the page boundary; storage remains paged.
    pub fn repositories(
        &self,
        mut visit: impl FnMut(StoredRepository) -> Result<bool, Failure>,
    ) -> Result<(), Failure> {
        let project = match self.scope {
            DataScope::Project { project_id } => {
                crate::services::projects::visible_project(
                    self.read.projects(),
                    self.context,
                    project_id,
                )?;
                Some(project_id.clone())
            }
            DataScope::Workspace if self.context.project_id().is_none() => None,
            DataScope::Workspace => {
                return Err(Refusal::new(
                    DomainErrorCode::PermissionDenied,
                    "workspace_scope",
                    None,
                )
                .into());
            }
        };
        let filter = RepositoryFilter {
            project,
            text: None,
            languages: Vec::new(),
            include_deleted: false,
        };
        let mut after = None;
        loop {
            let rows = self
                .read
                .projects()
                .repositories(&filter, after.as_ref(), 128)?;
            let more = rows.len() == 128;
            for repository in rows {
                if !self.budget.borrow_mut().visit() {
                    return Ok(());
                }
                super::scope::authorize(self.read, self.context, self.scope, &repository.id)?;
                after = Some(repository.id.clone());
                if !visit(repository)? {
                    return Ok(());
                }
            }
            if !more {
                return Ok(());
            }
        }
    }
    pub fn retain_wire<T: serde::Serialize>(&self, value: &T) -> Result<bool, Failure> {
        let size = serde_json::to_vec(value)
            .map_err(|_| Refusal::new(DomainErrorCode::ViewUnavailable, "graph_projection", None))?
            .len();
        if self.encoded_bytes.get() != 0
            && self.encoded_bytes.get().saturating_add(size) > PAGE_BYTES
        {
            return Ok(false);
        }
        self.encoded_bytes
            .set(self.encoded_bytes.get().saturating_add(size));
        Ok(true)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Position {
    pub offset: usize,
    pub property: Option<properties::Position>,
    pub edge_property: Option<properties::Position>,
    pub source_byte: Option<usize>,
    pub drilldown: Option<DrilldownPosition>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DrilldownPosition {
    /// Stable ordinal of the exact repository/module identity, not a potentially 64KiB name.
    pub module_index: usize,
    pub node_kind: String,
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
    indexing: Option<&'a crate::services::indexing::IndexEnv>,
    cursors: Option<&'a super::cursor::Cursors>,
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
            indexing: None,
            cursors: None,
        }
    }

    pub fn with_indexing(
        mut self,
        indexing: Option<&'a crate::services::indexing::IndexEnv>,
    ) -> Self {
        self.indexing = indexing;
        self
    }
    pub fn with_cursors(mut self, cursors: &'a super::cursor::Cursors) -> Self {
        self.cursors = Some(cursors);
        self
    }
    pub fn cursor(&self, position: &Position) -> Result<String, Failure> {
        self.cursors
            .ok_or(StorageError::CursorExpired)?
            .encode(position)
    }
    pub fn file_language(&self, path: &str) -> (Option<String>, bool) {
        super::languages::describe(self.indexing.map(|env| &*env.parser), path)
    }

    pub fn readme_source(
        &mut self,
        repository: &Id,
    ) -> Result<Option<crate::protocol::records::CodeSourceLocation>, Failure> {
        let scope = self.scoped_repository(repository)?;
        let mut selected: Option<GraphNode> = None;
        super::scan::nodes(
            self.read,
            &scope,
            &GraphNodeFilter {
                pattern: Some("readme%".into()),
                kinds: vec!["File".into()],
                ..Default::default()
            },
            &mut self.budget,
            |node, _| {
                let path = &node.node.unit_key;
                let lower = path.to_lowercase();
                if path.contains('/') || !(lower == "readme" || lower.starts_with("readme.")) {
                    return Ok(true);
                }
                let rank = (lower != "readme.md", path.as_str());
                let better = selected.as_ref().is_none_or(|old| {
                    rank < (
                        old.node.unit_key.to_lowercase() != "readme.md",
                        old.node.unit_key.as_str(),
                    )
                });
                if better {
                    selected = Some(node);
                }
                Ok(true)
            },
        )?;
        if self.budget.exhausted.is_some() {
            if !self.gaps.contains(&"readme_selection_incomplete") {
                self.gaps.push("readme_selection_incomplete");
            }
            return Ok(None);
        }
        let Some(node) = selected else {
            return Ok(None);
        };
        let view = self
            .selected
            .layers
            .iter()
            .find(|v| &v.view.repository_id == repository)
            .expect("scoped repository view");
        let Some((unit, capture)) = model::capture(self.read, view, &node.node.unit_key)? else {
            return Ok(None);
        };
        model::location(&view.view, &unit, &capture, &Value::Null)
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
        let owner = self.internal_module(raw)?;
        model::node(
            self.read,
            &self.selected.pin.scope,
            &self.selected.layers,
            raw,
            property,
            owner.as_deref(),
        )
    }

    pub fn internal_module(&mut self, raw: &GraphNode) -> Result<Option<String>, Failure> {
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
        Ok(owner.map(str::to_owned))
    }

    pub fn module_matches(&mut self, raw: &GraphNode, requested: &str) -> Result<bool, Failure> {
        Ok(self.internal_module(raw)?.as_deref() == Some(requested))
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
