//! Repository discovery (`indexing.discover`, the retained `DiscoverRepositoriesJob`): scans the
//! configured sections' base directories for Git checkouts and keeps every registration's exact
//! source identity current.
//!
//! A scan walks each base directory breadth-first to [`super::IndexSettings::discovery_depth`]
//! levels and at most [`super::IndexSettings::discovery_max_dirs`] directories. It never follows
//! a link or junction, never descends into a found repository (nested repositories are not
//! separate registrations), and treats a `.git` file (a linked worktree or submodule) as not a
//! primary checkout. A found checkout is matched to a registration by its exact canonical
//! location; a new one is registered through the projects port (`insert_repository`), named
//! after its directory. Repository and group exclusions of the section skip it. Registrations
//! whose location no longer holds a checkout are recorded `Missing` (the registration, its
//! memberships and history stay). Each registration records its `origin` remote (never with
//! credentials), `HEAD` commit and branch, object format and worktrees; an identity change
//! advances the source revision. Registration never adds project membership: that is the
//! user's `projects.membership` decision.
use super::git::Git;
use super::paths::{location_identity, native_location};
use super::{IndexEnv, section_config};
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::ports::IdKind;
use crate::ports::indexing::{EntryKind, GitMarker};
use crate::ports::storage::*;
use crate::protocol::{Id, ProjectId};
use crate::services::indexing::config::EffectiveConfig;
use crate::services::projects::carries_credentials;
use crate::settings::catalog::descriptor;
use crate::settings::model::{chain, current_chain};
use serde_json::json;
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

/// The repository base directory setting (SET-07/08).
pub const BASE_DIRECTORY_KEY: &str = "repositories.baseDirectory";

/// What one scan did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscoveryReport {
    /// Sections scanned (`workspace` or a project ID) and their base directory.
    pub sections: Vec<(String, String)>,
    /// Registrations found under a base directory (new or existing), by location.
    pub found: Vec<Id>,
    pub registered: Vec<Id>,
    /// Registrations whose source identity changed.
    pub changed: Vec<Id>,
    pub missing: Vec<Id>,
    /// Checkouts skipped by an exclusion, and sections or paths that could not be scanned.
    pub excluded: Vec<String>,
    pub problems: Vec<String>,
    /// The directory bound stopped a scan before it finished.
    pub truncated: bool,
}

/// The effective base directory of a section, `~` expanded (`None`: not absolute or unset).
pub fn base_directory(
    read: &dyn ConfigurationRead,
    home: Option<&Path>,
    project: Option<&ProjectId>,
) -> Result<Option<PathBuf>, StorageError> {
    let scope = project.map_or(ConfigScope::Workspace, |p| ConfigScope::Project(p.clone()));
    let layers = current_chain(read, &scope)?;
    let d = descriptor(BASE_DIRECTORY_KEY).expect("catalog key");
    let value = chain(d, &layers)
        .ok()
        .and_then(|e| e.into_iter().next())
        .and_then(|e| e.value.value.0.as_str().map(str::to_owned))
        .or_else(|| d.default_value().as_str().map(str::to_owned));
    Ok(value.and_then(|v| expand(&v, home)))
}
fn expand(v: &str, home: Option<&Path>) -> Option<PathBuf> {
    let p = if v == "~" {
        home?.to_path_buf()
    } else if let Some(rest) = v.strip_prefix("~/").or_else(|| v.strip_prefix("~\\")) {
        home?.join(rest)
    } else {
        PathBuf::from(v)
    };
    p.is_absolute().then_some(p)
}

/// The directory segments between a registration's base directory and the repository (its
/// group, for group exclusions).
pub fn group_path(
    tx: &mut dyn WriteTransaction,
    r: &StoredRepository,
) -> Result<Vec<String>, StorageError> {
    let source = tx.indexing().repository_source(&r.id)?;
    Ok(source
        .and_then(|s| s.section_root.map(|root| groups(&root, &s.location)))
        .unwrap_or_default())
}
/// The directory segments between `root` and `location` (excluding the repository itself).
pub fn groups(root: &str, location: &str) -> Vec<String> {
    groups_on(root, location, cfg!(windows))
}

