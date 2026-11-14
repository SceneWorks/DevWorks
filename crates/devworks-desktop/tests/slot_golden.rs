//! Persisted state stays readable while screens move to their own slot types. The fixtures were
//! written by `main` before the first screen migrated: every `Slot` variant's JSON and Debug
//! text, and a `ui-state.json` holding unconfirmed commands.
mod common;
use common::*;
use devworks_desktop::artifacts_screen::{ArtifactsSlot, BrowserSlot, ResourcesSlot};
use devworks_desktop::backend::{CommandDispatch, Slot};
use devworks_desktop::conventions_ops::ConventionsSlot;
use devworks_desktop::databases_ops::DatabasesSlot;
use devworks_desktop::graph_ops::GraphSlot;
use devworks_desktop::help_ops::HelpSlot;
use devworks_desktop::indexing_ops::IndexingSlot;
use devworks_desktop::instructions_ops::InstructionsSlot;
use devworks_desktop::memory::Memory;
use devworks_desktop::projects_ops::ProjectsSlot;
use devworks_desktop::screen::ScreenId;
use devworks_desktop::usage_ops::UsageSlot;
use serde::de::DeserializeOwned;

const SLOTS: &str = include_str!("fixtures/slots-main.jsonl");
const SLOTS_DEBUG: &str = include_str!("fixtures/slots-main-debug.txt");
const UI_STATE: &str = include_str!("fixtures/ui-state-main.json");

