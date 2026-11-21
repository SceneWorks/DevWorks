//! Settings catalog, validation and view-model behavior (pure; storage-backed use cases run
//! against real SQLite in `devworks-adapters`).
use devworks_core::admission::{self, Selection};
use devworks_core::ports::storage::ConfigScope;
use devworks_core::protocol::records::{SettingEntry, ValidationReport};
use devworks_core::protocol::{Id, ProjectId};
use devworks_core::registry::{self, Mode};
use devworks_core::settings::catalog::{
    AgentRole, Category, NOT_YET_ACTIVE, NOT_YET_ACTIVE_PREFIX, ScopeKind, ValueKind, WritePolicy,
    catalog, descriptor,
};
use devworks_core::settings::service::OPERATIONS;
use devworks_core::settings::validate::{
    SECRET_REF_PREFIX, check_directory, check_value, codes, looks_like_credential, mint_secret_ref,
};
use devworks_core::settings::view::{Health, SettingsView, Source, Status, ViewError};
use serde_json::{Value, json};

fn entry(key: &str, value: Value, scope: Value, revision: i64) -> SettingEntry {
    let d = descriptor(key).unwrap();
    serde_json::from_value(json!({
        "key": key,
        "value": {"schema": d.schema_ref(), "value": value},
        "sourceScope": scope,
        "revision": revision.to_string(),
    }))
    .unwrap()
}
fn defaults() -> Vec<SettingEntry> {
    catalog()
        .iter()
        .map(|d| entry(d.key, d.default_value(), json!({"kind": "Workspace"}), 1))
        .collect()
}

#[test]
fn the_catalog_is_typed_unique_and_self_consistent() {
    let mut keys = std::collections::HashSet::new();
    for d in catalog() {
        assert!(keys.insert(d.key), "duplicate key {}", d.key);
        assert!(d.scopes.contains(&ScopeKind::Workspace), "{}", d.key);
        // Schema references are valid qualified IDs and stable digests.
        let r = serde_json::to_value(d.schema_ref()).unwrap();
        let reparsed: devworks_core::protocol::provider::SchemaRef =
            serde_json::from_value(r.clone()).expect("valid schema reference");
        assert_eq!(serde_json::to_value(reparsed).unwrap(), r);
        // Defaults satisfy their own type and never look like credentials.
        if !d.default_value().is_null() {
            assert_eq!(check_value(d, &d.default_value()), vec![], "{}", d.key);
        }
        assert!(!d.help_id().is_empty() && !d.label.is_empty());
    }
    for c in Category::ALL {
        assert!(catalog().iter().any(|d| d.category == c), "{c:?} is empty");
    }
    for role in AgentRole::ALL {
        let d = descriptor(role.instructions_key()).expect("every role has instructions");
        assert_eq!(d.kind, ValueKind::Instructions);
        assert!(!d.locked());
    }
    // Retained engineering defaults and scopes.
    let triage = descriptor("github.issueTriageIntervalSeconds").unwrap();
    assert_eq!(triage.default_value(), json!(1800));
    assert_eq!(triage.scopes, &[ScopeKind::Workspace]);
    assert_eq!(
        descriptor("reviews.automationWaitSeconds")
            .unwrap()
            .default_value(),
        json!(900)
    );
    assert!(
        descriptor("repositories.baseDirectory")
            .unwrap()
            .allows(ScopeKind::Project)
    );
    assert!(descriptor("safety.reviewersReadOnly").unwrap().locked());
    assert!(descriptor("delivery.mandatoryReview").unwrap().locked());
    let tiers: Vec<Value> = ["Max", "High", "Mid", "Low"]
        .iter()
        .map(|t| {
            descriptor(&format!("routing.tier{t}.members"))
                .unwrap()
                .default_value()
        })
        .collect();
    assert_eq!(
        tiers,
        [
            json!(["fable-latest", "astra-latest"]),
            json!(["opus-latest", "sol-latest"]),
            json!(["sonnet-latest", "terra-latest"]),
            json!(["haiku-latest", "luna-latest"])
        ]
    );
    assert!(descriptor("nope.unknown").is_none());
}

