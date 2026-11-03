//! Pure authority contracts: shell/declared-effect recognition, redaction, provider disclosure
//! and the access-profile catalog. Real-store and real-filesystem cases live in
//! `devworks-adapters`.
use devworks_core::authority::AccessProfile;
use devworks_core::authority::destructive::{DestructiveKind, InputNeeded};
use devworks_core::authority::disclosure::{Candidate, ContentKind, Exclusion, disclose};
use devworks_core::authority::redact::{REDACTED, Redactor, SecretRef};
use devworks_core::authority::shell::{
    DeclaredEffect, ShellAssessment, assess_command, assess_command_with, assess_declared,
};
use devworks_core::ports::storage::GrantKind;
use devworks_core::settings::catalog::{AgentRole, descriptor};
use serde_json::json;

fn argv(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| (*s).to_owned()).collect()
}
fn plans(a: &[&str]) -> Vec<(DestructiveKind, Vec<String>)> {
    match assess_command(&argv(a), Some("/work")) {
        ShellAssessment::Destructive { plans, residual } => {
            assert!(!residual.is_empty(), "residual risk is always reported");
            plans.into_iter().map(|p| (p.kind, p.targets)).collect()
        }
        other => panic!("{a:?}: {other:?}"),
    }
}
fn needs(a: &[&str]) -> InputNeeded {
    match assess_command(&argv(a), Some("/work")) {
        ShellAssessment::NeedsInput(n) => n,
        other => panic!("{a:?}: {other:?}"),
    }
}

#[test]
fn shell_deletions_are_recognized_from_arguments_not_command_strings() {
    use DestructiveKind::*;
    assert_eq!(plans(&["rm", "-rf", "/"]), [(RemoveTree, vec!["/".into()])]);
    assert_eq!(
        plans(&["/bin/rm", "-f", "--", "-r"]),
        [(Unlink, vec!["-r".into()])]
    );
    assert_eq!(
        plans(&["env", "A=1", "sudo", "-n", "rm", "--recursive", "a", "b"]),
        [(RemoveTree, vec!["a".into(), "b".into()])]
    );
    assert_eq!(
        plans(&[
            "bash",
            "-lc",
            "true && rm -r 'my dir' ; echo hi > out.txt 2>/dev/null"
        ]),
        [
            (RemoveTree, vec!["my dir".into()]),
            (Truncate, vec!["out.txt".into()])
        ]
    );
    assert_eq!(
        plans(&["find", "build", "-name", "*.o", "-delete"]),
        [(EraseContents, vec!["build".into()])]
    );
    assert_eq!(
        plans(&["find", ".", "-exec", "rm", "{}", ";"]),
        [(EraseContents, vec![".".into()])]
    );
    assert_eq!(
        plans(&["cmd.exe", "/c", "rd", "/s", "/q", "C:\\"]),
        [(RemoveTree, vec!["C:\\".into()])]
    );
    assert_eq!(
        plans(&["cmd", "/c", "del", "/s", "/q", "D:\\data"]),
        [(EraseContents, vec!["D:\\data".into()])]
    );
    assert_eq!(
        plans(&[
            "pwsh",
            "-Command",
            "Remove-Item",
            "-Recurse",
            "-Force",
            "-Path",
            "C:\\repo"
        ]),
        [(RemoveTree, vec!["C:\\repo".into()])]
    );
    assert_eq!(plans(&["unlink", "link"]), [(Unlink, vec!["link".into()])]);
}

