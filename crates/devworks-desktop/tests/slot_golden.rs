//! Persisted state stays readable while screens move to their own slot types. The fixtures were
//! written by `main` before the first screen migrated: every `Slot` variant's JSON and Debug
//! text, and a `ui-state.json` holding unconfirmed commands.
mod common;
use common::*;
use devworks_desktop::addons_ops::AddonsSlot;
use devworks_desktop::artifacts_screen::{ArtifactsSlot, BrowserSlot, ResourcesSlot};
use devworks_desktop::backend::{CommandDispatch, Slot};
use devworks_desktop::chat::ChatSlot;
use devworks_desktop::conventions_ops::ConventionsSlot;
use devworks_desktop::databases_ops::DatabasesSlot;
use devworks_desktop::decision_ops::DecisionsSlot;
use devworks_desktop::graph_ops::GraphSlot;
use devworks_desktop::help_ops::HelpSlot;
use devworks_desktop::indexing_ops::IndexingSlot;
use devworks_desktop::instructions_ops::InstructionsSlot;
use devworks_desktop::jobs_ops::JobsSlot;
use devworks_desktop::knowledge_ops::KnowledgeSlot;
use devworks_desktop::memory::Memory;
use devworks_desktop::projects_ops::ProjectsSlot;
use devworks_desktop::purge_ops::PurgeSlot;
use devworks_desktop::screen::ScreenId;
use devworks_desktop::settings_ops::SettingsSlot;
use devworks_desktop::usage_ops::UsageSlot;
use devworks_desktop::watch_ops::WatchSlot;
use devworks_desktop::work_ops::WorkSlot;
use serde::de::DeserializeOwned;

const SLOTS: &str = include_str!("fixtures/slots-main.jsonl");
const SLOTS_DEBUG: &str = include_str!("fixtures/slots-main-debug.txt");
const UI_STATE: &str = include_str!("fixtures/ui-state-main.json");

