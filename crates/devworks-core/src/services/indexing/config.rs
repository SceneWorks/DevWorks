//! Index source configuration (`indexing.configure_sources`): per-section languages, semantic
//! packs, exclusions and the file size bound, the effective configuration of a repository, and
//! the exclusion matcher.
//!
//! # Sections
//!
//! A section is the workspace or one project. Discovery scans each section's repository base
//! directory (`repositories.baseDirectory`, SET-07/08: the app-wide default and each project's
//! override). Each section also carries an index configuration. A registration is shared, so
//! its effective configuration combines the workspace section with every project it belongs to,
//! deterministically: exclusions are the union, the structural language selection is the
//! intersection of the non-empty selections (empty means every supported language), semantic
//! packs are the union and the size bound is the smallest. The digest of the effective
//! configuration is recorded with every generation; a changed digest shows the index stale.
//!
//! # Exclusions
//!
//! Matching is case-insensitive, as the retained CodeGraph globbing matcher was.
//!
//! * `path:<glob>` or a plain `<glob>`: a repository-relative path. `**` matches any number of
//!   segments, `*` and `?` match within one segment, `[...]` is a character class. A pattern
//!   without `/` matches any single path segment (`node_modules` excludes every such
//!   directory).
//! * `repository:<glob>`: a repository name; discovery does not register it and indexing
//!   refuses it (the retained `complete` repository exclusion).
//! * `group:<name>`: any directory segment between a section's base directory and the
//!   repository (the retained group exclusion).
//!
//! The retained default skip patterns apply until a section replaces them.
use crate::fingerprint::sha256_hex;
use crate::ports::storage::{StoredSourceConfig, WORKSPACE_SCOPE_KEY};
use serde_json::json;

/// The retained default file size bound (CodeGraph `MaxFileSizeKb = 512`).
pub const DEFAULT_MAX_FILE_BYTES: i64 = 512 * 1024;
/// The retained Swift exception (`MaxFileSizeKbByExtension[".swift"] = 2048`), applied while
/// the section keeps the default bound.
pub const SWIFT_MAX_FILE_BYTES: i64 = 2048 * 1024;
/// The largest bound a section may set (a file is read into memory to parse it).
pub const MAX_FILE_BYTES_LIMIT: i64 = 64 * 1024 * 1024;
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
/// Dependency directories excluded only beside their manifest (CodeGraph's conditional skips).
pub const MANIFEST_DIRECTORIES: [(&str, &[&str]); 3] = [
    ("target", &["Cargo.toml"]),
    ("Pods", &["Podfile"]),
    ("Carthage", &["Cartfile", "Cartfile.resolved"]),
];
/// Bump when extraction output changes for unchanged input: every unit is re-extracted.
pub const EXTRACTOR_VERSION: i64 = 1;

/// One parsed exclusion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exclusion {
    Path(Glob),
    Repository(Glob),
    Group(String),
}

/// Parses and checks one exclusion pattern.
pub fn parse_exclusion(raw: &str) -> Result<Exclusion, &'static str> {
    if raw.is_empty() || raw.chars().count() > 512 || raw.contains('\0') {
        return Err("an exclusion has 1-512 characters and no NUL");
    }
    if let Some(p) = raw.strip_prefix("repository:") {
        return Glob::new(p).map(Exclusion::Repository);
    }
    if let Some(g) = raw.strip_prefix("group:") {
        if g.is_empty() || g.contains(['/', '\\']) {
            return Err("a group exclusion names one directory segment");
        }
        return Ok(Exclusion::Group(g.to_lowercase()));
    }
    let p = raw.strip_prefix("path:").unwrap_or(raw);
    if p.starts_with('/') || p.contains('\\') || p.split('/').any(|s| s == "..") {
        return Err("a path exclusion is relative, uses '/' and has no '..'");
    }
    Glob::new(p).map(Exclusion::Path)
}

