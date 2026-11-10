//! Bounded discovery of file-backed skills. A skill is a directory holding `SKILL.md`; every
//! other regular file beneath it (documents, scripts, assets) is a resource resolved relative to
//! that owning directory. Links are never followed and nothing outside the root is read.
use crate::fingerprint::sha256_hex;
use crate::ports::indexing::{EntryKind, SourceFault, SourceTree};
use serde_json::json;
use std::path::Path;

pub const SKILL_FILE: &str = "SKILL.md";
pub const MAX_SKILLS: usize = 512;
pub const MAX_INSTRUCTION_BYTES: u64 = 256 * 1024;
pub const MAX_RESOURCE_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_RESOURCES: usize = 256;
pub const MAX_SKILL_TOTAL_BYTES: u64 = 32 * 1024 * 1024;
const MAX_DEPTH: usize = 6;
const MAX_ENTRIES: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedFile {
    pub path: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
    /// Script resources are invoked only through ordinary process tools and permissions.
    pub script: bool,
    /// The file's execute permission as observed, restored on materialization independently of
    /// whether it is recognized as a script (native helpers keep working).
    pub executable: bool,
}
impl ScannedFile {
    pub fn new(path: String, bytes: Vec<u8>) -> Self {
        let script = is_script(&path, &bytes);
        Self {
            sha256: sha256_hex(&bytes),
            path,
            bytes,
            script,
            executable: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedSkill {
    /// The directory name, which is the skill's identity within its root.
    pub name: String,
    pub instruction: ScannedFile,
    /// Sorted by path.
    pub resources: Vec<ScannedFile>,
    pub digest: String,
}

/// A skill directory that could not be captured, with the visible reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedSkill {
    pub name: String,
    pub reason: String,
}

pub fn is_script(path: &str, bytes: &[u8]) -> bool {
    bytes.starts_with(b"#!")
        || [
            ".sh", ".bash", ".zsh", ".py", ".rb", ".pl", ".ps1", ".js", ".mjs", ".ts",
        ]
        .iter()
        .any(|ext| path.ends_with(ext))
}

/// A valid skill directory name: one path segment, no control characters, at most 128 chars.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 128
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && !name.contains(['/', '\\', ':'])
        && !name.chars().any(char::is_control)
}

/// Whole-revision digest: the instruction bytes plus every resource path, digest and execute
/// permission.
pub fn revision_digest(instruction_sha256: &str, resources: &[(String, String, bool)]) -> String {
    let value = json!({
        "instruction": instruction_sha256,
        "resources": resources.iter().map(|(p, d, s)| json!([p, d, s])).collect::<Vec<_>>(),
    });
    sha256_hex(
        crate::jcs::canonicalize(&value)
            .expect("canonical revision digest")
            .as_bytes(),
    )
}

impl ScannedSkill {
    pub fn from_files(
        name: String,
        instruction: ScannedFile,
        mut resources: Vec<ScannedFile>,
    ) -> Self {
        resources.sort_by(|a, b| a.path.cmp(&b.path));
        let digest = revision_digest(
            &instruction.sha256,
            &resources
                .iter()
                .map(|r| (r.path.clone(), r.sha256.clone(), r.executable))
                .collect::<Vec<_>>(),
        );
        Self {
            name,
            instruction,
            resources,
            digest,
        }
    }
}

/// The `description:` of a leading YAML front matter block, if any (display only).
pub fn description(text: &str) -> Option<String> {
    let rest = text.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    rest[..end].lines().find_map(|line| {
        line.strip_prefix("description:")
            .map(|v| v.trim().trim_matches('"').trim_matches('\'').to_owned())
            .filter(|v| !v.is_empty())
    })
}

/// Every skill directly beneath `root`. A missing root is an empty set; an unreadable root is
/// an error so its previously discovered skills are not mistaken for deletions.
pub fn scan_root(
    source: &dyn SourceTree,
    root: &Path,
) -> Result<(Vec<ScannedSkill>, Vec<SkippedSkill>), SourceFault> {
    let entries = match source.list_dir(root, MAX_SKILLS + 1) {
        Ok(entries) => entries,
        Err(SourceFault::Missing) => return Ok((Vec::new(), Vec::new())),
        Err(e) => return Err(e),
    };
    // A truncated root listing would make unlisted skills look deleted: refuse it whole, so
    // every previously discovered skill keeps its state.
    if entries.len() > MAX_SKILLS {
        return Err(SourceFault::Refused(format!(
            "more than {MAX_SKILLS} entries in the skill root"
        )));
    }
    let mut skills = Vec::new();
    let mut skipped = Vec::new();
    for entry in entries {
        if entry.kind != EntryKind::Directory || !valid_name(&entry.name) {
            continue;
        }
        match scan_skill(source, root, &entry.name) {
            Ok(Some(skill)) => skills.push(skill),
            Ok(None) => {}
            Err(reason) => skipped.push(SkippedSkill {
                name: entry.name,
                reason,
            }),
        }
    }
    Ok((skills, skipped))
}

/// One skill directory: `Ok(None)` when it has no `SKILL.md` (not a skill).
pub fn scan_skill(
    source: &dyn SourceTree,
    root: &Path,
    name: &str,
) -> Result<Option<ScannedSkill>, String> {
    let instruction =
        match source.read_file(root, &format!("{name}/{SKILL_FILE}"), MAX_INSTRUCTION_BYTES) {
            Ok(bytes) => bytes,
            Err(SourceFault::Missing) => return Ok(None),
            Err(e) => return Err(format!("{SKILL_FILE}: {e}")),
        };
    if std::str::from_utf8(&instruction).is_err() {
        return Err(format!("{SKILL_FILE} is not UTF-8"));
    }
    let mut resources = Vec::new();
    let mut total = instruction.len() as u64;
    let mut stack = vec![(String::new(), 0usize)];
    while let Some((relative, depth)) = stack.pop() {
        let dir = if relative.is_empty() {
            root.join(name)
        } else {
            root.join(name).join(&relative)
        };
        let shown = if relative.is_empty() {
            "."
        } else {
            relative.as_str()
        };
        // One more than the bound detects truncation: a partial listing is never captured as a
        // complete revision (materializing it would delete the files it omitted).
        let entries = source
            .list_dir(&dir, MAX_ENTRIES + 1)
            .map_err(|e| format!("{shown}: {e}"))?;
        if entries.len() > MAX_ENTRIES {
            return Err(format!("{shown}: more than {MAX_ENTRIES} entries"));
        }
        for entry in entries {
            if entry.name.starts_with('.') {
                continue;
            }
            let path = if relative.is_empty() {
                entry.name.clone()
            } else {
                format!("{relative}/{}", entry.name)
            };
            match entry.kind {
                EntryKind::Directory if depth + 1 < MAX_DEPTH => stack.push((path, depth + 1)),
                EntryKind::Directory => return Err(format!("{path}: nested too deeply")),
                EntryKind::File if path == SKILL_FILE => {}
                EntryKind::File => {
                    if resources.len() >= MAX_RESOURCES {
                        return Err(format!("more than {MAX_RESOURCES} resources"));
                    }
                    let bytes = source
                        .read_file(root, &format!("{name}/{path}"), MAX_RESOURCE_BYTES)
                        .map_err(|e| format!("{path}: {e}"))?;
                    total += bytes.len() as u64;
                    if total > MAX_SKILL_TOTAL_BYTES {
                        return Err("skill exceeds its total size bound".into());
                    }
                    let mut file = ScannedFile::new(path, bytes);
                    file.executable = source
                        .executable(root, &format!("{name}/{}", file.path))
                        .unwrap_or(false);
                    resources.push(file);
                }
                // Links and special files are never followed or captured.
                EntryKind::Link | EntryKind::Other => {
                    return Err(format!("{path}: links and special files are not supported"));
                }
            }
        }
    }
    Ok(Some(ScannedSkill::from_files(
        name.to_owned(),
        ScannedFile::new(SKILL_FILE.into(), instruction),
        resources,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn description_reads_front_matter_only() {
        assert_eq!(
            description("---\nname: x\ndescription: \"Does things\"\n---\nbody"),
            Some("Does things".into())
        );
        assert_eq!(description("description: not front matter"), None);
    }

    #[test]
    fn revision_digest_binds_resources_and_kind() {
        let a = revision_digest("a", &[("x.sh".into(), "b".into(), true)]);
        let b = revision_digest("a", &[("x.sh".into(), "b".into(), false)]);
        let c = revision_digest("a", &[]);
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    /// A tree whose `big` skill directory holds more entries than the listing bound.
    struct Crowded;
    impl SourceTree for Crowded {
        fn canonical_dir(&self, p: &Path) -> Result<std::path::PathBuf, SourceFault> {
            Ok(p.to_path_buf())
        }
        fn list_dir(
            &self,
            dir: &Path,
            limit: usize,
        ) -> Result<Vec<crate::ports::indexing::DirEntry>, SourceFault> {
            let names: Vec<String> = if dir.ends_with("big") {
                (0..MAX_ENTRIES + 5)
                    .map(|i| format!(".hidden{i:05}"))
                    .chain(["visible.md".into()])
                    .collect()
            } else {
                vec!["big".into()]
            };
            let kind = if dir.ends_with("big") {
                EntryKind::File
            } else {
                EntryKind::Directory
            };
            let mut names = names;
            names.sort();
            Ok(names
                .into_iter()
                .take(limit)
                .map(|name| crate::ports::indexing::DirEntry { name, kind })
                .collect())
        }
        fn git_marker(
            &self,
            _: &Path,
        ) -> Result<Option<crate::ports::indexing::GitMarker>, SourceFault> {
            Ok(None)
        }
        fn read_file(&self, _: &Path, relative: &str, _: u64) -> Result<Vec<u8>, SourceFault> {
            if relative == "big/SKILL.md" {
                Ok(b"skill".to_vec())
            } else {
                Ok(b"x".to_vec())
            }
        }
        fn case_insensitive(&self, _: &Path) -> bool {
            false
        }
        fn home(&self) -> Option<std::path::PathBuf> {
            None
        }
    }

    #[test]
    fn truncated_listings_are_refused_not_captured() {
        let (skills, skipped) = scan_root(&Crowded, Path::new("/root")).unwrap();
        assert!(
            skills.is_empty(),
            "a partial tree must never become a revision"
        );
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].reason.contains("more than"), "{:?}", skipped[0]);
    }

    #[test]
    fn names_are_single_visible_segments() {
        assert!(valid_name("codegraph-item"));
        for bad in ["", ".hidden", "a/b", "a\\b", "..", "c:"] {
            assert!(!valid_name(bad), "{bad}");
        }
    }
}
