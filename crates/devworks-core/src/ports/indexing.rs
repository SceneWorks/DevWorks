//! Indexing ports (DW-023): read-only access to repository source trees, structural parsing and
//! semantic enrichment. `devworks-adapters::index` implements the source tree (native,
//! no-follow, bounded) and the Tree-sitter parser; semantic language packs (items 24–27)
//! implement [`SemanticEnricher`]. The indexing service in `crate::services::indexing` owns
//! every decision about what is read, extracted, staged and published.
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------------------------
// Source trees

/// What a directory entry is, observed without following it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    File,
    /// A symbolic link, junction or reparse point: never followed by discovery or capture.
    Link,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub kind: EntryKind,
}

/// How a directory marks a Git checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitMarker {
    /// `.git` is a directory: a primary checkout.
    Directory,
    /// `.git` is a file (`gitdir:` pointer): a linked worktree or submodule.
    File(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceFault {
    /// The path does not exist (or vanished).
    Missing,
    /// The path leaves its root, crosses a link or is not a regular file/directory.
    Refused(String),
    /// Larger than the read bound; `size` is the observed length.
    TooLarge {
        size: u64,
    },
    Io(String),
}
impl std::fmt::Display for SourceFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("missing"),
            Self::Refused(why) => write!(f, "refused: {why}"),
            Self::TooLarge { size } => write!(f, "too large ({size} bytes)"),
            Self::Io(e) => write!(f, "I/O: {e}"),
        }
    }
}

/// Read-only, no-follow access to local source trees.
pub trait SourceTree: Send + Sync {
    /// The canonical absolute form of an existing directory (links resolved once, here only:
    /// discovery records exactly this location).
    fn canonical_dir(&self, path: &Path) -> Result<PathBuf, SourceFault>;
    /// At most `limit` entries of `dir`, sorted by name, without following links.
    fn list_dir(&self, dir: &Path, limit: usize) -> Result<Vec<DirEntry>, SourceFault>;
    /// Whether `dir` holds a `.git` directory or file (not followed).
    fn git_marker(&self, dir: &Path) -> Result<Option<GitMarker>, SourceFault>;
    /// Reads `relative` (a `/`-separated path without `..`) under `root`, refusing links at
    /// every component, anything but a regular file, and more than `max_bytes`.
    fn read_file(
        &self,
        root: &Path,
        relative: &str,
        max_bytes: u64,
    ) -> Result<Vec<u8>, SourceFault>;
    /// Whether filenames under `root` compare case-insensitively (macOS and Windows defaults).
    fn case_insensitive(&self, root: &Path) -> bool;
    /// The user's home directory (`~` in a configured base directory).
    fn home(&self) -> Option<PathBuf>;
    /// Whether the regular file `relative` under `root` carries an execute permission (links
    /// are not followed). A port that cannot tell answers `false`.
    fn executable(&self, _root: &Path, _relative: &str) -> Result<bool, SourceFault> {
        Ok(false)
    }
}

// ---------------------------------------------------------------------------------------------
// Structural parsing

/// How a language is covered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LanguageMode {
    /// A Tree-sitter grammar extracts definitions, imports and relationships.
    Structural,
    /// No grammar is available: files are inventoried and indexed as text only.
    TextFallback,
}

/// One language the parser knows, with the honest extent of its coverage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanguageInfo {
    /// Lowercase ID (`rust`, `typescript`, `coldfusion`, ...).
    pub id: &'static str,
    pub name: &'static str,
    /// Lowercase extensions with the dot (`.rs`) and exact file names (`Dockerfile`).
    pub extensions: &'static [&'static str],
    pub mode: LanguageMode,
    /// The grammar crate and version (`tree-sitter-rust 0.24.2`), when structural.
    pub grammar: Option<&'static str>,
    /// What the extractor supports (`definitions`, `imports`, `calls`, `inheritance`, ...).
    pub capabilities: &'static [&'static str],
    /// Why a language is text-only or partial.
    pub note: Option<&'static str>,
}

/// A source span: 1-based lines and columns (columns count UTF-8 bytes), plus byte offsets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub start_byte: u32,
    pub end_byte: u32,
}

/// How a relationship's target was established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// The target is a definition in the same unit, established syntactically.
    Resolved,
    /// The target was chosen by name (a unique candidate), not by compiler semantics.
    Inferred,
    /// No unique target: the name is recorded as a symbol.
    Unresolved,
}
impl Resolution {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resolved => "Resolved",
            Self::Inferred => "Inferred",
            Self::Unresolved => "Unresolved",
        }
    }
}

/// A definition (or the file itself, index 0) found in one file.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtractedNode {
    /// CodeGraph node label (`File`, `Class`, `Interface`, `Struct`, `Enum`, `Function`,
    /// `Method`, `Property`, `Module`, `Namespace`, `TerraformResource`, ...).
    pub kind: String,
    pub name: String,
    /// The name qualified by its containers within the file (`Outer.Inner.method`).
    pub qualified_name: String,
    pub span: Span,
    /// Index of the containing node (`None` only for the file node).
    pub parent: Option<usize>,
    /// Raw syntactic facts (`return_type`, `parameters`, `superclass`, `abi`, ...).
    pub properties: serde_json::Map<String, serde_json::Value>,
}