/// A case-insensitive, segment-aware glob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Glob {
    segments: Vec<String>,
    /// No `/` in the pattern: it matches any one segment.
    any_segment: bool,
}
impl Glob {
    pub fn new(pattern: &str) -> Result<Self, &'static str> {
        if pattern.is_empty() {
            return Err("an empty glob");
        }
        let lower = pattern.to_lowercase();
        let segments: Vec<String> = lower.split('/').map(str::to_owned).collect();
        for s in &segments {
            if s.is_empty() {
                return Err("a glob has no empty segments");
            }
            if s.contains("**") && s != "**" {
                return Err("'**' is a whole segment");
            }
            let mut depth = 0i32;
            for c in s.chars() {
                match c {
                    '[' => depth += 1,
                    ']' => depth -= 1,
                    _ => {}
                }
                if !(0..=1).contains(&depth) {
                    return Err("unbalanced character class");
                }
            }
            if depth != 0 {
                return Err("unbalanced character class");
            }
        }
        Ok(Self {
            any_segment: segments.len() == 1 && segments[0] != "**",
            segments,
        })
    }
    /// Whether `path` (`/`-separated) matches.
    pub fn matches(&self, path: &str) -> bool {
        let lower = path.to_lowercase();
        let parts: Vec<&str> = lower.split('/').filter(|s| !s.is_empty()).collect();
        if self.any_segment {
            return parts.iter().any(|p| segment_match(&self.segments[0], p));
        }
        let pattern: Vec<&str> = self.segments.iter().map(String::as_str).collect();
        path_match(&pattern, &parts)
    }
}
fn path_match(p: &[&str], s: &[&str]) -> bool {
    // Iterative with memo-free backtracking bounded by segment counts (paths are short).
    match p.first() {
        None => s.is_empty(),
        Some(&"**") => (0..=s.len()).any(|i| path_match(&p[1..], &s[i..])),
        Some(seg) => !s.is_empty() && segment_match(seg, s[0]) && path_match(&p[1..], &s[1..]),
    }
}
fn segment_match(p: &str, s: &str) -> bool {
    let p: Vec<char> = p.chars().collect();
    let s: Vec<char> = s.chars().collect();
    // Classic wildcard matching with a single backtrack point (linear for `*`).
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while si < s.len() {
        if pi < p.len() {
            match p[pi] {
                '*' => {
                    star = Some(pi);
                    mark = si;
                    pi += 1;
                    continue;
                }
                '?' => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                '[' => {
                    if let Some((ok, next)) = class_match(&p, pi, s[si])
                        && ok
                    {
                        pi = next;
                        si += 1;
                        continue;
                    }
                }
                c if c == s[si] => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                _ => {}
            }
        }
        match star {
            Some(st) => {
                pi = st + 1;
                mark += 1;
                si = mark;
            }
            None => return false,
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}
/// Matches `[...]` at `p[start]` against `c`; returns (matched, index after the class).
fn class_match(p: &[char], start: usize, c: char) -> Option<(bool, usize)> {
    let mut i = start + 1;
    let negate = matches!(p.get(i), Some('!') | Some('^'));
    if negate {
        i += 1;
    }
    let mut matched = false;
    let first = i;
    while i < p.len() && (p[i] != ']' || i == first) {
        if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
            if p[i] <= c && c <= p[i + 2] {
                matched = true;
            }
            i += 3;
        } else {
            if p[i] == c {
                matched = true;
            }
            i += 1;
        }
    }
    if i >= p.len() {
        return None;
    }
    Some((matched != negate, i + 1))
}

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

/// The configuration one repository is indexed under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveConfig {
    /// Empty: every supported language.
    pub languages: Vec<String>,
    pub packages: Vec<String>,
    pub excludes: Vec<String>,
    pub max_file_bytes: i64,
    /// The highest revision among the combined sections (what `expectedSourceRevision` of a
    /// single-section request is checked against is the section's own revision).
    pub digest: String,
    paths: Vec<Glob>,
    repositories: Vec<Glob>,
    groups: Vec<String>,
}
impl EffectiveConfig {
    /// Combines the workspace section with the given project sections (see the module docs).
    pub fn combine(workspace: &StoredSourceConfig, projects: &[StoredSourceConfig]) -> Self {
        let mut excludes: Vec<String> = workspace.exclude_patterns.clone();
        let mut packages: Vec<String> = workspace.semantic_packages.clone();
        let mut max = workspace.max_file_bytes;
        let mut selections: Vec<Vec<String>> = Vec::new();
        if !workspace.structural_languages.is_empty() {
            selections.push(workspace.structural_languages.clone());
        }
        for p in projects {
            excludes.extend(p.exclude_patterns.iter().cloned());
            packages.extend(p.semantic_packages.iter().cloned());
            max = max.min(p.max_file_bytes);
            if !p.structural_languages.is_empty() {
                selections.push(p.structural_languages.clone());
            }
        }
        excludes.sort();
        excludes.dedup();
        packages.sort();
        packages.dedup();
        let mut languages: Vec<String> = match selections.split_first() {
            None => Vec::new(),
            Some((first, rest)) => first
                .iter()
                .filter(|l| rest.iter().all(|r| r.contains(l)))
                .cloned()
                .collect(),
        };
        languages.sort();
        languages.dedup();
        // A selection whose intersection is empty selects nothing, not everything.
        let none_selected = !selections.is_empty() && languages.is_empty();
        if none_selected {
            languages.push("-".into());
        }
        let digest = sha256_hex(
            json!({
                "languages": languages,
                "packages": packages,
                "excludes": excludes,
                "maxFileBytes": max,
                "extractor": EXTRACTOR_VERSION,
            })
            .to_string()
            .as_bytes(),
        );
        let mut paths = Vec::new();
        let mut repositories = Vec::new();
        let mut groups = Vec::new();
        for e in &excludes {
            match parse_exclusion(e) {
                Ok(Exclusion::Path(g)) => paths.push(g),
                Ok(Exclusion::Repository(g)) => repositories.push(g),
                Ok(Exclusion::Group(g)) => groups.push(g),
                // Stored patterns were checked when written; an unreadable one excludes nothing.
                Err(_) => {}
            }
        }
        Self {
            languages,
            packages,
            excludes,
            max_file_bytes: max.clamp(1, MAX_FILE_BYTES_LIMIT),
            digest,
            paths,
            repositories,
            groups,
        }
    }
    /// Whether a structural language is selected.
    pub fn language_selected(&self, id: &str) -> bool {
        self.languages.is_empty() || self.languages.iter().any(|l| l == id)
    }
    /// Whether a repository-relative path is excluded by a path pattern.
    pub fn path_excluded(&self, path: &str) -> bool {
        self.paths.iter().any(|g| g.matches(path))
    }
    /// Whether a repository is excluded by name or by a group segment of `group_path` (the
    /// directories between its section's base directory and the repository).
    pub fn repository_excluded(&self, name: &str, group_path: &[String]) -> bool {
        self.repositories.iter().any(|g| g.matches(name))
            || group_path
                .iter()
                .any(|seg| self.groups.iter().any(|g| *g == seg.to_lowercase()))
    }
    /// The size bound of one path (the retained Swift exception while the default applies).
    pub fn max_bytes_for(&self, path: &str) -> i64 {
        if self.max_file_bytes == DEFAULT_MAX_FILE_BYTES && path.to_lowercase().ends_with(".swift")
        {
            SWIFT_MAX_FILE_BYTES
        } else {
            self.max_file_bytes
        }
    }
}