/// [`groups`] under the path rules of `windows`: both paths are compared in their
/// [`native_location`] identity form, so separators, the verbatim prefix, drive-letter case and
/// a trailing separator never hide the relationship; the drive and the rest compare exactly
/// otherwise (both come from the same file system's canonical spelling).
pub fn groups_on(root: &str, location: &str, windows: bool) -> Vec<String> {
    let sep = if windows { '\\' } else { '/' };
    let (root, location) = (
        native_location(root, windows),
        native_location(location, windows),
    );
    let root = root.trim_end_matches(sep);
    let Some(rel) = location
        .strip_prefix(root)
        .and_then(|r| r.strip_prefix(sep))
    else {
        return Vec::new();
    };
    let parts: Vec<String> = rel
        .split(sep)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    parts[..parts.len().saturating_sub(1)].to_vec()
}

/// The stable worktree ID of a checkout path.
pub fn worktree_id(repository: &Id, path: &str) -> Id {
    let d = sha256_hex(format!("{}\u{0}{path}", repository.as_str()).as_bytes());
    Id::new(format!("wt-{}", &d[..32])).expect("valid")
}

/// The observed identity of one checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observed {
    pub head_oid: Option<String>,
    pub head_ref: Option<String>,
    pub object_format: String,
    pub remote: Option<String>,
    pub worktrees: Vec<(String, Option<String>, Option<String>)>,
}

/// Reads a checkout's identity through Git.
pub fn observe_checkout(git: &Git<'_>, dir: &Path) -> Result<Observed, String> {
    let head = git.head(dir).map_err(|e| e.to_string())?;
    let head_ref = git.branch(dir).map_err(|e| e.to_string())?;
    let object_format = git
        .object_format(dir)
        .map_err(|e| e.to_string())?
        .to_owned();
    let remote = git
        .origin(dir)
        .map_err(|e| e.to_string())?
        .filter(|r| !carries_credentials(r) && r.len() <= 4096);
    let worktrees = git
        .worktrees(dir)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|w| !w.bare)
        .map(|w| (w.path, w.head, w.branch))
        .collect();
    Ok(Observed {
        head_oid: head,
        head_ref,
        object_format,
        remote,
        worktrees,
    })
}

