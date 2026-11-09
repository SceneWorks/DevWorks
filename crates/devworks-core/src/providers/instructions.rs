//! Exact, path-scoped repository guidance for a provider context. The caller supplies only a
//! host-authorized checkout root; SourceTree refuses links and reads outside that root.
use crate::{
    fingerprint::sha256_hex,
    ports::indexing::{SourceFault, SourceTree},
    protocol::Id,
    providers::context::TextBlock,
};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_SOURCES: usize = 128;
const MAX_TOTAL_BYTES: usize = 512 * 1024;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstructionSource {
    pub repository_id: Id,
    pub path: String,
    pub sha256: String,
    /// Exact targets within this checkout to which this file applies.
    pub applies_to: Vec<String>,
    pub text: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstructionBundle {
    pub repository_id: Id,
    pub targets: Vec<String>,
    /// SHA-256 over the ordered source paths, exact bytes and applicability.
    pub revision: String,
    pub sources: Vec<InstructionSource>,
}

impl InstructionBundle {
    /// Files appear ancestor first, and CLAUDE.md before AGENTS.md at one directory. Thus later
    /// applicable guidance wins a conflict while nonconflicting earlier guidance remains visible.
    pub fn text_blocks(&self) -> Vec<TextBlock> {
        self.sources
            .iter()
            .map(|source| TextBlock {
                text: format!(
                    "Repository instruction {} (sha256 {}, applies to {}):\n{}",
                    source.path,
                    source.sha256,
                    source.applies_to.join(", "),
                    source.text,
                ),
                cache: None,
            })
            .collect()
    }
}

#[derive(Debug)]
pub enum InstructionFault {
    InvalidTarget,
    TooManySources,
    TooManyBytes,
    InvalidUtf8(String),
    Read { path: String, source: SourceFault },
}

fn target(path: &str) -> Result<Vec<&str>, InstructionFault> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(InstructionFault::InvalidTarget);
    }
    Ok(path.split('/').collect())
}

/// Resolve exactly the instruction files on each target's ancestor chain, inside one qualified
/// checkout. Missing files are absent guidance; unreadable, linked or oversized files hold the
/// context instead of silently falling back to a weaker ancestor.
pub fn resolve(
    source: &dyn SourceTree,
    root: &Path,
    repository_id: Id,
    paths: &[String],
) -> Result<InstructionBundle, InstructionFault> {
    if paths.is_empty() {
        return Err(InstructionFault::InvalidTarget);
    }
    let mut targets = BTreeSet::new();
    let mut candidates: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for path in paths {
        let parts = target(path)?;
        targets.insert(path.clone());
        for depth in 0..parts.len() {
            let dir = parts[..depth].join("/");
            for name in ["CLAUDE.md", "AGENTS.md"] {
                let relative = if dir.is_empty() {
                    name.to_owned()
                } else {
                    format!("{dir}/{name}")
                };
                candidates.entry(relative).or_default().insert(path.clone());
            }
        }
    }
    if candidates.len() > MAX_SOURCES * 2 {
        return Err(InstructionFault::TooManySources);
    }
    let mut sources = Vec::new();
    let mut total = 0usize;
    for (path, applies_to) in candidates {
        let bytes = match source.read_file(root, &path, MAX_FILE_BYTES) {
            Ok(bytes) => bytes,
            Err(SourceFault::Missing) => continue,
            Err(error) => {
                return Err(InstructionFault::Read {
                    path,
                    source: error,
                });
            }
        };
        total = total
            .checked_add(bytes.len())
            .ok_or(InstructionFault::TooManyBytes)?;
        if total > MAX_TOTAL_BYTES {
            return Err(InstructionFault::TooManyBytes);
        }
        let text = String::from_utf8(bytes.clone())
            .map_err(|_| InstructionFault::InvalidUtf8(path.clone()))?;
        sources.push(InstructionSource {
            repository_id: repository_id.clone(),
            path,
            sha256: sha256_hex(&bytes),
            applies_to: applies_to.into_iter().collect(),
            text,
        });
        if sources.len() > MAX_SOURCES {
            return Err(InstructionFault::TooManySources);
        }
    }
    sources.sort_by(|a, b| {
        let depth = |p: &str| p.matches('/').count();
        depth(&a.path)
            .cmp(&depth(&b.path))
            .then_with(|| {
                a.path
                    .rsplit('/')
                    .next()
                    .cmp(&b.path.rsplit('/').next())
                    .reverse()
            })
            .then_with(|| a.path.cmp(&b.path))
    });
    let targets: Vec<String> = targets.into_iter().collect();
    let revision = sha256_hex(
        &serde_json::to_vec(&(&repository_id, &targets, &sources))
            .expect("serializable instruction bundle"),
    );
    Ok(InstructionBundle {
        repository_id,
        targets,
        revision,
        sources,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::indexing::{DirEntry, GitMarker};
    use std::path::PathBuf;

    struct Tree(BTreeMap<String, Vec<u8>>);
    impl SourceTree for Tree {
        fn canonical_dir(&self, path: &Path) -> Result<PathBuf, SourceFault> {
            Ok(path.to_path_buf())
        }
        fn list_dir(&self, _: &Path, _: usize) -> Result<Vec<DirEntry>, SourceFault> {
            unreachable!()
        }
        fn git_marker(&self, _: &Path) -> Result<Option<GitMarker>, SourceFault> {
            unreachable!()
        }
        fn read_file(
            &self,
            _: &Path,
            relative: &str,
            max_bytes: u64,
        ) -> Result<Vec<u8>, SourceFault> {
            let bytes = self.0.get(relative).ok_or(SourceFault::Missing)?;
            if bytes.len() as u64 > max_bytes {
                return Err(SourceFault::TooLarge {
                    size: bytes.len() as u64,
                });
            }
            Ok(bytes.clone())
        }
        fn case_insensitive(&self, _: &Path) -> bool {
            false
        }
        fn home(&self) -> Option<PathBuf> {
            None
        }
    }

    #[test]
    fn nearest_guidance_and_same_level_agents_take_precedence_with_exact_digests() {
        let tree = Tree(BTreeMap::from([
            ("CLAUDE.md".into(), b"root claude".to_vec()),
            ("AGENTS.md".into(), b"root agents".to_vec()),
            ("src/CLAUDE.md".into(), b"src claude".to_vec()),
            ("src/AGENTS.md".into(), b"src agents".to_vec()),
            ("other/AGENTS.md".into(), b"other agents".to_vec()),
        ]));
        let root = Path::new("/authorized");
        let repo = Id::new("repo").unwrap();
        let first = resolve(
            &tree,
            root,
            repo.clone(),
            &["src/lib.rs".into(), "README.md".into()],
        )
        .unwrap();
        assert_eq!(
            first
                .sources
                .iter()
                .map(|s| s.path.as_str())
                .collect::<Vec<_>>(),
            ["CLAUDE.md", "AGENTS.md", "src/CLAUDE.md", "src/AGENTS.md"]
        );
        assert_eq!(first.sources[2].applies_to, ["src/lib.rs"]);
        assert_eq!(first.sources[3].sha256, sha256_hex(b"src agents"));
        let mut changed = tree.0.clone();
        changed.insert("src/AGENTS.md".into(), b"edited".to_vec());
        let second = resolve(
            &Tree(changed),
            root,
            repo,
            &["src/lib.rs".into(), "README.md".into()],
        )
        .unwrap();
        assert_ne!(first.revision, second.revision);
        assert!(matches!(
            resolve(&tree, root, Id::new("repo").unwrap(), &["../escape".into()]),
            Err(InstructionFault::InvalidTarget)
        ));
    }
}
