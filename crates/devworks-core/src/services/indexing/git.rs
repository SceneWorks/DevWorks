//! The Git reads indexing needs, each one bounded helper process through the process owner.
//!
//! Every call disables repository-configured programs that read commands could otherwise run
//! (`core.fsmonitor` through the environment configuration, external diff and textconv drivers
//! per command) and optional locks, uses only subcommands the process owner's preflight models
//! as non-destructive, reads NUL-separated
//! output, and refuses paths that are not UTF-8, absolute or contain `..` (they are reported,
//! never indexed). Blob contents come from the object database (`cat-file --batch`), so a
//! baseline is exactly its commit, never the live worktree.
use super::helper::{HelperFault, HelperLimits, HelperRunner};
use super::paths::{relative_ok, unit_key};
use crate::context::InvocationContext;
use crate::ports::process::ProcessSpec;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The empty tree (SHA-1): the base of an unborn branch.
pub const EMPTY_TREE_SHA1: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// The empty tree (SHA-256).
pub const EMPTY_TREE_SHA256: &str =
    "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";
/// Most bytes a listing (`ls-tree`, `diff`, `ls-files`) may produce: a larger repository is
/// refused honestly rather than read unbounded.
pub const MAX_LISTING_BYTES: usize = 128 * 1024 * 1024;
/// Most content bytes one blob batch may produce: never below the largest file (or extraction
/// group) the configuration accepts, since a batch always admits its first blob whole.
pub const MAX_BATCH_BYTES: usize = {
    let file = super::config::MAX_FILE_BYTES_LIMIT as usize;
    let group = super::worker::MAX_GROUP_BYTES;
    if file > group { file } else { group }
};

/// The stdout bound of a `cat-file --batch` read of `count` blobs totalling `expected` bytes:
/// headers are at most ~100 bytes per object.
pub fn blob_output_limit(expected: usize, count: usize) -> usize {
    let headers = count.saturating_mul(128);
    expected
        .saturating_add(headers)
        .min(MAX_BATCH_BYTES.saturating_add(headers))
}

/// One Git invocation context: who runs it, how long it may take and when to stop.
pub struct Git<'a> {
    pub runner: &'a dyn HelperRunner,
    pub ctx: &'a InvocationContext,
    pub program: &'a str,
    pub deadline: Duration,
    pub stop: &'a (dyn Fn() -> bool + Sync),
}

/// A repository revision observed around a bounded worktree status read.
/// The owning Git service must first resolve the invocation's durable worktree identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevisionObservation {
    pub head: String,
    pub dirty: bool,
}

/// One entry of a commit's tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: String,
    pub oid: String,
    pub size: u64,
    pub path: String,
}
impl TreeEntry {
    /// A gitlink (submodule): never indexed as content.
    pub fn submodule(&self) -> bool {
        self.mode == "160000"
    }
    /// A symbolic link: recorded as a link unit, its target never followed.
    pub fn link(&self) -> bool {
        self.mode == "120000"
    }
}

/// One changed path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Added(String),
    Modified(String),
    Deleted(String),
    Renamed { from: String, to: String },
}

/// A worktree entry of `git worktree list --porcelain`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: String,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub bare: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitFault {
    Helper(HelperFault),
    /// Output that does not parse as Git's documented format.
    Malformed(String),
}
impl std::fmt::Display for GitFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Helper(h) => write!(f, "git {h}"),
            Self::Malformed(m) => write!(f, "unexpected git output: {m}"),
        }
    }
}
impl From<HelperFault> for GitFault {
    fn from(h: HelperFault) -> Self {
        Self::Helper(h)
    }
}
impl GitFault {
    /// Whether retrying the same request can succeed (a deadline, a busy store, a lost stop).
    pub fn transient(&self) -> bool {
        matches!(
            self,
            Self::Helper(
                HelperFault::TimedOut | HelperFault::Storage(_) | HelperFault::Uncertain(_)
            )
        )
    }
    pub fn cancelled(&self) -> bool {
        matches!(self, Self::Helper(HelperFault::Cancelled))
    }
}