/// Whether `path` lies in a dependency directory beside its manifest, given a lookup of the
/// paths present in the same inventory.
pub fn dependency_directory(path: &str, present: &dyn Fn(&str) -> bool) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    for (i, seg) in parts.iter().enumerate().take(parts.len().saturating_sub(1)) {
        for (dir, manifests) in MANIFEST_DIRECTORIES {
            if seg.eq_ignore_ascii_case(dir) {
                let parent = parts[..i].join("/");
                if manifests.iter().any(|m| {
                    present(&if parent.is_empty() {
                        (*m).to_owned()
                    } else {
                        format!("{parent}/{m}")
                    })
                }) {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_like_the_retained_matcher() {
        let g = |p: &str| Glob::new(p).unwrap();
        assert!(g("**/node_modules/**").matches("a/node_modules/x/y.js"));
        assert!(g("**/node_modules/**").matches("node_modules/x.js"));
        assert!(!g("**/node_modules/**").matches("a/node_modules_x/y.js"));
        assert!(
            g("**/*.min.js").matches("web/app.MIN.js"),
            "case-insensitive"
        );
        assert!(g("**/bin/**").matches("Src/Bin/Debug/x.dll"));
        assert!(!g("**/bin/**").matches("src/binary/x"));
        assert!(
            g("vendor").matches("a/vendor/b.go"),
            "a slashless pattern matches a segment"
        );
        assert!(g("src/*.rs").matches("src/lib.rs"));
        assert!(!g("src/*.rs").matches("src/a/lib.rs"));
        assert!(g("src/**/*.rs").matches("src/lib.rs"));
        assert!(g("src/**/*.rs").matches("src/a/b/lib.rs"));
        assert!(g("t?st/[a-c]x.txt").matches("test/bx.txt"));
        assert!(!g("t?st/[!a-c]x.txt").matches("test/bx.txt"));
        assert!(Glob::new("a**b").is_err());
        assert!(Glob::new("a[b").is_err());
    }

    #[test]
    fn exclusions_parse_by_kind_and_refuse_escapes() {
        assert!(matches!(
            parse_exclusion("repository:legacy-*"),
            Ok(Exclusion::Repository(_))
        ));
        assert_eq!(
            parse_exclusion("group:Archive"),
            Ok(Exclusion::Group("archive".into()))
        );
        assert!(parse_exclusion("../x").is_err());
        assert!(parse_exclusion("/abs/**").is_err());
        assert!(parse_exclusion("a\\b").is_err());
        assert!(parse_exclusion("group:a/b").is_err());
        assert!(parse_exclusion("").is_err());
    }

    #[test]
    fn sections_combine_deterministically() {
        let mut w = default_config(WORKSPACE_SCOPE_KEY);
        w.structural_languages = vec!["rust".into(), "go".into()];
        let mut p = default_config("project-a");
        p.structural_languages = vec!["rust".into(), "python".into()];
        p.exclude_patterns = vec!["**/generated/**".into()];
        p.max_file_bytes = 1000;
        let e = EffectiveConfig::combine(&w, &[p.clone()]);
        assert_eq!(e.languages, vec!["rust".to_owned()]);
        assert!(e.path_excluded("x/generated/y.rs"));
        assert!(e.path_excluded("x/node_modules/y.js"));
        assert_eq!(e.max_file_bytes, 1000);
        assert_eq!(
            e,
            EffectiveConfig::combine(&w, &[p.clone()]),
            "deterministic"
        );
        let mut q = p.clone();
        q.structural_languages = vec!["python".into()];
        let none = EffectiveConfig::combine(&w, &[q]);
        assert!(!none.language_selected("rust") && !none.language_selected("python"));
        assert_ne!(e.digest, EffectiveConfig::combine(&w, &[]).digest);
    }

    #[test]
    fn dependency_directories_need_their_manifest() {
        let present = |p: &str| p == "crate/Cargo.toml" || p == "Podfile";
        assert!(dependency_directory("crate/target/debug/x.rs", &present));
        assert!(!dependency_directory("other/target/x.rs", &present));
        assert!(dependency_directory("Pods/A/a.swift", &present));
        assert!(
            !dependency_directory("crate/target", &present),
            "the directory itself is a file path here"
        );
    }
}
