//! Graph/retrieval, immutable artifact and Git/release delivery DTOs adopted from the recorded
//! candidates (tools/contracts-eval/src/data_delivery.rs). Wire shapes and pure cross-field
//! checks only; views, stores and delivery services are owned by later items.
use super::provider::{ArtifactRef, Digest};
use super::records::*;
use super::shared::*;
use super::support::{Check, Revisioned};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitOid {
    pub format: ObjectFormat,
    #[schemars(regex(pattern = "^[a-f0-9]{40}([a-f0-9]{24})?$"))]
    pub hex: String,
}
impl GitOid {
    pub fn validate(&self) -> Result<(), &'static str> {
        let len = if matches!(self.format, ObjectFormat::Sha1) {
            40
        } else {
            64
        };
        if self.hex.len() != len
            || !self
                .hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("object format mismatch");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum DataScope {
    Workspace,
    Project { project_id: ProjectId },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ViewSelection {
    SchemaPublished {
        #[schemars(length(min = 1, max = 256))]
        connection_ids: Vec<Id>,
    },
    Published {
        #[schemars(length(min = 1, max = 256))]
        repository_ids: Vec<Id>,
    },
    Exact {
        #[schemars(length(min = 1, max = 256))]
        view_ids: Vec<Id>,
    },
    Pinned {
        pin_id: Id,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositorySnapshot {
    pub repository_id: Id,
    pub base: GitOid,
    pub head: GitOid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_digest: Option<Digest>,
}
impl RepositorySnapshot {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.base.validate()?;
        self.head.validate()?;
        if self.base.format != self.head.format
            || self.worktree_id.is_some() != self.capture_digest.is_some()
        {
            return Err("snapshot capture mismatch");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LayerRef {
    pub generation_id: Id,
    pub manifest_digest: Digest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_generation_id: Option<Id>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum CoverageState {
    Complete,
    Partial,
    Unavailable,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Coverage {
    pub state: CoverageState,
    #[schemars(length(max = 128))]
    pub gaps: Vec<Id>,
    pub more_gaps: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryView {
    pub view_id: Id,
    pub snapshot: RepositorySnapshot,
    pub source_revision: Revision,
    pub baseline: LayerRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<LayerRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<LayerRef>,
    pub coverage: Coverage,
}
impl RepositoryView {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.snapshot.validate()?;
        let mut layers = HashSet::new();
        layers.insert(&self.baseline.generation_id.0);
        if self
            .branch
            .as_ref()
            .is_some_and(|b| !layers.insert(&b.generation_id.0))
            || self
                .worktree
                .as_ref()
                .is_some_and(|w| !layers.insert(&w.generation_id.0))
        {
            return Err("duplicate view generation");
        }
        if self.snapshot.worktree_id.is_some() != self.worktree.is_some()
            || self.snapshot.head != self.snapshot.base && self.branch.is_none()
        {
            return Err("snapshot layer omitted");
        }
        if self.baseline.parent_generation_id.is_some() {
            return Err("baseline cannot have parent");
        }
        if let Some(b) = &self.branch
            && b.parent_generation_id.as_ref() != Some(&self.baseline.generation_id)
        {
            return Err("branch parent mismatch");
        }
        if let Some(w) = &self.worktree {
            let parent = self
                .branch
                .as_ref()
                .map(|b| &b.generation_id)
                .unwrap_or(&self.baseline.generation_id);
            if w.parent_generation_id.as_ref() != Some(parent)
                || self.snapshot.worktree_id.is_none()
            {
                return Err("worktree parent/capture mismatch");
            }
        }
        if self.coverage.gaps.len() > 128
            || matches!(self.coverage.state, CoverageState::Complete)
                && (!self.coverage.gaps.is_empty() || self.coverage.more_gaps)
        {
            return Err("coverage claim mismatch");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SchemaView {
    pub connection_id: Id,
    pub snapshot_id: Id,
    pub generation_id: Id,
    pub schema_digest: Digest,
    pub access_revision: Revision,
    pub coverage: Coverage,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResolvedViewSet {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 256))]
    pub schema_views: Vec<SchemaView>,
    pub pin_id: Id,
    pub stamp: ReadStamp,
    pub expires_at_ms: Counter,
    #[schemars(length(max = 256))]
    pub views: Vec<RepositoryView>,
}
impl ResolvedViewSet {
    pub fn validate(&self) -> Result<(), &'static str> {
        let mut ids = HashSet::new();
        if self.views.len() + self.schema_views.len() == 0
            || self.views.len() + self.schema_views.len() > 256
        {
            return Err("invalid view set");
        }
        let mut connections = HashSet::new();
        for schema in &self.schema_views {
            if !connections.insert(&schema.connection_id.0)
                || schema.coverage.gaps.len() > 128
                || matches!(schema.coverage.state, CoverageState::Complete)
                    && (!schema.coverage.gaps.is_empty() || schema.coverage.more_gaps)
            {
                return Err("invalid schema view");
            }
        }
        for v in &self.views {
            v.validate()?;
            if !ids.insert(&v.snapshot.repository_id.0) {
                return Err("duplicate repository view");
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueryLimits {
    #[schemars(range(min = 1, max = 500))]
    pub max_results: u16,
    #[schemars(range(min = 1, max = 1000000))]
    pub max_visited: u32,
    #[schemars(range(min = 1, max = 300000))]
    pub deadline_ms: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum Direction {
    Forward,
    Reverse,
    Both,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum GraphQuery {
    #[schemars(extend("oneOf" = [{"required": ["kind", "text", "nodeKinds"], "x-until": 3}, {"required": ["kind", "text", "nodeKinds"], "not": {"required": ["selection"]}, "x-since": 3}, {"required": ["kind", "selection"], "not": {"anyOf": [{"required": ["text"]}, {"required": ["nodeKinds"]}]}, "x-since": 3}]))]
    Search {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "String", length(min = 1, max = 8192))]
        text: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "Vec<Id>", length(max = 64))]
        node_kinds: Option<Vec<Id>>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "CodeSearchSelection", extend("x-since" = 3))]
        selection: Option<CodeSearchSelection>,
    },
    Overview {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "Id", extend("x-since" = 3))]
        repository_id: Option<Id>,
    },
    Schema,
    Node {
        node_id: Id,
    },
    #[schemars(extend("oneOf" = [{"required": ["kind", "nodeId", "maxLines"], "x-until": 3}, {"required": ["kind", "nodeId", "maxLines"], "not": {"required": ["selection"]}, "x-since": 3}, {"required": ["kind", "selection"], "not": {"anyOf": [{"required": ["nodeId"]}, {"required": ["maxLines"]}]}, "x-since": 3}]))]
    Source {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "Id")]
        node_id: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "u16", range(min = 1, max = 2000))]
        max_lines: Option<u16>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "CodeSourceSelection", extend("x-since" = 3))]
        selection: Option<CodeSourceSelection>,
    },
    Traverse {
        #[schemars(length(min = 1, max = 64))]
        start_ids: Vec<Id>,
        direction: Direction,
        #[schemars(length(max = 64))]
        edge_kinds: Vec<Id>,
        #[schemars(range(min = 0, max = 128))]
        max_depth: u16,
    },
    #[schemars(extend("oneOf" = [{"required": ["kind", "fromId", "toId", "maxDepth"], "x-until": 3}, {"required": ["kind", "fromId", "toId", "maxDepth"], "not": {"required": ["selection"]}, "x-since": 3}, {"required": ["kind", "selection"], "not": {"anyOf": [{"required": ["fromId"]}, {"required": ["toId"]}, {"required": ["maxDepth"]}]}, "x-since": 3}]))]
    Path {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "Id")]
        from_id: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "Id")]
        to_id: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "u16", range(min = 1, max = 128))]
        max_depth: Option<u16>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "CodeCallSelection", extend("x-since" = 3))]
        selection: Option<CodeCallSelection>,
    },
    #[schemars(extend("oneOf" = [{"required": ["kind", "nodeId"], "x-until": 3}, {"required": ["kind", "nodeId"], "not": {"required": ["selection"]}, "x-since": 3}, {"required": ["kind", "selection"], "not": {"anyOf": [{"required": ["nodeId"]}]}, "x-since": 3}]))]
    NodeImpact {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "Id")]
        node_id: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "CodeImpactSelection", extend("x-since" = 3))]
        selection: Option<CodeImpactSelection>,
    },

    #[schemars(extend("oneOf" = [{"required": ["kind", "repositoryId", "unitKey"], "x-until": 3}, {"required": ["kind", "repositoryId", "unitKey"], "not": {"required": ["selection"]}, "x-since": 3}, {"required": ["kind", "selection"], "not": {"anyOf": [{"required": ["repositoryId"]}, {"required": ["unitKey"]}]}, "x-since": 3}]))]
    FileImpact {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "Id")]
        repository_id: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "Id")]
        unit_key: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "CodeFileSelection", extend("x-since" = 3))]
        selection: Option<CodeFileSelection>,
    },
    #[schemars(extend("oneOf" = [{"required": ["kind", "repositoryId", "unitKey"], "x-until": 3}, {"required": ["kind", "repositoryId", "unitKey"], "not": {"required": ["selection"]}, "x-since": 3}, {"required": ["kind", "selection"], "not": {"anyOf": [{"required": ["repositoryId"]}, {"required": ["unitKey"]}]}, "x-since": 3}]))]
    NodesByFile {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "Id")]
        repository_id: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "Id")]
        unit_key: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "CodeFileSelection", extend("x-since" = 3))]
        selection: Option<CodeFileSelection>,
    },
    RepositoryDependencies {
        repository_id: Id,
    },
    #[schemars(extend("oneOf" = [{"required": ["kind", "nodeId"], "x-until": 3}, {"required": ["kind", "nodeId"], "not": {"required": ["selection"]}, "x-since": 3}, {"required": ["kind", "selection"], "not": {"anyOf": [{"required": ["nodeId"]}]}, "x-since": 3}]))]
    Consumers {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "Id")]
        node_id: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "CodeNodeSelection", extend("x-since" = 3))]
        selection: Option<CodeNodeSelection>,
    },
    #[schemars(extend("oneOf" = [{"required": ["kind", "nodeId"], "x-until": 3}, {"required": ["kind", "nodeId"], "not": {"required": ["selection"]}, "x-since": 3}, {"required": ["kind", "selection"], "not": {"anyOf": [{"required": ["nodeId"]}]}, "x-since": 3}]))]
    Publishers {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "Id")]
        node_id: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "CodeNodeSelection", extend("x-since" = 3))]
        selection: Option<CodeNodeSelection>,
    },
    #[schemars(extend("oneOf" = [{"required": ["kind", "nodeId"], "x-until": 3}, {"required": ["kind", "nodeId"], "not": {"required": ["selection"]}, "x-since": 3}, {"required": ["kind", "selection"], "not": {"anyOf": [{"required": ["nodeId"]}]}, "x-since": 3}]))]
    DataLineage {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "Id")]
        node_id: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "CodeNodeSelection", extend("x-since" = 3))]
        selection: Option<CodeNodeSelection>,
    },
    ScopeStatus,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(extend("oneOf" = [{"required": ["protocol", "queryId", "scope", "view", "query", "limits"]}, {"required": ["protocol", "queryId", "scope", "current", "query", "limits"], "x-since": 3}]))]