/// One value of every variant, in the order of the fixtures.
fn samples() -> Vec<Slot> {
    vec![
        Slot::Purge(PurgeSlot::PurgePrepare),
        Slot::Purge(PurgeSlot::PurgePage),
        Slot::Purge(PurgeSlot::PurgeConfirm),
        Slot::Purge(PurgeSlot::PurgeStatus),
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
        Slot::Decisions(DecisionsSlot::ApprovalsList),
        Slot::Decisions(DecisionsSlot::ApprovalInspect("s".into())),
        Slot::Decisions(DecisionsSlot::ApprovalRefresh("s".into())),
        Slot::Decisions(DecisionsSlot::ApprovalContent("s".into())),
        Slot::Decisions(DecisionsSlot::ApprovalDecision("s".into())),
        Slot::Decisions(DecisionsSlot::ReviewQueue),
        Slot::Decisions(DecisionsSlot::NotificationsList),
        Slot::Decisions(DecisionsSlot::CompletionList),
        Slot::Decisions(DecisionsSlot::NotificationRead("s".into())),
        Slot::Decisions(DecisionsSlot::NotificationChange("s".into())),
        Slot::Decisions(DecisionsSlot::NotificationSettings),
        Slot::Decisions(DecisionsSlot::NotificationSettingsSave),
        Slot::Decisions(DecisionsSlot::OsNotificationPermission),
        Slot::Settings(SettingsSlot::SettingsRead),
        Slot::Settings(SettingsSlot::ContainerCapabilities),
        Slot::Settings(SettingsSlot::SettingsValidate),
        Slot::Settings(SettingsSlot::SettingsHealth),
        Slot::Settings(SettingsSlot::SettingsSave),
        Slot::Settings(SettingsSlot::SettingsSecret("s".into())),
        Slot::Settings(SettingsSlot::SettingsExplain("s".into())),
        Slot::Chat(ChatSlot::SessionsList),
        Slot::Chat(ChatSlot::SessionStart),
        Slot::Chat(ChatSlot::Transcript("s".into())),
        Slot::Chat(ChatSlot::TextPreview("s".into(), 7)),
        Slot::Chat(ChatSlot::Append("s".into())),
        Slot::Chat(ChatSlot::Attach("s".into(), 7)),
        Slot::Chat(ChatSlot::Preview("s".into(), 7)),
        Slot::Chat(ChatSlot::DeleteAttachment("s".into())),
        Slot::Artifacts(ArtifactsSlot::ArtifactsList),
        Slot::Resources(ResourcesSlot::ResourcesList),
        Slot::Browser(BrowserSlot::BrowserSessions),
        Slot::Browser(BrowserSlot::ProjectBrowserSessions("s".into())),
        Slot::Browser(BrowserSlot::BrowserClose("s".into())),
        Slot::AddOns(AddonsSlot::AddonsList),
        Slot::AddOns(AddonsSlot::AddonsLogs("s".into())),
        Slot::AddOns(AddonsSlot::AddonsCommand),
        Slot::AddOns(AddonsSlot::AddonsSave {
            update: Some("o".into()),
            registry: -2,
            download: Some("/tmp/x".into()),
        }),
        Slot::AddOns(AddonsSlot::AddonsSave {
            update: None,
            registry: 4,
            download: None,
        }),
        Slot::AddOns(AddonsSlot::AddonsDiscovery),
        Slot::Chat(ChatSlot::RouterCatalog(7)),
        Slot::Chat(ChatSlot::RouteSession("s".into())),
        Slot::Chat(ChatSlot::RouteStatus("s".into())),
        Slot::Chat(ChatSlot::RoutePin("s".into())),
        Slot::Chat(ChatSlot::RouteFirst("s".into())),
        Slot::Settings(SettingsSlot::WebFetch),
        Slot::Settings(SettingsSlot::WebSearch),
        Slot::Settings(SettingsSlot::WebActivity),
        Slot::Settings(SettingsSlot::McpServers("s".into(), Some("o".into()))),
        Slot::Settings(SettingsSlot::McpOperation("s".into())),
        Slot::Settings(SettingsSlot::McpCapabilities(
            "s".into(),
            "s".into(),
            Some("o".into()),
        )),
        Slot::Settings(SettingsSlot::McpContent("s".into(), "s".into())),
        Slot::Settings(SettingsSlot::McpCommand),
        Slot::Settings(SettingsSlot::McpRelease),
        Slot::Projects(ProjectsSlot::ProjectsList),
        Slot::Projects(ProjectsSlot::DeletedProjects),
        Slot::Projects(ProjectsSlot::RepositoriesList),
        Slot::Projects(ProjectsSlot::ProjectRepositories("s".into())),
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
        Slot::Knowledge(KnowledgeSlot::KnowledgeList(7)),
        Slot::Knowledge(KnowledgeSlot::KnowledgeRead("s".into())),
        Slot::Knowledge(KnowledgeSlot::KnowledgeDiagnostics),
        Slot::Knowledge(KnowledgeSlot::KnowledgePreview("s".into())),
        Slot::Knowledge(KnowledgeSlot::KnowledgeImportSave),
        Slot::Knowledge(KnowledgeSlot::KnowledgeImportPreview),
        Slot::Knowledge(KnowledgeSlot::KnowledgeImport),
        Slot::Knowledge(KnowledgeSlot::KnowledgeWrite),
        Slot::Jobs(JobsSlot::JobsList),
        Slot::Jobs(JobsSlot::JobsMore(7)),
        Slot::Jobs(JobsSlot::JobsRefresh(7)),
        Slot::Jobs(JobsSlot::ProjectJobs("s".into())),
        Slot::Jobs(JobsSlot::JobInspect("s".into())),
        Slot::Jobs(JobsSlot::JobHistory("s".into())),
        Slot::Jobs(JobsSlot::JobActiveRun("s".into())),
        Slot::Jobs(JobsSlot::JobPrompts(7, Some("o".into()))),
        Slot::Jobs(JobsSlot::JobCreate),
        Slot::Jobs(JobsSlot::JobCommand("s".into())),
        Slot::Work(WorkSlot::WorkFeatures("s".into())),
        Slot::Work(WorkSlot::WorkWorkflow("s".into(), 7)),
        Slot::Work(WorkSlot::WorkWorkflowCommand("s".into())),
        Slot::Work(WorkSlot::AllocationQueue(
            "s".into(),
            Some("o".into()),
            Some(3),
        )),
        Slot::Work(WorkSlot::AllocationClaim("s".into(), Some("o".into()))),
        Slot::Work(WorkSlot::AllocationInspect("s".into())),
        Slot::Work(WorkSlot::AllocationRelease("s".into())),
        Slot::Work(WorkSlot::WorkCards("s".into())),
        Slot::Work(WorkSlot::WorkActivity("s".into(), Some("o".into()), 7)),
        Slot::Work(WorkSlot::WorkMessagesHistory("s".into())),
        Slot::Work(WorkSlot::WorkMessagesInbox("s".into())),
        Slot::Work(WorkSlot::WorkMessagesEvents("s".into())),
        Slot::Work(WorkSlot::WorkMessagePost("s".into())),
        Slot::Work(WorkSlot::WorkMessageAck("s".into(), "s".into())),
        Slot::Work(WorkSlot::WorkDetail("s".into())),
        Slot::Work(WorkSlot::WorkGit("s".into())),
        Slot::Work(WorkSlot::WorkGitInspect("s".into(), "s".into())),
        Slot::Projects(ProjectsSlot::GitInspect("s".into())),
        Slot::Projects(ProjectsSlot::GitDiff("s".into())),
        Slot::Projects(ProjectsSlot::GitDiffContent("s".into())),
        Slot::Work(WorkSlot::WorkCommand("s".into())),
        Slot::Work(WorkSlot::WorkCreate("s".into())),
        Slot::Work(WorkSlot::WorkOrder("s".into())),
        Slot::Work(WorkSlot::WorkPlanText("s".into())),
        Slot::Work(WorkSlot::WorkPlanDrop("s".into())),
        Slot::Work(WorkSlot::WorkPlan("s".into())),
        Slot::Work(WorkSlot::WorkInspect("s".into())),
        Slot::Work(WorkSlot::WorkRun("s".into())),
        Slot::Work(WorkSlot::ArchiveCount("s".into(), true)),
        Slot::Work(WorkSlot::WorkOpen("s".into(), true)),
        Slot::Settings(SettingsSlot::Connections(7)),
        Slot::Settings(SettingsSlot::ConnectionCatalog(7)),
        Slot::Settings(SettingsSlot::EmbeddingSpaces(7)),
        Slot::Settings(SettingsSlot::Configure(7)),
        Slot::Instructions(InstructionsSlot::InstructionSkills(7)),
        Slot::Instructions(InstructionsSlot::InstructionProjects(7)),
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
        (ScreenId::Watchers, variant_names::<WatchSlot>()),
        (ScreenId::Chat, variant_names::<ChatSlot>()),
        (ScreenId::Jobs, variant_names::<JobsSlot>()),
        (ScreenId::Decisions, variant_names::<DecisionsSlot>()),
        (ScreenId::Work, variant_names::<WorkSlot>()),
        (ScreenId::Settings, variant_names::<SettingsSlot>()),
        (ScreenId::AddOns, variant_names::<AddonsSlot>()),
        (ScreenId::Knowledge, variant_names::<KnowledgeSlot>()),
        (ScreenId::Purge, variant_names::<PurgeSlot>()),
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
        ScreenId::Watchers => format!("{:?}", WatchSlot::try_from(slot.clone()).unwrap()),
        ScreenId::Chat => format!("{:?}", ChatSlot::try_from(slot.clone()).unwrap()),
        ScreenId::Jobs => format!("{:?}", JobsSlot::try_from(slot.clone()).unwrap()),
        ScreenId::Decisions => {
            format!("{:?}", DecisionsSlot::try_from(slot.clone()).unwrap())
        }
        ScreenId::Work => format!("{:?}", WorkSlot::try_from(slot.clone()).unwrap()),
        ScreenId::Settings => format!("{:?}", SettingsSlot::try_from(slot.clone()).unwrap()),
        ScreenId::AddOns => format!("{:?}", AddonsSlot::try_from(slot.clone()).unwrap()),
        ScreenId::Knowledge => format!("{:?}", KnowledgeSlot::try_from(slot.clone()).unwrap()),
        ScreenId::Purge => format!("{:?}", PurgeSlot::try_from(slot.clone()).unwrap()),
        ScreenId::Placeholder => unreachable!("no slot belongs to the placeholder screens"),
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
        let owner = seen
            .get(&tag(&json))
            .copied()
            .expect("every name has a screen");
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
        Slot::Settings(SettingsSlot::SettingsSave),
        Slot::AddOns(AddonsSlot::AddonsSave {
            update: None,
            registry: 4,
            download: None,
        }),
        Slot::Decisions(DecisionsSlot::ApprovalDecision("approval-1".into())),
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
