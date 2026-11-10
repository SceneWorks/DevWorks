//! The harness-owned skill directory (`<data-root>/skills`). Only approved harness revisions are
//! written here; shared skill roots are read through [`super::indexing::SourceTree`] and never
//! written.
use std::path::PathBuf;

/// One file of a materialized skill, relative to its skill directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillFile {
    /// `/`-separated, without `.`/`..` segments.
    pub path: String,
    pub bytes: Vec<u8>,
    /// Marked executable where the platform supports it (a script resource).
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillFileFault {
    InvalidName,
    InvalidPath(String),
    Io(String),
}
impl std::fmt::Display for SkillFileFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName => f.write_str("invalid skill directory name"),
            Self::InvalidPath(p) => write!(f, "invalid skill file path {p}"),
            Self::Io(e) => write!(f, "skill directory I/O: {e}"),
        }
    }
}

pub trait SkillFiles: Send + Sync {
    /// The managed harness skill root (created when missing).
    fn root(&self) -> Result<PathBuf, SkillFileFault>;
    /// Replaces the skill directory `name` under the root with exactly `files`: the new tree is
    /// written and flushed beside it, then swapped in with a rename, so readers see the old or
    /// the new tree, never a mixture.
    fn materialize(&self, name: &str, files: &[SkillFile]) -> Result<(), SkillFileFault>;
}