impl Git<'_> {
    fn spec(&self, dir: &Path, args: &[&str], stdin: Option<Vec<u8>>) -> ProcessSpec {
        let mut argv: Vec<String> = vec![self.program.to_owned()];
        argv.extend(args.iter().map(|a| (*a).to_owned()));
        let mut spec = ProcessSpec::new(&argv).expect("a program");
        spec.cwd = Some(PathBuf::from(dir));
        // Host-fixed hardening through Git's environment configuration (never caller input):
        // no file-system monitor program runs for a read, no optional lock is taken and no
        // credential prompt can block. External diff and text conversion drivers are disabled
        // per command (`--no-ext-diff`, `--no-textconv`).
        spec.env = vec![
            ("GIT_CONFIG_COUNT".into(), "1".into()),
            ("GIT_CONFIG_KEY_0".into(), "core.fsmonitor".into()),
            ("GIT_CONFIG_VALUE_0".into(), "false".into()),
            ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ("LC_ALL".into(), "C".into()),
        ];
        spec.stdin = stdin;
        spec
    }
    fn run(&self, dir: &Path, args: &[&str], max: usize) -> Result<Vec<u8>, GitFault> {
        Ok(self.runner.run(
            self.ctx,
            self.spec(dir, args, None),
            HelperLimits {
                deadline: self.deadline,
                max_stdout: max,
            },
            self.stop,
        )?)
    }
    fn line(&self, dir: &Path, args: &[&str]) -> Result<Option<String>, GitFault> {
        match self.run(dir, args, 64 * 1024) {
            Ok(out) => {
                Ok(Some(String::from_utf8_lossy(&out).trim().to_owned()).filter(|s| !s.is_empty()))
            }
            // Git's "not found / not set" answers (`rev-parse -q`, `config --get`, `symbolic-ref -q`).
            Err(GitFault::Helper(HelperFault::Failed { code: Some(1), .. })) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The commit `HEAD` names (`None` on an unborn branch).
    pub fn head(&self, dir: &Path) -> Result<Option<String>, GitFault> {
        let oid = self.line(dir, &["rev-parse", "-q", "--verify", "HEAD^{commit}"])?;
        match oid {
            Some(o) if oid_ok(&o) => Ok(Some(o)),
            Some(o) => Err(GitFault::Malformed(format!("HEAD {o:?}"))),
            None => Ok(None),
        }
    }
    /// Observe the tested revision and all staged, unstaged and untracked changes. Refuse
    /// attribution if HEAD changes during observation; never substitute a repository's
    /// primary checkout for the host-resolved invocation worktree.
    pub fn revision_observation(&self, dir: &Path) -> Result<RevisionObservation, GitFault> {
        let before = self
            .head(dir)?
            .ok_or_else(|| GitFault::Malformed("unborn HEAD has no tested revision".into()))?;
        let status = self.run(
            dir,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=normal",
                "--ignore-submodules=none",
            ],
            MAX_LISTING_BYTES,
        )?;
        let after = self.head(dir)?;
        if after.as_deref() != Some(before.as_str()) {
            return Err(GitFault::Malformed(
                "HEAD changed during revision observation".into(),
            ));
        }
        Ok(RevisionObservation {
            head: before,
            dirty: !status.is_empty(),
        })
    }

    /// The branch `HEAD` points at (`refs/heads/...`), `None` when detached.
    pub fn branch(&self, dir: &Path) -> Result<Option<String>, GitFault> {
        // `HEAD` itself when detached; an unborn branch has no resolvable `HEAD` (Git answers
        // with an error): no branch is recorded.
        match self.line(dir, &["rev-parse", "--symbolic-full-name", "HEAD"]) {
            Ok(r) => Ok(r.filter(|r| r.starts_with("refs/"))),
            Err(GitFault::Helper(HelperFault::Failed { .. })) => Ok(None),
            Err(e) => Err(e),
        }
    }
    /// `Sha1` or `Sha256`.
    pub fn object_format(&self, dir: &Path) -> Result<&'static str, GitFault> {
        Ok(
            match self
                .line(dir, &["rev-parse", "--show-object-format"])?
                .as_deref()
            {
                Some("sha256") => "Sha256",
                _ => "Sha1",
            },
        )
    }
    /// The commit `name` names now and the full ref it resolved through (`None` for a commit
    /// ID): a full commit ID, a full `refs/...` name, or a branch, tag or `origin` branch, tried
    /// in that order (`refs/heads/`, `refs/tags/`, `refs/remotes/origin/`, `refs/remotes/`). The
    /// caller has checked `name` is a ref name or a commit ID (never revision or option syntax).
    /// `None`: it names no commit.
    pub fn resolve(
        &self,
        dir: &Path,
        name: &str,
    ) -> Result<Option<(String, Option<String>)>, GitFault> {
        let candidates: Vec<String> = if oid_ok(name) || name.starts_with("refs/") {
            vec![name.to_owned()]
        } else {
            [
                "refs/heads/",
                "refs/tags/",
                "refs/remotes/origin/",
                "refs/remotes/",
            ]
            .iter()
            .map(|p| format!("{p}{name}"))
            .collect()
        };
        for c in candidates {
            let spec = format!("{c}^{{commit}}");
            match self.line(dir, &["rev-parse", "-q", "--verify", &spec])? {
                Some(o) if oid_ok(&o) => return Ok(Some((o, (!oid_ok(name)).then_some(c)))),
                Some(o) => return Err(GitFault::Malformed(format!("{c} {o:?}"))),
                None => {}
            }
        }
        Ok(None)
    }
    /// The configured `origin` remote URL.
    pub fn origin(&self, dir: &Path) -> Result<Option<String>, GitFault> {
        self.line(dir, &["config", "--get", "remote.origin.url"])
    }
    /// The repository's worktrees (the primary checkout first).
    pub fn worktrees(&self, dir: &Path) -> Result<Vec<WorktreeEntry>, GitFault> {
        let out = self.run(dir, &["worktree", "list", "--porcelain"], 4 * 1024 * 1024)?;
        Ok(parse_worktrees(&String::from_utf8_lossy(&out)))
    }
    /// Every blob of `commit` (recursively), with its size; unreadable paths are returned in
    /// the second list.
    pub fn tree(
        &self,
        dir: &Path,
        commit: &str,
    ) -> Result<(Vec<TreeEntry>, Vec<String>), GitFault> {
        if !oid_ok(commit) {
            return Err(GitFault::Malformed("commit".into()));
        }
        let out = self.run(
            dir,
            &["ls-tree", "-r", "-z", "-l", "--full-tree", commit],
            MAX_LISTING_BYTES,
        )?;
        parse_tree(&out)
    }
    /// The contents of `oids`, in order (one `cat-file --batch` process).
    pub fn blobs(
        &self,
        dir: &Path,
        oids: &[String],
        expected_bytes: usize,
    ) -> Result<Vec<Vec<u8>>, GitFault> {
        if oids.is_empty() {
            return Ok(Vec::new());
        }
        if !oids.iter().all(|o| oid_ok(o)) {
            return Err(GitFault::Malformed("blob id".into()));
        }
        let mut request = String::new();
        for o in oids {
            request.push_str(o);
            request.push('\n');
        }
        let max = blob_output_limit(expected_bytes, oids.len());
        let out = self.runner.run(
            self.ctx,
            self.spec(dir, &["cat-file", "--batch"], Some(request.into_bytes())),
            HelperLimits {
                deadline: self.deadline,
                max_stdout: max,
            },
            self.stop,
        )?;
        parse_batch(&out, oids)
    }
    /// The paths that differ between two commits (renames detected, `-M`).
    pub fn diff(
        &self,
        dir: &Path,
        base: &str,
        head: &str,
    ) -> Result<(Vec<Change>, Vec<String>), GitFault> {
        if !oid_ok(base) || !oid_ok(head) {
            return Err(GitFault::Malformed("commit".into()));
        }
        let out = self.run(
            dir,
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--name-status",
                "-z",
                "-M",
                base,
                head,
                "--",
            ],
            MAX_LISTING_BYTES,
        )?;
        parse_name_status(&out)
    }
    /// Staged and unstaged changes of a worktree against `base` (its `HEAD`, or the empty tree
    /// on an unborn branch), plus indexable untracked files (ignored files excluded).
    pub fn local_changes(
        &self,
        dir: &Path,
        base: &str,
    ) -> Result<(Vec<Change>, Vec<String>), GitFault> {
        let out = self.run(
            dir,
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--name-status",
                "-z",
                "-M",
                base,
                "--",
            ],
            MAX_LISTING_BYTES,
        )?;
        let (mut changes, mut refused) = parse_name_status(&out)?;
        let untracked = self.run(
            dir,
            &["ls-files", "-z", "--others", "--exclude-standard"],
            MAX_LISTING_BYTES,
        )?;
        for raw in untracked.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            match path_of(raw) {
                Some(p) => changes.push(Change::Added(p)),
                None => refused.push(String::from_utf8_lossy(raw).into_owned()),
            }
        }
        Ok((changes, refused))
    }
}