/// One value of every variant, in the order of the fixtures.
fn samples() -> Vec<Slot> {
    vec![
        Slot::PurgePrepare,
        Slot::PurgePage,
        Slot::PurgeConfirm,
        Slot::PurgeStatus,
        Slot::Databases(DatabasesSlot::DatabaseList(7, true)),
        Slot::Databases(DatabasesSlot::DatabaseObjects(7, true)),
        Slot::Databases(DatabasesSlot::DatabaseRelations(7, true)),
        Slot::Databases(DatabasesSlot::DatabasePrepare),
        Slot::Databases(DatabasesSlot::DatabaseCaPrepare),
        Slot::Databases(DatabasesSlot::DatabaseCa),
        Slot::Databases(DatabasesSlot::DatabaseCommand("s".into())),
        Slot::Graph(GraphSlot::GraphRead(7)),
        Slot::Graph(GraphSlot::GraphTrust("s".into())),
        Slot::Usage(UsageSlot::Usage(7, true)),
        Slot::Usage(UsageSlot::UsageOptions(7)),
        Slot::Usage(UsageSlot::Prices(7, true)),
        Slot::Usage(UsageSlot::PriceProbe(7)),
        Slot::Usage(UsageSlot::PriceOverride),
        Slot::Usage(UsageSlot::PriceRefresh),
        Slot::Usage(UsageSlot::PriceOperation),
        Slot::Help(HelpSlot::HelpTopics(7, true)),
        Slot::Help(HelpSlot::HelpProbe(7)),
        Slot::Help(HelpSlot::HelpRead(7)),
        Slot::Help(HelpSlot::HelpGenerate),
        Slot::ApprovalsList,
        Slot::ApprovalInspect("s".into()),
        Slot::ApprovalRefresh("s".into()),
        Slot::ApprovalContent("s".into()),
        Slot::ApprovalDecision("s".into()),
        Slot::ReviewQueue,
        Slot::NotificationsList,
        Slot::CompletionList,
        Slot::NotificationRead("s".into()),
        Slot::NotificationChange("s".into()),
        Slot::NotificationSettings,
        Slot::NotificationSettingsSave,
        Slot::OsNotificationPermission,
        Slot::SettingsRead,
        Slot::ContainerCapabilities,
        Slot::SettingsValidate,
        Slot::SettingsHealth,
        Slot::SettingsSave,
        Slot::SettingsSecret("s".into()),
        Slot::SettingsExplain("s".into()),
        Slot::SessionsList,
        Slot::SessionStart,
        Slot::Transcript("s".into()),
        Slot::TextPreview("s".into(), 7),
        Slot::Append("s".into()),
        Slot::Attach("s".into(), 7),
        Slot::Preview("s".into(), 7),
        Slot::DeleteAttachment("s".into()),
        Slot::Artifacts(ArtifactsSlot::ArtifactsList),
        Slot::Resources(ResourcesSlot::ResourcesList),
        Slot::Browser(BrowserSlot::BrowserSessions),
        Slot::Browser(BrowserSlot::ProjectBrowserSessions("s".into())),
        Slot::Browser(BrowserSlot::BrowserClose("s".into())),
        Slot::AddonsList,
        Slot::AddonsLogs("s".into()),
        Slot::AddonsCommand,
        Slot::AddonsSave {
            update: Some("o".into()),
            registry: -2,
            download: Some("/tmp/x".into()),
        },
        Slot::AddonsSave {
            update: None,
            registry: 4,
            download: None,
        },
        Slot::AddonsDiscovery,
        Slot::RouterCatalog(7),
        Slot::RouteSession("s".into()),
        Slot::RouteStatus("s".into()),
        Slot::RoutePin("s".into()),
        Slot::RouteFirst("s".into()),
        Slot::WebFetch,
        Slot::WebSearch,
        Slot::WebActivity,
        Slot::McpServers("s".into(), Some("o".into())),
        Slot::McpOperation("s".into()),
        Slot::McpCapabilities("s".into(), "s".into(), Some("o".into())),
        Slot::McpContent("s".into(), "s".into()),
        Slot::McpCommand,
        Slot::McpRelease,
        Slot::ProjectsList,
        Slot::Projects(ProjectsSlot::DeletedProjects),
        Slot::RepositoriesList,
        Slot::ProjectRepositories("s".into()),
        Slot::Projects(ProjectsSlot::ProjectCreate),
        Slot::Projects(ProjectsSlot::ProjectCommand("s".into())),
        Slot::Projects(ProjectsSlot::ProjectSettings("s".into())),
        Slot::Projects(ProjectsSlot::ProjectSettingsHealth("s".into())),
        Slot::Projects(ProjectsSlot::ProjectSettingsSave("s".into())),
        Slot::Projects(ProjectsSlot::RepositoryText("s".into(), true)),
        Slot::Projects(ProjectsSlot::RepositoryContent("s".into(), true)),
        Slot::Projects(ProjectsSlot::RemoveIndex("s".into())),
        Slot::Indexing(IndexingSlot::IndexStatus("s".into())),
        Slot::Indexing(IndexingSlot::IndexRuns("s".into())),
        Slot::Indexing(IndexingSlot::IndexCommand("s".into())),
        Slot::Indexing(IndexingSlot::IndexCancel("s".into())),
        Slot::Indexing(IndexingSlot::IndexRetire("s".into())),
        Slot::Indexing(IndexingSlot::IndexSourcesSave),
        Slot::Indexing(IndexingSlot::IndexDiscover),
        Slot::Conventions(ConventionsSlot::ConventionsList("s".into(), 7, true, true)),
        Slot::Conventions(ConventionsSlot::ConventionRead("s".into(), 7)),
        Slot::Conventions(ConventionsSlot::ConventionEditor("s".into(), "s".into(), 7)),
        Slot::Conventions(ConventionsSlot::ConventionVersions("s".into(), 7, true)),
        Slot::Conventions(ConventionsSlot::ConventionResources("s".into(), 7, true)),
        Slot::Conventions(ConventionsSlot::ConventionParents(
            "s".into(),
            "s".into(),
            7,
            true,
        )),
        Slot::Conventions(ConventionsSlot::ConventionCommand("s".into(), "s".into())),
        Slot::Conventions(ConventionsSlot::ConventionOperation("s".into(), "s".into())),
        Slot::Conventions(ConventionsSlot::ConventionPrepare("s".into(), "s".into())),
        Slot::Conventions(ConventionsSlot::ConventionArtifact("s".into(), "s".into())),
        Slot::Conventions(ConventionsSlot::ConventionTransfer("s".into(), "s".into())),
        Slot::KnowledgeList(7),
        Slot::KnowledgeRead("s".into()),
        Slot::KnowledgeDiagnostics,
        Slot::KnowledgePreview("s".into()),
        Slot::KnowledgeImportSave,
        Slot::KnowledgeImportPreview,
        Slot::KnowledgeImport,
        Slot::KnowledgeWrite,
        Slot::JobsList,
        Slot::JobsMore(7),
        Slot::JobsRefresh(7),
        Slot::ProjectJobs("s".into()),
        Slot::JobInspect("s".into()),
        Slot::JobHistory("s".into()),
        Slot::JobActiveRun("s".into()),
        Slot::JobPrompts(7, Some("o".into())),
        Slot::JobCreate,
        Slot::JobCommand("s".into()),
        Slot::WorkFeatures("s".into()),
        Slot::WorkWorkflow("s".into(), 7),
        Slot::WorkWorkflowCommand("s".into()),
        Slot::AllocationQueue("s".into(), Some("o".into()), Some(3)),
        Slot::AllocationClaim("s".into(), Some("o".into())),
        Slot::AllocationInspect("s".into()),
        Slot::AllocationRelease("s".into()),
        Slot::WorkCards("s".into()),
        Slot::WorkActivity("s".into(), Some("o".into()), 7),
        Slot::WorkMessagesHistory("s".into()),
        Slot::WorkMessagesInbox("s".into()),
        Slot::WorkMessagesEvents("s".into()),
        Slot::WorkMessagePost("s".into()),
        Slot::WorkMessageAck("s".into(), "s".into()),
        Slot::WorkDetail("s".into()),
        Slot::WorkGit("s".into()),
        Slot::WorkGitInspect("s".into(), "s".into()),
        Slot::Projects(ProjectsSlot::GitInspect("s".into())),
        Slot::Projects(ProjectsSlot::GitDiff("s".into())),
        Slot::Projects(ProjectsSlot::GitDiffContent("s".into())),
        Slot::WorkCommand("s".into()),
        Slot::WorkCreate("s".into()),
        Slot::WorkOrder("s".into()),
        Slot::WorkPlanText("s".into()),
        Slot::WorkPlanDrop("s".into()),
        Slot::WorkPlan("s".into()),
        Slot::WorkInspect("s".into()),
        Slot::WorkRun("s".into()),
        Slot::ArchiveCount("s".into(), true),
        Slot::WorkOpen("s".into(), true),
        Slot::Connections(7),
        Slot::ConnectionCatalog(7),
        Slot::EmbeddingSpaces(7),
        Slot::Configure(7),
        Slot::Instructions(InstructionsSlot::InstructionSkills(7)),
        Slot::Instructions(InstructionsSlot::InstructionRole("s".into())),
        Slot::Instructions(InstructionsSlot::InstructionHistory(7)),
        Slot::Instructions(InstructionsSlot::InstructionText(7)),
        Slot::Instructions(InstructionsSlot::InstructionResources(7)),
        Slot::Instructions(InstructionsSlot::InstructionPrepare(7)),
        Slot::Instructions(InstructionsSlot::InstructionArtifact(7)),
        Slot::Instructions(InstructionsSlot::InstructionCommand(7)),
    ]
}