#[test]
fn settings_nothing_reads_are_marked_not_yet_active() {
    for key in [
        "delivery.autoMerge",
        "delivery.maxReviewFixCycles",
        "reviews.automationWaitSeconds",
        "knowledge.visionExtraction",
    ] {
        assert!(descriptor(key).unwrap().not_yet_active(), "{key}");
    }
    for key in [
        "github.pollForChanges",
        "github.importOpenIssues",
        "github.syncLinkedIssueFields",
        "github.issueTriageIntervalSeconds",
    ] {
        assert!(!descriptor(key).unwrap().not_yet_active(), "{key}");
    }
    // The list and the visible description agree in both directions, and a harness policy is
    // never listed: it is shown as an enforced protection, not as a value awaiting a reader.
    for d in catalog() {
        assert_eq!(
            d.not_yet_active(),
            d.description.starts_with(NOT_YET_ACTIVE_PREFIX),
            "{}",
            d.key
        );
        assert!(!(d.not_yet_active() && d.policy == WritePolicy::HarnessEnforced));
    }
    assert!(NOT_YET_ACTIVE.iter().all(|key| descriptor(key).is_some()));
    assert!(!descriptor("web.timeoutMs").unwrap().not_yet_active());
    assert!(
        !descriptor("agents.compactionThresholdPercent")
            .unwrap()
            .not_yet_active()
    );
}

#[test]
fn settings_operations_are_registered_to_the_configuration_service() {
    for op in OPERATIONS {
        let d = registry::find(op).expect("registered");
        assert_eq!((d.family, d.owner), ("settings", "ConfigurationService"));
    }
    // The recorded decode rule still guards the app-only triage interval at admission.
    let d = descriptor("github.issueTriageIntervalSeconds").unwrap();
    let frame = json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": "k",
        "body": {"kind": "settings.save_revision", "scope": {"kind": "Project", "projectId": "p"},
            "expectedRevision": "1", "entries": [{"key": d.key, "value": {"schema": d.schema_ref(), "value": 60}}]}});
    assert!(
        admission::decode(
            Selection {
                operation: "settings.save_revision",
                mode: Mode::Command
            },
            frame.to_string().as_bytes()
        )
        .is_err()
    );
}

