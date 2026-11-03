//! Real disposable filesystem targets: destructive preflight cases, verified no-follow reads,
//! identity-checked removal and shell plans resolved against actual paths.
use super::NativeFilesystem;
use devworks_core::authority::destructive::{
    BlockReason, DestructiveKind, EffectPlan, FilesystemResolver, InputNeeded, IntendedScope,
    OwnedWorktree, PreflightDecision, PreflightVerdict, ProtectedRoots, TargetClass, preflight,
};
use devworks_core::authority::reads::{ReadRefusal, ReadRequest, ReadRoot, VerifiedReader};
use devworks_core::authority::shell::{ShellAssessment, assess_command};
use devworks_core::ports::storage::RegisteredRepository;
use devworks_core::protocol::Id;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "devworks-fs-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn p(&self, rel: &str) -> String {
        self.0.join(rel).to_string_lossy().into_owned()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}

/// A scratch tree with a registered primary repository, an owned worktree and app data.
struct Tree {
    s: Scratch,
    roots: ProtectedRoots,
}
fn tree(tag: &str) -> Tree {
    let s = Scratch::new(tag);
    for d in [
        "repo/.git",
        "repo/src",
        "worktrees/task-a/src",
        "outside",
        "data",
        "busy",
    ] {
        std::fs::create_dir_all(s.0.join(d)).unwrap();
    }
    std::fs::write(s.0.join("repo/src/lib.rs"), b"source").unwrap();
    std::fs::write(s.0.join("worktrees/task-a/src/x.rs"), b"x").unwrap();
    std::fs::write(s.0.join("outside/secret.txt"), b"private").unwrap();
    let roots = ProtectedRoots::new(
        vec![RegisteredRepository {
            id: id("repo-a"),
            location: s.p("repo"),
            revision: 1,
        }],
        vec![
            OwnedWorktree {
                id: id("wt-a"),
                repository_id: id("repo-a"),
                location: s.p("worktrees/task-a"),
                stopped: true,
                active_users: 0,
            },
            OwnedWorktree {
                id: id("wt-busy"),
                repository_id: id("repo-a"),
                location: s.p("busy"),
                stopped: false,
                active_users: 1,
            },
            OwnedWorktree {
                id: id("wt-primary"),
                repository_id: id("repo-a"),
                location: s.p("repo"),
                stopped: true,
                active_users: 0,
            },
        ],
        Some(s.p("data")),
    );
    Tree { s, roots }
}

fn check(
    t: &Tree,
    kind: DestructiveKind,
    targets: &[&str],
    cwd: Option<&str>,
) -> PreflightDecision {
    check_in(t, kind, targets, cwd, IntendedScope::Ordinary)
}
fn check_in(
    t: &Tree,
    kind: DestructiveKind,
    targets: &[&str],
    cwd: Option<&str>,
    scope: IntendedScope,
) -> PreflightDecision {
    preflight(
        &NativeFilesystem::new(),
        &t.roots,
        &EffectPlan {
            kind,
            targets: targets.iter().map(|s| (*s).to_owned()).collect(),
            working_directory: cwd.map(str::to_owned),
            scope,
        },
    )
}
fn blocked(d: &PreflightDecision) -> Option<&BlockReason> {
    match &d.verdict {
        PreflightVerdict::Blocked(r) => Some(r),
        _ => None,
    }
}
fn repo_block() -> BlockReason {
    BlockReason::PrimaryRepository {
        repository_id: id("repo-a"),
    }
}

#[test]
fn filesystem_roots_home_and_app_data_are_blocked() {
    let t = tree("roots");
    let root =
        t.s.0
            .ancestors()
            .last()
            .unwrap()
            .to_string_lossy()
            .into_owned();
    for kind in [
        DestructiveKind::RemoveTree,
        DestructiveKind::EraseContents,
        DestructiveKind::Unlink,
    ] {
        assert_eq!(
            blocked(&check(&t, kind, &[&root], None)),
            Some(&BlockReason::FilesystemRoot),
            "{kind:?} {root}"
        );
    }
    // Whole-content erasure of a root through a glob.
    let all = format!("{}*", root);
    assert_eq!(
        blocked(&check(&t, DestructiveKind::RemoveTree, &[&all], None)),
        Some(&BlockReason::FilesystemRoot)
    );
    if NativeFilesystem::new().home().is_some() {
        assert_eq!(
            blocked(&check(&t, DestructiveKind::RemoveTree, &["~"], None)),
            Some(&BlockReason::ProtectedUserDirectory)
        );
    }
    assert_eq!(
        blocked(&check(
            &t,
            DestructiveKind::RemoveTree,
            &[&t.s.p("data")],
            None
        )),
        Some(&BlockReason::ApplicationData)
    );
    std::fs::write(t.s.0.join("data/workspace.sqlite"), b"db").unwrap();
    assert_eq!(
        blocked(&check(
            &t,
            DestructiveKind::Unlink,
            &[&t.s.p("data/workspace.sqlite")],
            None
        )),
        Some(&BlockReason::ApplicationData)
    );
}

