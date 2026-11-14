//! Kinds of record a project purge identifies. The port names records by this enum, so no
//! storage table name crosses into core or the desktop. Each kind's wire name is the
//! persisted record-class vocabulary of reviewed purge manifests and their digests, so it
//! must not change; the variants are declared in wire-name order, which keeps ordered
//! collections (and so every digest over them) identical to the former string keys.

macro_rules! record_kinds {
    ($($kind:ident,)*) => {
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
        )]
        #[serde(rename_all = "snake_case")]
        pub enum ProjectRecordKind {
            $($kind,)*
        }
        impl ProjectRecordKind {
            /// Every kind, in wire-name order.
            pub const ALL: &'static [ProjectRecordKind] = &[$(ProjectRecordKind::$kind,)*];
        }
    };
}

record_kinds! {
    Actors,
    AgentAssignmentContext,
    AgentMessageDeliveries,
    AgentMessageEvents,
    AgentMessageSequenceHeads,
    AgentMessages,
    AllocationClaims,
    AllocationReplays,
    AppBoots,
    ApprovalResumes,
    ArtifactBlobPins,
    ArtifactLinks,
    ArtifactShares,
    ArtifactTags,
    ArtifactVersions,
    Artifacts,
    Assignments,
    AuthorityGrants,
    Blobs,
    Blockers,
    BrowserObservations,
    BrowserSessions,
    CommandReceipts,
    ConfigurationProposals,
    ConfigurationRevisions,
    ConsumerProgress,
    ConventionChunkTerms,
    ConventionChunks,
    ConventionEmbeddingInputRefs,
    ConventionEmbeddingInputs,
    ConventionEmbeddingSpaces,
    ConventionHeads,
    ConventionImportArtifacts,
    ConventionImportMetadata,
    ConventionImports,
    ConventionIndexHeads,
    ConventionIndexScopeRevisions,
    ConventionIndexes,
    ConventionResourceHeads,
    ConventionResourceVersions,
    ConventionScopeRevisions,
    ConventionSectionTerms,
    ConventionSections,
    ConventionTermBatches,
    ConventionTerms,
    ConventionVersions,
    Conversations,
    DecisionApprovals,
    DiagnosticExports,
    DiagnosticRecords,
    DiagnosticRetention,
    EffectiveConfiguration,
    EmbeddingSpaces,
    EvalRuns,
    Events,
    Executions,
    ExternalCursors,
    FeaturePlans,
    FeatureReadiness,
    GitOwnedWorktrees,
    GithubIntakeClock,
    GraphEdges,
    GraphNodes,
    GraphReadPinViews,
    GraphReadPins,
    GraphSourceBindings,
    GraphSourceCaptures,
    GraphTrust,
    GraphTrustChanges,
    HelpContents,
    HelpGenerations,
    HelpHead,
    HelpTopicScreens,
    HelpTopics,
    HistoryLinks,
    HumanDecisions,
    IndexBatches,
    IndexGenerations,
    IndexOverlays,
    IndexPins,
    IndexRuns,
    IndexSourceConfigs,
    IndexViews,
    InstructionRevisions,
    Instructions,
    IntegrationLinks,
    IntegrationRecords,
    JobActions,
    JobCheckpoints,
    JobRunLinks,
    JobRunUsage,
    JobRuns,
    KnowledgeAddresses,
    KnowledgeChallenges,
    KnowledgeClaimBasis,
    KnowledgeClaimEvidence,
    KnowledgeClaimFts,
    KnowledgeClaimFtsConfig,
    KnowledgeClaimFtsContent,
    KnowledgeClaimFtsData,
    KnowledgeClaimFtsDocsize,
    KnowledgeClaimFtsIdx,
    KnowledgeClaimTags,
    KnowledgeClaims,
    KnowledgeDeliveries,
    KnowledgeEntities,
    KnowledgeEntityFts,
    KnowledgeEntityFtsConfig,
    KnowledgeEntityFtsContent,
    KnowledgeEntityFtsData,
    KnowledgeEntityFtsDocsize,
    KnowledgeEntityFtsIdx,
    KnowledgeEvidence,
    KnowledgeHistory,
    KnowledgeImportMap,
    KnowledgeLessonEvents,
    KnowledgeRelations,
    KnowledgeScopes,
    KnowledgeSourceDeletions,
    KnowledgeSourceHeads,
    KnowledgeWriteTargets,
    KnowledgeWrites,
    NotificationAudience,
    Notifications,
    OperationAttempts,
    Operations,
    OsNotificationDeliveries,
    OwnedContainers,
    OwnedProcesses,
    PackageHandlerCommits,
    PackageHandlerState,
    PackageIndexes,
    PackageObservations,
    PackageProposals,
    PackageRegistry,
    PackageStateGenerations,
    PackageVersions,
    Packages,
    PlanProposals,
    PrincipalAuthority,
    ProjectDetails,
    ProjectManagedFiles,
    ProjectPurgeEffects,
    ProjectPurgeEntries,
    ProjectPurgeManifests,
    ProjectPurgeRows,
    ProjectPurgedSubjects,
    ProjectRepositories,
    Projects,
    ProviderConnectionIdentities,
    PublishedViews,
    QueueEntries,
    ReceiptReplies,
    Repositories,
    RepositoryDetails,
    RepositoryIndexViews,
    RepositorySources,
    RepositoryTexts,
    RepositoryWorktrees,
    ResourceLeases,
    Resources,
    ResumeGrants,
    RootControls,
    RouterConnections,
    RouterCooldowns,
    RouterDecisionModels,
    RouterEmbeddingCandidates,
    RouterEmbeddingInventory,
    RouterEmbeddingPriceSubjects,
    RouterEmbeddingQualifications,
    RouterEmbeddingSelection,
    RouterModels,
    RouterPins,
    RouterRequests,
    RouterTierCursors,
    RunInstructions,
    Schedules,
    SchemaCertificates,
    SchemaInventory,
    SchemaMigrations,
    SchemaObjects,
    SchemaPublications,
    SchemaReadPins,
    SchemaRelations,
    SchemaSources,
    SchemaViews,
    SessionCheckpoints,
    SessionContextEpochs,
    SessionContextSources,
    SessionProviderContextHeads,
    SessionToolCalls,
    SessionTurns,
    SessionUsage,
    Sessions,
    SettingsSecretHandles,
    SourceUnits,
    StopCoverage,
    StoreMeta,
    ToolInvocations,
    TranscriptBatches,
    TranscriptChunks,
    UsageAttemptBalances,
    UsageAttempts,
    UsageBootstrap,
    UsageChanges,
    UsageFeatureBudgets,
    UsageObservations,
    UsageOrigins,
    UsagePriceOverrides,
    UsagePriceRevision,
    UsagePriceSnapshots,
    UsagePriceStatus,
    WatcherEvents,
    Watchers,
    WorkDependencies,
    WorkDetails,
    WorkItems,
    WorkQueues,
    WorkRepositories,
    WorkflowStatuses,
}