#[test]
fn values_are_validated_by_type_and_credentials_are_refused() {
    let fix = descriptor("delivery.maxReviewFixCycles").unwrap();
    assert_eq!(check_value(fix, &json!(21))[0].code, codes::RANGE);
    assert_eq!(check_value(fix, &json!("3"))[0].code, codes::TYPE);
    let runtime = descriptor("containers.preferredRuntime").unwrap();
    assert_eq!(check_value(runtime, &json!("lxc"))[0].code, codes::CHOICE);
    let tiers = descriptor("routing.tierMax.members").unwrap();
    assert_eq!(check_value(tiers, &json!(["a", "a"]))[0].code, codes::TYPE);
    assert!(check_value(tiers, &json!(["fable", "custom-1"])).is_empty());
    let tools = descriptor("tools.overrides").unwrap();
    assert!(
        check_value(
            tools,
            &json!({"devworks/sessions.get": false,
        "mcp/server.read": true})
        )
        .is_empty()
    );
    assert!(!check_value(tools, &json!({"devworks/sessions.get": "false"})).is_empty());
    assert!(!check_value(tools, &json!({"bad capability": true})).is_empty());

    let secret = descriptor("github.credential").unwrap();
    for raw in [
        json!("hunter2"),
        json!({"secretRef": "x", "token": "y"}),
        json!(42),
    ] {
        assert_eq!(
            check_value(secret, &raw)[0].code,
            codes::RAW_CREDENTIAL,
            "{raw}"
        );
    }
    assert!(check_value(secret, &json!({"secretRef": "dwsecret-0123456789abcdef"})).is_empty());
    // Every minted reference is valid, including ids whose hex would look like a token in
    // another alphabet; a sweep over bit patterns covers the whole digit range.
    assert_eq!(
        mint_secret_ref(0),
        format!("{SECRET_REF_PREFIX}{}", "0".repeat(32))
    );
    for shift in 0..128 {
        for id in [
            1u128 << shift,
            u128::MAX >> shift,
            0xA41A_u128 << (shift % 112),
        ] {
            let minted = mint_secret_ref(id);
            assert!(
                check_value(secret, &json!({ "secretRef": minted })).is_empty(),
                "{minted}"
            );
        }
    }
    // A raw token in the reference field, a token wrapped in the reference shape, references
    // that are not host-minted and a minted id that is itself credential-shaped are all refused.
    let github = format!("ghp_{}", "A1b2C3d4".repeat(5));
    for reference in [
        github.clone(),
        format!("{SECRET_REF_PREFIX}{github}"),
        "secret-1".to_owned(),
        format!("{SECRET_REF_PREFIX}short"),
        format!("{SECRET_REF_PREFIX}AKIAIOSFODNN7EXAMPLE"),
    ] {
        let issues = check_value(secret, &json!({ "secretRef": reference }));
        assert_eq!(
            issues.first().map(|i| i.code),
            Some(codes::RAW_CREDENTIAL),
            "{reference}"
        );
    }
    let token = format!("sk-{}", "x".repeat(40));
    assert!(looks_like_credential(&format!("use \"{token}\" please")));
    assert!(looks_like_credential(
        "-----BEGIN RSA PRIVATE KEY-----\nabc"
    ));
    assert!(!looks_like_credential("sk-learn and ghp_ are words"));
    let notes = descriptor("roles.worker.instructions").unwrap();
    assert_eq!(
        check_value(notes, &json!(format!("token {token}")))[0].code,
        codes::RAW_CREDENTIAL
    );

    assert!(check_directory("k", "/srv/repos").is_empty());
    assert!(check_directory("k", "~/Repos").is_empty());
    assert!(check_directory("k", "~\\Repos").is_empty());
    assert!(check_directory("k", "C:\\Users\\me\\Repos").is_empty());
    assert_eq!(
        check_directory("k", "Repos")[0].code,
        codes::PATH_NOT_ABSOLUTE
    );
    assert_eq!(
        check_directory("k", "/srv/../etc")[0].code,
        codes::PATH_INVALID
    );
    assert_eq!(check_directory("k", " /srv")[0].code, codes::PATH_INVALID);
}

#[test]
fn view_model_searches_categorizes_and_tracks_drafts() {
    let mut view = SettingsView::new(ConfigScope::Workspace, 1, &defaults());
    let total: usize = view.categories().iter().map(|(_, n)| n).sum();
    assert_eq!(total, catalog().len());
    view.set_category(Some(Category::Instructions));
    assert_eq!(view.visible().len(), AgentRole::ALL.len());
    view.set_category(None);
    view.set_query("triage");
    let keys: Vec<&str> = view.visible().iter().map(|i| i.descriptor.key).collect();
    assert_eq!(keys, ["github.issueTriageIntervalSeconds"]);
    assert_eq!(view.categories(), [(Category::GitHub, 1)]);
    view.set_query("");

    // Harness policies are visible but never editable.
    let policy = view.item("safety.reviewersReadOnly").unwrap();
    assert_eq!(
        (policy.status, &policy.source),
        (Status::Enforced, &Source::HarnessPolicy)
    );
    assert_eq!(
        view.edit("safety.reviewersReadOnly", json!(false))
            .unwrap_err(),
        ViewError::NotEditable
    );

    // Local validation reuses the service rules; drafts survive until saved or discarded.
    let issues = view
        .edit("delivery.maxReviewFixCycles", json!(0))
        .unwrap()
        .to_vec();
    assert_eq!(issues[0].code, codes::RANGE);
    assert!(view.has_blocking_issues());
    view.edit("delivery.maxReviewFixCycles", json!(4)).unwrap();
    assert!(!view.has_blocking_issues());
    view.edit("power.keepAwake", json!(true)).unwrap();
    assert_eq!(
        view.dirty_keys(),
        ["delivery.maxReviewFixCycles", "power.keepAwake"]
    );
    let input = view.save_input().unwrap();
    assert_eq!(input.expected_revision.get(), 1);
    assert_eq!(input.entries.0.len(), 2);
    view.discard("power.keepAwake");
    assert_eq!(view.dirty_keys(), ["delivery.maxReviewFixCycles"]);

    // A concurrent save elsewhere: drafts are preserved and the conflict names changed keys.
    let mut newer = defaults();
    let i = newer
        .iter()
        .position(|e| e.key.0 == "delivery.maxReviewFixCycles")
        .unwrap();
    newer[i] = entry(
        "delivery.maxReviewFixCycles",
        json!(7),
        json!({"kind": "Workspace"}),
        2,
    );
    view.refresh(2, &newer);
    let conflict = view.conflict.clone().unwrap();
    assert_eq!(
        (conflict.previous_revision, conflict.current_revision),
        (1, 2)
    );
    assert_eq!(conflict.changed, ["delivery.maxReviewFixCycles"]);
    assert_eq!(
        view.item("delivery.maxReviewFixCycles").unwrap().shown(),
        &json!(4)
    );
    assert_eq!(view.save_input().unwrap().expected_revision.get(), 2);
}

