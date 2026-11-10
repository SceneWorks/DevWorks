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
}
impl ScannedFile {
    pub fn new(path: String, bytes: Vec<u8>) -> Self {
        let script = is_script(&path, &bytes);
        Self {
            sha256: sha256_hex(&bytes),
            path,
            bytes,
            script,
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

/// Whole-revision digest: the instruction bytes plus every resource path, digest and kind.
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
                .map(|r| (r.path.clone(), r.sha256.clone(), r.script))
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
    let mut skills = Vec::new();
    let mut skipped = Vec::new();
    for entry in entries.into_iter().take(MAX_SKILLS) {
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
        let entries = source
            .list_dir(&dir, MAX_ENTRIES)
            .map_err(|e| format!("{}: {e}", if relative.is_empty() { "." } else { &relative }))?;
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
                    resources.push(ScannedFile::new(path, bytes));
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

    #[test]
    fn names_are_single_visible_segments() {
        assert!(valid_name("codegraph-item"));
        for bad in ["", ".hidden", "a/b", "a\\b", "..", "c:"] {
            assert!(!valid_name(bad), "{bad}");
        }
    }
}