pub fn oid_ok(s: &str) -> bool {
    matches!(s.len(), 40 | 64)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn path_of(raw: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(raw).ok()?;
    relative_ok(s).then(|| unit_key(s))
}

pub fn parse_worktrees(text: &str) -> Vec<WorktreeEntry> {
    let mut out = Vec::new();
    let mut cur: Option<WorktreeEntry> = None;
    for line in text.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if let Some(w) = cur.take() {
                out.push(w);
            }
            continue;
        }
        if let Some(p) = line.strip_prefix("worktree ") {
            if let Some(w) = cur.take() {
                out.push(w);
            }
            cur = Some(WorktreeEntry {
                path: p.to_owned(),
                head: None,
                branch: None,
                bare: false,
            });
        } else if let Some(w) = cur.as_mut() {
            if let Some(h) = line.strip_prefix("HEAD ") {
                w.head = oid_ok(h).then(|| h.to_owned());
            } else if let Some(b) = line.strip_prefix("branch ") {
                w.branch = Some(b.to_owned());
            } else if line == "bare" {
                w.bare = true;
            }
        }
    }
    out
}

pub fn parse_tree(out: &[u8]) -> Result<(Vec<TreeEntry>, Vec<String>), GitFault> {
    let mut entries = Vec::new();
    let mut refused = Vec::new();
    for rec in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let tab = rec
            .iter()
            .position(|b| *b == b'\t')
            .ok_or_else(|| GitFault::Malformed("ls-tree record".into()))?;
        let head = std::str::from_utf8(&rec[..tab])
            .map_err(|_| GitFault::Malformed("ls-tree header".into()))?;
        let mut parts = head.split_whitespace();
        let (Some(mode), Some(kind), Some(oid), Some(size)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(GitFault::Malformed("ls-tree fields".into()));
        };
        if kind != "blob" && kind != "commit" {
            continue;
        }
        let Some(path) = path_of(&rec[tab + 1..]) else {
            refused.push(String::from_utf8_lossy(&rec[tab + 1..]).into_owned());
            continue;
        };
        entries.push(TreeEntry {
            mode: mode.to_owned(),
            oid: oid.to_owned(),
            size: size.parse().unwrap_or(0),
            path,
        });
    }
    Ok((entries, refused))
}