#[test]
fn view_model_distinguishes_inherited_overridden_and_health() {
    let project = ConfigScope::Project(ProjectId(Id::new("project-a").unwrap()));
    let mut entries = defaults();
    let base = entries
        .iter()
        .position(|e| e.key.0 == "repositories.baseDirectory")
        .unwrap();
    entries[base] = entry(
        "repositories.baseDirectory",
        json!("/srv/repos"),
        json!({"kind": "Workspace"}),
        3,
    );
    let mut view = SettingsView::new(project, 1, &entries);
    let item = view.item("repositories.baseDirectory").unwrap();
    assert_eq!(
        (item.status, &item.source),
        (Status::Inherited, &Source::Workspace { revision: 3 })
    );
    assert_eq!(
        view.item("tools.shell").unwrap().status,
        Status::WorkspaceOnly
    );
    assert_eq!(
        view.item("delivery.autoMerge").unwrap().status,
        Status::Default
    );
    // Inherit on a value not overridden here is not a change.
    view.inherit("repositories.baseDirectory").unwrap();
    assert!(!view.is_dirty());

    // Validation advisories become health; the configured value is unchanged.
    let report: ValidationReport = serde_json::from_value(json!({
        "valid": true,
        "issues": [{"path": "repositories.baseDirectory", "code": codes::PATH_UNAVAILABLE, "message": "/srv/repos does not exist or is not mounted"}]
    }))
    .unwrap();
    view.apply_report(&report);
    let item = view.item("repositories.baseDirectory").unwrap();
    assert!(matches!(item.health, Health::Unavailable(_)));
    assert_eq!(item.effective, json!("/srv/repos"));
    assert!(item.issues.is_empty());

    // An overridden project value can be cleared back to inheritance.
    entries[base] = entry(
        "repositories.baseDirectory",
        json!("~/work"),
        json!({"kind": "Project", "projectId": "project-a"}),
        2,
    );
    view.refresh(2, &entries);
    assert_eq!(
        view.item("repositories.baseDirectory").unwrap().status,
        Status::Overridden
    );
    view.inherit("repositories.baseDirectory").unwrap();
    let input = view.save_input().unwrap();
    assert!(input.entries.0[0].value.value.0.is_null());
    let explained = [
        entries[base].clone(),
        entry(
            "repositories.baseDirectory",
            json!("/srv/repos"),
            json!({"kind": "Workspace"}),
            3,
        ),
        entry(
            "repositories.baseDirectory",
            json!("~/DevWorks/Repositories"),
            json!({"kind": "Workspace"}),
            1,
        ),
    ];
    view.apply_explanation("repositories.baseDirectory", &explained);
    let item = view.item("repositories.baseDirectory").unwrap();
    assert_eq!(item.chain.len(), 3);
    assert_eq!(item.chain[2].source, Source::Default);
    assert_eq!(
        item.shown(),
        &json!("/srv/repos"),
        "a cleared override shows the inherited value"
    );
}

