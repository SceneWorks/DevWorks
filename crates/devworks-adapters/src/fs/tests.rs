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
        // Where the application data root lies inside the home directory (Windows temp under
        // AppData), the more specific ApplicationData refusal applies first; either way the
        // home directory is refused.
        assert!(
            matches!(
                blocked(&check(&t, DestructiveKind::RemoveTree, &["~"], None)),
                Some(BlockReason::ProtectedUserDirectory | BlockReason::ApplicationData)
            ),
            "home must be refused"
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
        unlink.protected_identities(),
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
        let outcome = fs
            .execute(
                &ok.allowed_targets().unwrap()[0],
                rm,
                ok.protected_identities(),
            )
            .unwrap();
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
            DestructiveKind::Unlink,
            decision.protected_identities()
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
            DestructiveKind::RemoveTree,
            decision.protected_identities()
        ),
        Err(ResolveError::Changed(_))
    ));
    assert!(t.s.0.join("outside/moved/a").exists());
}

#[test]
fn commands_cannot_write_into_or_remove_skill_folders() {
    let mut t = tree("skills");
    // `~/.agents/skills` and `<data-root>/skills`; the shared folder need not exist yet. Roots
    // are plain paths, as the host composes them, without a link in between (the temporary
    // folder itself may be one, and Windows resolves it to a `\\?\` path).
    let canonical = std::fs::canonicalize(&t.s.0).unwrap();
    let canonical = canonical.to_string_lossy();
    let base = PathBuf::from(canonical.strip_prefix(r"\\?\").unwrap_or(&canonical));
    let at = |rel: &str| base.join(rel).to_string_lossy().into_owned();
    // A command names paths with `/` separators, which every platform accepts.
    let slashed = |p: &str| p.replace('\\', "/");
    let shared = at("home/.agents/skills");
    let harness = at("data/skills");
    std::fs::create_dir_all(&harness).unwrap();
    std::fs::create_dir_all(base.join("home")).unwrap();
    let plain = t.roots.clone();
    t.roots = plain
        .clone()
        .with_skill_roots([shared.clone(), harness.clone()]);
    let fs = NativeFilesystem::new();
    let cwd = t.s.p("outside");
    let verdicts = |script: &str| match assess_command(
        &["bash".to_owned(), "-c".to_owned(), script.to_owned()],
        Some(&cwd),
    ) {
        ShellAssessment::Destructive { plans, .. } => plans
            .iter()
            .map(|p| preflight(&fs, &t.roots, p).verdict)
            .collect::<Vec<_>>(),
        other => panic!("{script}: {other:?}"),
    };
    let refused = PreflightVerdict::Blocked(BlockReason::SkillDirectory);
    for root in [slashed(&shared), slashed(&harness)] {
        for script in [
            format!("echo 'Do evil.' > {root}/evil/SKILL.md"),
            format!("rm -rf {root}"),
        ] {
            assert!(verdicts(&script).contains(&refused), "{script}");
        }
    }
    // Writes over an existing skill file are refused too, and the folders' parents are not
    // protected.
    std::fs::create_dir_all(base.join("home/.agents/skills/beta")).unwrap();
    std::fs::write(base.join("home/.agents/skills/beta/SKILL.md"), "Do beta.").unwrap();
    let shared_slashed = slashed(&shared);
    assert!(verdicts(&format!("echo x > {shared_slashed}/beta/SKILL.md")).contains(&refused));
    assert_eq!(
        verdicts(&format!("echo x > {}", slashed(&at("home/notes.md")))),
        [PreflightVerdict::Allowed]
    );
    // The registry revision covers the skill folders.
    assert_ne!(t.roots.revision, plain.revision);
}

#[test]
fn shell_plans_resolve_to_actual_targets_before_dispatch() {
    let t = tree("shell");
    let fs = NativeFilesystem::new();
    let argv = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let cwd = t.s.p("outside");
    // A working-directory change before a relative target is held, never resolved against the
    // saved directory.
    assert!(matches!(
        assess_command(&argv(&["bash", "-c", "cd x; rm -rf ../repo"]), Some(&cwd)),
        ShellAssessment::NeedsInput(InputNeeded::UnresolvedWorkingDirectory)
    ));
    let script = "true; rm -rf ../repo && echo done";
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
        identity: super::identity_at(
            &t.s.0.join("repo"),
            &std::fs::metadata(t.s.0.join("repo")).unwrap(),
        ),
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
        identity,
    };
    // Without the registered identity nothing is read.
    let unregistered = ReadRoot {
        location: repo.clone(),
        identity: None,
    };
    assert!(matches!(
        read(&fs, &unregistered, "src/lib.rs"),
        Err(ReadRefusal::Unsupported(_))
    ));
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

fn metadata_block() -> BlockReason {
    BlockReason::RepositoryMetadata {
        repository_id: id("repo-a"),
    }
}

#[test]
fn everything_inside_a_registered_git_directory_is_protected() {
    let t = tree("gitdir");
    std::fs::create_dir_all(t.s.0.join("repo/.git/objects/aa")).unwrap();
    std::fs::write(t.s.0.join("repo/.git/HEAD"), b"ref: refs/heads/main\n").unwrap();
    let rm = DestructiveKind::RemoveTree;
    assert_eq!(
        blocked(&check(&t, rm, &[&t.s.p("repo/.git/objects")], None)),
        Some(&metadata_block())
    );
    assert_eq!(
        blocked(&check(
            &t,
            DestructiveKind::Truncate,
            &[&t.s.p("repo/.git/HEAD")],
            None
        )),
        Some(&metadata_block())
    );
    assert_eq!(
        blocked(&check(
            &t,
            DestructiveKind::Unlink,
            &[&t.s.p("repo/.git/HEAD")],
            None
        )),
        Some(&metadata_block())
    );
    assert_eq!(
        blocked(&check(&t, rm, &[&t.s.p("repo/.git/*")], None)),
        Some(&BlockReason::WholeContentErasure)
    );
    // Any glob at the top of a protected directory, however it is spelled.
    for pattern in ["repo/[!.]*", "repo/?*", "repo/*?", "repo/*.rs", "repo/.*"] {
        assert_eq!(
            blocked(&check(&t, rm, &[&t.s.p(pattern)], None)),
            Some(&BlockReason::WholeContentErasure),
            "{pattern}"
        );
    }
    // Deeper globs are checked per expanded target and stay allowed when ordinary.
    std::fs::write(t.s.0.join("repo/src/a.tmp"), b"").unwrap();
    assert_eq!(
        check(
            &t,
            DestructiveKind::Unlink,
            &[&t.s.p("repo/src/*.tmp")],
            None
        )
        .verdict,
        PreflightVerdict::Allowed
    );
}

#[test]
fn a_gitfile_points_protection_at_the_real_git_directory() {
    let s = Scratch::new("gitfile");
    std::fs::create_dir_all(s.0.join("main/.git/worktrees/linked/refs")).unwrap();
    std::fs::create_dir_all(s.0.join("linked/src")).unwrap();
    std::fs::create_dir_all(s.0.join("modules/sub.git/objects")).unwrap();
    std::fs::create_dir_all(s.0.join("super/sub")).unwrap();
    // A linked worktree registered as its own repository, and a relative submodule pointer.
    std::fs::write(
        s.0.join("linked/.git"),
        format!("gitdir: {}\n", s.p("main/.git/worktrees/linked")),
    )
    .unwrap();
    std::fs::write(
        s.0.join("super/sub/.git"),
        "gitdir: ../../modules/sub.git\n",
    )
    .unwrap();
    let roots = ProtectedRoots::new(
        vec![
            RegisteredRepository {
                id: id("linked"),
                location: s.p("linked"),
                revision: 1,
            },
            RegisteredRepository {
                id: id("sub"),
                location: s.p("super/sub"),
                revision: 1,
            },
        ],
        Vec::new(),
        None,
    );
    let run = |kind, target: String| {
        preflight(
            &NativeFilesystem::new(),
            &roots,
            &EffectPlan {
                kind,
                targets: vec![target],
                working_directory: None,
                scope: IntendedScope::Ordinary,
            },
        )
    };
    let meta = |r: &str| BlockReason::RepositoryMetadata {
        repository_id: id(r),
    };
    assert_eq!(
        blocked(&run(
            DestructiveKind::RemoveTree,
            s.p("main/.git/worktrees/linked")
        )),
        Some(&meta("linked"))
    );
    assert_eq!(
        blocked(&run(
            DestructiveKind::RemoveTree,
            s.p("main/.git/worktrees/linked/refs")
        )),
        Some(&meta("linked"))
    );
    // A recursive step over the ancestor of an external Git directory destroys it too.
    assert_eq!(
        blocked(&run(DestructiveKind::RemoveTree, s.p("modules"))),
        Some(&meta("sub"))
    );
    assert_eq!(
        blocked(&run(DestructiveKind::Unlink, s.p("linked/.git"))),
        Some(&meta("linked"))
    );
}

#[test]
fn an_unresolvable_protected_root_fails_closed() {
    let s = Scratch::new("unresolvable");
    let roots = ProtectedRoots::new(
        vec![RegisteredRepository {
            id: id("repo-b"),
            location: "relative/repo".into(),
            revision: 1,
        }],
        Vec::new(),
        None,
    );
    let d = preflight(
        &NativeFilesystem::new(),
        &roots,
        &EffectPlan {
            kind: DestructiveKind::RemoveTree,
            targets: vec!["relative/repo".into()],
            working_directory: Some(s.p("")),
            scope: IntendedScope::Ordinary,
        },
    );
    assert_eq!(
        d.verdict,
        PreflightVerdict::NeedsInput(InputNeeded::UnresolvedProtectedRoot)
    );
    // An unresolvable application data root likewise.
    let roots = ProtectedRoots::new(Vec::new(), Vec::new(), Some("data".into()));
    let d = preflight(
        &NativeFilesystem::new(),
        &roots,
        &EffectPlan {
            kind: DestructiveKind::Unlink,
            targets: vec![s.p("x")],
            working_directory: None,
            scope: IntendedScope::Ordinary,
        },
    );
    assert_eq!(
        d.verdict,
        PreflightVerdict::NeedsInput(InputNeeded::UnresolvedProtectedRoot)
    );
}

#[cfg(unix)]
#[test]
fn a_repository_moved_into_the_tree_after_preflight_is_not_deleted() {
    use devworks_core::authority::destructive::DestructiveExecutor;
    let t = tree("moved-in");
    let doomed = t.s.p("outside/doomed");
    std::fs::create_dir_all(format!("{doomed}/a")).unwrap();
    std::fs::write(format!("{doomed}/a/f"), b"x").unwrap();
    let decision = check(&t, DestructiveKind::RemoveTree, &[&doomed], None);
    assert_eq!(decision.verdict, PreflightVerdict::Allowed);
    // Between preflight and execution the registered repository is renamed into the target.
    std::fs::rename(t.s.0.join("repo"), t.s.0.join("outside/doomed/repo")).unwrap();
    let fs = NativeFilesystem::new();
    let outcome = fs
        .execute(
            &decision.allowed_targets().unwrap()[0],
            DestructiveKind::RemoveTree,
            decision.protected_identities(),
        )
        .unwrap();
    assert!(!outcome.complete);
    assert!(
        outcome.remaining.iter().any(|r| r.ends_with("repo")),
        "{outcome:?}"
    );
    assert!(t.s.0.join("outside/doomed/repo/src/lib.rs").exists());
    assert!(!t.s.0.join("outside/doomed/a").exists());
    // Without the protected set the same step would have taken the repository.
    assert!(!decision.protected_identities().is_empty());
}

#[test]
fn windows_device_names_and_streams_are_not_plain_components() {
    use super::windows_unsafe_component as unsafe_name;
    for name in [
        "CON",
        "con",
        "NUL.txt",
        "aux.",
        "COM1",
        "lpt9.log",
        "COM\u{b9}",
        "PRN ",
        "a:stream",
        "file.txt:$DATA",
        "CONIN$",
    ] {
        assert!(unsafe_name(name), "{name}");
    }
    for name in ["CONSOLE", "nullable.rs", "COM10", "lib.rs", "LPT", "comma"] {
        assert!(!unsafe_name(name), "{name}");
    }
}

/// The verification probe's argv cases. None may come back allowed or as plain residual risk.
#[cfg(unix)]
mod probe {
    use super::*;

    struct Fixture {
        s: Scratch,
        roots: ProtectedRoots,
        repo: String,
        work: String,
        wt: String,
    }
    fn fixture() -> Fixture {
        let s = Scratch::new("probe");
        let repo = s.0.join("repo");
        std::fs::create_dir_all(repo.join(".git/objects/aa")).unwrap();
        std::fs::create_dir_all(repo.join(".git/worktrees/wt")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(repo.join("keep"), "k").unwrap();
        std::fs::create_dir_all(s.0.join("caf\u{e9}/.git")).unwrap();
        std::fs::create_dir_all(s.0.join("wt/src")).unwrap();
        std::fs::write(
            s.0.join("wt/.git"),
            format!("gitdir: {}\n", repo.join(".git/worktrees/wt").display()),
        )
        .unwrap();
        std::fs::write(s.0.join("wt/src/a.rs"), "x").unwrap();
        std::fs::create_dir_all(s.0.join("work")).unwrap();
        let roots = ProtectedRoots::new(
            vec![
                RegisteredRepository {
                    id: id("repo-a"),
                    location: s.p("repo"),
                    revision: 1,
                },
                RegisteredRepository {
                    id: id("repo-b"),
                    location: s.p("caf\u{e9}"),
                    revision: 1,
                },
            ],
            vec![OwnedWorktree {
                id: id("wt-a"),
                repository_id: id("repo-a"),
                location: s.p("wt"),
                stopped: true,
                active_users: 0,
            }],
            None,
        );
        Fixture {
            repo: s.p("repo"),
            work: s.p("work"),
            wt: s.p("wt"),
            roots,
            s,
        }
    }

    /// `true` when the assessment (and every plan's preflight) let the command through.
    fn allowed(f: &Fixture, argv: &[String], cwd: &str) -> Option<String> {
        let fs = NativeFilesystem::new();
        match assess_command(argv, Some(cwd)) {
            ShellAssessment::Destructive { plans, .. } => {
                let verdicts: Vec<_> = plans
                    .iter()
                    .map(|p| preflight(&fs, &f.roots, p).verdict)
                    .collect();
                verdicts
                    .iter()
                    .all(|v| *v == PreflightVerdict::Allowed)
                    .then(|| format!("ALLOWED {verdicts:?}"))
            }
            ShellAssessment::NoRecognizedDestruction { .. } => Some("NO_RECOGNIZED".into()),
            ShellAssessment::NeedsInput(_) | ShellAssessment::ProtectedBranch { .. } => None,
        }
    }
    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    const POSIX: &[&str] = &[
        "rm -rf {R}",
        "rm -rf \"$(printf {R})\"",
        "bash -c \"bash -c 'rm -rf {R}'\"",
        "env -i rm -rf {R}",
        "command -p rm -rf {R}",
        "\\rm -rf {R}",
        "/bin/rm -rf {R}",
        "busybox rm -rf {R}",
        "perl -e 'use File::Path; rmtree(\"{R}\")'",
        "python3 -c 'import shutil; shutil.rmtree(\"{R}\")'",
        "git -C {R} clean -fdx",
        "git --git-dir={R}/.git gc --prune=now",
        "rm -rf -- {R}",
        "rm -r -f {R}/",
        "rm -rf {R}/.",
        "rm -rf {R}/..",
        "rm -rf {B}//repo//",
        "rm -rf {B}/rep*",
        "rm -rf {R}/.git/*",
        "rm -rf {R}/*",
        ": > {R}/.git/HEAD",
        "dd if=/dev/zero of={R}/.git/HEAD",
        "unlink {R}/.git/HEAD",
        "shred -u {R}/.git/HEAD",
        "find -L {R} -delete",
        "find {R} -delete",
        "find {B} -name repo -exec rm -rf {} +",
        "rsync -a --delete {B}/work/ {R}/",
        "git worktree remove --force {R}",
        "git clean -ffdx {R}",
        "mv {R} {B}/gone",
        "mv -t {B}/gone {R}",
        "if true; then rm -rf {R}; fi",
        "{ rm -rf {R}; }",
        "f() { rm -rf {R}; }; f",
        "[ -d {R} ] && rm -rf {R}",
        "rm -rf {R} & wait",
        "nohup rm -rf {R}",
        "exec -a x rm -rf {R}",
        "timeout -k 5 10 rm -rf {R}",
        "nice -n5 rm -rf {R}",
        "echo {R} | xargs rm -rf",
        "rm -rf $'{R}'",
        "cd {B} && rm -rf repo",
        "rm -rf {B}/{repo,x}",
        "echo rm -rf {R} | sh",
        "bash -c -- 'rm -rf {R}'",
        "bash -c -e 'rm -rf {R}'",
        "bash -O extglob -c 'rm -rf {R}'",
        "time -p rm -rf {R}",
        "sudo -iu root rm -rf {R}",
        "sudo -Eu root rm -rf {R}",
        "env -iu FOO rm -rf {R}",
        "doas -nu root rm -rf {R}",
        "trap 'rm -rf {R}' EXIT",
        "bash <<< 'rm -rf {R}'",
        "bash <(echo rm -rf {R})",
        "/bin/r? -rf {R}",
        "/bin/r[m] -rf {R}",
        "rm -rf ~michael",
        "rm -rf ~+/x",
        "zsh -c 'rm -rf {B}/re(po|zz)'",
        "fish --command 'rm -rf {R}'",
        "csh -c 'rm -rf {R}'",
        "tcsh -c 'rm -rf {R}'",
        "mv -ft {B}/gone {R}",
        "GIT_WORK_TREE={R} git clean -fdx",
        "git --work-tree={R} clean -fdx",
        "git --git-dir={R}/.git --work-tree={R} clean -fdx",
        "git push origin '+refs/heads/*:refs/heads/*'",
        "git push -f origin @",
        "git push -f origin main",
        "git push origin +HEAD:Main",
        "git push origin :refs/heads/MAIN",
        "git -c alias.p='push -f' p origin main",
        "git -c alias.x='!rm -rf {R}' x",
        "cp /dev/null {R}/.git/HEAD",
        "echo x | tee {R}/.git/HEAD",
        "truncate -s 0 {R}/.git/HEAD",
        "mv {B}/work/x {R}/.git/HEAD",
        "ln -sfn /tmp {R}/.git/HEAD",
        "git -C {R} reset --hard",
        "git -C {R} rm -rf .",
        "git -C {R} update-ref -d refs/heads/main",
        "git -C {R} branch -D main",
        "alias x='rm -rf {R}'; x",
        "node -e 'require(\"fs\").rmSync(\"{R}\", {recursive: true})'",
        "ruby -e 'require \"fileutils\"; FileUtils.rm_rf(\"{R}\")'",
    ];
    const POWERSHELL: &[&str] = &[
        "Remove-Item {R} -Recurse -Force",
        "ri -r {R}",
        "rd -r {R}",
        "del -r {R}",
        "erase -r {R}",
        "rmdir -r {R}",
        "rm -r {R}",
        "Remove-Item -Path {R} -Recurse",
        "Remove-Item -LiteralPath {R} -Recurse",
        "Remove-Item -Path:{R} -Recurse",
        "[IO.Directory]::Delete('{R}', $true)",
        "[System.IO.Directory]::Delete(\"{R}\", $true)",
        "[IO.File]::WriteAllText('{R}/.git/HEAD', 'x')",
        "Get-Item {R} | Remove-Item -Recurse",
        "if (1) { Remove-Item {R} -Recurse }",
        "& { Remove-Item {R} -Recurse }",
        "try { Remove-Item {R} -Recurse } catch {}",
        "1 | ForEach-Object { Remove-Item {R} -Recurse }",
        "foreach ($i in 1) { Remove-Item {R} -Recurse }",
        "Remove-Item {R},{B}/x -Recurse",
        "Rename-Item {R} gone",
        "Move-Item {R} {B}/gone",
        "Clear-Content {R}/.git/HEAD",
        "Set-Content {R}/.git/HEAD x",
        "'x' | Out-File {R}/.git/HEAD",
        "Copy-Item {B}/work/x {R}/.git/HEAD",
    ];
    const CMD: &[&str] = &[
        "del /s /q {R}",
        "rd /s /q {R}",
        "rmdir /S /Q {R}",
        "rd /s/q {B}",
        "del /f/s/q {B}",
        "if exist {R} rd /s /q {R}",
        "if 1==1 rd /s /q {R}",
        "(rd /s /q {R})",
        "for %d in ({R}) do rd /s /q %d",
        "move {R} {B}/gone",
        "ren {R} gone",
        "copy nul {R}/.git/HEAD",
    ];

    #[test]
    fn verification_probe_cases_are_never_allowed() {
        let f = fixture();
        let b = f.s.p("");
        let b = b.trim_end_matches('/');
        let sub = |s: &str| s.replace("{R}", &f.repo).replace("{B}", b);
        let mut leaks = Vec::new();
        for case in POSIX {
            let s = sub(case);
            if let Some(v) = allowed(&f, &argv(&["sh", "-c", &s]), &f.work) {
                leaks.push(format!("sh -c {case}: {v}"));
            }
        }
        for case in POWERSHELL {
            let s = sub(case);
            for shell in ["pwsh", "powershell"] {
                for flag in ["-Command", "-CommandWithArgs"] {
                    if let Some(v) = allowed(&f, &argv(&[shell, flag, &s]), &f.work) {
                        leaks.push(format!("{shell} {flag} {case}: {v}"));
                    }
                }
            }
        }
        for case in CMD {
            let s = sub(case);
            let mut a = argv(&["cmd", "/c"]);
            a.extend(s.split(' ').map(str::to_owned));
            if let Some(v) = allowed(&f, &a, &f.work) {
                leaks.push(format!("cmd /c {case}: {v}"));
            }
        }
        let joined = argv(&["cmd", "/crd", "/s", "/q", &f.repo]);
        if let Some(v) = allowed(&f, &joined, &f.work) {
            leaks.push(format!("cmd /crd: {v}"));
        }
        // `git clean` excludes take a value; the target is still the repository.
        for case in [
            "git clean -fdx",
            "git clean -fdx -e keep",
            "git clean -fdx --exclude=keep",
        ] {
            if let Some(v) = allowed(&f, &argv(&["sh", "-c", case]), &f.repo) {
                leaks.push(format!("{case} in the repository: {v}"));
            }
        }
        // Case and Unicode-normalization aliases resolve to the same directory only on
        // filesystems that fold them; there they must be refused too.
        if std::path::Path::new(&format!("{b}/REPO")).exists() {
            for case in ["rm -rf {B}/REPO", "rm -rf {B}/REPO/.GIT/objects"] {
                if let Some(v) = allowed(&f, &argv(&["sh", "-c", &sub(case)]), &f.work) {
                    leaks.push(format!("{case}: {v}"));
                }
            }
        }
        if std::path::Path::new(&format!("{b}/cafe\u{301}")).exists() {
            let case = "rm -rf {B}/cafe\u{301}";
            if let Some(v) = allowed(&f, &argv(&["sh", "-c", &sub(case)]), &f.work) {
                leaks.push(format!("{case}: {v}"));
            }
        }
        assert!(
            leaks.is_empty(),
            "{} leaks:\n{}",
            leaks.len(),
            leaks.join("\n")
        );
    }

    #[test]
    fn commands_without_consequential_tokens_stay_residual_and_cleanup_stays_allowed() {
        let f = fixture();
        for case in [
            "ls -la",
            "echo hello",
            "cargo build --release",
            "python3 tools/gen.py",
            "sh scripts/check.sh",
            "grep -r TODO src",
        ] {
            assert!(
                matches!(
                    assess_command(&argv(&["sh", "-c", case]), Some(&f.work)),
                    ShellAssessment::NoRecognizedDestruction { .. }
                ),
                "{case}"
            );
        }
        // Modeled commands with ordinary targets are still precise plans.
        for (case, cwd) in [
            ("git clean -fdx", &f.wt),
            ("rm -rf target", &f.work),
            ("git status", &f.repo),
        ] {
            match assess_command(&argv(&["sh", "-c", case]), Some(cwd)) {
                ShellAssessment::Destructive { plans, .. } => {
                    for p in &plans {
                        assert_eq!(
                            preflight(&NativeFilesystem::new(), &f.roots, p).verdict,
                            PreflightVerdict::Allowed,
                            "{case}"
                        );
                    }
                }
                ShellAssessment::NoRecognizedDestruction { .. } => {}
                other => panic!("{case}: {other:?}"),
            }
        }
    }

    #[test]
    fn the_backstop_is_what_holds_the_unmodeled_forms() {
        use devworks_core::authority::shell::assess_structured_only;
        let f = fixture();
        // Without the backstop, these forms come back as nothing destructive.
        for case in [
            "trap 'rm -rf {R}' EXIT",
            "alias x='rm -rf {R}'; x",
            "eval 'rm -rf {R}'",
            "echo 'git clean -fdx'",
        ] {
            let a = argv(&["sh", "-c", &case.replace("{R}", &f.repo)]);
            assert!(
                matches!(
                    assess_structured_only(&a, Some(&f.work)),
                    ShellAssessment::NoRecognizedDestruction { .. }
                ),
                "{case}"
            );
            assert!(
                matches!(
                    assess_command(&a, Some(&f.work)),
                    ShellAssessment::NeedsInput(InputNeeded::UnmodeledConsequentialCommand)
                ),
                "{case}"
            );
        }
    }
}

/// Blocked > NeedsInput > allowed plans > residual, across every step and both layers.
mod precedence {
    use super::*;
    use devworks_core::authority::shell::{Blocking, CommandVerdict, preflight_command};

    /// A path quoted for a POSIX `sh` script: forward slashes (a backslash is an escape under
    /// `sh`, and Windows accepts `/`) inside single quotes.
    fn q(path: &str) -> String {
        format!("'{}'", path.replace('\\', "/"))
    }
    /// A path quoted for PowerShell: a single-quoted literal (no escapes inside).
    fn ps(path: &str) -> String {
        format!("'{path}'")
    }

    fn verdict(t: &Tree, argv: &[&str], cwd: &str) -> CommandVerdict {
        let argv: Vec<String> = argv.iter().map(|s| (*s).to_owned()).collect();
        preflight_command(&NativeFilesystem::new(), &t.roots, &argv, Some(cwd), &[])
    }
    fn blocked_reasons(v: &CommandVerdict) -> Vec<&'static str> {
        match v {
            CommandVerdict::Blocked { blocked, .. } => {
                blocked.iter().map(Blocking::reason).collect()
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn a_refusable_step_blocks_the_whole_command_whatever_else_it_holds() {
        let t = tree("precedence");
        let raw = t.s.p("repo");
        let repo = q(&raw);
        let git = |rest: &str| q(&format!("{raw}/.git/{rest}"));
        let cwd = t.s.p("outside");
        let sh = |script: String| verdict(&t, &["sh", "-c", &script], &cwd);
        assert_eq!(
            blocked_reasons(&sh(format!("if true; then rm -rf {repo}; fi; echo $(x)"))),
            ["PrimaryRepository"]
        );
        // Held by the backstop as well: still Blocked, with the hold kept as secondary.
        match sh(format!("rm -rf {repo}; cp a b")) {
            CommandVerdict::Blocked { blocked, held } => {
                assert_eq!(blocked.len(), 1);
                assert!(held.contains(&InputNeeded::UnmodeledConsequentialCommand));
                assert!(blocked[0].rationale().contains("dedicated"));
            }
            other => panic!("{other:?}"),
        }
        // A resolvable operand next to a dynamic one is still checked.
        assert_eq!(
            blocked_reasons(&sh(format!("rm -rf {repo} $X"))),
            ["PrimaryRepository"]
        );
        // Every refused step is reported.
        assert_eq!(
            blocked_reasons(&sh(format!(
                "rm -rf {}; : > {}; rm -rf {repo}",
                git("objects"),
                git("HEAD")
            ))),
            [
                "RepositoryMetadata",
                "RepositoryMetadata",
                "PrimaryRepository"
            ]
        );
        // A protected-branch push mixed with an unmodeled command.
        assert_eq!(
            blocked_reasons(&sh("git push -f origin main; cp a b".into())),
            ["ProtectedBranch"]
        );
        // A PowerShell block deleting the repository next to an unmodeled construct.
        let script = format!(
            "Rename-Item x y; if (1) {{ Remove-Item {} -Recurse }}",
            ps(&raw)
        );
        assert_eq!(
            blocked_reasons(&verdict(&t, &["pwsh", "-Command", &script], &cwd)),
            ["PrimaryRepository"]
        );
        let script = format!(
            "try {{ Remove-Item {} -Recurse }} catch {{ Set-Content x y }}",
            ps(&raw)
        );
        assert_eq!(
            blocked_reasons(&verdict(&t, &["pwsh", "-Command", &script], &cwd)),
            ["PrimaryRepository"]
        );
    }

    #[test]
    fn approving_a_held_command_never_dispatches_a_refused_step() {
        let t = tree("approval");
        let repo = q(&t.s.p("repo"));
        let cwd = t.s.p("outside");
        let argv = |script: String| vec!["sh".to_owned(), "-c".to_owned(), script];
        let run = |roots: &ProtectedRoots, script: String| {
            preflight_command(
                &NativeFilesystem::new(),
                roots,
                &argv(script),
                Some(&cwd),
                &[],
            )
        };
        // Blocked: an approval has nothing to dispatch.
        let blocked = run(&t.roots, format!("rm -rf {repo}; cp a b"));
        assert!(matches!(blocked, CommandVerdict::Blocked { .. }));
        assert!(blocked.dispatchable_after_approval().is_none());
        // Held only: approval may dispatch exactly the allowed steps.
        std::fs::create_dir_all(t.s.0.join("outside/build")).unwrap();
        let script = format!("rm -rf {}; cp a b", q(&t.s.p("outside/build")));
        let held = run(&t.roots, script.clone());
        let CommandVerdict::NeedsInput { allowed, .. } = &held else {
            panic!("{held:?}")
        };
        assert_eq!(allowed.len(), 1);
        assert_eq!(held.dispatchable_after_approval().map(<[_]>::len), Some(1));
        // If the target became protected before the approval is acted on, the required fresh
        // preflight refuses it and the approval dispatches nothing.
        let mut repos = t.roots.repositories.clone();
        repos.push(RegisteredRepository {
            id: id("build"),
            location: t.s.p("outside/build"),
            revision: 1,
        });
        let now = ProtectedRoots::new(
            repos,
            t.roots.worktrees.clone(),
            t.roots.app_data_root.clone(),
        );
        let fresh = run(&now, script);
        assert_eq!(blocked_reasons(&fresh), ["PrimaryRepository"]);
        assert!(fresh.dispatchable_after_approval().is_none());
    }

    #[test]
    fn a_windows_path_mangled_by_sh_escapes_is_held_never_allowed() {
        let t = tree("mangled");
        let cwd = t.s.p("outside");
        // Under `sh` the backslashes are escapes: `C:\Users\x\repo` becomes `C:Usersxrepo`,
        // which names nothing that was meant. It must be held, on every platform.
        for script in [
            r"rm -rf C:\Users\runner\repo".to_owned(),
            format!("rm -rf {}", t.s.p("repo").replace('/', "\\")),
            "rm -rf C:repo".to_owned(),
            r"rm -rf \\server\share\repo".to_owned(),
        ] {
            match verdict(&t, &["sh", "-c", &script], &cwd) {
                CommandVerdict::NeedsInput { held, allowed } => {
                    assert!(allowed.is_empty(), "{script}: {allowed:?}");
                    assert!(
                        held.iter().any(|h| matches!(
                            h,
                            InputNeeded::UnresolvedTarget | InputNeeded::UnresolvedExpansion
                        )),
                        "{script}: {held:?}"
                    );
                }
                other => panic!("{script}: {other:?}"),
            }
        }
        // The alias-bypass idiom `\rm` stays a precise, refusable command.
        let repo = q(&t.s.p("repo"));
        assert_eq!(
            blocked_reasons(&verdict(
                &t,
                &["sh", "-c", &format!(r"\rm -rf {repo}")],
                &cwd
            )),
            ["PrimaryRepository"]
        );
    }
}

/// Windows path forms resolve through the same identity checks, or are not resolved at all.
#[cfg(windows)]
#[test]
fn windows_path_forms_resolve_through_identity_or_are_held() {
    let t = tree("winforms");
    let fs = NativeFilesystem::new();
    // Drive-relative and device-namespace paths are never resolved.
    for path in ["C:repo", "C:", r"\\.\PhysicalDrive0", r"\\.\C:\x"] {
        assert!(fs.resolve(path, None).is_err(), "{path}");
        assert_eq!(
            check(&t, DestructiveKind::RemoveTree, &[path], None).verdict,
            PreflightVerdict::NeedsInput(InputNeeded::UnresolvedTarget),
            "{path}"
        );
    }
    // The verbatim form and the 8.3 short form (the runner's temp directory) of the
    // registered root are the same protected root.
    let canonical = std::fs::canonicalize(t.s.0.join("repo")).unwrap();
    let verbatim = canonical.to_string_lossy().into_owned();
    assert!(verbatim.starts_with(r"\\?\"), "{verbatim}");
    assert_eq!(
        blocked(&check(&t, DestructiveKind::RemoveTree, &[&verbatim], None)),
        Some(&repo_block())
    );
    let forward = t.s.p("repo").replace('\\', "/");
    assert_eq!(
        blocked(&check(&t, DestructiveKind::RemoveTree, &[&forward], None)),
        Some(&repo_block())
    );
    assert_eq!(
        blocked(&check(
            &t,
            DestructiveKind::RemoveTree,
            &[&t.s.p("REPO")],
            None
        )),
        Some(&repo_block())
    );
}

#[test]
fn physical_identity_survives_rename_and_refuses_replaced_read_roots() {
    let t = Scratch::new("physical-identity");
    let fs = NativeFilesystem::new();
    let repo = t.0.join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("data"), b"original").unwrap();
    let locate = |path: &std::path::Path| {
        fs.resolve(path.to_str().unwrap(), None)
            .unwrap()
            .target
            .unwrap()
    };
    let original = locate(&repo);
    let identity = original
        .identity
        .expect("native physical directory identity");
    assert_eq!(
        locate(&std::fs::canonicalize(&repo).unwrap()).identity,
        Some(identity)
    );
    let root = ReadRoot {
        location: original.path,
        identity: Some(identity),
    };
    let request = ReadRequest {
        root: &root,
        relative: "data",
        max_bytes: 64,
        allow_hard_links: false,
    };
    assert_eq!(fs.read(&request).unwrap().bytes, b"original");
    let moved = t.0.join("moved");
    std::fs::rename(&repo, &moved).unwrap();
    assert_eq!(locate(&moved).identity, Some(identity));
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("data"), b"replacement").unwrap();
    assert_ne!(locate(&repo).identity, Some(identity));
    assert!(
        locate(&repo.join("data"))
            .ancestors
            .contains(&locate(&repo).identity.unwrap())
    );
    assert_eq!(fs.read(&request), Err(ReadRefusal::RootChanged));
}