/// Scans the visible sections (`only`: just these section keys) and reconciles registrations.
pub fn scan(
    env: &IndexEnv,
    ctx: &InvocationContext,
    only: Option<&[String]>,
    stop: &(dyn Fn() -> bool + Sync),
) -> Result<DiscoveryReport, StorageError> {
    let mut report = DiscoveryReport::default();
    let home = env.source.home();
    // Sections: the workspace, then each active project with its own base directory.
    let sections = read_snapshot(&*env.store, &ReadOptions::default(), |s| {
        let mut out: Vec<(String, Option<ProjectId>, Option<PathBuf>, EffectiveConfig)> =
            Vec::new();
        let workspace = section_config(s.indexing(), WORKSPACE_SCOPE_KEY)?;
        let wdir = base_directory(s.configuration(), home.as_deref(), None)?;
        if ctx.project_id().is_none() {
            out.push((
                WORKSPACE_SCOPE_KEY.into(),
                None,
                wdir.clone(),
                EffectiveConfig::combine(&workspace, &[]),
            ));
        }
        let filter = ProjectFilter {
            only: ctx.project_id().cloned(),
            lifecycles: vec![ACTIVE.into()],
        };
        let mut after = None;
        loop {
            let page = s.projects().projects(&filter, after.as_ref(), MAX_PAGE)?;
            for p in &page {
                let dir = base_directory(s.configuration(), home.as_deref(), Some(&p.id))?;
                // A project without its own override shares the workspace directory.
                if dir.is_some() && dir != wdir {
                    let cfg = section_config(s.indexing(), p.id.0.as_str())?;
                    out.push((
                        p.id.0.as_str().to_owned(),
                        Some(p.id.clone()),
                        dir,
                        EffectiveConfig::combine(&workspace, &[cfg]),
                    ));
                }
            }
            if page.len() < MAX_PAGE as usize {
                break;
            }
            after = page.last().map(|p| ProjectPosition {
                created_at_ms: p.created_at_ms,
                id: p.id.clone(),
            });
        }
        Ok(out)
    })?;
    let git_ctx = env.host_context(None);
    let git = Git {
        runner: &*env.helpers,
        ctx: &git_ctx,
        program: &env.settings.git_program,
        deadline: env.settings.git_deadline,
        stop,
    };
    let mut seen: BTreeMap<String, ()> = BTreeMap::new();
    for (key, _project, dir, cfg) in sections {
        if only.is_some_and(|o| !o.contains(&key)) {
            continue;
        }
        let Some(dir) = dir else {
            report
                .problems
                .push(format!("{key}: the base directory is not an absolute path"));
            continue;
        };
        let root = match env.source.canonical_dir(&dir) {
            Ok(r) => r,
            Err(e) => {
                report.problems.push(format!(
                    "{key}: base directory {} unavailable ({e})",
                    dir.display()
                ));
                continue;
            }
        };
        report
            .sections
            .push((key.clone(), root.display().to_string()));
        let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::from([(root.clone(), 0)]);
        let mut visited = 0usize;
        while let Some((d, depth)) = queue.pop_front() {
            if stop() {
                return Err(StorageError::Cancelled);
            }
            visited += 1;
            if visited > env.settings.discovery_max_dirs {
                report.truncated = true;
                break;
            }
            match env.source.git_marker(&d) {
                Ok(Some(GitMarker::Directory)) => {
                    let location = d.display().to_string();
                    if seen.insert(location.clone(), ()).is_some() {
                        continue;
                    }
                    let name = d
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "repository".into());
                    let group = groups(&root.display().to_string(), &location);
                    if cfg.repository_excluded(&name, &group) {
                        report.excluded.push(location);
                        continue;
                    }
                    match reconcile(env, &git, &key, &root, &d, &name) {
                        Ok((id, new, changed)) => {
                            report.found.push(id.clone());
                            if new {
                                report.registered.push(id.clone());
                            }
                            if changed {
                                report.changed.push(id);
                            }
                        }
                        Err(e) => report.problems.push(format!("{location}: {e}")),
                    }
                    continue;
                }
                // A linked worktree or submodule: recorded through its primary checkout.
                Ok(Some(GitMarker::File(_))) => continue,
                Ok(None) => {}
                Err(e) => {
                    report.problems.push(format!("{}: {e}", d.display()));
                    continue;
                }
            }
            if depth >= env.settings.discovery_depth {
                continue;
            }
            match env.source.list_dir(&d, 4096) {
                Ok(entries) => {
                    for e in entries {
                        // Links are never followed (no cycles, no escape from the base directory).
                        if e.kind == EntryKind::Directory {
                            queue.push_back((d.join(&e.name), depth + 1));
                        }
                    }
                }
                Err(e) => report.problems.push(format!("{}: {e}", d.display())),
            }
        }
    }
    // Registrations already recorded but not under a scanned base directory are re-observed at
    // their own location, so every source identity stays current.
    let known = read_snapshot(&*env.store, &ReadOptions::default(), |s| {
        let filter = RepositoryFilter {
            project: ctx.project_id().cloned(),
            ..RepositoryFilter::default()
        };
        let mut out = Vec::new();
        let mut after = None;
        loop {
            let page = s
                .projects()
                .repositories(&filter, after.as_ref(), MAX_PAGE)?;
            for r in &page {
                if let Some(src) = s.indexing().repository_source(&r.id)? {
                    out.push((r.clone(), src));
                }
            }
            if page.len() < MAX_PAGE as usize {
                break;
            }
            after = page.last().map(|r| r.id.clone());
        }
        Ok(out)
    })?;
    for (r, src) in known {
        if seen.contains_key(&src.location) || r.lifecycle == DELETED {
            continue;
        }
        if stop() {
            return Err(StorageError::Cancelled);
        }
        let dir = PathBuf::from(&src.location);
        match env.source.git_marker(&dir) {
            Ok(Some(GitMarker::Directory)) => {
                let root = src
                    .section_root
                    .clone()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| dir.clone());
                match reconcile(env, &git, &src.section, &root, &dir, &r.name) {
                    Ok((id, _, changed)) if changed => report.changed.push(id),
                    Ok(_) => {}
                    Err(e) => report.problems.push(format!("{}: {e}", src.location)),
                }
            }
            _ => {
                let id = r.id.clone();
                let missing = if env.source.canonical_dir(&dir).is_ok() {
                    SOURCE_NOT_REPOSITORY
                } else {
                    SOURCE_MISSING
                };
                if src.state != missing {
                    let now = env.now();
                    run_write(&*env.store, WriteOptions::default(), move |tx| {
                        // Only the identity this scan read: a concurrent observation wins.
                        let current = tx.indexing().repository_source(&src.repository_id)?;
                        if current.as_ref() != Some(&src) {
                            return Ok(());
                        }
                        let mut next = src.clone();
                        next.state = missing.into();
                        next.revision += 1;
                        next.observed_at_ms = now;
                        tx.indexing().put_repository_source(&next)?;
                        source_event(tx, &next, "RepositorySourceMissing")
                    })?;
                }
                report.missing.push(id);
            }
        }
    }
    Ok(report)
}