/// Where an extracted relationship points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgeTarget {
    /// A node of the same file.
    Local(usize),
    /// A name for the indexer to resolve across the repository (or leave unresolved).
    Symbol(String),
    /// A node another unit of the same extraction group defines (a repository-level pass: a
    /// path dependency naming another manifest's package). Its ID is derived exactly.
    Node {
        unit: String,
        kind: String,
        qualified_name: String,
    },
    /// An exact captured semantic reference to a different registered repository. The reader
    /// may resolve it only inside its authorized selected view set; it never selects a target
    /// repository's current branch implicitly. This does not infer cross-repository symbols.
    RepositoryNode {
        repository_id: crate::protocol::Id,
        unit: String,
        kind: String,
        qualified_name: String,
        /// The actual captured target name, when the semantic producer knows it.
        target_name: Option<String>,
    },
}

/// A relationship found in one file.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtractedEdge {
    /// CodeGraph edge type (`DEFINES`, `DEFINES_METHOD`, `IMPORTS`, `CALLS`, `INHERITS`,
    /// `IMPLEMENTS`, `REFERENCES_PACKAGE`, ...).
    pub kind: String,
    pub source: usize,
    pub target: EdgeTarget,
    pub resolution: Resolution,
    /// Confidence in 0..=1 (the retained call-site values: 0.7 bare, 0.65 resolved receiver,
    /// 0.45 unresolved receiver; 1.0 syntactic containment).
    pub confidence: f32,
    pub properties: serde_json::Map<String, serde_json::Value>,
}

/// What one file contributed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileExtract {
    pub nodes: Vec<ExtractedNode>,
    pub edges: Vec<ExtractedEdge>,
    /// Syntax errors the grammar recovered from (the extraction is then partial).
    pub parse_errors: u32,
    /// Passes over this file that could not complete (`backend-endpoints: ...`): the unit is
    /// recorded, but its coverage is partial.
    pub incomplete: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtractFault {
    /// The stop signal was raised (run cancellation).
    Cancelled,
    /// The per-file deadline passed.
    TimedOut,
    /// The grammar could not be loaded or the source is not text.
    Failed(String),
}

/// Structural parsing (Tree-sitter in production).
pub trait StructuralParser: Send + Sync {
    /// Every language the build knows, structural or text-only.
    fn languages(&self) -> &[LanguageInfo];
    /// The language of a repository-relative path, by exact file name then extension.
    fn language_of(&self, path: &str) -> Option<&LanguageInfo>;
    /// Extracts one file of `repository` (its registration name, which a few retained
    /// identities carry: a standalone Ansible role is named after its repository). `stop` is
    /// polled during the parse (cooperative cancellation within a bounded interval) and
    /// `budget_ms` bounds the parse time.
    fn extract(
        &self,
        language: &str,
        path: &str,
        repository: &str,
        source: &[u8],
        stop: &(dyn Fn() -> bool + Sync),
        budget_ms: u64,
    ) -> Result<FileExtract, ExtractFault>;

    /// The repository-level extraction group a path belongs to (`cargo`, `swiftpm`), whatever
    /// its language selection: a group's members are extracted together because their output
    /// depends on one another (workspace inheritance, path dependencies, resolved pins).
    fn group_of(&self, _path: &str) -> Option<&'static str> {
        None
    }

    /// Extracts one group from every member's bytes (`files`: repository-relative path and
    /// content). Returns each member's share: a [`FileExtract`] whose node 0 stands for the
    /// member's file node (it is not stored again) and whose other nodes and edges are added to
    /// the member's unit. A member the group cannot read is reported in its share's
    /// `incomplete`.
    fn extract_group(
        &self,
        _group: &str,
        _repository: &str,
        _files: &[(String, Vec<u8>)],
        _stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<std::collections::BTreeMap<String, FileExtract>, ExtractFault> {
        Ok(std::collections::BTreeMap::new())
    }
}

// ---------------------------------------------------------------------------------------------
// Semantic enrichment (items 24–27)

/// What a semantic pack declares (CODE-04).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackDescriptor {
    pub id: String,
    pub name: String,
    /// Structural language IDs it enriches.
    pub languages: Vec<String>,
    pub version: String,
    /// Toolchain prerequisites it found missing (empty: ready).
    pub missing: Vec<String>,
}

/// One semantic contribution for a unit, built against the exact source snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct Enrichment {
    pub unit_key: String,
    pub nodes: Vec<ExtractedNode>,
    pub edges: Vec<ExtractedEdge>,
}

/// The snapshot an enricher is asked about.
#[derive(Clone, Debug)]
pub struct EnrichmentRequest<'a> {
    pub repository_id: &'a str,
    /// The checkout the pack may read (never written).
    pub root: &'a Path,
    /// The exact commit, and the capture digest for a worktree layer.
    pub commit: &'a str,
    pub capture_digest: Option<&'a str>,
    /// The units of this generation the pack may contribute to.
    pub units: &'a [String],
}

/// A semantic language pack (SCIP or a native compiler index adapted into the shared graph).
/// Its contributions are validated and published by the indexing service into the same
/// generation, with the pack's provenance; a pack never writes graph tables or declares an
/// incomplete index current.
pub trait SemanticEnricher: Send + Sync {
    fn descriptor(&self) -> PackDescriptor;
    /// Contributions for `request`, or why none were produced. `stop` is the run's
    /// cancellation; helper processes must be launched through the process owner.
    fn enrich(
        &self,
        request: &EnrichmentRequest<'_>,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<Enrichment>, String>;
}