/// The variant names of a migrated screen's slot enum, as serde reports them.
fn variant_names<T: DeserializeOwned>() -> Vec<String> {
    let error = serde_json::from_str::<T>("\"NoSuchSlotVariant\"")
        .err()
        .expect("an unknown variant is refused")
        .to_string();
    let expected = error
        .split_once("expected ")
        .expect("serde lists the variants")
        .1;
    expected
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect()
}

/// Every migrated screen's slot variant names.
fn screen_variants() -> Vec<(ScreenId, Vec<String>)> {
    vec![
        (ScreenId::Help, variant_names::<HelpSlot>()),
        (ScreenId::Usage, variant_names::<UsageSlot>()),
        (ScreenId::Graph, variant_names::<GraphSlot>()),
        (ScreenId::Databases, variant_names::<DatabasesSlot>()),
        (ScreenId::Instructions, variant_names::<InstructionsSlot>()),
        (ScreenId::Conventions, variant_names::<ConventionsSlot>()),
        (ScreenId::Artifacts, variant_names::<ArtifactsSlot>()),
        (ScreenId::Resources, variant_names::<ResourcesSlot>()),
        (ScreenId::Browser, variant_names::<BrowserSlot>()),
        (ScreenId::Projects, variant_names::<ProjectsSlot>()),
        (ScreenId::Indexing, variant_names::<IndexingSlot>()),
    ]
}

/// The variant name a slot's JSON carries.
fn tag(json: &serde_json::Value) -> String {
    match json {
        serde_json::Value::String(name) => name.clone(),
        serde_json::Value::Object(map) => map.keys().next().expect("one key").clone(),
        other => panic!("unexpected slot JSON {other}"),
    }
}