static WIRE_NAMES: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| {
    ProjectRecordKind::ALL
        .iter()
        .map(|kind| match serde_json::to_value(kind) {
            Ok(serde_json::Value::String(name)) => name,
            _ => unreachable!("a unit variant serializes as a string"),
        })
        .collect()
});

impl ProjectRecordKind {
    /// The persisted wire name.
    pub fn as_str(self) -> &'static str {
        &WIRE_NAMES[self as usize]
    }
    /// The kind with this wire name.
    pub fn from_wire(name: &str) -> Option<Self> {
        WIRE_NAMES
            .binary_search_by(|probe| probe.as_str().cmp(name))
            .ok()
            .map(|index| Self::ALL[index])
    }
}

impl std::fmt::Display for ProjectRecordKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq<str> for ProjectRecordKind {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for ProjectRecordKind {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[cfg(test)]
mod tests {
    use super::ProjectRecordKind;

    #[test]
    fn declaration_order_is_wire_name_order_and_names_round_trip() {
        for (index, kind) in ProjectRecordKind::ALL.iter().enumerate() {
            assert_eq!(*kind as usize, index);
            assert_eq!(ProjectRecordKind::from_wire(kind.as_str()), Some(*kind));
            let json = serde_json::to_string(kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
            assert_eq!(
                serde_json::from_str::<ProjectRecordKind>(&json).unwrap(),
                *kind
            );
        }
        for pair in ProjectRecordKind::ALL.windows(2) {
            assert!(pair[0].as_str() < pair[1].as_str());
            assert!(pair[0] < pair[1]);
        }
        assert_eq!(ProjectRecordKind::from_wire("unknown_kind"), None);
    }
}