#[test]
fn unresolved_expansion_and_opaque_targets_are_held() {
    assert_eq!(
        needs(&["sh", "-c", "rm -rf \"$HOME\"/x"]),
        InputNeeded::UnresolvedExpansion
    );
    assert_eq!(
        needs(&["sh", "-c", "rm -rf `pwd`"]),
        InputNeeded::UnresolvedExpansion
    );
    assert_eq!(
        needs(&["sh", "-c", "rm -rf $(ls)"]),
        InputNeeded::UnresolvedExpansion
    );
    assert_eq!(
        needs(&["cmd", "/c", "rd", "/s", "%USERPROFILE%"]),
        InputNeeded::UnresolvedExpansion
    );
    assert_eq!(
        needs(&["xargs", "-0", "rm", "-rf"]),
        InputNeeded::UnresolvedTarget
    );
    // Single quotes are literal.
    assert_eq!(
        plans(&["sh", "-c", "rm '$literal'"]),
        [(DestructiveKind::Unlink, vec!["$literal".into()])]
    );
    // Nothing recognized is reported with residual risk, never as safe.
    for cmd in [
        &["ls", "-la"][..],
        &["python3", "-c", "import shutil"],
        &["sh", "script.sh"],
    ] {
        match assess_command(&argv(cmd), None) {
            ShellAssessment::NoRecognizedDestruction { residual } => {
                assert!(residual.contains("not proven harmless"))
            }
            other => panic!("{other:?}"),
        }
    }
    // Declared MCP/package effects: destructive intent without targets is held.
    assert!(matches!(
        assess_declared(
            &DeclaredEffect {
                destructive: true,
                recursive: true,
                targets: None
            },
            None
        ),
        ShellAssessment::NeedsInput(InputNeeded::UnresolvedDeclaredTarget)
    ));
    match assess_declared(
        &DeclaredEffect {
            destructive: true,
            recursive: true,
            targets: Some(vec!["/srv/x".into()]),
        },
        None,
    ) {
        ShellAssessment::Destructive { plans, .. } => {
            assert_eq!(plans[0].kind, DestructiveKind::RemoveTree)
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn redaction_removes_known_values_headers_typed_fields_and_token_shapes() {
    let mut r = Redactor::new();
    r.register("hunter2-secret-value");
    let text = "login with hunter2-secret-value\nAuthorization: Bearer abc.def\npassword=letmein token: x123 ok\nkey ghp_0123456789abcdefABCDEF here\nsha 0123456789abcdef0123456789abcdef01234567\n";
    let out = r.redact_text(text);
    assert!(!out.contains("hunter2-secret-value"));
    assert!(!out.contains("abc.def"));
    assert!(!out.contains("letmein"));
    assert!(!out.contains("x123"));
    assert!(!out.contains("ghp_0123456789abcdefABCDEF"));
    assert!(out.contains("Authorization: [REDACTED]"));
    assert!(out.contains("password=[REDACTED]"));
    // Ordinary content survives, including a lowercase Git object ID.
    assert!(out.contains("0123456789abcdef0123456789abcdef01234567"));
    assert!(out.contains(" ok\n"));
    let v = r.redact_json(&json!({
        "user": "me",
        "api_key": "k",
        "nested": [{"clientSecret": "s", "note": "uses hunter2-secret-value"}],
        "refresh-token": null,
    }));
    assert_eq!(v["user"], "me");
    assert_eq!(v["api_key"], REDACTED);
    assert_eq!(v["nested"][0]["clientSecret"], REDACTED);
    assert_eq!(v["nested"][0]["note"], format!("uses {REDACTED}"));
    assert!(v["refresh-token"].is_null());
    // Secret references are opaque IDs and never print a value.
    // Secret references use the settings format and survive redaction, as text and as a
    // credential setting value.
    let minted = SecretRef::mint(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
    assert_eq!(minted.as_str(), "dwsecret-0123456789abcdef0123456789abcdef");
    assert_eq!(SecretRef::parse(minted.as_str()), Some(minted.clone()));
    assert_eq!(
        format!("{minted:?}"),
        format!("SecretRef({})", minted.as_str())
    );
    assert!(SecretRef::parse("github-default").is_none());
    assert!(SecretRef::parse("dwsecret-ghp_0123456789abcdefABCD").is_none());
    let line = format!("using {} for GitHub", minted.as_str());
    assert_eq!(r.redact_text(&line), line);
    let setting =
        json!({"credential": {"secretRef": minted.as_str()}, "token": "raw-session-value"});
    let v = r.redact_json(&setting);
    assert_eq!(v["credential"]["secretRef"], minted.as_str());
    assert_eq!(v["token"], REDACTED);
}

#[test]
fn provider_disclosure_matches_what_is_sent() {
    let mut r = Redactor::new();
    r.register("sk-live-0123456789abcdef");
    let (sent, record) = disclose(
        "https://api.provider.invalid",
        "model-a",
        None,
        &r,
        &[
            Candidate {
                kind: ContentKind::File,
                project_id: None,
                source: "src/lib.rs",
                revision: Some("abc"),
                text: "fn main() {}",
                authorized: true,
            },
            Candidate {
                kind: ContentKind::File,
                project_id: None,
                source: ".env",
                revision: None,
                text: "KEY=sk-live-0123456789abcdef",
                authorized: true,
            },
            Candidate {
                kind: ContentKind::Snippet,
                project_id: None,
                source: "other-project/notes.md",
                revision: None,
                text: "private",
                authorized: false,
            },
        ],
    );
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().all(|t| !t.contains("sk-live")));
    assert_eq!(record.items.len(), 2);
    assert!(record.items[1].redacted);
    assert_eq!(record.items[0].bytes, 12);
    let reasons: Vec<_> = record
        .excluded
        .iter()
        .map(|e| (e.source.as_str(), e.reason))
        .collect();
    assert_eq!(
        reasons,
        [
            (".env", Exclusion::SecretRedacted),
            ("other-project/notes.md", Exclusion::Unauthorized)
        ]
    );
    let data = serde_json::to_value(&record).unwrap();
    assert_eq!(data["destination"], "https://api.provider.invalid");
    assert_eq!(data["items"][0]["source"], "src/lib.rs");
}

#[test]
fn access_profile_setting_defaults_to_full_and_names_round_trip() {
    let d = descriptor("safety.accessProfile").unwrap();
    assert_eq!(d.default_value(), json!("full"));
    assert!(
        !d.locked(),
        "the profile is a user choice; mandatory guards are separate policies"
    );
    for p in [AccessProfile::Full, AccessProfile::ProjectScope] {
        assert_eq!(AccessProfile::parse(p.as_str()), Some(p));
    }
    assert_eq!(AccessProfile::parse("sandboxed"), None);
    for r in AgentRole::ALL {
        assert_eq!(AgentRole::parse(r.as_str()), Some(r));
    }
    for k in [
        GrantKind::Operation,
        GrantKind::NativeExecution,
        GrantKind::PrimaryRepositoryDeletion,
    ] {
        assert_eq!(GrantKind::parse(k.as_str()), Some(k));
    }
    for key in [
        "safety.reviewersReadOnly",
        "safety.protectMainBranch",
        "safety.destructiveOperationGuard",
    ] {
        assert!(
            descriptor(key).unwrap().locked(),
            "{key} stays harness-enforced"
        );
    }
}

fn sh(script: &str) -> ShellAssessment {
    assess_command(&argv(&["sh", "-c", script]), Some("/tmp"))
}
fn sh_plans(script: &str) -> Vec<(DestructiveKind, Vec<String>)> {
    match sh(script) {
        ShellAssessment::Destructive { plans, .. } => {
            plans.into_iter().map(|p| (p.kind, p.targets)).collect()
        }
        other => panic!("{script}: {other:?}"),
    }
}
fn sh_needs(script: &str) -> InputNeeded {
    match sh(script) {
        ShellAssessment::NeedsInput(n) => n,
        other => panic!("{script}: {other:?}"),
    }
}

#[test]
fn compound_commands_groups_substitutions_and_wrapper_options_are_seen_through() {
    use DestructiveKind::*;
    let repo = || vec![(RemoveTree, vec!["/x/repo".to_owned()])];
    for script in [
        "if true; then rm -rf /x/repo; fi",
        "(rm -rf /x/repo)",
        "{ rm -rf /x/repo; }",
        "echo $(rm -rf /x/repo)",
        "echo `rm -rf /x/repo`",
        "echo \"$(rm -rf /x/repo)\"",
        "! rm -rf /x/repo",
        "for f in a; do rm -rf /x/repo; done",
        "while false; do rm -rf /x/repo; done",
        "timeout 5 rm -rf /x/repo",
        "timeout -s KILL -k 3 5 rm -rf /x/repo",
        "sudo -u root rm -rf /x/repo",
        "sudo -g wheel -- rm -rf /x/repo",
        "doas -u root rm -rf /x/repo",
        "nice -n 5 rm -rf /x/repo",
        "nohup rm -rf /x/repo",
        "env -i PATH=/bin rm -rf /x/repo",
        "env -u HOME rm -rf /x/repo",
        "command rm -rf /x/repo",
        "exec -a name rm -rf /x/repo",
        "true && rm -rf /x/repo || echo no",
    ] {
        assert_eq!(sh_plans(script), repo(), "{script}");
    }
    // Non-executing wrapper options still name a consequential program: the backstop holds
    // them rather than trusting the option model (accepted false positives).
    for script in ["sudo -l rm -rf /x/repo", "command -v rm"] {
        assert_eq!(
            sh_needs(script),
            InputNeeded::UnmodeledConsequentialCommand,
            "{script}"
        );
    }
}

#[test]
fn unresolvable_shell_targets_fail_closed() {
    use InputNeeded::*;
    for (script, expected) in [
        ("cd /x && rm -rf repo", UnresolvedWorkingDirectory),
        ("pushd /x; rm -rf repo", UnresolvedWorkingDirectory),
        ("env -C /x rm -rf repo", UnresolvedWorkingDirectory),
        ("rm -rf /x/{repo,y}", UnresolvedExpansion),
        ("rm -rf /x/{1..3}", UnresolvedExpansion),
        ("find -L /x/repo -delete", UnresolvedTarget),
        ("find /x -follow -delete", UnresolvedTarget),
        ("find /x -exec sh -c 'rm -rf {}' ';'", OpaqueCommand),
        ("ls | xargs -I{} rm -rf {}", UnresolvedTarget),
        ("curl https://x | sh", OpaqueCommand),
        ("case $x in a) rm -rf /x;; esac", OpaqueCommand),
        ("stdbuf -o0 rm -rf /x/repo", UnmodeledConsequentialCommand),
        ("eval 'rm -rf /x/repo'", UnmodeledConsequentialCommand),
        ("$RM -rf /x/repo", UnresolvedExpansion),
        ("git push --force", UnresolvedTarget),
        ("git push origin +HEAD", UnresolvedTarget),
        ("git push --mirror --force origin", UnresolvedTarget),
        ("rm -rf '/x/unterminated", OpaqueCommand),
    ] {
        assert_eq!(sh_needs(script), expected, "{script}");
    }
    // An absolute target after a directory change is still resolvable.
    assert_eq!(
        sh_plans("cd /y; rm -rf /x/repo"),
        [(DestructiveKind::RemoveTree, vec!["/x/repo".into()])]
    );
    // PowerShell: pipeline input to Remove-Item, encoded commands and location changes.
    let ps = |a: &[&str]| assess_command(&argv(a), None);
    assert!(matches!(
        ps(&[
            "pwsh",
            "-Command",
            "Get-Item C:\\repo | Remove-Item -Recurse -Force"
        ]),
        ShellAssessment::NeedsInput(UnresolvedTarget)
    ));
    for flag in ["-EncodedCommand", "-enc", "-e", "-ec"] {
        assert!(
            matches!(
                ps(&["powershell", flag, "UgBlAG0AbwB2AGUA"]),
                ShellAssessment::NeedsInput(UnresolvedExpansion)
            ),
            "{flag}"
        );
    }
    assert!(matches!(
        ps(&[
            "pwsh",
            "-Command",
            "Set-Location C:\\x; Remove-Item -Recurse repo"
        ]),
        ShellAssessment::NeedsInput(UnresolvedWorkingDirectory)
    ));
    assert!(matches!(
        ps(&["cmd", "/c", "cd /d C:\\x && rd /s /q repo"]),
        ShellAssessment::NeedsInput(UnresolvedWorkingDirectory)
    ));
}

#[test]
fn git_moves_and_rsync_deletions_become_plans_or_protected_branch_refusals() {
    use DestructiveKind::*;
    for script in [
        "git push -f origin main",
        "git push --force-with-lease origin HEAD:main",
        "git push origin +master",
        "git push origin :main",
        "git push --delete origin main",
        "git push -d origin MAIN",
        "git push origin +refs/heads/Main",
        "git -C /x/repo push --force origin main",
    ] {
        assert!(
            matches!(sh(script), ShellAssessment::ProtectedBranch { .. }),
            "{script}"
        );
    }
    // The remote default branch, when known, is protected the same way.
    let trunk = vec!["trunk".to_owned()];
    assert!(matches!(
        assess_command_with(
            &argv(&["git", "push", "-f", "origin", "trunk"]),
            None,
            &trunk
        ),
        ShellAssessment::ProtectedBranch { .. }
    ));
    // Ordinary and feature-branch pushes are not refused here.
    for script in ["git push origin main", "git push -f origin feature/x"] {
        assert!(
            !matches!(sh(script), ShellAssessment::ProtectedBranch { .. }),
            "{script}"
        );
    }
    assert_eq!(
        sh_plans("git clean -fdx"),
        [(EraseContents, vec![".".into()])]
    );
    assert_eq!(
        sh_plans("git -C /x/repo clean -ffdx"),
        [(EraseContents, vec!["/x/repo".into()])]
    );
    assert!(matches!(
        sh("git clean -n -d"),
        ShellAssessment::NoRecognizedDestruction { .. }
    ));
    assert_eq!(
        sh_plans("git worktree remove --force /x/wt"),
        [(RemoveTree, vec!["/x/wt".into()])]
    );
    // The sources leave, and the destination may be overwritten.
    assert_eq!(
        sh_plans("mv /x/repo /y/"),
        [
            (Move, vec!["/x/repo".into()]),
            (Truncate, vec!["/y/".into()])
        ]
    );
    assert_eq!(
        sh_plans("mv -t /y /x/repo /x/other"),
        [
            (Move, vec!["/x/repo".into(), "/x/other".into()]),
            (Truncate, vec!["/y".into()])
        ]
    );
    assert_eq!(
        sh_plans("mv -ft /y /x/repo"),
        [
            (Move, vec!["/x/repo".into()]),
            (Truncate, vec!["/y".into()])
        ]
    );
    assert_eq!(
        sh_plans("rsync -a --delete /src/ /x/repo/"),
        [(EraseContents, vec!["/x/repo/".into()])]
    );
    assert!(matches!(
        sh("rsync -a --delete /src/ host:/x/repo/"),
        ShellAssessment::NoRecognizedDestruction { .. }
    ));
    assert_eq!(
        sh_plans("dd if=/dev/zero of=/x/disk.img"),
        [(Truncate, vec!["/x/disk.img".into()])]
    );
}

#[test]
fn redaction_covers_userinfo_flags_secret_fields_and_short_pem_blocks() {
    let r = Redactor::new();
    let out = r.redact_text(
        "git clone https://bob:s3cretpass@example.invalid/r.git\nmysql -pS3cretPass9 db\nrun --password hunter2x --token=abc123 --secret xyz789 -pthread\n",
    );
    for leaked in ["s3cretpass", "S3cretPass9", "hunter2x", "abc123", "xyz789"] {
        assert!(!out.contains(leaked), "{leaked} in {out}");
    }
    assert!(out.contains("https://bob:[REDACTED]@example.invalid/r.git"));
    assert!(out.contains("-pthread"), "ordinary flags survive");
    let pem = "before\n-----BEGIN PRIVATE KEY-----\nMIIBVQ==\n-----END PRIVATE KEY-----\nafter";
    assert_eq!(r.redact_text(pem), format!("before\n{REDACTED}\nafter"));
    let open = r.redact_text("-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA");
    assert!(!open.contains("b3BlbnNzaA"));
    let cert = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----";
    assert_eq!(r.redact_text(cert), cert, "public certificates are kept");
    let v = r.redact_json(&json!({
        "aws_secret_access_key": "x",
        "client_secret_id": "y",
        "client_secret": "w",
        "credentialRevision": "7",
        "credentialRef": "store-entry-9",
        "dbPassword": "z",
        "tokenCount": 3,
        "secretRef": "dwsecret-0123456789abcdef0123456789abcdef",
    }));
    assert_eq!(v["aws_secret_access_key"], REDACTED);
    // Metadata about a secret (identifiers, revisions, references) is kept.
    assert_eq!(v["client_secret_id"], "y");
    assert_eq!(v["credentialRevision"], "7");
    assert_eq!(v["credentialRef"], "store-entry-9");
    assert_eq!(v["client_secret"], REDACTED);
    assert_eq!(v["dbPassword"], REDACTED);
    assert_eq!(v["tokenCount"], 3);
    assert_eq!(v["secretRef"], "dwsecret-0123456789abcdef0123456789abcdef");
}

#[test]
fn consequential_git_and_file_writers_are_refused_planned_or_held_never_residual() {
    use DestructiveKind::*;
    for script in [
        "git -C /x/repo branch -D main",
        "git branch --delete --force master",
        "git update-ref -d refs/heads/main",
    ] {
        assert!(
            matches!(sh(script), ShellAssessment::ProtectedBranch { .. }),
            "{script}"
        );
    }
    for (script, target) in [
        ("git -C /x/repo reset --hard", "/x/repo"),
        ("git -C /x/repo rm -rf .", "/x/repo"),
        ("git -C /x/repo gc --prune=now", "/x/repo"),
        ("git -C /x/repo branch -D feature/x", "/x/repo"),
        ("git -C /x/repo checkout -- .", "/x/repo"),
        ("git -C /x/repo stash clear", "/x/repo"),
    ] {
        assert_eq!(
            sh_plans(script),
            [(Consequential, vec![target.to_owned()])],
            "{script}"
        );
    }
    for script in [
        "git push origin '+refs/heads/*:refs/heads/*'",
        "git push -f origin @",
        "git -c alias.p='push -f' p origin main",
        "git -c core.worktree=/x/repo clean -fdx",
        "git --git-dir=/x/repo/.git gc",
        "GIT_DIR=/x/repo/.git git clean -fdx",
        "git filter-branch --force HEAD",
        "cp /dev/null /x/repo/.git/HEAD",
        "echo x | tee /x/repo/.git/HEAD",
        "truncate -s 0 /x/repo/.git/HEAD",
        "ln -sfn /tmp /x/repo/.git/HEAD",
        "rm --unknown-flag /x/repo",
        "perl -e 'unlink q(/x/repo/a)'",
    ] {
        assert!(
            matches!(sh(script), ShellAssessment::NeedsInput(_)),
            "{script}"
        );
    }
    let ps = |s: &str| assess_command(&argv(&["pwsh", "-Command", s]), None);
    for script in [
        "Set-Content C:/x/repo/.git/HEAD x",
        "Clear-Content C:/x/repo/.git/HEAD",
        "'x' | Out-File C:/x/repo/.git/HEAD",
        "Rename-Item C:/x/repo gone",
        "Copy-Item a C:/x/repo/.git/HEAD",
        "[IO.File]::Delete($p)",
    ] {
        assert!(
            matches!(
                ps(script),
                ShellAssessment::NeedsInput(InputNeeded::UnmodeledConsequentialCommand)
                    | ShellAssessment::NeedsInput(InputNeeded::UnresolvedExpansion)
            ),
            "{script}"
        );
    }
    // A literal .NET delete target becomes a consequential plan (refused for protected roots).
    match ps("[IO.Directory]::Delete('C:/x/repo', $true)") {
        ShellAssessment::Destructive { plans, .. } => {
            assert_eq!(plans[0].kind, Consequential);
            assert_eq!(plans[0].targets, ["C:/x/repo"]);
        }
        other => panic!("{other:?}"),
    }
    // Joined cmd switches and a joined `/c`.
    let cmd = |a: &[&str]| assess_command(&argv(a), None);
    match cmd(&["cmd", "/crd", "/s/q", "C:\\x"]) {
        ShellAssessment::Destructive { plans, .. } => {
            assert_eq!(
                (plans[0].kind, plans[0].targets.clone()),
                (RemoveTree, vec!["C:\\x".to_owned()])
            )
        }
        other => panic!("{other:?}"),
    }
    match cmd(&["cmd", "/c", "del", "/f/s/q", "C:\\x"]) {
        ShellAssessment::Destructive { plans, .. } => assert_eq!(plans[0].kind, EraseContents),
        other => panic!("{other:?}"),
    }
}