#[test]
fn package_index_sources_default_to_none_and_need_a_location_and_key() {
    let d = descriptor("packages.indexSources").unwrap();
    assert_eq!((d.default)(), json!([]));
    assert_eq!(d.scopes, &[ScopeKind::Workspace]);
    let key = "0123456789abcdef".repeat(4);
    // An absolute directory on this platform (a drive letter on Windows).
    let dir = if cfg!(windows) {
        r"C:\srv\packs"
    } else {
        "/srv/packs"
    };
    for ok in [
        json!([]),
        json!([{"location": "https://packs.example.test/index.json", "publicKey": key}]),
        json!([{"location": dir, "publicKey": key}]),
    ] {
        assert!(check_value(d, &ok).is_empty(), "{ok}");
    }
    for bad in [
        json!([{"location": "http://packs.example.test/index.json", "publicKey": key}]),
        json!([{"location": "relative/packs", "publicKey": key}]),
        json!([{"location": dir, "publicKey": key.to_uppercase()}]),
        json!([{"location": dir, "publicKey": "abc"}]),
        json!([{"location": dir}]),
        json!([{"location": dir, "publicKey": key, "extra": 1}]),
        json!("https://packs.example.test/index.json"),
        Value::Array(vec![json!({"location": dir, "publicKey": key}); 17]),
    ] {
        assert!(!check_value(d, &bad).is_empty(), "{bad}");
    }
}

fn repository_scope() -> ConfigScope {
    ConfigScope::Repository {
        project: ProjectId(Id::new("project-a").unwrap()),
        repository: Id::new("repo-a").unwrap(),
    }
}

#[test]
fn only_delivery_settings_are_repository_scoped_and_the_mandate_is_not_one_of_them() {
    let repository_scoped: Vec<&str> = catalog()
        .iter()
        .filter(|d| d.allows(ScopeKind::Repository))
        .map(|d| d.key)
        .collect();
    assert_eq!(
        repository_scoped,
        [
            "delivery.autoMerge",
            "delivery.maxReviewFixCycles",
            "reviews.automationWaitSeconds"
        ]
    );
    // Every repository-scoped setting is also a project and a workspace setting.
    for d in catalog().iter().filter(|d| d.allows(ScopeKind::Repository)) {
        assert!(d.allows(ScopeKind::Project) && d.allows(ScopeKind::Workspace));
    }
    // The mandatory review is a harness protection: no scope (a repository included) can set or
    // weaken it, not even to a stricter value.
    let mandate = descriptor("delivery.mandatoryReview").unwrap();
    assert!(mandate.locked());
    assert_eq!(mandate.scopes, [ScopeKind::Workspace]);
    let layer = devworks_core::settings::model::Layer::initial(repository_scope());
    for value in [json!(false), json!(true)] {
        let plan = devworks_core::settings::model::plan(
            &layer,
            &[serde_json::from_value(json!({"key": mandate.key,
                "value": {"schema": mandate.schema_ref(), "value": value}}))
            .unwrap()],
        );
        assert!(
            plan.blocking().any(|i| i.code == codes::HARNESS_ENFORCED),
            "{value}"
        );
        assert!(plan.values.is_empty());
    }
    // A setting that is not repository-scoped is refused at a repository, not stored.
    let shell = descriptor("tools.shell").unwrap();
    let plan = devworks_core::settings::model::plan(
        &layer,
        &[serde_json::from_value(json!({"key": shell.key,
            "value": {"schema": shell.schema_ref(), "value": true}}))
        .unwrap()],
    );
    assert!(plan.blocking().any(|i| i.code == codes::SCOPE_NOT_ALLOWED));
    let base = descriptor("repositories.baseDirectory").unwrap();
    let plan = devworks_core::settings::model::plan(
        &layer,
        &[serde_json::from_value(json!({"key": base.key,
            "value": {"schema": base.schema_ref(), "value": "/srv/x"}}))
        .unwrap()],
    );
    assert!(plan.blocking().any(|i| i.code == codes::SCOPE_NOT_ALLOWED));
}

