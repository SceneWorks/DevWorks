//! Architecture candidates for graph/retrieval, immutable artifacts and delivery; no services.
use crate::provider::{ArtifactRef, Digest};
use crate::*;
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
        if let Some(b) = &self.branch {
            if b.parent_generation_id.as_ref() != Some(&self.baseline.generation_id) {
                return Err("branch parent mismatch");
            }
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
    Search {
        #[schemars(length(min = 1, max = 8192))]
        text: String,
        #[schemars(length(max = 64))]
        node_kinds: Vec<Id>,
    },
    Overview,
    Schema,
    Node {
        node_id: Id,
    },
    Source {
        node_id: Id,
        #[schemars(range(min = 1, max = 2000))]
        max_lines: u16,
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
    Path {
        from_id: Id,
        to_id: Id,
        #[schemars(range(min = 1, max = 128))]
        max_depth: u16,
    },
    NodeImpact {
        node_id: Id,
    },
    FileImpact {
        repository_id: Id,
        unit_key: Id,
    },
    NodesByFile {
        repository_id: Id,
        unit_key: Id,
    },
    RepositoryDependencies {
        repository_id: Id,
    },
    Consumers {
        node_id: Id,
    },
    Publishers {
        node_id: Id,
    },
    DataLineage {
        node_id: Id,
    },
    ScopeStatus,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GraphReadRequest {
    pub protocol: Protocol,
    pub query_id: QueryId,
    pub scope: DataScope,
    pub view: ViewSelection,
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
        match &self.view {
            ViewSelection::SchemaPublished { connection_ids } => {
                unique_ids(connection_ids, 1, 256)?
            }
            ViewSelection::Published { repository_ids } => unique_ids(repository_ids, 1, 256)?,
            ViewSelection::Exact { view_ids } => unique_ids(view_ids, 1, 256)?,
            _ => {}
        }
        match &self.query {
            GraphQuery::Search { text, node_kinds } => {
                if text.is_empty() || text.len() > 8192 || node_kinds.len() > 64 {
                    return Err("invalid search bounds");
                }
            }
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
            GraphQuery::Source { max_lines, .. } if *max_lines == 0 || *max_lines > 2000 => {
                return Err("invalid source limit");
            }
            GraphQuery::Path { max_depth, .. } if *max_depth == 0 || *max_depth > 128 => {
                return Err("invalid path depth");
            }
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
            RetrievalInput::Vector { space, float32_le } => {
                if space.dimensions == 0
                    || space.dimensions > 65536
                    || float32_le.byte_length.0.parse::<u64>().unwrap()
                        != u64::from(space.dimensions) * 4
                {
                    return Err("vector shape mismatch");
                }
            }
            _ => {}
        }
        if let DocumentVersions::Exact { versions } = &self.versions {
            if versions.is_empty() || versions.len() > 128 {
                return Err("invalid version selection");
            }
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
    RefreshOverlay {
        repository_id: Id,
        worktree_id: Id,
        expected_source_revision: Revision,
    },
    Cancel {
        operation_id: OperationId,
    },
    SetTrust {
        repository_id: Id,
        node_id: Id,
        expected_revision: Revision,
        view_id: Id,
        expected_content_digest: Digest,
        trusted: bool,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IndexCommandRequest {
    pub protocol: Protocol,
    pub request_key: RequestKey,
    pub command: IndexCommand,
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
/// A set of opaque artifact labels (schema revision 2): at most 32, each 1-64 characters without
/// NUL, no duplicates. Decoding sorts it, so equal sets normalize identically.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ArtifactTags(pub Vec<String>);
impl ArtifactTags {
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
            deserialize_with = "crate::domain_support::gated_member"
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
            deserialize_with = "crate::domain_support::gated_member"
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
            deserialize_with = "crate::domain_support::gated_member"
        )]
        #[schemars(with = "bool", extend("x-since" = 3, "const" = true))]
        workspace: Option<bool>,
        /// Only artifacts carrying every listed tag (schema revision 2).
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "crate::domain_support::gated_member"
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
                ..
            } => {
                expected_head.validate()?;
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
    Nodes {
        nodes: Vec<NodeView>,
    },
    Graph {
        nodes: Vec<NodeView>,
        edges: Vec<EdgeView>,
    },
    Source {
        citation: Citation,
        text: String,
    },
    Schema {
        schema: ArtifactRef,
    },
    ScopeStatus {
        coverage: Coverage,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GraphPage {
    pub protocol: Protocol,
    pub query_id: QueryId,
    pub view: ResolvedViewSet,
    pub completion: QueryCompletion,
    pub result: GraphResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}
impl GraphPage {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.view.validate()?;
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
                            if !self.view.views.iter().any(|v| {
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
                            if !self.view.schema_views.iter().any(|v| {
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
                        if self.view.views.iter().any(|v| {
                            serde_json::to_value(&v.snapshot).unwrap()
                                == serde_json::to_value(snapshot).unwrap()
                        }) => {}
                    CitationSource::DatabaseSchema {
                        connection_id,
                        snapshot_id,
                        schema_digest,
                    } if self.view.schema_views.iter().any(|v| {
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
        {
            if title.is_empty()
                || title.len() > 1024
                || media_type.is_empty()
                || media_type.len() > 128
            {
                return Err("artifact metadata bounds");
            }
        }
        Ok(())
    }
}
impl crate::domain_support::Revisioned for ArtifactCommandRequest {
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
impl crate::domain_support::Revisioned for ArtifactQueryRequest {
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
impl crate::domain_support::Check for ArtifactTags {
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