#[test]
fn registered_repository_roots_resolve_through_aliases() {
    let t = tree("repo");
    let rm = DestructiveKind::RemoveTree;
    assert_eq!(
        blocked(&check(&t, rm, &[&t.s.p("repo")], None)),
        Some(&repo_block())
    );
    // Parent traversal and relative forms resolve to the same root.
    assert_eq!(
        blocked(&check(&t, rm, &["../repo"], Some(&t.s.p("outside")))),
        Some(&repo_block())
    );
    assert_eq!(
        blocked(&check(&t, rm, &[&t.s.p("repo/src/../../repo")], None)),
        Some(&repo_block())
    );
    // A recursive ancestor would destroy it too.
    let mut repos_only = t.roots.clone();
    repos_only.app_data_root = None;
    let ancestor = preflight(
        &NativeFilesystem::new(),
        &repos_only,
        &EffectPlan {
            kind: rm,
            targets: vec![t.s.p("")],
            working_directory: None,
            scope: IntendedScope::Ordinary,
        },
    );
    assert_eq!(blocked(&ancestor), Some(&repo_block()));
    // Erasing every entry is destruction even though the directory survives.
    assert_eq!(
        blocked(&check(&t, rm, &[&t.s.p("repo/*")], None)),
        Some(&BlockReason::WholeContentErasure)
    );
    assert_eq!(
        blocked(&check(
            &t,
            DestructiveKind::EraseContents,
            &[&t.s.p("repo")],
            None
        )),
        Some(&repo_block())
    );
    assert_eq!(
        blocked(&check(&t, rm, &[&t.s.p("repo/.git")], None)),
        Some(&BlockReason::RepositoryMetadata {
            repository_id: id("repo-a")
        })
    );
    // Ordinary bounded source removal remains permitted.
    let ok = check(
        &t,
        DestructiveKind::Unlink,
        &[&t.s.p("repo/src/lib.rs")],
        None,
    );
    assert_eq!(ok.verdict, PreflightVerdict::Allowed);
    assert_eq!(
        ok.allowed_targets().unwrap()[0].class(),
        TargetClass::Ordinary
    );
    let ok = check(&t, rm, &["src"], Some(&t.s.p("repo")));
    assert_eq!(ok.verdict, PreflightVerdict::Allowed);
    // Nothing at the target is not a destruction.
    let missing = check(&t, rm, &[&t.s.p("repo/none")], None);
    assert_eq!(
        missing.allowed_targets().unwrap()[0].class(),
        TargetClass::NoTarget
    );
    // A relative target without a saved working directory cannot be resolved.
    assert_eq!(
        check(&t, rm, &["src"], None).verdict,
        PreflightVerdict::NeedsInput(InputNeeded::UnresolvedTarget)
    );
}

