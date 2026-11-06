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
use crate::protocol::OperationId;
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
    pub branch: Option<String>,
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
    SourceChanged,
    Unborn,
    Unqualified,
    /// The reviewed mutation cannot run Git with its required helper-free launch policy.
    ReviewedUnavailable,
    /// Output that does not parse as Git's documented format.
    Malformed(String),
}
impl std::fmt::Display for GitFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Helper(h) => write!(f, "git {h}"),
            Self::SourceChanged => f.write_str("Git HEAD changed during observation"),
            Self::Unqualified => {
                f.write_str("dirty-state reading cannot safely qualify this checkout")
            }
            Self::ReviewedUnavailable => {
                f.write_str("helper-free reviewed Git execution is unavailable")
            }
            Self::Unborn => f.write_str("unborn HEAD has no tested revision"),
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

/// Represent a filesystem path for Git without changing stored paths or identity witnesses.
/// Never apply this to remote URLs or configuration values: Git rewrites bind their original
/// string. NT-only Windows names cannot safely lose their verbatim namespace.
pub fn path_argument(path: &Path) -> Result<String, GitFault> {
    let value = path.to_str().ok_or(GitFault::Unqualified)?;
    #[cfg(windows)]
    {
        windows_path_argument(value).ok_or(GitFault::Unqualified)
    }
    #[cfg(not(windows))]
    {
        Ok(value.to_owned())
    }
}

#[cfg(any(windows, test))]
fn windows_path_argument(value: &str) -> Option<String> {
    if value.starts_with(r"\\.\") {
        return None;
    }
    if let Some(path) = value.strip_prefix(r"\\?\UNC\") {
        return path
            .split(['\\', '/'])
            .all(windows_component_safe)
            .then(|| format!(r"\\{path}"));
    }
    if let Some(path) = value.strip_prefix(r"\\?\") {
        return (path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            && path.as_bytes().get(1) == Some(&b':')
            && path
                .as_bytes()
                .get(2)
                .is_some_and(|b| matches!(b, b'\\' | b'/'))
            && path[3..].split(['\\', '/']).all(windows_component_safe))
        .then(|| path.to_owned());
    }
    Some(value.to_owned())
}