pub fn parse_name_status(out: &[u8]) -> Result<(Vec<Change>, Vec<String>), GitFault> {
    let fields: Vec<&[u8]> = out.split(|b| *b == 0).collect();
    let mut i = 0;
    let mut changes = Vec::new();
    let mut refused = Vec::new();
    while i < fields.len() {
        let status = fields[i];
        if status.is_empty() {
            i += 1;
            continue;
        }
        let code = status[0];
        let two = matches!(code, b'R' | b'C');
        let need = if two { 2 } else { 1 };
        if i + need >= fields.len() || fields[i + 1..=i + need].iter().any(|f| f.is_empty()) {
            return Err(GitFault::Malformed("name-status record".into()));
        }
        let first = fields.get(i + 1).copied().unwrap_or_default();
        let second = fields.get(i + 2).copied().unwrap_or_default();
        i += 1 + need;
        let (a, b) = (path_of(first), if two { path_of(second) } else { None });
        match code {
            b'R' => match (a, b) {
                (Some(from), Some(to)) => changes.push(Change::Renamed { from, to }),
                (Some(from), None) => {
                    refused.push(String::from_utf8_lossy(second).into_owned());
                    changes.push(Change::Deleted(from));
                }
                (None, Some(to)) => {
                    refused.push(String::from_utf8_lossy(first).into_owned());
                    changes.push(Change::Added(to));
                }
                (None, None) => refused.push(String::from_utf8_lossy(first).into_owned()),
            },
            // A copy keeps its source: only the new path changed.
            b'C' => match b {
                Some(to) => changes.push(Change::Added(to)),
                None => refused.push(String::from_utf8_lossy(second).into_owned()),
            },
            _ => match a {
                None => refused.push(String::from_utf8_lossy(first).into_owned()),
                Some(p) => changes.push(match code {
                    b'A' => Change::Added(p),
                    b'D' => Change::Deleted(p),
                    _ => Change::Modified(p),
                }),
            },
        }
    }
    Ok((changes, refused))
}