#[cfg(unix)]
#[test]
fn links_are_checked_at_the_entry_and_the_resolved_target() {
    use std::os::unix::fs::symlink;
    let t = tree("links");
    symlink(t.s.0.join("repo"), t.s.0.join("outside/to-repo")).unwrap();
    symlink(
        t.s.0.ancestors().last().unwrap(),
        t.s.0.join("outside/to-root"),
    )
    .unwrap();
    let rm = DestructiveKind::RemoveTree;
    assert_eq!(
        blocked(&check(&t, rm, &[&t.s.p("outside/to-repo")], None)),
        Some(&repo_block())
    );
    assert_eq!(
        blocked(&check(&t, rm, &[&t.s.p("outside/to-root")], None)),
        Some(&BlockReason::FilesystemRoot)
    );
    // Through a linked ancestor.
    assert_eq!(
        blocked(&check(&t, rm, &[&t.s.p("outside/to-repo/.git")], None)),
        Some(&BlockReason::RepositoryMetadata {
            repository_id: id("repo-a")
        })
    );
    // Unlinking the link itself does not traverse it.
    let unlink = check(
        &t,
        DestructiveKind::Unlink,
        &[&t.s.p("outside/to-repo")],
        None,
    );
    assert_eq!(unlink.verdict, PreflightVerdict::Allowed);
    let fs = NativeFilesystem::new();
    use devworks_core::authority::destructive::DestructiveExecutor;
    fs.execute(
        &unlink.allowed_targets().unwrap()[0],
        DestructiveKind::Unlink,
    )
    .unwrap();
    assert!(t.s.0.join("repo/src/lib.rs").exists());
    assert!(!t.s.0.join("outside/to-repo").exists());
}

#[test]
fn glob_expansion_is_bounded_and_explicit() {
    let t = tree("glob");
    std::fs::create_dir_all(t.s.0.join("many")).unwrap();
    for i in 0..300 {
        std::fs::write(t.s.0.join(format!("many/f{i}.tmp")), b"").unwrap();
    }
    for i in 0..3 {
        std::fs::write(t.s.0.join(format!("outside/o{i}.log")), b"").unwrap();
    }
    assert_eq!(
        check(&t, DestructiveKind::Unlink, &[&t.s.p("many/*.tmp")], None).verdict,
        PreflightVerdict::NeedsInput(InputNeeded::UnboundedExpansion)
    );
    let few = check(
        &t,
        DestructiveKind::Unlink,
        &[&t.s.p("outside/o[0-1].log")],
        None,
    );
    assert_eq!(few.allowed_targets().unwrap().len(), 2);
    // A glob that reaches a protected root is checked per expanded target.
    assert_eq!(
        blocked(&check(
            &t,
            DestructiveKind::RemoveTree,
            &[&t.s.p("re*")],
            None
        )),
        Some(&repo_block())
    );
}

#[test]
fn owned_worktree_cleanup_is_bounded_to_the_exact_stopped_worktree() {
    let t = tree("worktree");
    let cleanup = |id_: &str| IntendedScope::OwnedWorktreeCleanup {
        worktree_id: id(id_),
    };
    let rm = DestructiveKind::RemoveTree;
    let ok = check_in(&t, rm, &[&t.s.p("worktrees/task-a")], None, cleanup("wt-a"));
    assert_eq!(ok.verdict, PreflightVerdict::Allowed);
    assert_eq!(
        ok.allowed_targets().unwrap()[0].class(),
        TargetClass::OwnedWorktree
    );
    // Erasing the worktree's own contents is its cleanup, not a protected erasure.
    let contents = check_in(
        &t,
        rm,
        &[&t.s.p("worktrees/task-a/*")],
        None,
        cleanup("wt-a"),
    );
    assert_eq!(contents.verdict, PreflightVerdict::Allowed);
    assert_eq!(
        blocked(&check_in(
            &t,
            rm,
            &[&t.s.p("worktrees")],
            None,
            cleanup("wt-a")
        )),
        Some(&BlockReason::EscapesScope)
    );
    assert_eq!(
        blocked(&check_in(
            &t,
            rm,
            &[&t.s.p("outside")],
            None,
            cleanup("wt-a")
        )),
        Some(&BlockReason::EscapesScope)
    );
    assert_eq!(
        blocked(&check_in(
            &t,
            rm,
            &["../../outside"],
            Some(&t.s.p("worktrees/task-a")),
            cleanup("wt-a")
        )),
        Some(&BlockReason::EscapesScope)
    );
    assert_eq!(
        blocked(&check_in(
            &t,
            rm,
            &[&t.s.p("busy")],
            None,
            cleanup("wt-busy")
        )),
        Some(&BlockReason::WorktreeInUse)
    );
    assert_eq!(
        blocked(&check_in(
            &t,
            rm,
            &[&t.s.p("worktrees/task-a")],
            None,
            cleanup("wt-unknown")
        )),
        Some(&BlockReason::WorktreeNotOwned)
    );
    // A "worktree" that is the primary checkout is primary repository deletion.
    assert_eq!(
        blocked(&check_in(
            &t,
            rm,
            &[&t.s.p("repo")],
            None,
            cleanup("wt-primary")
        )),
        Some(&repo_block())
    );
    #[cfg(unix)]
    {
        use devworks_core::authority::destructive::{DestructiveExecutor, revalidate};
        std::os::unix::fs::symlink(t.s.0.join("outside"), t.s.0.join("worktrees/task-a/out"))
            .unwrap();
        assert_eq!(
            blocked(&check_in(
                &t,
                rm,
                &[&t.s.p("worktrees/task-a/out")],
                None,
                cleanup("wt-a")
            )),
            Some(&BlockReason::LinkEscape)
        );
        assert_eq!(
            blocked(&check_in(
                &t,
                rm,
                &[&t.s.p("worktrees/task-a/out/secret.txt")],
                None,
                cleanup("wt-a")
            )),
            Some(&BlockReason::EscapesScope)
        );
        // The guarded cleanup removes the worktree, including the link itself, never its target.
        let ok = check_in(&t, rm, &[&t.s.p("worktrees/task-a")], None, cleanup("wt-a"));
        let fs = NativeFilesystem::new();
        revalidate(&fs, &ok, &t.roots.revision).unwrap();
        let outcome = fs.execute(&ok.allowed_targets().unwrap()[0], rm).unwrap();
        assert!(outcome.complete, "{outcome:?}");
        assert!(outcome.removed_entries >= 4);
        assert!(!t.s.0.join("worktrees/task-a").exists());
        assert!(t.s.0.join("outside/secret.txt").exists());
    }
}