pub struct GraphReadRequest {
    pub protocol: Protocol,
    pub query_id: QueryId,
    pub scope: DataScope,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::support::gated_member"
    )]
    #[schemars(with = "ViewSelection")]
    pub view: Option<ViewSelection>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::support::gated_member"
    )]
    #[schemars(with = "bool", extend("const" = true), extend("x-since" = 3))]
    pub current: Option<bool>,
    pub query: GraphQuery,
    pub limits: QueryLimits,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 4096))]
    pub cursor: Option<String>,
}
impl GraphReadRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.limits.max_results == 0
            || self.limits.max_results > 500
            || self.limits.max_visited == 0
            || self.limits.max_visited > 1000000
            || self.limits.deadline_ms == 0
            || self.limits.deadline_ms > 300000
        {
            return Err("unbounded graph query");
        }
        match (&self.view, self.current) {
            (Some(_), None) | (None, Some(true)) => {}
            _ => return Err("exactly one explicit or current graph view is required"),
        }
        if let Some(view) = &self.view {
            match view {
                ViewSelection::SchemaPublished { connection_ids } => {
                    unique_ids(connection_ids, 1, 256)?
                }
                ViewSelection::Published { repository_ids } => unique_ids(repository_ids, 1, 256)?,
                ViewSelection::Exact { view_ids } => unique_ids(view_ids, 1, 256)?,
                _ => {}
            }
        }
        match &self.query {
            GraphQuery::Search {
                text,
                node_kinds,
                selection,
            } => match (text, node_kinds, selection) {
                (Some(text), Some(kinds), None)
                    if !text.is_empty() && text.len() <= 8192 && kinds.len() <= 64 => {}
                (None, None, Some(selection)) => check_graph_selection(selection)?,
                _ => return Err("exactly one search selection is required"),
            },
            GraphQuery::Traverse {
                start_ids,
                edge_kinds,
                max_depth,
                ..
            } => {
                unique_ids(start_ids, 1, 64)?;
                if edge_kinds.len() > 64 || *max_depth > 128 {
                    return Err("invalid traversal bounds");
                }
            }
            GraphQuery::Source {
                node_id,
                max_lines,
                selection,
            } => match (node_id, max_lines, selection) {
                (Some(_), Some(lines), None) if *lines > 0 && *lines <= 2000 => {}
                (None, None, Some(selection)) => check_graph_selection(selection)?,
                _ => return Err("exactly one source selection is required"),
            },
            GraphQuery::Path {
                from_id,
                to_id,
                max_depth,
                selection,
            } => match (from_id, to_id, max_depth, selection) {
                (Some(_), Some(_), Some(depth), None) if *depth > 0 && *depth <= 128 => {}
                (None, None, None, Some(selection)) => check_graph_selection(selection)?,
                _ => return Err("exactly one path selection is required"),
            },
            GraphQuery::NodeImpact { node_id, selection } => {
                one_target(node_id.is_some(), selection.is_some())?;
                if let Some(v) = selection {
                    check_graph_selection(v)?;
                }
            }
            GraphQuery::Consumers { node_id, selection }
            | GraphQuery::Publishers { node_id, selection }
            | GraphQuery::DataLineage { node_id, selection } => {
                one_target(node_id.is_some(), selection.is_some())?;
                if let Some(v) = selection {
                    check_graph_selection(v)?;
                }
            }
            GraphQuery::FileImpact {
                repository_id,
                unit_key,
                selection,
            }
            | GraphQuery::NodesByFile {
                repository_id,
                unit_key,
                selection,
            } => match (repository_id, unit_key, selection) {
                (Some(_), Some(_), None) => {}
                (None, None, Some(v)) => check_graph_selection(v)?,
                _ => return Err("exactly one file selection is required"),
            },
            _ => {}
        }
        if self
            .cursor
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > 4096)
        {
            return Err("invalid cursor");
        }
        Ok(())
    }
}
fn check_graph_selection<T: Check + Serialize>(selection: &T) -> Result<(), &'static str> {
    selection.check().map_err(|_| "invalid graph selection")?;
    fn walk(v: &serde_json::Value) -> bool {
        if v["kind"] == "Name" && !v["name"].as_str().is_some_and(|s| !s.trim().is_empty()) {
            return false;
        }
        if v["kind"] == "File" {
            let start = v["startLine"]
                .as_str()
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(0);
            let end = v["endLine"]
                .as_str()
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(0);
            if end > 0 && start > end {
                return false;
            }
        }
        match v {
            serde_json::Value::Object(o) => o.values().all(walk),
            serde_json::Value::Array(a) => a.iter().all(walk),
            _ => true,
        }
    }
    if walk(&serde_json::to_value(selection).map_err(|_| "invalid graph selection")?) {
        Ok(())
    } else {
        Err("invalid named target or source range")
    }
}

fn one_target(old: bool, current: bool) -> Result<(), &'static str> {
    if old == current {
        Err("exactly one graph target is required")
    } else {
        Ok(())
    }
}