fn source_event(
    tx: &mut dyn WriteTransaction,
    s: &StoredRepositorySource,
    kind: &str,
) -> Result<(), StorageError> {
    let host = tx.store().host_actor.clone();
    super::history(
        tx,
        &host,
        None,
        (
            crate::services::projects::REPOSITORY_AGGREGATE,
            &s.repository_id,
            s.revision,
        ),
        kind,
        json!({
            "location": s.location, "section": s.section, "state": s.state,
            "headOid": s.head_oid, "headRef": s.head_ref, "objectFormat": s.object_format,
            "remoteRecorded": s.remote_url.is_some(), "sourceRevision": s.revision,
            "principalKind": "Host",
        }),
    )
}

/// Registers or refreshes one checkout; returns (registration, newly registered, identity
/// changed).
fn reconcile(
    env: &IndexEnv,
    git: &Git<'_>,
    section: &str,
    root: &Path,
    dir: &Path,
    name: &str,
) -> Result<(Id, bool, bool), String> {
    let mut observed = observe_checkout(git, dir)?;
    // Git reports worktree paths in its own spelling (`C:/x`, the path given at `worktree add`,
    // possibly an 8.3 short name): each is recorded in the same identity form as registrations,
    // canonical when it exists.
    for w in &mut observed.worktrees {
        w.0 = location_identity(&*env.source, Path::new(&w.0));
    }
    let location = location_identity(&*env.source, dir);
    let root = root.display().to_string();
    let section = section.to_owned();
    let name: String = name.chars().take(256).collect();
    let now = env.now();
    let new_id = env.ids.next_id(IdKind::Repository);
    run_write(&*env.store, WriteOptions::default(), move |tx| {
        let existing = tx.indexing().repository_at(&location)?;
        let (id, new) = match existing {
            Some(id) => (id, false),
            None => {
                tx.projects().insert_repository(&StoredRepository {
                    id: new_id.clone(),
                    location: location.clone(),
                    name: if name.is_empty() {
                        "repository".into()
                    } else {
                        name.clone()
                    },
                    remote_url: observed.remote.clone(),
                    primary: true,
                    lifecycle: ACTIVE.into(),
                    languages: Vec::new(),
                    revision: 1,
                    registered_at_ms: now,
                })?;
                (new_id.clone(), true)
            }
        };
        let prior = tx.indexing().repository_source(&id)?;
        let identity = |s: &StoredRepositorySource| {
            (
                s.state.clone(),
                s.head_oid.clone(),
                s.head_ref.clone(),
                s.object_format.clone(),
                s.remote_url.clone(),
                s.location.clone(),
            )
        };
        let mut next = StoredRepositorySource {
            repository_id: id.clone(),
            section: prior
                .as_ref()
                .map_or(section.clone(), |p| p.section.clone()),
            section_root: prior
                .as_ref()
                .and_then(|p| p.section_root.clone())
                .or(Some(root.clone())),
            location: location.clone(),
            state: SOURCE_PRESENT.into(),
            head_oid: observed.head_oid.clone(),
            head_ref: observed.head_ref.clone(),
            object_format: observed.object_format.clone(),
            remote_url: observed.remote.clone(),
            revision: 1,
            observed_at_ms: now,
        };
        let changed = match &prior {
            None => true,
            Some(p) => identity(p) != identity(&next),
        };
        next.revision = match &prior {
            None => 1,
            Some(p) if changed => p.revision + 1,
            Some(p) => p.revision,
        };
        tx.indexing().put_repository_source(&next)?;
        let worktrees: Vec<StoredWorktree> = observed
            .worktrees
            .iter()
            .take(256)
            .map(|(path, head, branch)| StoredWorktree {
                repository_id: id.clone(),
                worktree_id: worktree_id(&id, path),
                path: path.clone(),
                head_oid: head.clone(),
                head_ref: branch.clone(),
                main: Path::new(path) == Path::new(&location),
                observed_at_ms: now,
            })
            .collect();
        tx.indexing().put_worktrees(&id, &worktrees)?;
        if new {
            source_event(tx, &next, "RepositoryRegistered")?;
        } else if changed {
            source_event(tx, &next, "RepositorySourceObserved")?;
        }
        Ok((id, new, changed))
    })
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_are_the_segments_between_root_and_repository() {
        // POSIX rules.
        assert_eq!(
            groups_on("/src", "/src/team/legacy/app", false),
            vec!["team", "legacy"]
        );
        assert!(groups_on("/src", "/src/app", false).is_empty());
        assert!(groups_on("/src", "/other/app", false).is_empty());
        assert!(groups_on("/src", "/srcx/team/app", false).is_empty());
        assert_eq!(groups_on("/src/", "/src/team/app", false), vec!["team"]);
        // Windows rules, on every platform: mixed separators, the verbatim prefix, drive case
        // and a trailing separator all name the same directories.
        for (root, location) in [
            (r"C:\src", r"C:\src\team\legacy\app"),
            ("C:/src", r"C:\src\team/legacy/app"),
            (r"c:\src\", r"\\?\C:\src\team\legacy\app"),
        ] {
            assert_eq!(
                groups_on(root, location, true),
                vec!["team", "legacy"],
                "{root} {location}"
            );
        }
        assert!(groups_on(r"C:\src", r"C:\srcx\team\app", true).is_empty());
        assert!(groups_on(r"C:\src", r"D:\src\team\app", true).is_empty());
        // The host's own rules.
        let base = std::env::temp_dir().join("src");
        let repo = base.join("team").join("app");
        assert_eq!(
            groups(&base.display().to_string(), &repo.display().to_string()),
            vec!["team"]
        );
    }

    #[test]
    fn a_configured_base_directory_expands_home_and_must_be_absolute() {
        // An absolute home on this platform (`/home/me` is not absolute on Windows).
        let home = std::env::temp_dir().join("home-me");
        assert!(expand("relative/x", None).is_none());
        assert_eq!(expand("~/x", Some(&home)), Some(home.join("x")));
        assert_eq!(expand("~\\x", Some(&home)), Some(home.join("x")));
        assert_eq!(expand("~", Some(&home)), Some(home.clone()));
        assert!(expand("~/x", None).is_none());
    }

    #[test]
    fn worktree_ids_are_stable_per_repository_and_path() {
        let a = Id::new("repo-a").unwrap();
        let b = Id::new("repo-b").unwrap();
        assert_eq!(worktree_id(&a, "/x"), worktree_id(&a, "/x"));
        assert_ne!(worktree_id(&a, "/x"), worktree_id(&b, "/x"));
        assert_ne!(worktree_id(&a, "/x"), worktree_id(&a, "/y"));
    }
}