#[cfg(unix)]
#[test]
fn a_target_replaced_after_its_check_is_refused_at_the_step() {
    use devworks_core::authority::destructive::{DestructiveExecutor, ResolveError, revalidate};
    use std::path::Path;
    let t = tree("replace");
    let target = t.s.p("outside/secret.txt");
    let decision = check(&t, DestructiveKind::Unlink, &[&target], None);
    let fs = NativeFilesystem::new();
    std::fs::remove_file(&target).unwrap();
    std::os::unix::fs::symlink(t.s.0.join("repo/src/lib.rs"), &target).unwrap();
    assert!(matches!(
        revalidate(&fs, &decision, &t.roots.revision),
        Err(ResolveError::Changed(_))
    ));
    assert!(matches!(
        fs.execute(
            &decision.allowed_targets().unwrap()[0],
            DestructiveKind::Unlink
        ),
        Err(ResolveError::Changed(_))
    ));
    assert!(Path::new(&target).exists());
    // A changed protected-root registry also refuses the step.
    assert!(revalidate(&fs, &decision, "another-revision").is_err());
    // Recursive removal of a directory swapped for another refuses too.
    let dir = t.s.p("outside/tree");
    std::fs::create_dir_all(format!("{dir}/a")).unwrap();
    let decision = check(&t, DestructiveKind::RemoveTree, &[&dir], None);
    std::fs::rename(&dir, t.s.p("outside/moved")).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    assert!(matches!(
        fs.execute(
            &decision.allowed_targets().unwrap()[0],
            DestructiveKind::RemoveTree
        ),
        Err(ResolveError::Changed(_))
    ));
    assert!(t.s.0.join("outside/moved/a").exists());
}

#[test]
fn shell_plans_resolve_to_actual_targets_before_dispatch() {
    let t = tree("shell");
    let fs = NativeFilesystem::new();
    let argv = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let cwd = t.s.p("outside");
    let script = "cd x; rm -rf ../repo && echo done";
    let ShellAssessment::Destructive { plans, residual } =
        assess_command(&argv(&["bash", "-c", script]), Some(&cwd))
    else {
        panic!("recognized")
    };
    assert!(!residual.is_empty());
    let decision = preflight(&fs, &t.roots, &plans[0]);
    assert_eq!(blocked(&decision), Some(&repo_block()));
    // An unrecognized command is not proof of harmlessness.
    assert!(matches!(
        assess_command(&argv(&["python3", "cleanup.py"]), Some(&cwd)),
        ShellAssessment::NoRecognizedDestruction { .. }
    ));
}

