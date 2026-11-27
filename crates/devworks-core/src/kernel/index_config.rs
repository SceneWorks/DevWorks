//! Stored indexing section configuration and shared defaults.
use crate::ports::storage::{IndexRead, StorageError, StoredSourceConfig, WORKSPACE_SCOPE_KEY};
use crate::protocol::ProjectId;

/// The retained default file size bound (CodeGraph `MaxFileSizeKb = 512`).
pub const DEFAULT_MAX_FILE_BYTES: i64 = 512 * 1024;
/// The retained default skip patterns (CodeGraph `IndexingOptions.SkipPatterns`).
pub const DEFAULT_EXCLUDES: [&str; 15] = [
    "**/bin/**",
    "**/obj/**",
    "**/node_modules/**",
    "**/build/**",
    "**/managed_components/**",
    "**/.cache/**",
    "**/wwwroot/lib/**",
    "**/*.min.js",
    "**/.git/**",
    "**/TestResults/**",
    "**/.claude/worktrees/**",
    "**/.build/**",
    "**/.swiftpm/**",
    "**/DerivedData/**",
    "**/SourcePackages/**",
];
/// The defaults of a section never written.
pub fn default_config(scope_key: &str) -> StoredSourceConfig {
    StoredSourceConfig {
        scope_key: scope_key.to_owned(),
        project_id: (scope_key != WORKSPACE_SCOPE_KEY).then(|| {
            crate::protocol::ProjectId(crate::protocol::Id::new(scope_key).expect("project key"))
        }),
        revision: 1,
        structural_languages: Vec::new(),
        semantic_packages: Vec::new(),
        exclude_patterns: DEFAULT_EXCLUDES.iter().map(|s| (*s).to_owned()).collect(),
        max_file_bytes: DEFAULT_MAX_FILE_BYTES,
        updated_at_ms: 0,
    }
}

/// The section key of a scope.
pub fn section_key(scope: Option<&ProjectId>) -> String {
    scope.map_or_else(
        || WORKSPACE_SCOPE_KEY.to_owned(),
        |p| p.0.as_str().to_owned(),
    )
}

/// A section's stored configuration or its defaults.
pub fn section_config(read: &dyn IndexRead, key: &str) -> Result<StoredSourceConfig, StorageError> {
    Ok(read
        .source_config(key)?
        .unwrap_or_else(|| default_config(key)))
}
