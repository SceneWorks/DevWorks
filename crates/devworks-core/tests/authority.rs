//! Pure authority contracts: shell/declared-effect recognition, redaction, provider disclosure
//! and the access-profile catalog. Real-store and real-filesystem cases live in
//! `devworks-adapters`.
use devworks_core::authority::AccessProfile;
use devworks_core::authority::destructive::{DestructiveKind, InputNeeded};
use devworks_core::authority::disclosure::{Candidate, ContentKind, Exclusion, disclose};
use devworks_core::authority::redact::{REDACTED, Redactor, SecretRef};
use devworks_core::authority::shell::{
    DeclaredEffect, ShellAssessment, assess_command, assess_declared,
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
            "cd x && rm -r 'my dir' ; echo hi > out.txt 2>/dev/null"
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
        json!({"credential": {"secretRef": minted.as_str()}, "token": {"secretRef": "raw"}});
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