#[test]
fn a_repository_scope_has_a_project_qualified_key_and_inherits_through_its_project() {
    let scope = repository_scope();
    assert_eq!(scope.key(), "repository:project-a:repo-a");
    assert_eq!(ConfigScope::from_key(&scope.key()), Some(scope.clone()));
    assert_eq!(ConfigScope::from_key("repository:project-a"), None);
    assert_eq!(scope.project_id().map(|p| p.0.as_str()), Some("project-a"));
    assert_eq!(scope.target_id().as_str(), "repo-a");
    // The data scope names the owning project; a workspace scope cannot be narrowed.
    let project: devworks_core::protocol::data::DataScope =
        serde_json::from_value(json!({"kind": "Project", "projectId": "project-a"})).unwrap();
    let repo = Id::new("repo-a").unwrap();
    assert_eq!(
        ConfigScope::addressed(&project, Some(&repo)).unwrap(),
        scope
    );
    assert_eq!(
        ConfigScope::addressed(&project, None).unwrap(),
        ConfigScope::Project(ProjectId(Id::new("project-a").unwrap()))
    );
    let workspace: devworks_core::protocol::data::DataScope =
        serde_json::from_value(json!({"kind": "Workspace"})).unwrap();
    assert!(ConfigScope::addressed(&workspace, Some(&repo)).is_err());
}

#[test]
fn the_view_shows_a_repository_value_and_its_source_apart_from_the_project_and_workspace() {
    let project_scope = json!({"kind": "Project", "projectId": "project-a"});
    let mut entries = defaults();
    let put =
        |entries: &mut Vec<SettingEntry>, key: &str, value: Value, repo: Option<&str>, revision| {
            let i = entries.iter().position(|e| e.key.0 == key).unwrap();
            let mut e = entry(key, value, project_scope.clone(), revision);
            if let Some(repo) = repo {
                e.source_repository_id = Some(Id::new(repo).unwrap());
            }
            entries[i] = e;
        };
    // The repository's own value wins; the project's value shows for a setting it set.
    put(
        &mut entries,
        "delivery.maxReviewFixCycles",
        json!(9),
        Some("repo-a"),
        4,
    );
    put(&mut entries, "delivery.autoMerge", json!(false), None, 3);
    let mut view = SettingsView::new(repository_scope(), 4, &entries);
    let own = view.item("delivery.maxReviewFixCycles").unwrap();
    assert_eq!(
        (own.status, &own.source, &own.effective),
        (
            Status::Overridden,
            &Source::Repository { revision: 4 },
            &json!(9)
        )
    );
    assert_eq!(own.source.label(), "Repository");
    let inherited = view.item("delivery.autoMerge").unwrap();
    assert_eq!(
        (inherited.status, &inherited.source, &inherited.effective),
        (
            Status::Inherited,
            &Source::Project { revision: 3 },
            &json!(false)
        )
    );
    assert_eq!(
        view.item("reviews.automationWaitSeconds").unwrap().status,
        Status::Default
    );
    // The mandate is enforced and read-only; a workspace-only or project-only setting is not
    // editable at a repository.
    let mandate = view.item("delivery.mandatoryReview").unwrap();
    assert_eq!(mandate.status, Status::Enforced);
    assert!(!mandate.editable);
    assert_eq!(
        view.item("tools.shell").unwrap().status,
        Status::WorkspaceOnly
    );
    assert!(!view.item("repositories.baseDirectory").unwrap().editable);
    assert_eq!(
        view.edit("delivery.mandatoryReview", json!(false))
            .unwrap_err(),
        ViewError::NotEditable
    );
    // A draft saves against the repository: the input names the repository and its project.
    view.edit("delivery.autoMerge", json!(true)).unwrap();
    let input = view.save_input().unwrap();
    assert_eq!(
        input.repository_id.as_ref().map(|r| r.as_str()),
        Some("repo-a")
    );
    assert_eq!(serde_json::to_value(&input.scope).unwrap(), project_scope);
    // Project views never name a repository.
    let project_view = SettingsView::new(
        ConfigScope::Project(ProjectId(Id::new("project-a").unwrap())),
        1,
        &defaults(),
    );
    assert!(project_view.save_input().is_none());
}