pub fn parse_batch(out: &[u8], oids: &[String]) -> Result<Vec<Vec<u8>>, GitFault> {
    let mut pos = 0;
    let mut blobs = Vec::with_capacity(oids.len());
    for oid in oids {
        let nl = out[pos..]
            .iter()
            .position(|b| *b == b'\n')
            .ok_or_else(|| GitFault::Malformed("cat-file header".into()))?;
        let header = std::str::from_utf8(&out[pos..pos + nl])
            .map_err(|_| GitFault::Malformed("cat-file header".into()))?;
        pos += nl + 1;
        let mut parts = header.split(' ');
        let (Some(got), Some(kind)) = (parts.next(), parts.next()) else {
            return Err(GitFault::Malformed("cat-file header".into()));
        };
        if got != oid {
            return Err(GitFault::Malformed(
                "cat-file answered another object".into(),
            ));
        }
        if kind == "missing" {
            return Err(GitFault::Malformed(format!("object {oid} is missing")));
        }
        let size: usize = parts
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| GitFault::Malformed("cat-file size".into()))?;
        if pos + size + 1 > out.len() {
            return Err(GitFault::Malformed("cat-file output ended early".into()));
        }
        blobs.push(out[pos..pos + size].to_vec());
        pos += size + 1;
    }
    Ok(blobs)
}

#[cfg(test)]
mod tests {
    #[test]
    fn revision_observation_refuses_head_races_and_keeps_non_utf8_dirty_paths() {
        use crate::context::{InvocationContext, PrincipalKind};
        use crate::protocol::{ActorId, Counter, Id, Revision, StoreId};
        use std::collections::VecDeque;
        use std::sync::Mutex;
        struct Replies(Mutex<VecDeque<Vec<u8>>>);
        impl HelperRunner for Replies {
            fn run(
                &self,
                _: &InvocationContext,
                _: ProcessSpec,
                _: HelperLimits,
                _: &(dyn Fn() -> bool + Sync),
            ) -> Result<Vec<u8>, HelperFault> {
                Ok(self
                    .0
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("expected Git call"))
            }
        }
        let ctx = InvocationContext::new(
            ActorId(Id::new("host").unwrap()),
            PrincipalKind::Host,
            Id::new("invocation").unwrap(),
            Id::new("git").unwrap(),
            StoreId(Id::new("store").unwrap()),
            Counter::new(0).unwrap(),
            Revision::new(1).unwrap(),
        );
        let head = "a".repeat(40);
        for (status, changed) in [
            (Vec::new(), false),
            (b"?? invalid-\xff\0".to_vec(), false),
            (Vec::new(), true),
        ] {
            let after = if changed {
                "b".repeat(40)
            } else {
                head.clone()
            };
            let dirty = !status.is_empty();
            let runner = Replies(Mutex::new(VecDeque::from([
                head.as_bytes().to_vec(),
                status,
                after.into_bytes(),
            ])));
            let git = Git {
                runner: &runner,
                ctx: &ctx,
                program: "git",
                deadline: Duration::from_secs(5),
                stop: &|| false,
            };
            match git.revision_observation(Path::new("/owned/worktree")) {
                Ok(observed) => {
                    assert!(!changed);
                    assert_eq!(observed.head, head);
                    assert_eq!(observed.dirty, dirty);
                }
                Err(GitFault::Malformed(message)) => {
                    assert!(changed);
                    assert!(message.contains("HEAD changed"));
                }
                other => panic!("{other:?}"),
            }
        }
    }