#[test]
fn verified_reads_stay_inside_the_root_and_within_bounds() {
    let t = tree("read");
    let fs = NativeFilesystem::new();
    let root = ReadRoot {
        location: t.s.0.join("repo"),
        identity: super::identity_of(&std::fs::metadata(t.s.0.join("repo")).unwrap()),
    };
    let read = |rel: &str, max: u64| {
        fs.read(&ReadRequest {
            root: &root,
            relative: rel,
            max_bytes: max,
            allow_hard_links: false,
        })
    };
    assert_eq!(read("src/lib.rs", 64).unwrap().bytes, b"source");
    for escape in ["../outside/secret.txt", "", "./src/lib.rs"] {
        assert_eq!(read(escape, 64), Err(ReadRefusal::Escape), "{escape}");
    }
    assert_eq!(
        read(&t.s.p("outside/secret.txt"), 64),
        Err(ReadRefusal::Escape)
    );
    assert_eq!(
        read("src/lib.rs", 3),
        Err(ReadRefusal::TooLarge { limit: 3 })
    );
    assert_eq!(read("src", 64), Err(ReadRefusal::NotRegularFile));
    assert_eq!(read("src/none", 64), Err(ReadRefusal::NotFound));
}

#[cfg(unix)]
#[test]
fn verified_reads_refuse_links_hard_links_swaps_and_moved_roots() {
    use std::os::unix::fs::symlink;
    let t = tree("read-unix");
    let repo = t.s.0.join("repo");
    let identity = super::identity_of(&std::fs::metadata(&repo).unwrap());
    let root = ReadRoot {
        location: repo.clone(),
        identity,
    };
    symlink(t.s.0.join("outside/secret.txt"), repo.join("final-link")).unwrap();
    symlink(t.s.0.join("outside"), repo.join("parent-link")).unwrap();
    std::fs::hard_link(t.s.0.join("outside/secret.txt"), repo.join("hard")).unwrap();
    let fs = NativeFilesystem::new();
    let read = |fs: &NativeFilesystem, root: &ReadRoot, rel: &str| {
        fs.read(&ReadRequest {
            root,
            relative: rel,
            max_bytes: 1024,
            allow_hard_links: false,
        })
    };
    assert_eq!(
        read(&fs, &root, "final-link"),
        Err(ReadRefusal::LinkRefused)
    );
    assert_eq!(
        read(&fs, &root, "parent-link/secret.txt"),
        Err(ReadRefusal::LinkRefused)
    );
    assert_eq!(read(&fs, &root, "hard"), Err(ReadRefusal::HardLinked));
    // FIFOs and other non-regular files never block or leak.
    let fifo = std::ffi::CString::new(repo.join("pipe").to_string_lossy().as_bytes()).unwrap();
    // SAFETY: NUL-terminated path, standard permissions.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert_eq!(read(&fs, &root, "pipe"), Err(ReadRefusal::NotRegularFile));
    // A root moved or replaced since registration is refused, as is a root that is a link.
    let other = ReadRoot {
        location: repo.clone(),
        identity: super::identity_of(&std::fs::metadata(t.s.0.join("outside")).unwrap()),
    };
    assert_eq!(
        read(&fs, &other, "src/lib.rs"),
        Err(ReadRefusal::RootChanged)
    );
    symlink(&repo, t.s.0.join("root-link")).unwrap();
    let linked = ReadRoot {
        location: t.s.0.join("root-link"),
        identity: None,
    };
    assert_eq!(
        read(&fs, &linked, "src/lib.rs"),
        Err(ReadRefusal::RootChanged)
    );
    // The path is swapped for a link to private content between open and verification.
    let swap_dir = repo.join("src");
    let (from, to, secret) = (
        swap_dir.join("lib.rs"),
        swap_dir.join("lib.rs.orig"),
        t.s.0.join("outside/secret.txt"),
    );
    let swapping = NativeFilesystem {
        after_open: Some(Box::new(move || {
            std::fs::rename(&from, &to).unwrap();
            symlink(&secret, &from).unwrap();
        })),
    };
    assert_eq!(
        read(&swapping, &root, "src/lib.rs"),
        Err(ReadRefusal::Changed)
    );
    // Afterwards the substituted link is refused outright.
    assert_eq!(
        read(&fs, &root, "src/lib.rs"),
        Err(ReadRefusal::LinkRefused)
    );
}