fn unique_ids(ids: &[Id], minimum: usize, maximum: usize) -> Result<(), &'static str> {
    let mut set = HashSet::new();
    if ids.len() < minimum || ids.len() > maximum || ids.iter().any(|id| !set.insert(&id.0)) {
        return Err("invalid identity set");
    }
    Ok(())
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum Locator {
    SchemaObject {
        object_id: Id,
    },
    Text {
        first_line: u32,
        last_line: u32,
    },
    Pdf {
        page: u32,
    },
    Docx {
        part: Id,
        paragraph: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        table_cell: Option<Id>,
    },
    Code {
        snapshot: RepositorySnapshot,
        unit_key: Id,
        first_line: u32,
        last_line: u32,
    },
    Knowledge {
        entry_id: Id,
        revision: Revision,
    },
}
impl Locator {
    fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Text {
                first_line,
                last_line,
            }
            | Self::Code {
                first_line,
                last_line,
                ..
            } if *first_line == 0 || last_line < first_line => return Err("invalid source range"),
            Self::Pdf { page } if *page == 0 => return Err("invalid page"),
            Self::Docx { paragraph, .. } if *paragraph == 0 => return Err("invalid paragraph"),
            _ => {}
        }
        if let Self::Code { snapshot, .. } = self {
            snapshot.validate()?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum CitationSource {
    DatabaseSchema {
        connection_id: Id,
        snapshot_id: Id,
        schema_digest: Digest,
    },
    Managed {
        version: ArtifactRef,
        extraction_revision: Revision,
    },
    Repository {
        snapshot: RepositorySnapshot,
        unit_key: Id,
        source_digest: Digest,
    },
    Knowledge {
        entry_id: Id,
        revision: Revision,
    },
    Convention {
        version: ArtifactRef,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Citation {
    pub source: CitationSource,
    pub block_id: Id,
    pub content_digest: Digest,
    pub locator: Locator,
}
impl Citation {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.locator.validate()?;
        match &self.source {
            CitationSource::DatabaseSchema { .. } => {
                if !matches!(self.locator, Locator::SchemaObject { .. }) {
                    return Err("schema citation mismatch");
                }
            }
            CitationSource::Managed { version, .. } | CitationSource::Convention { version } => {
                version.validate()?
            }
            CitationSource::Repository { snapshot, .. } => {
                snapshot.validate()?;
                if let Locator::Code { snapshot: loc, .. } = &self.locator {
                    if serde_json::to_value(snapshot).unwrap() != serde_json::to_value(loc).unwrap()
                    {
                        return Err("citation snapshot mismatch");
                    }
                } else {
                    return Err("repository citation needs code locator");
                }
            }
            CitationSource::Knowledge { entry_id, revision } => {
                if !matches!(&self.locator,Locator::Knowledge{entry_id:id,revision:rev}if id==entry_id&&rev==revision)
                {
                    return Err("knowledge citation mismatch");
                }
            }
        }
        if matches!(
            self.source,
            CitationSource::Managed { .. } | CitationSource::Convention { .. }
        ) && matches!(
            self.locator,
            Locator::Code { .. } | Locator::Knowledge { .. }
        ) {
            return Err("managed citation source mismatch");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum Metric {
    L2,
    Cosine,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EmbeddingSpace {
    pub space_id: Id,
    pub model_digest: Digest,
    #[schemars(range(min = 1, max = 65536))]
    pub dimensions: u32,
    pub metric: Metric,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum RetrievalInput {
    Text {
        #[schemars(length(min = 1, max = 8192))]
        text: String,
    },
    Vector {
        space: EmbeddingSpace,
        float32_le: ArtifactRef,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum RetrievalMode {
    Lexical,
    Vector,
    Hybrid,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum RetrievalSource {
    Code,
    Document,
    Knowledge,
    Convention,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum DocumentVersions {
    Current,
    Exact {
        #[schemars(length(min = 1, max = 128))]
        versions: Vec<ArtifactRef>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RetrievalRequest {
    pub protocol: Protocol,
    pub query_id: QueryId,
    pub scope: DataScope,
    #[schemars(length(min = 1, max = 4))]
    pub sources: Vec<RetrievalSource>,
    pub versions: DocumentVersions,
    pub input: RetrievalInput,
    pub mode: RetrievalMode,
    pub lexical_fallback: bool,
    pub limits: QueryLimits,
}
impl RetrievalRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.sources.is_empty()
            || self.sources.len() > 4
            || self.limits.max_results == 0
            || self.limits.max_results > 500
        {
            return Err("retrieval bounds");
        }
        match &self.input {
            RetrievalInput::Text { text } if text.is_empty() || text.len() > 8192 => {
                return Err("query text bounds");
            }
            RetrievalInput::Vector { space, float32_le }
                if (space.dimensions == 0
                    || space.dimensions > 65536
                    || float32_le.byte_length.get() != i64::from(space.dimensions) * 4) =>
            {
                return Err("vector shape mismatch");
            }
            _ => {}
        }
        if let DocumentVersions::Exact { versions } = &self.versions
            && (versions.is_empty() || versions.len() > 128)
        {
            return Err("invalid version selection");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum QueryCompletion {
    Complete,
    ResultLimit,
    WorkLimit,
    Deadline,
    Cancelled,
}
/// Graph-only completion; retrieval keeps its existing five-value contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraphCompletion {
    Complete,
    ResultLimit,
    WorkLimit,
    Deadline,
    Cancelled,
    Partial,
}
impl schemars::JsonSchema for GraphCompletion {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "GraphCompletion".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"x-earlier":{"name":"QueryCompletion","before":3},"oneOf":[{"type":"string","enum":["Complete","ResultLimit","WorkLimit","Deadline","Cancelled"]},{"type":"string","const":"Partial","x-since":3}]})
    }
}
impl From<QueryCompletion> for GraphCompletion {
    fn from(value: QueryCompletion) -> Self {
        match value {
            QueryCompletion::Complete => Self::Complete,
            QueryCompletion::ResultLimit => Self::ResultLimit,
            QueryCompletion::WorkLimit => Self::WorkLimit,
            QueryCompletion::Deadline => Self::Deadline,
            QueryCompletion::Cancelled => Self::Cancelled,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Passage {
    pub chunk_id: Id,
    pub citation: Citation,
    #[schemars(length(max = 65536))]
    pub text: String,
    pub rank: u16,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RetrievalPage {
    pub protocol: Protocol,
    pub query_id: QueryId,
    pub stamp: ReadStamp,
    pub generation_ids: Vec<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<EmbeddingSpace>,
    pub effective_mode: RetrievalMode,
    pub completion: QueryCompletion,
    #[schemars(length(max = 500))]
    pub passages: Vec<Passage>,
}
impl RetrievalPage {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.passages.len() > 500 {
            return Err("too many passages");
        }
        let mut chunks = HashSet::new();
        for p in &self.passages {
            p.citation.validate()?;
            if p.text.len() > 65536 || p.rank == 0 || !chunks.insert(&p.chunk_id.0) {
                return Err("invalid passage result");
            }
        }
        if !matches!(self.effective_mode, RetrievalMode::Lexical) && self.space.is_none() {
            return Err("missing embedding space");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum IndexLayer {
    Baseline,
    Branch,
    Worktree,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum IndexCommand {
    // The overlay of exactly one source: a worktree, or from schema revision 2 a ref (a branch,
    // tag or full commit, resolved to its current commit). Revision 1 requires `worktreeId`.
    #[schemars(extend("oneOf" = [{"required": ["kind", "repositoryId", "worktreeId", "expectedSourceRevision"]}, {"required": ["kind", "repositoryId", "ref", "expectedSourceRevision"], "x-since": 2}]))]
    RefreshOverlay {
        repository_id: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(with = "Id")]
        worktree_id: Option<Id>,
        /// Schema revision 2: a branch name, tag or full commit SHA instead of `worktreeId`.
        #[serde(
            rename = "ref",
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "String", length(min = 1, max = 256), extend("x-since" = 2))]
        git_ref: Option<String>,
        expected_source_revision: Revision,
    },
    Cancel {
        operation_id: OperationId,
    },
    #[schemars(extend("oneOf" = [{"required": ["kind", "repositoryId", "nodeId", "expectedRevision", "viewId", "expectedContentDigest", "trusted"], "x-until": 3}, {"required": ["kind", "repositoryId", "nodeId", "expectedRevision", "viewId", "expectedContentDigest", "trusted", "unitKey", "expectedViewDigest", "scope"], "not": {"required": ["expectedContentIdentity"]}, "x-since": 3}, {"required": ["kind", "repositoryId", "nodeId", "expectedRevision", "viewId", "trusted", "unitKey", "expectedViewDigest", "expectedContentIdentity", "scope"], "not": {"required": ["expectedContentDigest"]}, "x-since": 3}]))]
    SetTrust {
        repository_id: Id,
        node_id: Id,
        expected_revision: Revision,
        view_id: Id,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "Digest")]
        expected_content_digest: Option<Digest>,
        trusted: bool,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "super::support::GraphUnitKey", extend("x-since" = 3))]
        unit_key: Option<super::support::GraphUnitKey>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "Digest", extend("x-since" = 3))]
        expected_view_digest: Option<Digest>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with="super::support::Content",extend("x-since"=3))]
        expected_content_identity: Option<super::support::Content>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with="DataScope",extend("x-since"=3))]
        scope: Option<DataScope>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with="Id",extend("x-since"=3))]
        pin_id: Option<Id>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IndexCommandRequest {
    pub protocol: Protocol,
    pub request_key: RequestKey,
    pub command: IndexCommand,
}
/// Whether `s` names a branch, tag or full commit: a full lowercase object ID, or a ref name
/// under `git check-ref-format` rules (no revision syntax, no option spelling).
pub fn overlay_ref_ok(s: &str) -> bool {
    let oid =
        matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    oid || (!s.is_empty()
        && s.len() <= 256
        && !s.starts_with(['-', '/'])
        && !s.ends_with(['/', '.'])
        && !s.ends_with(".lock")
        && s != "@"
        && !s.bytes().any(|b| b < 0x20 || b == 0x7f)
        && !s.contains([' ', '~', '^', ':', '?', '*', '[', '\\'])
        && !s.contains("..")
        && !s.contains("@{")
        && !s.contains("//")
        && !s.split('/').any(|c| c.starts_with('.')))
}
impl IndexCommandRequest {
    pub fn validate_revision(&self, revision: u32) -> Result<(), &'static str> {
        if let IndexCommand::SetTrust {
            scope,
            unit_key,
            expected_view_digest,
            expected_content_digest,
            expected_content_identity,
            ..
        } = &self.command
        {
            if revision >= 3 {
                if scope.is_none()
                    || unit_key.is_none()
                    || expected_view_digest.is_none()
                    || expected_content_digest.is_some() == expected_content_identity.is_some()
                {
                    return Err(
                        "current trust change requires full view/unit identity and exactly one content witness",
                    );
                }
            } else if expected_content_digest.is_none() {
                return Err("legacy trust change requires its recorded content digest");
            }
            if expected_content_identity
                .as_ref()
                .is_some_and(|s| s.0.is_empty())
            {
                return Err("empty unit content identity");
            }
            if let Some(unit) = unit_key {
                unit.check().map_err(|_| "invalid trust unit identity")?;
            }
        }
        Ok(())
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        if let IndexCommand::RefreshOverlay {
            worktree_id,
            git_ref,
            ..
        } = &self.command
        {
            match (worktree_id, git_ref) {
                (Some(_), None) => {}
                (None, Some(r)) if overlay_ref_ok(r) => {}
                (None, Some(_)) => return Err("an overlay ref is a branch, tag or full commit"),
                _ => return Err("an overlay names exactly one of a worktree and a ref"),
            }
        }
        Ok(())
    }
}
impl super::support::Revisioned for IndexCommandRequest {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 3
            && let IndexCommand::SetTrust { scope, pin_id, .. } = &mut self.command
        {
            *scope = None;
            *pin_id = None;
        }

        if revision < 3
            && let IndexCommand::SetTrust {
                expected_content_identity,
                ..
            } = &mut self.command
        {
            *expected_content_identity = None;
        }

        if revision < 3
            && let IndexCommand::SetTrust {
                unit_key,
                expected_view_digest,
                ..
            } = &mut self.command
        {
            *unit_key = None;
            *expected_view_digest = None;
        }

        if let IndexCommand::RefreshOverlay { git_ref, .. } = &mut self.command
            && revision < 2
        {
            *git_ref = None;
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationRequest {
    pub protocol: Protocol,
    pub operation_id: OperationId,
    pub source_id: Id,
    pub expected_source_revision: Revision,
    pub expected_pointer_revision: Counter,
    pub view: RepositoryView,
    pub sealed_manifest: ArtifactRef,
}
impl PublicationRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.view.validate()?;
        self.sealed_manifest.validate()?;
        if self.source_id != self.view.snapshot.repository_id
            || self.expected_source_revision != self.view.source_revision
        {
            return Err("publication source mismatch");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ArtifactSource {
    Upload { ticket_id: Id },
    Attachment { attachment_id: Id },
    WorkOutput { file_handle: Id },
}
/// Most tags one artifact carries.
pub const MAX_ARTIFACT_TAGS: usize = 32;
/// Longest tag, in characters.
pub const MAX_ARTIFACT_TAG_CHARS: usize = 64;

/// A set of opaque artifact labels (schema revision 2): at most [`MAX_ARTIFACT_TAGS`], each
/// 1–[`MAX_ARTIFACT_TAG_CHARS`] characters without NUL, no duplicates. It is a set, so decoding
/// sorts it (byte order): equal sets normalize, fingerprint and persist identically.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ArtifactTags(pub Vec<String>);
impl ArtifactTags {
    /// Checks the bounds and returns the sorted set.
    pub fn new(mut tags: Vec<String>) -> Result<Self, &'static str> {
        if tags.len() > MAX_ARTIFACT_TAGS {
            return Err("too many artifact tags");
        }
        if tags.iter().any(|t| {
            let n = t.chars().count();
            n == 0 || n > MAX_ARTIFACT_TAG_CHARS || t.contains('\0')
        }) {
            return Err("artifact tag length or NUL outside contract");
        }
        tags.sort();
        if tags.windows(2).any(|w| w[0] == w[1]) {
            return Err("duplicate artifact tag");
        }
        Ok(Self(tags))
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
impl<'de> Deserialize<'de> for ArtifactTags {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(Vec::<String>::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
impl JsonSchema for ArtifactTags {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ArtifactTags".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "array",
            "maxItems": MAX_ARTIFACT_TAGS,
            "uniqueItems": true,
            "items": {
                "type": "string",
                "minLength": 1,
                "maxLength": MAX_ARTIFACT_TAG_CHARS,
                "pattern": "^[^\\u0000]*$"
            }
        })
    }
}
/// An empty optional tag set is the same intent as no tags.
fn normalize_tags(tags: &mut Option<ArtifactTags>) {
    if tags.as_ref().is_some_and(ArtifactTags::is_empty) {
        *tags = None;
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ArtifactCommand {
    // A new artifact in exactly one owner scope: `projectId`, or from schema revision 3 the
    // workspace (`workspace: true`). Revisions 1 and 2 require `projectId`.
    #[schemars(extend("oneOf" = [{"required": ["kind", "projectId", "title", "mediaType", "source", "expectedDigest", "expectedBytes"]}, {"required": ["kind", "workspace", "title", "mediaType", "source", "expectedDigest", "expectedBytes"], "x-since": 3}]))]
    Save {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(with = "ProjectId")]
        project_id: Option<ProjectId>,
        /// Schema revision 3: save into the workspace scope (owned by the invoking principal)
        /// instead of `projectId`; always `true` when present.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "bool", extend("x-since" = 3, "const" = true))]
        workspace: Option<bool>,
        title: String,
        media_type: String,
        source: ArtifactSource,
        expected_digest: Digest,
        expected_bytes: Counter,
        /// Labels of the new artifact (schema revision 2).
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "ArtifactTags", extend("x-since" = 2))]
        tags: Option<ArtifactTags>,
    },
    Replace {
        artifact_id: Id,
        expected_revision: Revision,
        source: ArtifactSource,
        expected_digest: Digest,
        expected_bytes: Counter,
    },
    Reference {
        artifact_id: Id,
        version_id: Id,
        subject: Subject,
        expected_revision: Revision,
    },
    DeleteVersion {
        artifact_id: Id,
        version_id: Id,
        expected_revision: Revision,
        expected_reference_revision: Revision,
    },
    DetachProject {
        artifact_id: Id,
        project_id: ProjectId,
        expected_reference_revision: Revision,
    },
    Share {
        artifact_id: Id,
        target_project_id: ProjectId,
        expected_scope_revision: Revision,
    },
    Move {
        artifact_id: Id,
        target_project_id: ProjectId,
        expected_scope_revision: Revision,
    },
    DisableRetrieval {
        artifact_id: Id,
        expected_revision: Revision,
    },
    RemoveIndex {
        artifact_id: Id,
        expected_revision: Revision,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactCommandRequest {
    pub protocol: Protocol,
    pub request_key: RequestKey,
    pub command: ArtifactCommand,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ArtifactQuery {
    // The artifacts of exactly one scope: `projectId`, or from schema revision 3 the workspace
    // (`workspace: true`). Revisions 1 and 2 require `projectId`.
    #[schemars(extend("oneOf" = [{"required": ["kind", "projectId"]}, {"required": ["kind", "workspace"], "x-since": 3}]))]
    List {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(with = "ProjectId")]
        project_id: Option<ProjectId>,
        /// Schema revision 3: list the workspace scope instead of `projectId`; always `true`.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "bool", extend("x-since" = 3, "const" = true))]
        workspace: Option<bool>,
        /// Only artifacts carrying every listed tag (schema revision 2).
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "ArtifactTags", extend("x-since" = 2))]
        tags: Option<ArtifactTags>,
    },
    Read {
        artifact_id: Id,
        version_id: Id,
    },
    Preview {
        artifact_id: Id,
        version_id: Id,
    },
    Export {
        artifact_id: Id,
        version_id: Id,
    },
    ResolveCitation {
        citation: Citation,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactQueryRequest {
    pub protocol: Protocol,
    pub query_id: QueryId,
    pub query: ArtifactQuery,
    #[schemars(range(min = 1, max = 500))]
    pub limit: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}
impl ArtifactQueryRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.limit == 0
            || self.limit > 500
            || self
                .cursor
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 4096)
        {
            return Err("invalid artifact page");
        }
        if let ArtifactQuery::ResolveCitation { citation } = &self.query {
            citation.validate()?;
        }
        if let ArtifactQuery::List {
            project_id,
            workspace,
            ..
        } = &self.query
        {
            one_scope(project_id, workspace)?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ArtifactDecision {
    VersionPublished {
        artifact_id: Id,
        version: ArtifactRef,
        revision: Revision,
    },
    ReferenceChanged {
        artifact_id: Id,
        scope_revision: Revision,
    },
    VersionUnavailable {
        artifact_id: Id,
        version_id: Id,
        physical_delete_pending: bool,
    },
    SharedReferencesRemain {
        artifact_id: Id,
    },
    RevisionConflict {
        revision_kind: RevisionKind,
        expected: Revision,
        current: Revision,
    },
    IndexDisabled {
        artifact_id: Id,
        revision: Revision,
    },
}
pub type ArtifactCommandReply = CommandReply<ArtifactDecision>;
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum DeliveryKind {
    Feature,
    StandaloneIssue,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ReviewSource {
    GitHubAutomated,
    InAppIndependent,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum MergeMethod {
    MergeCommit,
    Squash,
    Rebase,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryDelivery {
    pub repository_id: Id,
    pub pull_request_id: Id,
    pub base_ref: String,
    pub expected_head: GitOid,
    pub observed_base: GitOid,
    pub review_source: ReviewSource,
    pub merge_method: MergeMethod,
    pub evidence: ArtifactRef,
    pub policy_revision: Revision,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliveryDependency {
    pub before: Id,
    pub after: Id,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliveryBarrier {
    pub protocol: Protocol,
    pub delivery_id: Id,
    pub root_id: RootId,
    pub kind: DeliveryKind,
    pub revision: Revision,
    pub approved_scope_digest: Digest,
    #[schemars(length(min = 1, max = 256))]
    pub repositories: Vec<RepositoryDelivery>,
    #[schemars(length(max = 4096))]
    pub dependencies: Vec<DeliveryDependency>,
}
impl DeliveryBarrier {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.repositories.is_empty()
            || self.repositories.len() > 256
            || self.dependencies.len() > 4096
        {
            return Err("barrier bounds");
        }
        let mut ids = HashSet::new();
        for r in &self.repositories {
            r.evidence.validate()?;
            r.expected_head.validate()?;
            r.observed_base.validate()?;
            r.evidence.validate()?;
            if r.base_ref != "refs/heads/main"
                || r.expected_head.format != r.observed_base.format
                || !ids.insert(r.repository_id.0.clone())
            {
                return Err("invalid final delivery target");
            }
        }
        let mut edges = std::collections::HashMap::<&str, Vec<&str>>::new();
        for d in &self.dependencies {
            if d.before == d.after || !ids.contains(&d.before.0) || !ids.contains(&d.after.0) {
                return Err("unknown delivery dependency");
            }
            edges.entry(&d.before.0).or_default().push(&d.after.0);
        }
        fn visit<'a>(
            n: &'a str,
            e: &std::collections::HashMap<&'a str, Vec<&'a str>>,
            active: &mut HashSet<&'a str>,
            done: &mut HashSet<&'a str>,
        ) -> bool {
            if done.contains(n) {
                return true;
            }
            if !active.insert(n) {
                return false;
            }
            for next in e.get(n).into_iter().flatten() {
                if !visit(next, e, active, done) {
                    return false;
                }
            }
            active.remove(n);
            done.insert(n);
            true
        }
        let mut done = HashSet::new();
        for id in &ids {
            if !visit(id, &edges, &mut HashSet::new(), &mut done) {
                return Err("cyclic delivery order");
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum PushMode {
    Normal,
    ForceWithLease,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum GitCommand {
    Commit {
        worktree_id: Id,
        expected_head: GitOid,
        message: ArtifactRef,
    },
    Push {
        worktree_id: Id,
        remote_id: Id,
        target_ref: String,
        expected_head: GitOid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_remote: Option<GitOid>,
        mode: PushMode,
    },
    OpenPullRequest {
        repository_id: Id,
        head_ref: String,
        base_ref: String,
        expected_head: GitOid,
        description: ArtifactRef,
        /// Complete explicit task coverage of a grouped PR (revision 2). Absence covers its
        /// owning task only, preserving revision 1.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::support::gated_member"
        )]
        #[schemars(with = "Vec<Id>", extend("x-since" = 2), length(min = 1, max = 256))]
        covered_work_ids: Option<Vec<Id>>,
    },
    Integrate {
        delivery_id: Id,
        expected_barrier_revision: Revision,
        repository_id: Id,
        expected_head: GitOid,
    },
    Cleanup {
        worktree_id: Id,
        expected_ownership_revision: Revision,
        expected_head: GitOid,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitCommandRequest {
    pub protocol: Protocol,
    pub request_key: RequestKey,
    pub command: GitCommand,
}
fn branch_ref(s: &str) -> bool {
    s.starts_with("refs/heads/")
        && !s.ends_with('/')
        && !s.contains([' ', '\\', '~', '^', ':', '?', '*', '[', '\r', '\n'])
        && !s.contains("..")
        && !s.contains("@{")
        && !s.contains("//")
        && !s.ends_with(".lock")
}
impl GitCommandRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        match &self.command {
            GitCommand::Commit {
                expected_head,
                message,
                ..
            } => {
                expected_head.validate()?;
                message.validate()?;
            }
            GitCommand::Push {
                target_ref,
                expected_head,
                expected_remote,
                mode,
                ..
            } => {
                expected_head.validate()?;
                if !branch_ref(target_ref) {
                    return Err("invalid push target");
                }
                if let Some(remote) = expected_remote {
                    remote.validate()?;
                    if remote.format != expected_head.format {
                        return Err("mixed object formats");
                    }
                }
                if matches!(mode, PushMode::ForceWithLease)
                    && (target_ref == "refs/heads/main" || expected_remote.is_none())
                {
                    return Err("force requires lease and cannot target main");
                }
            }
            GitCommand::OpenPullRequest {
                head_ref,
                base_ref,
                expected_head,
                description,
                covered_work_ids,
                ..
            } => {
                expected_head.validate()?;
                if let Some(ids) = covered_work_ids
                    && (ids.is_empty()
                        || ids.len() > 256
                        || ids
                            .iter()
                            .map(|id| id.as_str())
                            .collect::<HashSet<_>>()
                            .len()
                            != ids.len())
                {
                    return Err("invalid or duplicate PR task coverage");
                }
                description.validate()?;
                if !branch_ref(head_ref) || !branch_ref(base_ref) || head_ref == base_ref {
                    return Err("invalid PR refs");
                }
            }
            GitCommand::Integrate { expected_head, .. }
            | GitCommand::Cleanup { expected_head, .. } => expected_head.validate()?,
        }
        Ok(())
    }
}
impl super::support::Revisioned for GitCommandRequest {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2
            && let GitCommand::OpenPullRequest {
                covered_work_ids, ..
            } = &mut self.command
        {
            *covered_work_ids = None;
        }
    }
}
impl GitCommandRequest {
    pub fn normalize(&mut self) {
        if let GitCommand::OpenPullRequest {
            covered_work_ids: Some(ids),
            ..
        } = &mut self.command
        {
            ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum MergeObservation {
    Accepted,
    Queued,
    NotMerged,
    Merged,
    OutcomeUnknown,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryMerge {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_ticket_id: Option<Id>,
    pub evidence: ArtifactRef,
    pub repository_id: Id,
    pub expected_head: GitOid,
    pub observation: MergeObservation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_commit: Option<GitOid>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliverySnapshot {
    pub delivery_id: Id,
    pub revision: Revision,
    pub repositories: Vec<RepositoryMerge>,
    pub feature_complete: bool,
    pub cleanup_pending: bool,
    pub release_pending: bool,
    pub deployment_pending: bool,
}
impl DeliverySnapshot {
    pub fn validate(&self) -> Result<(), &'static str> {
        let all = self
            .repositories
            .iter()
            .all(|r| matches!(r.observation, MergeObservation::Merged) && r.merge_commit.is_some());
        if self.repositories.is_empty() || self.feature_complete && !all {
            return Err("unverified delivery completion");
        }
        let mut ids = HashSet::new();
        for r in &self.repositories {
            r.evidence.validate()?;
            r.expected_head.validate()?;
            if let Some(commit) = &r.merge_commit {
                commit.validate()?;
            }
            if !ids.insert(&r.repository_id.0)
                || matches!(r.observation, MergeObservation::Merged) != r.merge_commit.is_some()
            {
                return Err("invalid merge observation");
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum PublicationAction {
    Prepare,
    Publish,
    Deploy,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReleaseCommandRequest {
    pub protocol: Protocol,
    pub request_key: RequestKey,
    pub action: PublicationAction,
    pub repository_id: Id,
    pub expected_revision: GitOid,
    #[schemars(
        length(min = 1, max = 128),
        regex(pattern = "^[A-Za-z0-9][A-Za-z0-9._+-]{0,127}$")
    )]
    pub version: String,
    pub destination_id: Id,
    pub artifact: ArtifactRef,
    pub expected_policy_revision: Revision,
}
impl ReleaseCommandRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.expected_revision.validate()?;
        self.artifact.validate()?;
        if self.version.is_empty()
            || self.version.len() > 128
            || !self.version.as_bytes()[0].is_ascii_alphanumeric()
            || !self
                .version
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._+-".contains(&c))
        {
            return Err("invalid release version");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NodeView {
    pub node_id: Id,
    pub unit_key: Id,
    pub kind: Id,
    #[schemars(length(max = 1024))]
    pub name: String,
    pub citation: Citation,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EdgeView {
    pub edge_id: Id,
    pub source_id: Id,
    pub target_id: Id,
    pub kind: Id,
    pub producing_unit: Id,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum GraphResult {
    #[schemars(extend("x-until" = 3))]
    Nodes { nodes: Vec<NodeView> },
    #[schemars(extend("x-until" = 3))]
    Graph {
        nodes: Vec<NodeView>,
        edges: Vec<EdgeView>,
    },
    #[schemars(extend("x-until" = 3))]
    Source { citation: Citation, text: String },
    #[schemars(extend("x-until" = 3))]
    Schema { schema: ArtifactRef },
    #[schemars(extend("x-until" = 3))]
    ScopeStatus { coverage: Coverage },
    #[schemars(extend("x-since" = 3))]
    CodeSearch { value: CodeSearchResult },
    #[schemars(extend("x-since" = 3))]
    CodeNodes { value: CodeNodesResult },
    #[schemars(extend("x-since" = 3))]
    CodeGraph { value: CodeGraphResult },
    #[schemars(extend("x-since" = 3))]
    NodeDetail { value: CodeNodeDetail },
    #[schemars(extend("x-since" = 3))]
    SourceText { value: CodeSourceText },
    #[schemars(extend("x-since" = 3))]
    RepositoryOverview { value: CodeOverview },
    #[schemars(extend("x-since" = 3))]
    Architecture { value: CodeArchitecture },
    #[schemars(extend("x-since" = 3))]
    Impact { value: CodeImpact },
    #[schemars(extend("x-since" = 3))]
    CallTrace { value: CodeCallTrace },
    #[schemars(extend("x-since" = 3))]
    Lineage { value: CodeLineage },
    #[schemars(extend("x-since" = 3))]
    Related { value: CodeRelated },
    #[schemars(extend("x-since" = 3))]
    Dependencies { value: CodeDependencies },
    #[schemars(extend("x-since" = 3))]
    SchemaDetails { value: CodeSchema },
    #[schemars(extend("x-since" = 3))]
    ScopeDetails { value: CodeScopeStatus },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(extend("oneOf" = [{"required": ["protocol", "queryId", "view", "completion", "result"], "x-until": 3}, {"required": ["protocol", "queryId", "view", "completion", "result", "scopeCoverage"], "x-since": 3}, {"required": ["protocol", "queryId", "completion", "result", "scopeCoverage"], "not": {"required": ["view"]}, "properties": {"result": {"oneOf": [{"properties": {"kind": {"const": "ScopeDetails"}, "value": {"required": ["unavailableReason"]}}, "required": ["kind", "value"]}, {"properties": {"kind": {"const": "SchemaDetails"}, "value": {"required": ["unavailableReason"], "properties": {"origin": {"const": "Declared"}}}}, "required": ["kind", "value"]}, {"properties": {"kind": {"const": "RepositoryOverview"}, "value": {"required": ["unavailableReason"], "properties": {"graphCoverage": {"properties": {"state": {"const": "Unavailable"}}}, "dependencies": {"maxItems": 0}, "internalModules": {"maxItems": 0}}, "not": {"required": ["graphTotals"]}}}, "required": ["kind", "value"]}]}}, "x-since": 3}]))]
pub struct GraphPage {
    pub protocol: Protocol,
    pub query_id: QueryId,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::support::gated_member"
    )]
    #[schemars(with = "ResolvedViewSet")]
    pub view: Option<ResolvedViewSet>,

    pub completion: GraphCompletion,
    pub result: GraphResult,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::support::gated_member"
    )]
    #[schemars(with="CodeReadCoverage",extend("x-since"=3))]
    pub scope_coverage: Option<CodeReadCoverage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}
impl GraphPage {
    pub fn validate(&self) -> Result<(), &'static str> {
        if let Some(view) = &self.view {
            view.validate()?;
        } else {
            let metadata = match &self.result {
                GraphResult::ScopeDetails { value } => value.unavailable_reason.is_some(),
                GraphResult::SchemaDetails { value } => {
                    matches!(value.origin, CodeSchemaOrigin::Declared)
                        && value.unavailable_reason.is_some()
                }
                GraphResult::RepositoryOverview { value } => {
                    matches!(value.graph_coverage.state, CoverageState::Unavailable)
                        && value.unavailable_reason.is_some()
                        && value.dependencies.0.is_empty()
                        && value.internal_modules.0.is_empty()
                        && value.graph_totals.is_none()
                }
                _ => false,
            };
            if !metadata {
                return Err("view-less graph response requires explicit unavailable metadata");
            }
        }
        self.validate_current()?;
        let Some(view) = &self.view else {
            return Ok(());
        };
        match &self.result {
            GraphResult::Nodes { nodes } | GraphResult::Graph { nodes, .. } => {
                if nodes.len() > 500 {
                    return Err("too many nodes");
                }
                let mut ids = HashSet::new();
                for n in nodes {
                    n.citation.validate()?;
                    match &n.citation.source {
                        CitationSource::Repository { snapshot, .. } => {
                            if !view.views.iter().any(|v| {
                                serde_json::to_value(&v.snapshot).unwrap()
                                    == serde_json::to_value(snapshot).unwrap()
                            }) {
                                return Err("graph citation outside resolved view");
                            }
                        }
                        CitationSource::DatabaseSchema {
                            connection_id,
                            snapshot_id,
                            schema_digest,
                        } => {
                            if !view.schema_views.iter().any(|v| {
                                &v.connection_id == connection_id
                                    && &v.snapshot_id == snapshot_id
                                    && &v.schema_digest == schema_digest
                            }) {
                                return Err("schema citation outside resolved view");
                            }
                        }
                        _ => return Err("graph node source not in graph view"),
                    }
                    if !ids.insert(&n.node_id.0) {
                        return Err("duplicate graph node");
                    }
                }
                if let GraphResult::Graph { edges, .. } = &self.result {
                    if edges.len() > 500 {
                        return Err("too many edges");
                    }
                    for e in edges {
                        if !ids.contains(&e.source_id.0) || !ids.contains(&e.target_id.0) {
                            return Err("edge endpoint not in returned graph");
                        }
                    }
                }
            }
            GraphResult::Source { citation, text } => {
                citation.validate()?;
                match &citation.source {
                    CitationSource::Repository { snapshot, .. }
                        if view.views.iter().any(|v| {
                            serde_json::to_value(&v.snapshot).unwrap()
                                == serde_json::to_value(snapshot).unwrap()
                        }) => {}
                    CitationSource::DatabaseSchema {
                        connection_id,
                        snapshot_id,
                        schema_digest,
                    } if view.schema_views.iter().any(|v| {
                        &v.connection_id == connection_id
                            && &v.snapshot_id == snapshot_id
                            && &v.schema_digest == schema_digest
                    }) => {}
                    _ => return Err("source citation outside resolved view"),
                }
                if text.len() > 262144 {
                    return Err("source result bounds");
                }
            }
            _ => {}
        }
        Ok(())
    }
}
impl Revisioned for GraphReadRequest {
    fn retain_revision(&mut self, revision: u32) {
        if revision >= 3 {
            return;
        }
        self.current = None;
        match &mut self.query {
            GraphQuery::Search { selection, .. } => *selection = None,
            GraphQuery::Overview { repository_id } => *repository_id = None,
            GraphQuery::Source { selection, .. } => *selection = None,
            GraphQuery::Path { selection, .. } => *selection = None,
            GraphQuery::NodeImpact { selection, .. } => *selection = None,
            GraphQuery::Consumers { selection, .. }
            | GraphQuery::Publishers { selection, .. }
            | GraphQuery::DataLineage { selection, .. } => *selection = None,
            GraphQuery::FileImpact { selection, .. }
            | GraphQuery::NodesByFile { selection, .. } => *selection = None,
            _ => {}
        }
    }
}
impl GraphPage {
    fn validate_current(&self) -> Result<(), &'static str> {
        let current = !matches!(
            &self.result,
            GraphResult::Nodes { .. }
                | GraphResult::Graph { .. }
                | GraphResult::Source { .. }
                | GraphResult::Schema { .. }
                | GraphResult::ScopeStatus { .. }
        );
        if current {
            let coverage = self
                .scope_coverage
                .as_ref()
                .ok_or("current graph result requires explicit scope coverage")?;
            coverage
                .check()
                .map_err(|_| "invalid graph scope coverage")?;
            if matches!(self.completion, GraphCompletion::Partial)
                && !matches!(coverage.coverage.state, CoverageState::Partial)
            {
                return Err("partial completion requires actual partial scope coverage");
            }
            if matches!(coverage.coverage.state, CoverageState::Partial)
                && matches!(self.completion, GraphCompletion::Complete)
            {
                return Err("incomplete graph scope cannot claim complete results");
            }
        }
        let checked = match &self.result {
            GraphResult::CodeSearch { value } => value.check(),
            GraphResult::CodeNodes { value } => value.check(),
            GraphResult::CodeGraph { value } => value.check(),
            GraphResult::NodeDetail { value } => value.check(),
            GraphResult::SourceText { value } => value.check(),
            GraphResult::RepositoryOverview { value } => value.check(),
            GraphResult::Architecture { value } => value.check(),
            GraphResult::Impact { value } => value.check(),
            GraphResult::CallTrace { value } => value.check(),
            GraphResult::Lineage { value } => value.check(),
            GraphResult::Related { value } => value.check(),
            GraphResult::Dependencies { value } => value.check(),
            GraphResult::SchemaDetails { value } => value.check(),
            GraphResult::ScopeDetails { value } => value.check(),
            _ => Ok(()),
        };
        checked.map_err(|_| "invalid typed graph result")?;
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        match &self.result {
            GraphResult::CodeSearch { value } => {
                for hit in &value.items.0 {
                    if let CodeSearchHit::Node { node } = hit {
                        nodes.push(node);
                    }
                }
            }
            GraphResult::CodeNodes { value } => nodes.extend(value.nodes.0.iter()),
            GraphResult::CodeGraph { value } => {
                nodes.extend(value.nodes.0.iter());
                edges.extend(value.edges.0.iter());
            }
            GraphResult::NodeDetail { value } => {
                nodes.push(&value.node);
                nodes.extend(value.neighbors.0.iter());
                edges.extend(value.incoming.0.iter());
                edges.extend(value.outgoing.0.iter());
                edges.extend(value.cross_repository.0.iter());
            }
            GraphResult::Impact { value } => {
                nodes.extend(value.changed.0.iter().map(|n| &n.node));
                nodes.extend(value.affected.0.iter().map(|n| &n.node));
            }
            GraphResult::CallTrace { value } => {
                nodes.extend(value.start.iter());
                nodes.extend(value.entries.0.iter().map(|e| &e.node));
            }
            GraphResult::Lineage { value } => {
                nodes.extend(value.model.iter());
                nodes.extend(value.producers.0.iter().map(|e| &e.node));
                nodes.extend(value.consumers.0.iter().map(|e| &e.node));
                nodes.extend(value.related_nodes.0.iter());
                edges.extend(value.cross_repository.0.iter());
            }
            GraphResult::Related { value } => nodes.extend(value.nodes.0.iter().map(|n| &n.node)),
            GraphResult::SourceText { value } => self.current_source(&value.source)?,
            _ => {}
        }
        match &self.result {
            GraphResult::Architecture { value } => {
                self.validate_modules(&value.internal_modules.0)?
            }
            GraphResult::RepositoryOverview { value } => {
                self.validate_modules(&value.internal_modules.0)?
            }
            _ => {}
        }
        let mut identities = std::collections::HashMap::new();
        for node in nodes {
            let Some(view) = &self.view else {
                return Err("node without resolved graph view");
            };
            if !view.views.iter().any(|v| {
                v.view_id == node.view_id && v.snapshot.repository_id == node.repository_id
            }) {
                return Err("node outside resolved graph view");
            }
            if let Some(source) = &node.source {
                self.current_source(source)?;
                if source.snapshot.repository_id != node.repository_id
                    || source.unit_key.0 != node.unit_key.0
                {
                    return Err("node source identity mismatch");
                }
            }
            identities.insert(
                (&node.repository_id.0, &node.unit_key.0, &node.node_id.0),
                node,
            );
        }
        for edge in edges {
            if !identities.contains_key(&(
                &edge.source_repository_id.0,
                &edge.source_unit_key.0,
                &edge.source_id.0,
            )) || !identities.contains_key(&(
                &edge.target_repository_id.0,
                &edge.target_unit_key.0,
                &edge.target_id.0,
            )) {
                return Err("edge endpoint outside returned scoped graph");
            }
        }
        if self
            .next_cursor
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > 4096)
        {
            return Err("invalid graph continuation");
        }
        Ok(())
    }
    fn validate_modules(&self, modules: &[CodeModuleAnalysis]) -> Result<(), &'static str> {
        for module in modules {
            let Some(view) = &self.view else {
                return Err("module facts require actual scoped views");
            };
            if !view
                .views
                .iter()
                .any(|v| v.snapshot.repository_id == module.module.repository_id)
            {
                return Err("module outside graph scope");
            }
            let refs = module
                .module
                .indexed_node
                .iter()
                .chain(module.endpoints.nodes.0.iter())
                .chain(module.services.nodes.0.iter())
                .chain(module.tables.nodes.0.iter());
            for node in refs {
                if node.repository_id != module.module.repository_id
                    || !view.views.iter().any(|v| {
                        v.view_id == node.view_id && v.snapshot.repository_id == node.repository_id
                    })
                {
                    return Err("module drilldown outside graph scope");
                }
            }
        }
        Ok(())
    }
    fn current_source(&self, source: &CodeSourceLocation) -> Result<(), &'static str> {
        source.snapshot.validate()?;
        let Some(view) = &self.view else {
            return Err("source without resolved graph view");
        };
        if !view.views.iter().any(|v| {
            serde_json::to_value(&v.snapshot).unwrap()
                == serde_json::to_value(&source.snapshot).unwrap()
        }) {
            return Err("source outside resolved graph view");
        }
        if let Some(citation) = &source.legacy_citation {
            citation.validate()?;
            if !matches!(&citation.source,CitationSource::Repository{snapshot,unit_key,source_digest} if serde_json::to_value(snapshot).unwrap()==serde_json::to_value(&source.snapshot).unwrap()&&unit_key.0==source.unit_key.0&&source_digest==&source.source_digest)
            {
                return Err("legacy source witness mismatch");
            }
        }
        Ok(())
    }
}
fn legacy_node(node: &CodeNode) -> Option<NodeView> {
    Some(NodeView {
        node_id: node.node_id.clone(),
        unit_key: serde_json::from_value(serde_json::Value::String(node.unit_key.0.clone()))
            .ok()?,
        kind: serde_json::from_value(serde_json::Value::String(node.node_kind.0.clone())).ok()?,
        name: node.name.0.clone(),
        citation: node.source.as_ref()?.legacy_citation.clone()?,
    })
}
fn legacy_nodes<'a>(nodes: impl IntoIterator<Item = &'a CodeNode>) -> Option<Vec<NodeView>> {
    let nodes: Vec<_> = nodes.into_iter().map(legacy_node).collect::<Option<_>>()?;
    let mut ids = HashSet::new();
    if nodes
        .iter()
        .all(|n| ids.insert(n.node_id.0.clone()) && n.name.len() <= 1024)
    {
        Some(nodes)
    } else {
        None
    }
}
impl Revisioned for GraphPage {
    fn retain_revision(&mut self, revision: u32) {
        if revision >= 3 || self.view.is_none() {
            return;
        }
        if matches!(self.completion, GraphCompletion::Partial)
            || self
                .scope_coverage
                .as_ref()
                .is_some_and(|v| !matches!(v.coverage.state, CoverageState::Complete))
        {
            return;
        }
        self.scope_coverage = None;
        let projected = match &self.result {
            GraphResult::CodeNodes { value } => {
                legacy_nodes(&value.nodes.0).map(|nodes| GraphResult::Nodes { nodes })
            }
            GraphResult::CodeSearch { value } => {
                let nodes: Option<Vec<_>> = value
                    .items
                    .0
                    .iter()
                    .map(|hit| match hit {
                        CodeSearchHit::Node { node } => Some(node),
                        _ => None,
                    })
                    .collect();
                nodes
                    .and_then(legacy_nodes)
                    .map(|nodes| GraphResult::Nodes { nodes })
            }
            GraphResult::CodeGraph { value } => legacy_nodes(&value.nodes.0).and_then(|nodes| {
                let edges: Option<Vec<_>> = value
                    .edges
                    .0
                    .iter()
                    .map(|e| {
                        Some(EdgeView {
                            edge_id: e.edge_id.clone(),
                            source_id: e.source_id.clone(),
                            target_id: e.target_id.clone(),
                            kind: serde_json::from_value(serde_json::Value::String(
                                e.edge_kind.0.clone(),
                            ))
                            .ok()?,
                            producing_unit: serde_json::from_value(serde_json::Value::String(
                                e.producing_unit.as_ref()?.0.clone(),
                            ))
                            .ok()?,
                        })
                    })
                    .collect();
                edges.map(|edges| GraphResult::Graph { nodes, edges })
            }),
            GraphResult::SourceText { value } => {
                value
                    .source
                    .legacy_citation
                    .clone()
                    .map(|citation| GraphResult::Source {
                        citation,
                        text: value.text.0.clone(),
                    })
            }
            GraphResult::SchemaDetails { value } => value
                .artifact
                .clone()
                .map(|schema| GraphResult::Schema { schema }),
            GraphResult::ScopeDetails { value } if self.view.is_some() => {
                Some(GraphResult::ScopeStatus {
                    coverage: value.coverage.clone(),
                })
            }
            _ => None,
        };
        if let Some(projected) = projected {
            self.result = projected;
        }
    }
    fn check_revision(&self, revision: u32) -> Result<(), String> {
        if revision < 3 && matches!(self.completion, GraphCompletion::Partial) {
            return Err("legacy graph completion cannot represent missing-index scope".into());
        }
        if revision >= 3 && self.scope_coverage.is_none() {
            return Err("qualified graph output requires scope coverage".into());
        }

        if revision < 3 && self.view.is_none() {
            return Err("legacy graph results require a real resolved view".into());
        }

        let current = !matches!(
            self.result,
            GraphResult::Nodes { .. }
                | GraphResult::Graph { .. }
                | GraphResult::Source { .. }
                | GraphResult::Schema { .. }
                | GraphResult::ScopeStatus { .. }
        );
        if revision < 3 && current {
            return Err("graph result cannot be represented at the negotiated revision".into());
        }
        if revision >= 3 && !current {
            return Err("revision 3 requires the qualified graph result".into());
        }
        Ok(())
    }
}

/// Exactly one owner scope: a project, or (schema revision 3) the workspace spelled `true`.
fn one_scope(project: &Option<ProjectId>, workspace: &Option<bool>) -> Result<(), &'static str> {
    match (project, workspace) {
        (Some(_), None) | (None, Some(true)) => Ok(()),
        _ => Err("an artifact scope is exactly one of a project and the workspace"),
    }
}
impl ArtifactCommandRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if let ArtifactCommand::Save {
            project_id,
            workspace,
            ..
        } = &self.command
        {
            one_scope(project_id, workspace)?;
        }
        if let ArtifactCommand::Save {
            title, media_type, ..
        } = &self.command
            && (title.is_empty()
                || title.len() > 1024
                || media_type.is_empty()
                || media_type.len() > 128)
        {
            return Err("artifact metadata bounds");
        }
        Ok(())
    }
    /// Tags are a set: an empty set is no tags (decoding already sorted them).
    pub fn normalize(&mut self) {
        if let ArtifactCommand::Save { tags, .. } = &mut self.command {
            normalize_tags(tags);
        }
    }
}
impl ArtifactQueryRequest {
    /// The tag filter is a set: an empty filter is no filter (decoding already sorted it).
    pub fn normalize(&mut self) {
        if let ArtifactQuery::List { tags, .. } = &mut self.query {
            normalize_tags(tags);
        }
    }
}
impl super::support::Revisioned for ArtifactCommandRequest {
    fn retain_revision(&mut self, revision: u32) {
        if let ArtifactCommand::Save {
            tags, workspace, ..
        } = &mut self.command
        {
            if revision < 2 {
                *tags = None;
            }
            if revision < 3 {
                *workspace = None;
            }
        }
    }
}
impl super::support::Revisioned for ArtifactQueryRequest {
    fn retain_revision(&mut self, revision: u32) {
        if let ArtifactQuery::List {
            tags, workspace, ..
        } = &mut self.query
        {
            if revision < 2 {
                *tags = None;
            }
            if revision < 3 {
                *workspace = None;
            }
        }
    }
}
impl super::support::Check for ArtifactTags {
    fn check(&self) -> Result<(), String> {
        Self::new(self.0.clone()).map(|_| ()).map_err(str::to_owned)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ArtifactDeletion {
    DeleteVersion {
        artifact_id: Id,
        version_id: Id,
        expected_revision: Revision,
        expected_reference_revision: Revision,
    },
    DeleteArtifact {
        artifact_id: Id,
        expected_revision: Revision,
        expected_reference_revision: Revision,
        expected_versions_digest: Digest,
    },
    DetachProject {
        artifact_id: Id,
        project_id: ProjectId,
        expected_reference_revision: Revision,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactDeletionRequest {
    pub protocol: Protocol,
    pub request_key: RequestKey,
    pub deletion: ArtifactDeletion,
}