    use super::*;

    #[test]
    fn a_blob_read_admits_the_largest_accepted_file() {
        let largest = super::super::config::MAX_FILE_BYTES_LIMIT as usize;
        // One file at the accepted maximum, with its header, fits the read bound.
        assert!(blob_output_limit(largest, 1) >= largest + 100);
        // So does a whole extraction group at its maximum.
        let group = super::super::worker::MAX_GROUP_BYTES;
        assert!(blob_output_limit(group, 200) >= group + 200 * 100);
        // A small read is bounded by what it expects.
        assert_eq!(blob_output_limit(10, 1), 10 + 128);
    }

    #[test]
    fn name_status_parses_renames_copies_and_refuses_escapes() {
        let out = b"M\0src/a.rs\0R087\0old.rs\0new.rs\0C100\0x.rs\0y.rs\0D\0gone.rs\0A\0../evil\0A\0ok/b.py\0";
        let (c, refused) = parse_name_status(out).unwrap();
        assert_eq!(
            c,
            vec![
                Change::Modified("src/a.rs".into()),
                Change::Renamed {
                    from: "old.rs".into(),
                    to: "new.rs".into()
                },
                Change::Added("y.rs".into()),
                Change::Deleted("gone.rs".into()),
                Change::Added("ok/b.py".into()),
            ]
        );
        assert_eq!(refused, vec!["../evil".to_owned()]);
        assert!(parse_name_status(b"R100\0only-one\0").is_err());
    }

    #[test]
    fn tree_and_batch_parse_and_check_identity() {
        let oid = "a".repeat(40);
        let tree = format!(
            "100644 blob {oid}     12\tsrc/lib.rs\0160000 commit {oid}       -\tvendor/sub\0"
        );
        let (entries, refused) = parse_tree(tree.as_bytes()).unwrap();
        assert!(refused.is_empty());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].size, 12);
        assert!(entries[1].submodule());
        let batch = format!("{oid} blob 3\nabc\n");
        assert_eq!(
            parse_batch(batch.as_bytes(), std::slice::from_ref(&oid)).unwrap(),
            vec![b"abc".to_vec()]
        );
        let other = "b".repeat(40);
        assert!(parse_batch(batch.as_bytes(), &[other]).is_err());
        assert!(
            parse_batch(
                format!("{oid} missing\n").as_bytes(),
                std::slice::from_ref(&oid)
            )
            .is_err()
        );
        assert!(parse_batch(format!("{oid} blob 9\nab\n").as_bytes(), &[oid]).is_err());
    }

    #[test]
    fn worktree_listing_parses_porcelain() {
        let oid = "c".repeat(40);
        let w = parse_worktrees(&format!(
            "worktree /src/a\nHEAD {oid}\nbranch refs/heads/main\n\nworktree /src/a-wt\nHEAD {oid}\ndetached\n\n"
        ));
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].branch.as_deref(), Some("refs/heads/main"));
        assert_eq!(w[1].branch, None);
        assert_eq!(w[1].head.as_deref(), Some(oid.as_str()));
    }
}