#[cfg(any(windows, test))]
fn windows_component_safe(part: &str) -> bool {
    if part.ends_with(['.', ' '])
        || part
            .chars()
            .any(|c| c.is_control() || ":*?\"<>|".contains(c))
    {
        return false;
    }
    let stem = part
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    !matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
    ) && !stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|n| {
            matches!(
                n,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
}

fn supported_reviewed_version(version: &str) -> bool {
    if version.lines().count() != 1 {
        return false;
    }
    let Some(number) = version
        .strip_prefix("git version ")
        .and_then(|v| v.split_whitespace().next())
    else {
        return false;
    };
    let mut parts = number.split('.');
    matches!((parts.next().and_then(|v| v.parse::<u64>().ok()), parts.next().and_then(|v| v.parse::<u64>().ok())), (Some(major), Some(minor)) if (major, minor) >= (2, 31))
}

/// Git for Windows' cmd/git.exe is a launcher. The --exec-path from the trusted configured
/// distribution identifies the actual mingw binary; never guess a different installation.
#[cfg(any(windows, test))]
fn windows_reviewed_program(exec_path: &str) -> Option<String> {
    let path = windows_path_argument(exec_path)?.replace('\\', "/");
    let rest = if path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        && path.as_bytes().get(1) == Some(&b':')
        && path.as_bytes().get(2) == Some(&b'/')
    {
        &path[3..]
    } else if let Some(rest) = path.strip_prefix("//") {
        rest
    } else {
        return None;
    };
    if rest
        .split('/')
        .any(|p| p.is_empty() || !windows_component_safe(p))
    {
        return None;
    }
    let distribution = path.strip_suffix("/libexec/git-core")?;
    if !matches!(
        distribution.rsplit('/').next()?,
        "mingw64" | "mingw32" | "clangarm64" | "ucrt64"
    ) {
        return None;
    }
    Some(format!("{distribution}/bin/git.exe"))
}

#[cfg(test)]
mod process_argument_tests {
    use super::windows_path_argument;

    #[test]
    fn canonical_windows_paths_keep_their_target_in_git_arguments() {
        for (input, expected) in [
            (r"\\?\C:\work tree\checkout", r"C:\work tree\checkout"),
            (r"\\?\UNC\server\share\tree", r"\\server\share\tree"),
            (r"C:\ordinary\tree", r"C:\ordinary\tree"),
        ] {
            assert_eq!(windows_path_argument(input).as_deref(), Some(expected));
        }
        for input in [
            r"\\?\Volume{device}\tree",
            r"\\.\PhysicalDrive0",
            r"\\?\C:\folder.\tree",
            r"\\?\C:\folder \tree",
            r"\\?\C:\AUX.txt\tree",
            r"\\?\C:\COM¹\tree",
            r"\\?\C:\folder\..\tree",
        ] {
            assert!(windows_path_argument(input).is_none(), "{input}");
        }
    }
}

impl Git<'_> {
    pub fn version(&self, dir: &Path) -> Result<String, GitFault> {
        self.line(dir, &["version"])?
            .ok_or_else(|| GitFault::Malformed("Git version unavailable".into()))
    }

    fn spec(&self, dir: &Path, args: &[&str], stdin: Option<Vec<u8>>) -> ProcessSpec {
        let mut argv: Vec<String> = vec![self.program.to_owned()];
        argv.extend(args.iter().map(|a| (*a).to_owned()));
        let mut spec = ProcessSpec::new(&argv).expect("a program");
        spec.cwd = Some(PathBuf::from(dir));
        // Host-fixed hardening through Git's environment configuration (never caller input):
        // no file-system monitor program runs for a read, no optional lock is taken and no
        // credential prompt can block. External diff and text conversion drivers are disabled
        // per command (`--no-ext-diff`, `--no-textconv`).
        // Git locator/configuration environment must not redirect an owned-worktree read.
        // Preserve the native host environment (PATH/SystemRoot etc.) without inherited GIT_*
        // overrides, then apply only this helper's fixed configuration.
        spec.clear_env = true;
        spec.env = self
            .runner
            .environment()
            .into_iter()
            .filter(|(key, _)| !key.to_ascii_uppercase().starts_with("GIT_"))
            .collect();
        spec.env.extend(vec![
            ("GIT_CONFIG_COUNT".into(), "2".into()),
            ("GIT_CONFIG_KEY_0".into(), "core.fsmonitor".into()),
            // Empty disables fsmonitor on the supported Git 2.31 floor too; older Git
            // interprets the nonempty string "false" as a monitor command pathname.
            ("GIT_CONFIG_VALUE_0".into(), String::new()),
            // v0/v1 hide non-HEAD symbolic refs. Local and qualified GitHub transports must
            // request v2 so a task-name alias cannot conceal a protected destination.
            ("GIT_CONFIG_KEY_1".into(), "protocol.version".into()),
            ("GIT_CONFIG_VALUE_1".into(), "2".into()),
            ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
            ("GIT_LITERAL_PATHSPECS".into(), "1".into()),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ("LC_ALL".into(), "C".into()),
        ]);
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

    /// Execute one fixed Git mutation under its committed operation identity. The caller
    /// owns branch/path/authority validation and must observe the postcondition separately.
    pub(crate) fn mutation(
        &self,
        dir: &Path,
        args: &[&str],
        stdin: Option<Vec<u8>>,
        operation: &OperationId,
        approved_hold: bool,
    ) -> Result<Vec<u8>, GitFault> {
        Ok(self.runner.run_recorded(
            self.ctx,
            self.spec(dir, args, stdin),
            HelperLimits {
                deadline: self.deadline,
                max_stdout: 1024 * 1024,
            },
            self.stop,
            operation,
            approved_hold,
        )?)
    }

    /// Qualify the actual executable under the same process policy used by reviewed writes.
    /// This runs before proposal creation, outside any storage writer. A Windows Git launcher
    /// cannot spawn its real Git under this policy, so locate the trusted distribution first.
    fn reviewed_program(&self, dir: &Path) -> Result<(String, String), GitFault> {
        #[cfg(windows)]
        let program = {
            let path = self
                .line(dir, &["--exec-path"])?
                .ok_or(GitFault::ReviewedUnavailable)?;
            windows_reviewed_program(&path).ok_or(GitFault::ReviewedUnavailable)?
        };
        #[cfg(not(windows))]
        let program = self.program.to_owned();
        let mut spec = self.spec(dir, &["--version"], None);
        spec.program = program.clone();
        spec.no_child_processes = true;
        let output = self
            .runner
            .run(
                self.ctx,
                spec,
                HelperLimits {
                    deadline: self.deadline,
                    max_stdout: 64 * 1024,
                },
                self.stop,
            )
            .map_err(|error| match error {
                HelperFault::Cancelled
                | HelperFault::TimedOut
                | HelperFault::Storage(_)
                | HelperFault::Uncertain(_) => GitFault::Helper(error),
                _ => GitFault::ReviewedUnavailable,
            })?;
        let version = std::str::from_utf8(&output)
            .map_err(|_| GitFault::ReviewedUnavailable)?
            .trim();
        if !supported_reviewed_version(version) {
            return Err(GitFault::ReviewedUnavailable);
        }
        Ok((program, version.to_owned()))
    }

    /// Names only: hook command bodies may contain secrets and never enter approval evidence.
    pub fn reviewed_commit_policy(&self, dir: &Path) -> Result<serde_json::Value, GitFault> {
        let (program, version) = self.reviewed_program(dir)?;
        let output = match self.run(
            dir,
            &[
                "config",
                "--null",
                "--name-only",
                "--get-regexp",
                "^hook\\..*\\.(event|command|parallel|enabled)$",
            ],
            64 * 1024,
        ) {
            Ok(bytes) => bytes,
            Err(GitFault::Helper(HelperFault::Failed { code: Some(1), .. })) => Vec::new(),
            Err(error) => return Err(error),
        };
        let mut names = std::collections::BTreeSet::new();
        for key in output.split(|b| *b == 0).filter(|key| !key.is_empty()) {
            let key = std::str::from_utf8(key).map_err(|_| GitFault::Unqualified)?;
            let name = key
                .strip_prefix("hook.")
                .and_then(|key| key.rsplit_once('.'))
                .map(|(name, _)| name)
                .ok_or(GitFault::Unqualified)?;
            if name.is_empty() || name.len() > 4096 || name.chars().any(char::is_control) {
                return Err(GitFault::Unqualified);
            }
            names.insert(name.to_owned());
            if names.len() > 256 {
                return Err(GitFault::Unqualified);
            }
        }
        Ok(serde_json::json!({"gitVersion":version,"gitProgram":program,"hookNames":names}))
    }

    /// Exact reviewed commits authorize contents, never repository hooks or signer programs.
    /// The current hook-name/version witness is checked on either side of every index write.
    pub(crate) fn reviewed_mutation(
        &self,
        dir: &Path,
        args: &[&str],
        stdin: Option<Vec<u8>>,
        operation: &OperationId,
        expected: &serde_json::Value,
    ) -> Result<Vec<u8>, GitFault> {
        let policy = self.reviewed_commit_policy(dir)?;
        if &policy != expected {
            return Err(GitFault::SourceChanged);
        }
        let mut spec = self.spec(dir, args, stdin);
        spec.program = policy["gitProgram"]
            .as_str()
            .ok_or(GitFault::ReviewedUnavailable)?
            .to_owned();
        spec.no_child_processes = true;
        let mut overrides = vec![
            ("index.threads".to_owned(), "1".to_owned()),
            ("pack.threads".to_owned(), "1".to_owned()),
            ("core.preloadIndex".to_owned(), "false".to_owned()),
            ("checkout.workers".to_owned(), "1".to_owned()),
            ("gc.auto".to_owned(), "0".to_owned()),
            ("maintenance.auto".to_owned(), "false".to_owned()),
            ("core.hooksPath".to_owned(), "/dev/null".to_owned()),
            ("commit.gpgSign".to_owned(), "false".to_owned()),
        ];
        for name in policy["hookNames"]
            .as_array()
            .ok_or(GitFault::Unqualified)?
        {
            let name = name.as_str().ok_or(GitFault::Unqualified)?;
            overrides.push((format!("hook.{name}.event"), String::new()));
            overrides.push((format!("hook.{name}.enabled"), "false".to_owned()));
        }
        let mut count = 2;
        for (key, value) in overrides {
            spec.env.push((format!("GIT_CONFIG_KEY_{count}"), key));
            spec.env.push((format!("GIT_CONFIG_VALUE_{count}"), value));
            count += 1;
        }
        spec.env
            .iter_mut()
            .find(|(key, _)| key == "GIT_CONFIG_COUNT")
            .expect("fixed helper configuration")
            .1 = count.to_string();
        let result = self.runner.run_recorded(
            self.ctx,
            spec,
            HelperLimits {
                deadline: self.deadline,
                max_stdout: 1024 * 1024,
            },
            self.stop,
            operation,
            false,
        )?;
        if self.reviewed_commit_policy(dir)? != policy {
            return Err(GitFault::SourceChanged);
        }
        Ok(result)
    }

    /// Resolve one literal ref, with no revision-expression interpretation.
    pub fn branch_head(&self, dir: &Path, reference: &str) -> Result<Option<String>, GitFault> {
        if !reference.starts_with("refs/heads/") || reference.contains(['\0', '\n']) {
            return Err(GitFault::Malformed("invalid branch ref".into()));
        }
        self.line(dir, &["show-ref", "--verify", "--hash", reference])
    }

    /// All push destinations of the named remote. The Git service requires exactly one and
    /// binds it to the journal; execution uses the resolved URL rather than re-reading a name.
    pub fn push_urls(&self, dir: &Path, remote: &str) -> Result<Vec<String>, GitFault> {
        if remote.starts_with('-') || remote.contains(['\0', '\n', ':', '/']) {
            return Err(GitFault::Malformed("invalid remote name".into()));
        }
        let out = self.run(
            dir,
            &["remote", "get-url", "--push", "--all", remote],
            64 * 1024,
        )?;
        let text =
            String::from_utf8(out).map_err(|_| GitFault::Malformed("remote encoding".into()))?;
        Ok(text.lines().map(str::to_owned).collect())
    }

    /// A resolved remote URL must remain literal when passed back to Git. Refuse a second
    /// pushInsteadOf rewrite, then ask Git itself about insteadOf without opening a connection
    /// (https://git-scm.com/docs/git-ls-remote#Documentation/git-ls-remote.txt---get-url).
    pub fn literal_push_url(&self, dir: &Path, url: &str) -> Result<String, GitFault> {
        let rules = match self.run(
            dir,
            &[
                "config",
                "--null",
                "--get-regexp",
                "^url\\..*\\.pushinsteadof$",
            ],
            64 * 1024,
        ) {
            Ok(rules) => rules,
            Err(GitFault::Helper(HelperFault::Failed { code: Some(1), .. })) => vec![],
            Err(error) => return Err(error),
        };
        for rule in rules.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let text = std::str::from_utf8(rule)
                .map_err(|_| GitFault::Malformed("push rewrite encoding".into()))?;
            let (_, prefix) = text
                .split_once('\n')
                .ok_or_else(|| GitFault::Malformed("push rewrite configuration".into()))?;
            if url.starts_with(prefix) {
                return Err(GitFault::Malformed(
                    "literal push URL matches pushInsteadOf".into(),
                ));
            }
        }
        self.line(dir, &["ls-remote", "--get-url", "--", url])?
            .ok_or_else(|| GitFault::Malformed("literal push URL unavailable".into()))
    }

    pub fn remote_default(&self, dir: &Path, url: &str) -> Result<Option<String>, GitFault> {
        let bytes = self.run(
            dir,
            &["ls-remote", "--symref", "--", url, "HEAD"],
            64 * 1024,
        )?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| GitFault::Malformed("remote HEAD encoding".into()))?;
        let mut found = None;
        for line in text.lines() {
            if let Some(reference) = line
                .strip_prefix("ref: ")
                .and_then(|r| r.strip_suffix("\tHEAD"))
            {
                if !reference.starts_with("refs/heads/") || found.is_some() {
                    return Err(GitFault::Malformed("remote symbolic HEAD".into()));
                }
                found = Some(reference.to_owned());
            }
        }
        Ok(found)
    }

    /// Native absolute Git metadata paths. Remove the output terminator only, preserving
    /// legitimate embedded whitespace in paths.
    pub fn metadata_dirs(&self, dir: &Path) -> Result<(String, String), GitFault> {
        Ok((
            self.absolute_path(dir, "--git-dir")?,
            self.absolute_path(dir, "--git-common-dir")?,
        ))
    }
    pub fn worktree_root(&self, dir: &Path) -> Result<String, GitFault> {
        self.absolute_path(dir, "--show-toplevel")
    }
    fn absolute_path(&self, dir: &Path, option: &str) -> Result<String, GitFault> {
        let mut bytes = self.run(
            dir,
            &["rev-parse", "--path-format=absolute", option],
            64 * 1024,
        )?;
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
            if cfg!(windows) && bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
        }
        let path = String::from_utf8(bytes)
            .map_err(|_| GitFault::Malformed("Git metadata path encoding".into()))?;
        if !Path::new(&path).is_absolute() {
            return Err(GitFault::Malformed(
                "Git metadata path is not absolute".into(),
            ));
        }
        Ok(path)
    }

    /// A remote branch may itself be a symbolic ref. Updating that alias would update its
    /// destination, so owned-branch authority cannot authorize it (not even with an OID lease).
    pub fn remote_symbolic_target(
        &self,
        dir: &Path,
        url: &str,
        reference: &str,
    ) -> Result<Option<String>, GitFault> {
        let bytes = self.run(
            dir,
            &["ls-remote", "--symref", "--", url, reference],
            64 * 1024,
        )?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| GitFault::Malformed("remote symbolic ref encoding".into()))?;
        let mut found = None;
        for line in text.lines() {
            if let Some(record) = line.strip_prefix("ref: ") {
                let (target, name) = record
                    .split_once('\t')
                    .ok_or_else(|| GitFault::Malformed("remote symbolic ref".into()))?;
                if name == reference {
                    if target.is_empty() || found.is_some() {
                        return Err(GitFault::Malformed("ambiguous remote symbolic ref".into()));
                    }
                    found = Some(target.to_owned());
                }
            }
        }
        Ok(found)
    }

    /// One remote branch's exact head. An absent ref is distinct from a failed observation.
    pub fn remote_head(
        &self,
        dir: &Path,
        url: &str,
        reference: &str,
    ) -> Result<Option<String>, GitFault> {
        let out = self.run(
            dir,
            &["ls-remote", "--refs", "--", url, reference],
            64 * 1024,
        )?;
        let text = String::from_utf8(out)
            .map_err(|_| GitFault::Malformed("remote ref encoding".into()))?;
        let lines: Vec<_> = text.lines().collect();
        match lines.as_slice() {
            [] => Ok(None),
            [line] => match line.split_once('\t') {
                Some((head, found)) if found == reference && oid_ok(head) => Ok(Some(head.into())),
                _ => Err(GitFault::Malformed("remote ref response".into())),
            },
            _ => Err(GitFault::Malformed("ambiguous remote ref response".into())),
        }
    }
    /// Whether the observed remote commit is an ancestor of the exact local head. Missing
    /// local objects are a refusal, never a reason to infer a fast forward.
    pub fn is_ancestor(&self, dir: &Path, base: &str, head: &str) -> Result<bool, GitFault> {
        if !oid_ok(base) || !oid_ok(head) {
            return Err(GitFault::Malformed("invalid ancestry IDs".into()));
        }
        match self.run(dir, &["merge-base", "--is-ancestor", base, head], 1024) {
            Ok(_) => Ok(true),
            Err(GitFault::Helper(HelperFault::Failed { code: Some(1), .. })) => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub fn commit_message(&self, dir: &Path, head: &str) -> Result<String, GitFault> {
        if !oid_ok(head) {
            return Err(GitFault::Malformed("invalid commit ID".into()));
        }
        let bytes = self.run(dir, &["cat-file", "commit", head], 64 * 1024 + 1024)?;
        let body = bytes
            .windows(2)
            .position(|w| w == b"\n\n")
            .ok_or_else(|| GitFault::Malformed("commit headers".into()))?;
        String::from_utf8(bytes[body + 2..].to_vec())
            .map_err(|_| GitFault::Malformed("commit message encoding".into()))
    }

    /// Parents of an exact commit object (never a revision expression).
    pub fn commit_parents(&self, dir: &Path, head: &str) -> Result<Vec<String>, GitFault> {
        if !oid_ok(head) {
            return Err(GitFault::Malformed("invalid commit ID".into()));
        }
        let parents = self
            .line(dir, &["show", "--no-patch", "--format=%P", head])?
            .unwrap_or_default();
        let parents: Vec<String> = parents.split_whitespace().map(str::to_owned).collect();
        if parents.iter().any(|p| !oid_ok(p)) {
            return Err(GitFault::Malformed("invalid commit parents".into()));
        }
        Ok(parents)
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
        let before = self.head(dir)?.ok_or(GitFault::Unborn)?;
        let dirty = self.safe_dirty(dir, 0)?;
        let branch = self.branch(dir)?;
        let after = self.head(dir)?;
        if after.as_deref() != Some(before.as_str()) {
            return Err(GitFault::SourceChanged);
        }
        Ok(RevisionObservation {
            head: before,
            dirty,
            branch,
        })
    }

    /// Head-only preparation never needs to hash worktree contents or run clean filters.
    pub fn head_observation(&self, dir: &Path) -> Result<RevisionObservation, GitFault> {
        let head = self.head(dir)?.ok_or(GitFault::Unborn)?;
        let branch = self.branch(dir)?;
        if self.head(dir)?.as_ref() != Some(&head) {
            return Err(GitFault::SourceChanged);
        }
        Ok(RevisionObservation {
            head,
            branch,
            dirty: false,
        })
    }

    /// Git may run arbitrary clean/process filters while computing status. Determine whether
    /// tracked or untracked paths need them before hashing; never invent a dirty bit using raw
    /// bytes in place of a configured canonical transformation. Submodules are read separately so
    /// each repository's local filter configuration receives the same checks.
    fn safe_dirty(&self, dir: &Path, depth: usize) -> Result<bool, GitFault> {
        if depth > 32 {
            return Err(GitFault::Unqualified);
        }
        let config = match self.run(
            dir,
            &[
                "config",
                "--null",
                "--get-regexp",
                "^filter\\..*\\.(clean|process)$",
            ],
            MAX_LISTING_BYTES,
        ) {
            Ok(out) => out,
            Err(GitFault::Helper(HelperFault::Failed { code: Some(1), .. })) => vec![],
            Err(e) => return Err(e),
        };
        let mut filters = std::collections::BTreeSet::new();
        let mut overrides = std::collections::BTreeSet::new();
        for record in config.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let (key, value) = record.split_at(
                record
                    .iter()
                    .position(|b| *b == b'\n')
                    .ok_or_else(|| GitFault::Malformed("filter configuration".into()))?,
            );
            let key = std::str::from_utf8(key).map_err(|_| GitFault::Unqualified)?;
            let name = key
                .strip_prefix("filter.")
                .and_then(|k| k.rsplit_once('.'))
                .map(|(n, _)| n)
                .ok_or(GitFault::Unqualified)?;
            overrides.insert(name.to_owned());
            if value.len() > 1 {
                filters.insert(name.to_owned());
            }
        }
        let listing = self.run(dir, &["ls-files", "--stage", "-z"], MAX_LISTING_BYTES)?;
        let mut paths = Vec::new();
        let mut submodules = Vec::new();
        for record in listing.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let tab = record
                .iter()
                .position(|b| *b == b'\t')
                .ok_or_else(|| GitFault::Malformed("index entry".into()))?;
            paths.extend_from_slice(&record[tab + 1..]);
            paths.push(0);
            if record.starts_with(b"160000 ") {
                let path =
                    std::str::from_utf8(&record[tab + 1..]).map_err(|_| GitFault::Unqualified)?;
                if !relative_ok(path) {
                    return Err(GitFault::Unqualified);
                }
                let header =
                    std::str::from_utf8(&record[..tab]).map_err(|_| GitFault::Unqualified)?;
                submodules.push((
                    path.to_owned(),
                    header
                        .split_whitespace()
                        .nth(1)
                        .ok_or(GitFault::Unqualified)?
                        .to_owned(),
                ));
            }
        }
        if !filters.is_empty() {
            // A no-index diff applies clean/process conversion to new files too. Qualify all
            // untracked paths without decoding or line-splitting their NUL-delimited names.
            let untracked = self.run(
                dir,
                &["ls-files", "-z", "--others", "--exclude-standard"],
                MAX_LISTING_BYTES,
            )?;
            paths.extend_from_slice(&untracked);
        }
        if !filters.is_empty() && !paths.is_empty() {
            let attributes = self.runner.run(
                self.ctx,
                self.spec(dir, &["check-attr", "-z", "--stdin", "filter"], Some(paths)),
                HelperLimits {
                    deadline: self.deadline,
                    max_stdout: MAX_LISTING_BYTES,
                },
                self.stop,
            )?;
            let fields: Vec<_> = attributes
                .split(|b| *b == 0)
                .filter(|r| !r.is_empty())
                .collect();
            if fields.len() % 3 != 0 {
                return Err(GitFault::Malformed("filter attributes".into()));
            }
            for attr in fields.as_chunks::<3>().0 {
                if filters.iter().any(|name| name.as_bytes() == attr[2]) {
                    return Err(GitFault::Unqualified);
                }
            }
        }
        let mut spec = self.spec(
            dir,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=normal",
                "--ignore-submodules=dirty",
            ],
            None,
        );
        // Even an attribute change between the check and status cannot select an executable
        // from the observed configuration. Configuration itself is host-controlled state.
        let mut count = 2;
        for name in overrides {
            for (suffix, value) in [("clean", ""), ("process", ""), ("required", "false")] {
                spec.env.push((
                    format!("GIT_CONFIG_KEY_{count}"),
                    format!("filter.{name}.{suffix}"),
                ));
                spec.env
                    .push((format!("GIT_CONFIG_VALUE_{count}"), value.into()));
                count += 1;
            }
        }
        spec.env
            .iter_mut()
            .find(|(k, _)| k == "GIT_CONFIG_COUNT")
            .unwrap()
            .1 = count.to_string();
        let status = self.runner.run(
            self.ctx,
            spec,
            HelperLimits {
                deadline: self.deadline,
                max_stdout: MAX_LISTING_BYTES,
            },
            self.stop,
        )?;
        let mut dirty = !status.is_empty();
        for (path, expected) in submodules {
            let child = dir.join(path);
            // An uninitialized submodule has no checkout and contributes no worktree dirt.
            // rev-parse would otherwise walk up to its parent repository.
            let fs = self.runner.filesystem().ok_or(GitFault::Unqualified)?;
            let marker = fs
                .resolve(
                    child.join(".git").to_str().ok_or(GitFault::Unqualified)?,
                    None,
                )
                .map_err(|_| GitFault::SourceChanged)?;
            if marker.target.is_none() {
                continue;
            }
            let root = self.worktree_root(&child)?;
            let expected_root = fs
                .resolve(child.to_str().ok_or(GitFault::Unqualified)?, None)
                .map_err(|_| GitFault::SourceChanged)?;
            let actual_root = fs
                .resolve(&root, None)
                .map_err(|_| GitFault::SourceChanged)?;
            if expected_root
                .target
                .as_ref()
                .and_then(|t| t.identity.as_ref())
                != actual_root
                    .target
                    .as_ref()
                    .and_then(|t| t.identity.as_ref())
            {
                return Err(GitFault::SourceChanged);
            }
            dirty |= self.head(&child)?.as_deref() != Some(&expected);
            dirty |= self.safe_dirty(&child, depth + 1)?;
        }
        Ok(dirty)
    }

    /// A complete bounded patch for immutable commits. Path selectors are literal, never
    /// Git pathspec programs or command options.
    pub fn patch(
        &self,
        dir: &Path,
        base: &str,
        head: &str,
        paths: &[String],
    ) -> Result<Vec<u8>, GitFault> {
        if !oid_ok(base) || !oid_ok(head) || paths.iter().any(|p| !relative_ok(p)) {
            return Err(GitFault::Malformed("diff identity or path".into()));
        }
        let mut args = vec![
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            "--submodule=short",
            base,
            head,
            "--",
        ];
        args.extend(paths.iter().map(String::as_str));
        self.run(dir, &args, MAX_LISTING_BYTES)
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
    /// Exact bounded tracked/index/untracked commit contents, including symlink target text.
    /// Patches are captured twice; a changing source is never presented as one stable review.
    /// No temporary index, staging, object write or source-file publication occurs here.
    pub fn commit_manifest(&self, dir: &Path, base: &str) -> Result<serde_json::Value, GitFault> {
        let first = self.capture_commit_manifest(dir, base, false, None)?;
        if self.capture_commit_manifest(dir, base, false, None)? != first {
            return Err(GitFault::SourceChanged);
        }
        Ok(first)
    }
    /// The exact index about to be committed. Matching the reviewed worktree manifest after
    /// `add --all` catches a content/filter/index race before the commit is admitted.
    pub fn staged_commit_manifest(
        &self,
        dir: &Path,
        base: &str,
    ) -> Result<serde_json::Value, GitFault> {
        let first = self.capture_commit_manifest(dir, base, true, None)?;
        if self.capture_commit_manifest(dir, base, true, None)? != first {
            return Err(GitFault::SourceChanged);
        }
        Ok(first)
    }
    /// The actual committed tree changes, in the same representation as the reviewed source.
    /// Reading fixed object IDs proves contents independently of the current worktree/index.
    pub fn committed_commit_manifest(
        &self,
        dir: &Path,
        base: &str,
        head: &str,
    ) -> Result<serde_json::Value, GitFault> {
        if !oid_ok(head) {
            return Err(GitFault::Malformed("commit".into()));
        }
        let first = self.capture_commit_manifest(dir, base, false, Some(head))?;
        if self.capture_commit_manifest(dir, base, false, Some(head))? != first {
            return Err(GitFault::SourceChanged);
        }
        Ok(first)
    }
    fn capture_commit_manifest(
        &self,
        dir: &Path,
        base: &str,
        staged: bool,
        committed: Option<&str>,
    ) -> Result<serde_json::Value, GitFault> {
        if !oid_ok(base) {
            return Err(GitFault::Malformed("commit".into()));
        }
        const BOUND: usize = 16 * 1024 * 1024;
        let execution_policy = self.reviewed_commit_policy(dir)?;
        let observe = || {
            if committed.is_some() {
                self.head_observation(dir)
            } else {
                self.revision_observation(dir)
            }
        };
        let before = observe()?;
        if committed.is_some_and(|head| head != before.head) {
            return Err(GitFault::SourceChanged);
        }
        let mut args = vec!["diff"];
        if staged {
            args.push("--cached");
        }
        args.extend([
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--name-only",
            "-z",
            base,
        ]);
        args.extend(committed);
        args.push("--");
        let changed = self.run(dir, &args, MAX_LISTING_BYTES)?;
        let mut paths = std::collections::BTreeMap::new();
        for raw in changed.split(|b| *b == 0).filter(|p| !p.is_empty()) {
            let path =
                path_of(raw).ok_or_else(|| GitFault::Malformed("tracked approval path".into()))?;
            paths.insert(path, false);
        }
        if !staged && committed.is_none() {
            let untracked = self.run(
                dir,
                &["ls-files", "-z", "--others", "--exclude-standard"],
                MAX_LISTING_BYTES,
            )?;
            for raw in untracked.split(|b| *b == 0).filter(|p| !p.is_empty()) {
                let path = path_of(raw)
                    .ok_or_else(|| GitFault::Malformed("untracked approval path".into()))?;
                paths.insert(path, true);
            }
        }
        let honor_filemode = if paths.values().any(|untracked| *untracked) {
            match self
                .line(dir, &["config", "--bool", "--get", "core.fileMode"])?
                .as_deref()
            {
                None | Some("true") => true,
                Some("false") => false,
                _ => return Err(GitFault::Malformed("core.fileMode".into())),
            }
        } else {
            true
        };
        let mut files = Vec::new();
        let mut total = 0usize;
        for (path, untracked) in paths {
            let mut bytes = if untracked {
                self.runner.run_difference(
                    self.ctx,
                    self.spec(
                        dir,
                        &[
                            "diff",
                            "--no-index",
                            "--no-color",
                            "--no-ext-diff",
                            "--no-textconv",
                            "--binary",
                            "--no-renames",
                            "--src-prefix=a/",
                            "--dst-prefix=b/",
                            "--",
                            "/dev/null",
                            &path,
                        ],
                        None,
                    ),
                    HelperLimits {
                        deadline: self.deadline,
                        max_stdout: BOUND,
                    },
                    self.stop,
                )?
            } else {
                let mut args = vec!["diff"];
                if staged {
                    args.push("--cached");
                }
                args.extend([
                    "--no-color",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--binary",
                    "--no-renames",
                    "--src-prefix=a/",
                    "--dst-prefix=b/",
                    base,
                ]);
                args.extend(committed);
                args.extend(["--", &path]);
                self.run(dir, &args, BOUND)?
            };
            // No-index diff uses raw filesystem modes. Git add ignores a new regular file's
            // executable bit when core.fileMode=false; match its mode without altering patch
            // content, binary data or symlink metadata. The new-file header is the second line.
            if untracked
                && !honor_filemode
                && let Some(start) = bytes.iter().position(|b| *b == b'\n').map(|at| at + 1)
                && bytes[start..].starts_with(b"new file mode 100755\n")
            {
                bytes[start..start + b"new file mode 100755\n".len()]
                    .copy_from_slice(b"new file mode 100644\n");
            }
            total = total.saturating_add(bytes.len());
            if total > BOUND {
                return Err(GitFault::Helper(HelperFault::OutputTooLarge));
            }
            files.push(
                serde_json::json!({"path":path,"patch":String::from_utf8_lossy(&bytes),
                "sha256":crate::fingerprint::sha256_hex(&bytes),"byteLength":bytes.len()}),
            );
        }
        let after = observe()?;
        if before.head != after.head
            || before.branch != after.branch
            || self.reviewed_commit_policy(dir)? != execution_policy
        {
            return Err(GitFault::SourceChanged);
        }
        let base_head = if committed.is_some() {
            base
        } else {
            &before.head
        };
        Ok(
            serde_json::json!({"head":base_head,"branch":before.branch,"executionPolicy":execution_policy,"files":files}),
        )
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
            fn environment(&self) -> Vec<(String, String)> {
                vec![
                    ("GIT_DIR".into(), "/unrelated/.git".into()),
                    ("git_work_tree".into(), "/unrelated".into()),
                    ("PATH".into(), "/usr/bin".into()),
                ]
            }
            fn run(
                &self,
                _: &InvocationContext,
                spec: ProcessSpec,
                _: HelperLimits,
                _: &(dyn Fn() -> bool + Sync),
            ) -> Result<Vec<u8>, HelperFault> {
                assert!(spec.clear_env);
                assert!(spec.env.iter().all(|(key, _)| !matches!(
                    key.to_ascii_uppercase().as_str(),
                    "GIT_DIR" | "GIT_WORK_TREE"
                )));
                assert!(
                    spec.env
                        .iter()
                        .any(|(key, value)| key == "PATH" && value == "/usr/bin")
                );
                if spec.args.iter().any(|a| a == "config" || a == "ls-files") {
                    return Ok(vec![]);
                }
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
                b"refs/heads/task-a".to_vec(),
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
                Err(GitFault::SourceChanged) => assert!(changed),
                other => panic!("{other:?}"),
            }
        }
    }

    use super::*;

    #[test]
    fn filter_qualification_preserves_every_nul_delimited_path_byte() {
        use crate::context::PrincipalKind;
        use crate::protocol::{ActorId, Counter, Id, Revision, StoreId};
        struct Qualification;
        impl HelperRunner for Qualification {
            fn run(
                &self,
                _: &InvocationContext,
                spec: ProcessSpec,
                _: HelperLimits,
                _: &(dyn Fn() -> bool + Sync),
            ) -> Result<Vec<u8>, HelperFault> {
                if spec.args.iter().any(|a| a == "rev-parse") {
                    return Ok("a".repeat(40).into_bytes());
                }
                if spec.args.iter().any(|a| a == "config") {
                    return Ok(b"filter.probe.clean\nexternal-program\0".to_vec());
                }
                if spec.args.iter().any(|a| a == "ls-files") {
                    if spec.args.iter().any(|a| a == "--stage") {
                        return Ok(
                            format!("100644 {} 0\t.gitattributes\0", "a".repeat(40)).into_bytes()
                        );
                    }
                    assert!(spec.args.iter().any(|a| a == "--others"));
                    return Ok(b"sub dir/line\nname.probe\0non-utf8-\xff.probe\0".to_vec());
                }
                if spec.args.iter().any(|a| a == "check-attr") {
                    assert_eq!(
                        spec.stdin.as_deref(),
                        Some(
                            b".gitattributes\0sub dir/line\nname.probe\0non-utf8-\xff.probe\0"
                                .as_slice()
                        )
                    );
                    return Ok(b".gitattributes\0filter\0unspecified\0sub dir/line\nname.probe\0filter\0probe\0non-utf8-\xff.probe\0filter\0probe\0".to_vec());
                }
                panic!(
                    "filter qualification must refuse before status or diff: {:?}",
                    spec.args
                );
            }
        }
        let ctx = InvocationContext::new(
            ActorId(Id::new("host").unwrap()),
            PrincipalKind::Host,
            Id::new("invoke").unwrap(),
            Id::new("git").unwrap(),
            StoreId(Id::new("store").unwrap()),
            Counter::new(0).unwrap(),
            Revision::new(1).unwrap(),
        );
        let capture = Git {
            runner: &Qualification,
            ctx: &ctx,
            program: "git",
            deadline: Duration::from_secs(5),
            stop: &|| false,
        };
        assert!(matches!(
            capture.revision_observation(Path::new("/owned/worktree")),
            Err(GitFault::Unqualified)
        ));
    }

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