/// The Debug text of the slot a screen formats in its reply messages.
fn reported(slot: &Slot) -> String {
    match slot.screen() {
        ScreenId::Legacy => format!("{slot:?}"),
        ScreenId::Help => format!("{:?}", HelpSlot::try_from(slot.clone()).unwrap()),
        ScreenId::Usage => format!("{:?}", UsageSlot::try_from(slot.clone()).unwrap()),
        ScreenId::Graph => format!("{:?}", GraphSlot::try_from(slot.clone()).unwrap()),
        ScreenId::Databases => format!("{:?}", DatabasesSlot::try_from(slot.clone()).unwrap()),
        ScreenId::Instructions => {
            format!("{:?}", InstructionsSlot::try_from(slot.clone()).unwrap())
        }
        ScreenId::Conventions => {
            format!("{:?}", ConventionsSlot::try_from(slot.clone()).unwrap())
        }
        ScreenId::Artifacts => format!("{:?}", ArtifactsSlot::try_from(slot.clone()).unwrap()),
        ScreenId::Resources => format!("{:?}", ResourcesSlot::try_from(slot.clone()).unwrap()),
        ScreenId::Browser => format!("{:?}", BrowserSlot::try_from(slot.clone()).unwrap()),
        ScreenId::Projects => format!("{:?}", ProjectsSlot::try_from(slot.clone()).unwrap()),
        ScreenId::Indexing => format!("{:?}", IndexingSlot::try_from(slot.clone()).unwrap()),
    }
}

#[test]
fn every_slot_serializes_as_main_did() {
    let samples = samples();
    let lines: Vec<&str> = SLOTS.lines().collect();
    let debug: Vec<&str> = SLOTS_DEBUG.lines().collect();
    assert_eq!(samples.len(), lines.len());
    assert_eq!(samples.len(), debug.len());
    for ((slot, line), debug) in samples.iter().zip(&lines).zip(&debug) {
        assert_eq!(&serde_json::to_string(slot).unwrap(), line);
        assert_eq!(&serde_json::from_str::<Slot>(line).unwrap(), slot, "{line}");
        assert_eq!(&reported(slot), debug);
    }
}

#[test]
fn slot_variant_names_are_unique_across_screens() {
    let screens = screen_variants();
    let mut seen = std::collections::BTreeMap::new();
    for (id, names) in &screens {
        assert!(!names.is_empty(), "{id:?}");
        for name in names {
            assert_eq!(
                seen.insert(name.clone(), *id),
                None,
                "{name} is declared twice"
            );
        }
    }
    // Each persisted name deserializes to the screen that declares it: a migrated name the
    // remaining single enum also declared would be read back as that variant instead.
    for line in SLOTS.lines() {
        let json: serde_json::Value = serde_json::from_str(line).unwrap();
        let slot: Slot = serde_json::from_value(json.clone()).unwrap();
        let owner = seen.get(&tag(&json)).copied().unwrap_or(ScreenId::Legacy);
        assert_eq!(slot.screen(), owner, "{line}");
    }
}

#[test]
fn ui_state_saved_by_main_restores_its_pending_commands() {
    let _native_case = native_runtime_case();
    let root = Root::new("ui-state-main");
    std::fs::write(root.memory(), UI_STATE).unwrap();
    let memory = Memory::load(&root.memory());
    assert_eq!(
        String::from_utf8(serde_json::to_vec_pretty(&memory).unwrap()).unwrap(),
        UI_STATE,
        "the file is read back without loss"
    );
    let expected = vec![
        Slot::Help(HelpSlot::HelpGenerate),
        Slot::SettingsSave,
        Slot::AddonsSave {
            update: None,
            registry: 4,
            download: None,
        },
        Slot::ApprovalDecision("approval-1".into()),
    ];
    let saved: Vec<Slot> = memory
        .pending
        .commands
        .iter()
        .map(|c| c.slot.clone())
        .collect();
    assert_eq!(saved, expected);

    let engine = start(&root);
    let mut shell = shell(&engine, &root, Script::default());
    let restored = shell.client().unwrap().unconfirmed();
    assert_eq!(
        restored.iter().map(|u| u.slot.clone()).collect::<Vec<_>>(),
        expected
    );
    assert_eq!(restored[3].dispatch, CommandDispatch::HumanDecision);
    assert_eq!(
        restored
            .iter()
            .map(|u| u.frame().to_vec())
            .collect::<Vec<_>>(),
        memory
            .pending
            .commands
            .iter()
            .map(|c| c.frame.clone())
            .collect::<Vec<_>>()
    );
    assert!(shell.help.generating && shell.help.generation_unknown);
    assert!(shell.settings.saving && shell.settings.unconfirmed);
    assert!(shell.addons.busy);
    shell.on_exit();
    drop(shell);
    assert_eq!(Memory::load(&root.memory()).pending, memory.pending);
}
