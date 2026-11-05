# Domain operation field reference

2 November 2026 · Candidate schemas, not production service acceptance. This reference is generated from the same explicit source as the native DTOs and coverage register. Read [the normative contracts](domain-operation-contracts.html) for scope, admission, results, faults and recovery. Optional fields use `?T`; all ordinary records/actions reject unknown fields. `Items`, `NonEmpty`, `Rows`, `Page`, text, counters, money and descriptor-bound JSON are defined there.

All 354 qualified operations have typed input/result candidates. Existing input selectors retain their earlier schemas. The host must apply selected operation/mode, current authority and semantic rules; schema validity alone cannot authorize or complete work.

## Operation families

### Projects

**Owner:** WorkService. Project boundaries and repository membership are revisioned. Archive fences work; delete only app records and requires concrete confirmation where destructive.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `projects.list` | **Query**<br>`scope`: `DataScope`<br>`page`: `PageRequest`<br>`lifecycle`: `?LifeState` | `Page<ProjectRecord>` |
| `projects.get` | **Query**<br>`projectId`: `ProjectId` | `ProjectRecord` |
| `projects.create` | **Command**<br>`name`: `Label`<br>`description`: `Content`<br>`repositoryIds`: `Items<Id>`<br>`initialPlan`: `?AtomicWorkPlan` | `ProjectRecord` |
| `projects.edit` | **Command**<br>`projectId`: `ProjectId`<br>`expectedRevision`: `Revision`<br>`changes`: `ProjectPatch` | `ProjectRecord` |
| `projects.membership` | **Command**<br>`projectId`: `ProjectId`<br>`expectedRevision`: `Revision`<br>`repositoryIds`: `Items<Id>` | `ProjectRecord` |
| `projects.archive` | **Command**<br>`projectId`: `ProjectId`<br>`expectedRevision`: `Revision`<br>`proposalId`: `?Id` | `ProjectRecord` |
| `projects.restore` | **Command**<br>`projectId`: `ProjectId`<br>`expectedRevision`: `Revision`<br>`proposalId`: `?Id` | `ProjectRecord` |
| `projects.delete` | **Command**<br>`projectId`: `ProjectId`<br>`expectedRevision`: `Revision`<br>`proposalId`: `?Id` | `ProjectRecord` |

### Features

**Owner:** WorkService. Features own their plan. One active approved plan plus at most one pending revision; no independent plan work item or user-facing version management. Draft edits/approval requests/rejection preserve the active plan and eligible execution. Exact human approval atomically replaces it; current authority/controls and affected task checks still apply.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `features.list` | **Query**<br>`projectId`: `ProjectId`<br>`page`: `PageRequest`<br>`statusIds`: `Items<Id>`<br>`repositoryIds`: `Items<Id>`<br>`text`: `?Text`<br>`detailLevel`: `DetailLevel`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `Page<WorkRecord>` |
| `features.get` | **Query**<br>`workId`: `Id`<br>`detailLevel`: `DetailLevel`<br>`detailPage`: `?PageRequest`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `WorkRecord` |
| `features.create` | **Command**<br>`projectId`: `ProjectId`<br>`creation`: `FeatureCreation`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `Page<WorkRecord>` |
| `features.edit_plan` | **Command**<br>`featureId`: `RootId`<br>`expectedRevision`: `Revision`<br>`plan`: `PlanDraft`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `WorkRecord` |
| `features.prioritize` | **Command**<br>`projectId`: `ProjectId`<br>`expectedQueueRevision`: `Revision`<br>`orderedWorkIds`: `?NonEmpty<Id>`<br>`placement`: `?QueuePlacement`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `MutationSummary` |
| `features.dependencies` | **Command**<br>`featureId`: `RootId`<br>`expectedRevision`: `Revision`<br>`replacement`: `DependencyReplacement`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `WorkRecord` |
| `features.request_plan_approval` | **Command**<br>`featureId`: `RootId`<br>`expectedRevision`: `Revision`<br>`planRevision`: `Revision`<br>`planDigest`: `Digest` | `ProposalRecord` |
| `features.edit` | **Command**<br>`workId`: `Id`<br>`expectedRevision`: `Revision`<br>`changes`: `FeaturePatch`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `WorkRecord` |

### Issues

**Owner:** WorkService. Clear standalone issues may implement immediately under explicit Start. Remote publish/link preserves exact issue/repository identity and conflict observations.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `issues.list` | **Query**<br>`projectId`: `ProjectId`<br>`page`: `PageRequest`<br>`statusIds`: `Items<Id>`<br>`repositoryIds`: `Items<Id>`<br>`text`: `?Text`<br>`detailLevel`: `DetailLevel`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `Page<WorkRecord>` |
| `issues.get` | **Query**<br>`workId`: `Id`<br>`detailLevel`: `DetailLevel`<br>`detailPage`: `?PageRequest`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `WorkRecord` |
| `issues.create` | **Command**<br>`projectId`: `ProjectId`<br>`title`: `Label`<br>`description`: `Content`<br>`repositoryIds`: `Items<Id>`<br>`statusId`: `Id`<br>`priority`: `i32`<br>`featureId`: `?RootId`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `WorkRecord` |
| `issues.edit` | **Command**<br>`workId`: `Id`<br>`expectedRevision`: `Revision`<br>`changes`: `IssuePatch`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `WorkRecord` |
| `issues.prioritize` | **Command**<br>`projectId`: `ProjectId`<br>`expectedQueueRevision`: `Revision`<br>`orderedWorkIds`: `?NonEmpty<Id>`<br>`placement`: `?QueuePlacement`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `MutationSummary` |
| `issues.publish` | **Command**<br>`issueId`: `RootId`<br>`expectedRevision`: `Revision`<br>`repositoryId`: `Id`<br>`expectedRemoteRevision`: `?Text` | `GitLink` |
| `issues.link` | **Command**<br>`issueId`: `RootId`<br>`expectedRevision`: `Revision`<br>`repositoryId`: `Id`<br>`remoteIssueNumber`: `PositiveCounter` | `GitLink` |

### Tasks

**Owner:** WorkService. ProjectTask belongs to one project and has an optional feature relationship. Standalone tasks are shown as issues and share the same stable work identity across task/issue entry points. Task/Issue wire tags cannot establish feature membership or execution authority. Feature-linked work stays in feature allocation; standalone work uses issue allocation/execution/usage/delivery. Assignment parentage is separate; no manual child-assignment Pause/Continue. Humans and agents share task priority and project-wide blocking-link operations. Cross-feature links do not change membership. Eligibility precedes descending priority and ascending queue rank; graph-cycle and current controls apply on every claim/dispatch.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `tasks.list` | **Query**<br>`projectId`: `ProjectId`<br>`page`: `PageRequest`<br>`statusIds`: `Items<Id>`<br>`repositoryIds`: `Items<Id>`<br>`text`: `?Text`<br>`detailLevel`: `DetailLevel`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `Page<WorkRecord>` |
| `tasks.get` | **Query**<br>`workId`: `Id`<br>`detailLevel`: `DetailLevel`<br>`detailPage`: `?PageRequest`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `WorkRecord` |
| `tasks.create` | **Command**<br>`projectId`: `ProjectId`<br>`title`: `Label`<br>`description`: `Content`<br>`repositoryIds`: `Items<Id>`<br>`parentId`: `?RootId`<br>`statusId`: `Id`<br>`priority`: `i32`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `WorkRecord` |
| `tasks.edit` | **Command**<br>`workId`: `Id`<br>`expectedRevision`: `Revision`<br>`changes`: `TaskPatch`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `WorkRecord` |
| `tasks.prioritize` | **Command**<br>`projectId`: `ProjectId`<br>`featureId`: `?RootId`<br>`expectedQueueRevision`: `Revision`<br>`priority`: `i32`<br>`orderedTaskIds`: `?NonEmpty<Id>`<br>`placement`: `?QueuePlacement`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `MutationSummary` |
| `tasks.move` | **Command**<br>`taskId`: `Id`<br>`expectedRevision`: `Revision`<br>`statusId`: `Id`<br>`expectedWorkflowRevision`: `Revision`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `WorkRecord` |
| `tasks.assign` | **Command**<br>`taskId`: `Id`<br>`expectedRevision`: `Revision`<br>`assignmentId`: `?AssignmentId`<br>`clearAssignment`: `bool`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `WorkRecord` |
| `tasks.dependency_links` | **Command**<br>`taskId`: `Id`<br>`expectedRevision`: `Revision`<br>`replacement`: `DependencyReplacement`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `WorkRecord` |
| `tasks.acceptance_evidence` | **Command**<br>`taskId`: `Id`<br>`expectedRevision`: `Revision`<br>`criteria`: `NonEmpty<AcceptanceCriterion>`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `WorkRecord` |
| `tasks.sync_plan` | **Command**<br>`featureId`: `RootId`<br>`expectedPlanRevision`: `Revision`<br>`expectedTaskSetRevision`: `Revision`<br>`tasks`: `PlanItems<TaskPlanEntry>`<br>`projectPlan`: `?AtomicWorkPlan`<br>`stagedProjectPlan`: `?ArtifactRef`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `Page<WorkRecord>` |
| `tasks.set_feature` | **Command**<br>`taskId`: `Id`<br>`expectedRevision`: `Revision`<br>`featureId`: `?RootId`<br>`clearFeature`: `bool`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `WorkRecord` |

### Workflows

**Owner:** ConfigurationService. Status identities are stable; occupied removal and reassignment is one transaction. Allocation reads the resulting current workflow.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `workflows.list` | **Query**<br>`projectId`: `ProjectId`<br>`page`: `PageRequest` | `Page<StatusRecord>` |
| `workflows.create_status` | **Command**<br>`projectId`: `ProjectId`<br>`expectedWorkflowRevision`: `Revision`<br>`label`: `Label`<br>`category`: `Label`<br>`afterStatusId`: `?Id` | `StatusRecord` |
| `workflows.rename_status` | **Command**<br>`projectId`: `ProjectId`<br>`statusId`: `Id`<br>`expectedRevision`: `Revision`<br>`label`: `Label` | `StatusRecord` |
| `workflows.order` | **Command**<br>`projectId`: `ProjectId`<br>`expectedWorkflowRevision`: `Revision`<br>`orderedStatusIds`: `NonEmpty<Id>` | `Page<StatusRecord>` |
| `workflows.reassign_and_remove` | **Command**<br>`projectId`: `ProjectId`<br>`expectedWorkflowRevision`: `Revision`<br>`removeStatusId`: `Id`<br>`destinationStatusId`: `Id`<br>`expectedAffectedWorkDigest`: `Digest` | `MutationSummary` |

### Next Work

**Owner:** WorkService. Atomically claim only eligible current work with host ownership/fence. Automatic feature claim requires approved implementation; claim is not Continue.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `next_work.claim_project_queue` | **Command**<br>`projectId`: `ProjectId`<br>`expectedQueueRevision`: `Revision`<br>`leaseMs`: `PositiveCounter` | `ClaimRecord` |
| `next_work.claim_feature_task` | **Command**<br>`rootId`: `RootId`<br>`expectedTaskSetRevision`: `Revision`<br>`leaseMs`: `PositiveCounter` | `ClaimRecord` |
| `next_work.inspect_claim` | **Query**<br>`claimId`: `Id` | `ClaimRecord` |
| `next_work.release_claim` | **Command**<br>`claimId`: `Id`<br>`expectedFence`: `Counter` | `ClaimRecord` |

### Runs

**Owner:** ControlService. Only root controls change desired execution. No agent/schedule may resume a human pause without current explicit authorization.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `runs.start` | **Command**<br>[Existing input schema](contracts/command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'runs.start'} | `RootSnapshot` |
| `runs.pause` | **Command**<br>[Existing input schema](contracts/command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'runs.pause'} | `RootSnapshot` |
| `runs.continue` | **Command**<br>[Existing input schema](contracts/command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'runs.continue'} | `RootSnapshot` |
| `runs.redirect` | **Command**<br>`rootId`: `RootId`<br>`expectedControlRevision`: `Revision`<br>`message`: `Content`<br>`attachments`: `Items<ArtifactRef>` | `MutationSummary` |
| `runs.inspect` | **Query**<br>[Existing input schema](contracts/query-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'runs.inspect'} | `RootSnapshot` |
| `runs.stop` | **Command**<br>`rootId`: `RootId`<br>`expectedControlRevision`: `Revision` | `OperationRef` |
| `runs.eligibility` | **Query**<br>[Existing input schema](contracts/query-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'runs.eligibility'} | `EligibilitySnapshot` |

### Agents

**Owner:** AgentSupervisor. Assignment ownership and parent scope are host-derived. Root cap is ten with no global cap. Inbox observation and acknowledgment confer no approval.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `agents.inspect` | **Query**<br>`assignmentId`: `AssignmentId` | `AgentRecord` |
| `agents.delegate` | **Command**<br>`rootId`: `RootId`<br>`expectedControlRevision`: `Revision`<br>`taskIds`: `Items<Id>`<br>`roleId`: `Id`<br>`instruction`: `Content`<br>`sourceCheckpointId`: `?Id` | `AgentRecord` |
| `agents.messages` | **Command**<br>`recipientAssignmentId`: `AssignmentId`<br>`messageClass`: `MessageClass`<br>`content`: `Content`<br>`attachments`: `Items<ArtifactRef>` | `AgentMessage` |
| `agents.inbox` | **Query**<br>`assignmentId`: `AssignmentId`<br>`page`: `PageRequest`<br>`unacknowledgedOnly`: `bool`<br>`direction`: `MessageDirection` | `Page<AgentMessage>` |
| `agents.acknowledge` | **Command**<br>`assignmentId`: `AssignmentId`<br>`messageId`: `Id`<br>`expectedRevision`: `Revision` | `AgentMessage` |
| `agents.checkpoint` | **Command**<br>`assignmentId`: `AssignmentId`<br>`expectedRevision`: `Revision`<br>`transcriptSequence`: `Counter`<br>`content`: `ArtifactRef` | `CheckpointRecord` |

### Sessions

**Owner:** ConversationService. Session identity, context epoch and native continuation belong to service. Query or conversation creation never starts/resumes work.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `sessions.list` | **Query**<br>`scope`: `DataScope`<br>`page`: `PageRequest`<br>`closed`: `?bool`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `Page<SessionRecord>` |
| `sessions.get` | **Query**<br>`sessionId`: `Id`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `SessionRecord` |
| `sessions.transcript` | **Query**<br>[Existing input schema](contracts/session-query-request.schema.json)<br>None | `TranscriptPage` |
| `sessions.briefing` | **Query**<br>`sessionId`: `Id`<br>`expectedContextEpoch`: `Counter`<br>`item`: `?RootId`<br>`paths`: `?PathSet`<br>`budgetTokens`: `?TokenBudget`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `Page<ContextSource>` |
| `sessions.append_message` | **Command**<br>[Existing input schema](contracts/session-command-request.schema.json)<br>None<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/session-command-request.schema.json)) | `SessionRecord` |
| `sessions.context_sources` | **Query**<br>`sessionId`: `Id`<br>`expectedContextEpoch`: `Counter`<br>`page`: `PageRequest` | `Page<ContextSource>` |
| `sessions.start` | **Command**<br>`scope`: `DataScope`<br>`rootId`: `?RootId`<br>`title`: `Label`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `SessionRecord` |
| `sessions.end` | **Command**<br>`sessionId`: `Id`<br>`expectedRevision`: `Revision`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `SessionRecord` |

### Ui

**Owner:** NavigationContextService. Current screen context is bounded host observation, no hidden authority or inference call.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `ui.current_screen` | **Query**<br>No caller fields | `ScreenContext` |

### History

**Owner:** HistoryService. Append-only attributed lineage; scoped stable pagination. Receipt lookup uses exact durable result schema/artifact, never guesses unacknowledged completion.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `history.events` | **Query**<br>[Existing input schema](contracts/query-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'history.events'}<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/query-request.r2.schema.json)) | `HistoryPage<Event>` |
| `history.work_lineage` | **Query**<br>`rootId`: `RootId`<br>`page`: `PageRequest`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `HistoryPage<LineageRecord>` |
| `history.git_links` | **Query**<br>`rootId`: `RootId`<br>`page`: `PageRequest`<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/domain-request.r3.schema.json)) | `HistoryPage<AttributedGitLink>` |
| `history.evidence_links` | **Query**<br>`rootId`: `RootId`<br>`page`: `PageRequest`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `HistoryPage<EvidenceLink>` |
| `history.operation_status` | **Query**<br>[Existing input schema](contracts/query-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'history.operation_status'} | `OperationRef` |
| `history.tool_usage` | **Query**<br>`rootId`: `?RootId`<br>`projectId`: `?ProjectId`<br>`workspace`: `?bool`<br>`page`: `PageRequest`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `UsagePage` |
| `history.command_receipt` | **Query**<br>[Existing input schema](contracts/query-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'history.command_receipt'} | `ReceiptObservation` |

### Code

**Owner:** GraphService. Existing exact-view read DTOs and source result/citation contracts remain authoritative; visibility before traversal, no main/view fallback and no raw graph query escape.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `code.search` | **Query**<br>[Existing input schema](contracts/graph-read-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'Search'} | `GraphPage` |
| `code.architecture` | **Query**<br>[Existing input schema](contracts/graph-read-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'Overview'} | `GraphPage` |
| `code.dependency` | **Query**<br>[Existing input schema](contracts/graph-read-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'Traverse'} | `GraphPage` |
| `code.impact` | **Query**<br>[Existing input schema](contracts/graph-read-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'NodeImpact'} | `GraphPage` |
| `code.source` | **Query**<br>[Existing input schema](contracts/graph-read-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'Source'} | `GraphPage` |
| `code.traverse` | **Query**<br>[Existing input schema](contracts/graph-read-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'Traverse'} | `GraphPage` |
| `code.graph_scope_status` | **Query**<br>[Existing input schema](contracts/graph-read-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'ScopeStatus'} | `GraphPage` |
| `code.overview` | **Query**<br>[Existing input schema](contracts/graph-read-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'Overview'} | `GraphPage` |
| `code.nodes_by_file` | **Query**<br>[Existing input schema](contracts/graph-read-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'NodesByFile'} | `GraphPage` |
| `code.node_detail` | **Query**<br>[Existing input schema](contracts/graph-read-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'Node'} | `GraphPage` |
| `code.set_trust` | **Command**<br>[Existing input schema](contracts/index-command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'SetTrust'} | `MutationSummary` |
| `code.file_impact` | **Query**<br>[Existing input schema](contracts/graph-read-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'FileImpact'} | `GraphPage` |
| `code.graph_schema` | **Query**<br>[Existing input schema](contracts/graph-read-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'Schema'} | `GraphPage` |
| `code.call_path` | **Query**<br>[Existing input schema](contracts/graph-read-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'Path'} | `GraphPage` |
| `code.data_lineage` | **Query**<br>[Existing input schema](contracts/graph-read-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'DataLineage'} | `GraphPage` |
| `code.consumers` | **Query**<br>[Existing input schema](contracts/graph-read-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'Consumers'} | `GraphPage` |
| `code.publishers` | **Query**<br>[Existing input schema](contracts/graph-read-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'Publishers'} | `GraphPage` |
| `code.snippet` | **Query**<br>[Existing input schema](contracts/graph-read-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'Source'} | `GraphPage` |
| `code.repository_dependencies` | **Query**<br>[Existing input schema](contracts/graph-read-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'RepositoryDependencies'} | `GraphPage` |

### Indexing

**Owner:** IndexCoordinator. Immutable staged views and current source fences; partial/canceled output cannot publish complete. Structural baseline survives optional semantic-tool absence.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `indexing.discover` | **Query**<br>`scope`: `DataScope`<br>`roots`: `Items<Id>`<br>`page`: `PageRequest` | `Page<RepositoryRecord>` |
| `indexing.index` | **Command**<br>`scope`: `DataScope`<br>`repositoryIds`: `NonEmpty<Id>`<br>`expectedSourceRevision`: `Revision`<br>`views`: `ViewSelection` | `IndexRunRecord` |
| `indexing.reindex` | **Command**<br>`scope`: `DataScope`<br>`repositoryIds`: `NonEmpty<Id>`<br>`expectedSourceRevision`: `Revision`<br>`views`: `ViewSelection` | `IndexRunRecord` |
| `indexing.refresh_overlay` | **Command**<br>[Existing input schema](contracts/index-command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'RefreshOverlay'}<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/index-command-request.r2.schema.json)) | `IndexRunRecord` |
| `indexing.retire_view` | **Command**<br>`viewId`: `Id`<br>`expectedRevision`: `Revision` | `MutationSummary` |
| `indexing.cancel` | **Command**<br>[Existing input schema](contracts/index-command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'Cancel'} | `IndexRunRecord` |
| `indexing.status` | **Query**<br>`repositoryId`: `?Id`<br>`overlayId`: `?Id`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `RepositoryIndexStatus` |
| `indexing.runs` | **Query**<br>`scope`: `DataScope`<br>`repositoryId`: `?Id`<br>`states`: `?NonEmpty<IndexRunState>`<br>`kinds`: `?NonEmpty<IndexRunKind>`<br>`page`: `PageRequest`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `Page<IndexRunRecord>` |
| `indexing.configure_sources` | **Command**<br>`scope`: `DataScope`<br>`expectedRevision`: `Revision`<br>`config`: `IndexSourceConfig` | `MutationSummary` |

### Analysis

**Owner:** AnalysisService. All observations bind exact views and declared coverage; inferred links stay inferred. Model-assisted effects are explicit commands with usage.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `analysis.health` | **Query**<br>`scope`: `DataScope`<br>`views`: `ViewSelection`<br>`page`: `PageRequest`<br>`filters`: `Items<Label>` | `AnalysisRecord` |
| `analysis.cross_repo_links` | **Query**<br>`scope`: `DataScope`<br>`views`: `ViewSelection`<br>`page`: `PageRequest`<br>`filters`: `Items<Label>` | `Page<MetricRecord>` |
| `analysis.community_analysis` | **Command**<br>`scope`: `DataScope`<br>`views`: `ViewSelection`<br>`expectedSourceRevision`: `Revision`<br>`nodeIds`: `Items<Id>`<br>`batchLimit`: `Limit`<br>`resumeRunId`: `?Id` | `AnalysisRecord` |
| `analysis.status` | **Query**<br>`scope`: `DataScope`<br>`views`: `ViewSelection`<br>`page`: `PageRequest`<br>`filters`: `Items<Label>` | `AnalysisRecord` |
| `analysis.clusters` | **Query**<br>`scope`: `DataScope`<br>`views`: `ViewSelection`<br>`page`: `PageRequest`<br>`filters`: `Items<Label>` | `Page<ClusterRecord>` |
| `analysis.cluster_detail` | **Query**<br>`scope`: `DataScope`<br>`views`: `ViewSelection`<br>`page`: `PageRequest`<br>`clusterId`: `Id`<br>`filters`: `Items<Label>` | `ClusterRecord` |
| `analysis.link_and_detect` | **Command**<br>`scope`: `DataScope`<br>`views`: `ViewSelection`<br>`expectedSourceRevision`: `Revision`<br>`nodeIds`: `Items<Id>`<br>`batchLimit`: `Limit`<br>`resumeRunId`: `?Id` | `AnalysisRecord` |
| `analysis.process_batch` | **Command**<br>`scope`: `DataScope`<br>`views`: `ViewSelection`<br>`expectedSourceRevision`: `Revision`<br>`nodeIds`: `Items<Id>`<br>`batchLimit`: `Limit`<br>`resumeRunId`: `?Id` | `AnalysisRecord` |
| `analysis.security` | **Query**<br>`scope`: `DataScope`<br>`views`: `ViewSelection`<br>`page`: `PageRequest`<br>`filters`: `Items<Label>` | `AnalysisRecord` |
| `analysis.metrics` | **Query**<br>`scope`: `DataScope`<br>`views`: `ViewSelection`<br>`page`: `PageRequest`<br>`filters`: `Items<Label>` | `Page<MetricRecord>` |
| `analysis.hotspots` | **Query**<br>`scope`: `DataScope`<br>`views`: `ViewSelection`<br>`page`: `PageRequest`<br>`filters`: `Items<Label>` | `Page<MetricRecord>` |
| `analysis.reanalyze` | **Command**<br>`scope`: `DataScope`<br>`views`: `ViewSelection`<br>`expectedSourceRevision`: `Revision`<br>`nodeIds`: `Items<Id>`<br>`batchLimit`: `Limit`<br>`resumeRunId`: `?Id` | `AnalysisRecord` |
| `analysis.synthesize` | **Command**<br>`scope`: `DataScope`<br>`views`: `ViewSelection`<br>`expectedSourceRevision`: `Revision`<br>`nodeIds`: `Items<Id>`<br>`batchLimit`: `Limit`<br>`resumeRunId`: `?Id` | `AnalysisRecord` |
| `analysis.fleet_health` | **Query**<br>`scope`: `DataScope`<br>`views`: `ViewSelection`<br>`page`: `PageRequest`<br>`filters`: `Items<Label>` | `Page<MetricRecord>` |
| `analysis.archival_candidates` | **Query**<br>`scope`: `DataScope`<br>`views`: `ViewSelection`<br>`page`: `PageRequest`<br>`filters`: `Items<Label>` | `Page<MetricRecord>` |
| `analysis.file_hotspot` | **Query**<br>`scope`: `DataScope`<br>`views`: `ViewSelection`<br>`page`: `PageRequest`<br>`path`: `RelativePath`<br>`filters`: `Items<Label>` | `Page<MetricRecord>` |

### Schemas

**Owner:** SchemaService. Metadata-only structural persistence; native engine profile and credential capabilities explicit. No source row values retained.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `schemas.connections` | **Query**<br>`scope`: `DataScope`<br>`page`: `PageRequest` | `Page<SchemaConnection>` |
| `schemas.objects` | **Query**<br>`scope`: `DataScope`<br>`views`: `ViewSelection`<br>`connectionId`: `Id`<br>`kinds`: `Items<MetadataObjectKind>`<br>`parentId`: `?Id`<br>`text`: `?Text`<br>`page`: `PageRequest` | `Page<SchemaObject>` |
| `schemas.relationships` | **Query**<br>`scope`: `DataScope`<br>`views`: `ViewSelection`<br>`connectionId`: `Id`<br>`objectIds`: `Items<Id>`<br>`page`: `PageRequest` | `Page<SchemaRelation>` |
| `schemas.index` | **Command**<br>`connectionId`: `Id`<br>`expectedRevision`: `Revision` | `IndexRunRecord` |
| `schemas.refresh` | **Command**<br>`connectionId`: `Id`<br>`expectedRevision`: `Revision` | `IndexRunRecord` |
| `schemas.remove_index` | **Command**<br>`connectionId`: `Id`<br>`expectedRevision`: `Revision` | `MutationSummary` |
| `schemas.configure` | **Command**<br>`scope`: `DataScope`<br>`connectionId`: `?Id`<br>`expectedRevision`: `?Revision`<br>`config`: `ConnectionConfig` | `SchemaConnection` |

### Database

**Owner:** DataExecutor. Separate connection/role write authority; known read-only adapters, bounded sensitive results and observation-first uncertain commit handling.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `database.read` | **Query**<br>`connectionId`: `Id`<br>`expectedConnectionRevision`: `Revision`<br>`statement`: `Content`<br>`parameters`: `Items<DbParameter>`<br>`rowLimit`: `Limit`<br>`timeoutMs`: `PositiveCounter` | `DbReadResult` |
| `database.execute_write` | **Command**<br>`connectionId`: `Id`<br>`expectedConnectionRevision`: `Revision`<br>`statement`: `Content`<br>`parameters`: `Items<DbParameter>`<br>`proposalId`: `?Id`<br>`timeoutMs`: `PositiveCounter` | `DbWriteResult` |
| `database.apply_migration` | **Command**<br>`connectionId`: `Id`<br>`expectedConnectionRevision`: `Revision`<br>`migration`: `ArtifactRef`<br>`expectedSchemaDigest`: `Digest`<br>`proposalId`: `?Id` | `DbWriteResult` |
| `database.direct_schema_change` | **Command**<br>`connectionId`: `Id`<br>`expectedConnectionRevision`: `Revision`<br>`statement`: `Content`<br>`parameters`: `Items<DbParameter>`<br>`proposalId`: `?Id`<br>`timeoutMs`: `PositiveCounter` | `DbWriteResult` |
| `database.configure_sensitive_columns` | **Command**<br>`connectionId`: `Id`<br>`expectedRevision`: `Revision`<br>`columnIds`: `Items<Id>` | `SchemaConnection` |

### Knowledge

**Owner:** KnowledgeService. Typed authored claims/entities/evidence with scope before traversal/ranking. Revisioned contradiction/retraction preserved; no raw internal SQL.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `knowledge.search` | **Query**<br>`scope`: `DataScope`<br>`text`: `Text`<br>`tags`: `Items<Label>`<br>`kinds`: `Items<KnowledgeKind>`<br>`page`: `PageRequest`<br>`entityLimit`: `Limit`<br>`claimLimit`: `Limit`<br>`states`: `?Items<KnowledgeLifecycle>`<br>`entryKinds`: `?Items<KnowledgeEntryKind>`<br>`source`: `?Label`<br>`stale`: `?bool`<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/domain-request.r3.schema.json)) | `Page<KnowledgeClaim>` |
| `knowledge.read` | **Query**<br>`scope`: `DataScope`<br>`claimId`: `Id`<br>`revision`: `?Revision`<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/domain-request.r3.schema.json)) | `KnowledgeBundle` |
| `knowledge.record` | **Command**<br>`scope`: `DataScope`<br>`batch`: `KnowledgeBatch`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `KnowledgeWriteReceipt` |
| `knowledge.revise` | **Command**<br>`scope`: `DataScope`<br>`claimId`: `Id`<br>`expectedRevision`: `Revision`<br>`replacement`: `ClaimDraft`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `KnowledgeWriteReceipt` |
| `knowledge.relate` | **Command**<br>`scope`: `DataScope`<br>`subjectId`: `Id`<br>`predicate`: `Label`<br>`objectId`: `Id`<br>`evidenceIds`: `Items<Id>`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `KnowledgeWriteReceipt` |
| `knowledge.tag` | **Command**<br>`scope`: `DataScope`<br>`claimIds`: `NonEmpty<Id>`<br>`expectedRevisions`: `NonEmpty<RevisionedRef>`<br>`tags`: `Items<Label>`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `KnowledgeWriteReceipt` |
| `knowledge.verify` | **Command**<br>`scope`: `DataScope`<br>`claimId`: `Id`<br>`expectedRevision`: `Revision`<br>`reason`: `Content`<br>`evidenceIds`: `Items<Id>`<br>`sourceRevision`: `?Text`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `KnowledgeWriteReceipt` |
| `knowledge.retract` | **Command**<br>`scope`: `DataScope`<br>`claimId`: `Id`<br>`expectedRevision`: `Revision`<br>`reason`: `Content`<br>`evidenceIds`: `Items<Id>`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `KnowledgeWriteReceipt` |
| `knowledge.scope` | **Command**<br>`claimIds`: `NonEmpty<RevisionedRef>`<br>`destination`: `DataScope`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `KnowledgeWriteReceipt` |
| `knowledge.diagnostics` | **Query**<br>`scope`: `DataScope`<br>`sourceId`: `?Id`<br>`importSource`: `?ArtifactRef`<br>`importSchema`: `?SchemaRef`<br>`sourceLabel`: `?Label`<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/domain-request.r3.schema.json)) | `Page<Diagnostic>` |
| `knowledge.delete_by_source` | **Command**<br>`scope`: `DataScope`<br>`sourceId`: `?Id`<br>`expectedScopeRevision`: `Revision`<br>`previewDigest`: `Digest`<br>`proposalId`: `?Id`<br>`sourceLabel`: `?Label`<br><br>**Schema revisions** 1, 3 ([revision 3 input](contracts/domain-request.r3.schema.json)) | `MutationSummary` |
| `knowledge.delete_selected` | **Command**<br>`scope`: `DataScope`<br>`claims`: `NonEmpty<RevisionedRef>`<br>`proposalId`: `?Id` | `MutationSummary` |
| `knowledge.write_status` | **Query**<br>`writeId`: `Id`<br>`cursor`: `?Cursor`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `KnowledgeWriteReceipt` |
| `knowledge.subgraph` | **Query**<br>`scope`: `DataScope`<br>`query`: `KnowledgeQuery`<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/domain-request.r3.schema.json)) | `KnowledgeBundle` |
| `knowledge.entity_bundle` | **Query**<br>`scope`: `DataScope`<br>`query`: `KnowledgeQuery`<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/domain-request.r3.schema.json)) | `KnowledgeBundle` |
| `knowledge.entity_graph` | **Query**<br>`scope`: `DataScope`<br>`query`: `KnowledgeQuery`<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/domain-request.r3.schema.json)) | `KnowledgeBundle` |
| `knowledge.claim_bundle` | **Query**<br>`scope`: `DataScope`<br>`query`: `KnowledgeQuery`<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/domain-request.r3.schema.json)) | `KnowledgeBundle` |
| `knowledge.expand_frontier` | **Query**<br>`scope`: `DataScope`<br>`query`: `KnowledgeQuery`<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/domain-request.r3.schema.json)) | `KnowledgeBundle` |
| `knowledge.render_summary` | **Query**<br>`scope`: `DataScope`<br>`query`: `KnowledgeQuery` | `ArtifactRef` |
| `knowledge.query` | **Query**<br>`scope`: `DataScope`<br>`query`: `KnowledgeQuery`<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/domain-request.r3.schema.json)) | `KnowledgeBundle` |
| `knowledge.import_user_data` | **Command**<br>`scope`: `DataScope`<br>`source`: `ArtifactRef`<br>`schema`: `SchemaRef`<br>`previewDigest`: `Digest`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `KnowledgeWriteReceipt` |
| `knowledge.promote` | **Command**<br>`claimId`: `Id`<br>`expectedRevision`: `Revision`<br>`destination`: `DataScope`<br>`proposalId`: `?Id`<br>`action`: `?PromoteAction`<br>`address`: `?LessonAddress`<br>`reason`: `?Content`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `KnowledgeWriteReceipt` |
| `knowledge.challenge` | **Command**<br>`scope`: `DataScope`<br>`claimId`: `Id`<br>`expectedRevision`: `Revision`<br>`reason`: `Content`<br>`evidenceIds`: `Items<Id>`<br>`resolveChallengeId`: `?Id`<br>`outcome`: `?ChallengeOutcome`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `KnowledgeWriteReceipt` |
| `knowledge.tagged_lessons` | **Query**<br>`scope`: `DataScope`<br>`tags`: `NonEmpty<Label>`<br>`page`: `PageRequest`<br>`ruleStates`: `?Items<RuleState>`<br>`activity`: `?Label`<br>`view`: `?LessonView`<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/domain-request.r3.schema.json)) | `Page<KnowledgeClaim>` |

### Dreaming

**Owner:** DreamingWorkflow. Checkpointed proposals, safe revert and no model polling while blocked. Human approval and root Continue remain distinct.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `dreaming.run` | **Command**<br>`scope`: `DataScope`<br>`expectedSettingsRevision`: `Revision`<br>`resumeRunId`: `?Id` | `DreamRun` |
| `dreaming.runs` | **Query**<br>`scope`: `DataScope`<br>`page`: `PageRequest` | `Page<DreamRun>` |
| `dreaming.report` | **Query**<br>`runId`: `Id` | `DreamRun` |
| `dreaming.settings` | **Query**<br>`scope`: `DataScope`<br><br>**Command**<br>`scope`: `DataScope`<br>`expectedRevision`: `Revision`<br>`config`: `DreamConfig` | `DreamSettingsRecord` |
| `dreaming.instructions` | **Query**<br>`scope`: `DataScope`<br><br>**Command**<br>`scope`: `DataScope`<br>`expectedRevision`: `Revision`<br>`content`: `ArtifactRef` | `Page<InstructionRecord>` |
| `dreaming.propose` | **Command**<br>`runId`: `Id`<br>`expectedRevision`: `Revision`<br>`claimIds`: `NonEmpty<RevisionedRef>`<br>`content`: `ArtifactRef` | `ProposalRecord` |
| `dreaming.revert` | **Command**<br>`runId`: `Id`<br>`expectedRevision`: `Revision`<br>`proposalId`: `Id`<br>`expectedChangeDigest`: `Digest`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `KnowledgeWriteReceipt` |
| `dreaming.review` | **Command**<br>`proposalId`: `Id`<br>`expectedRevision`: `Revision`<br>`decision`: `ProposalDecision`<br>`rationale`: `Content` | `ProposalRecord` |

### Documents

**Owner:** DocumentService. All imported versions retained until explicit deletion. Exact version/page/locator citations; missing current source never selects an older version.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `documents.import` | **Command**<br>`scope`: `DataScope`<br>`source`: `ArtifactRef`<br>`title`: `Label`<br>`replaceDocumentId`: `?Id`<br>`expectedRevision`: `?Revision` | `DocumentRecord` |
| `documents.list` | **Query**<br>`scope`: `DataScope`<br>`page`: `PageRequest`<br>`retrievalEnabled`: `?bool` | `Page<DocumentRecord>` |
| `documents.read` | **Query**<br>[Existing input schema](contracts/artifact-query-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'Read'} | `DocumentRecord` |
| `documents.search` | **Query**<br>[Existing input schema](contracts/retrieval-request.schema.json)<br>None | `RetrievalPage` |
| `documents.share` | **Command**<br>[Existing input schema](contracts/artifact-command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'Share'} | `DocumentRecord` |
| `documents.move` | **Command**<br>[Existing input schema](contracts/artifact-command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'Move'} | `DocumentRecord` |
| `documents.index` | **Command**<br>`documentId`: `Id`<br>`versionId`: `Id`<br>`expectedRevision`: `Revision` | `IndexRunRecord` |
| `documents.retry` | **Command**<br>`documentId`: `Id`<br>`versionId`: `Id`<br>`expectedRevision`: `Revision` | `IndexRunRecord` |
| `documents.disable_retrieval` | **Command**<br>[Existing input schema](contracts/artifact-command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'DisableRetrieval'} | `DocumentRecord` |
| `documents.remove_index` | **Command**<br>[Existing input schema](contracts/artifact-command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'RemoveIndex'} | `DocumentRecord` |

### Artifacts

**Owner:** ArtifactService. Existing immutable source/version/reference/deletion DTOs remain authoritative. Explicit deletion tombstones first, preserves surviving references, and never reactivates an old version.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `artifacts.save` | **Command**<br>[Existing input schema](contracts/artifact-command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'Save'}<br><br>**Schema revisions** 1, 2, 3, 4 ([revision 4 input](contracts/artifact-command-request.r3.schema.json)) | `ArtifactRecord` |
| `artifacts.list` | **Query**<br>[Existing input schema](contracts/artifact-query-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'List'}<br><br>**Schema revisions** 1, 2, 3, 4 ([revision 4 input](contracts/artifact-query-request.r3.schema.json)) | `Page<ArtifactRecord>` |
| `artifacts.read` | **Query**<br>[Existing input schema](contracts/artifact-query-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'Read'}<br><br>**Schema revisions** 1, 4 ([revision 4 input](contracts/artifact-query-request.r3.schema.json)) | `ArtifactReadContent` |
| `artifacts.preview` | **Query**<br>[Existing input schema](contracts/artifact-query-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'Preview'} | `ArtifactPreview` |
| `artifacts.export` | **Query**<br>[Existing input schema](contracts/artifact-query-request.schema.json)<br>{'pointer': '/query/kind', 'equals': 'Export'} | `ArtifactExport` |
| `artifacts.reference` | **Command**<br>[Existing input schema](contracts/artifact-command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'Reference'} | `MutationSummary` |
| `artifacts.delete` | **Command**<br>[Existing input schema](contracts/artifact-deletion-request.schema.json)<br>None | `MutationSummary` |
| `artifacts.tag` | **Command**<br>`artifactId`: `Id`<br>`expectedRevision`: `Revision`<br>`tags`: `ArtifactTags` | `MutationSummary` |

### Conventions

**Owner:** ConventionService. Versioned authored guidance, scope and resource lifecycle. Guidance is data and cannot override authority or user instructions.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `conventions.list` | **Query**<br>`scope`: `DataScope`<br>`page`: `PageRequest`<br>`applicability`: `Items<Label>` | `Page<InstructionRecord>` |
| `conventions.read` | **Query**<br>`conventionId`: `Id`<br>`revision`: `?Revision` | `InstructionRecord` |
| `conventions.edit` | **Command**<br>`conventionId`: `Id`<br>`expectedRevision`: `Revision`<br>`content`: `ArtifactRef` | `InstructionRecord` |
| `conventions.version` | **Query**<br>`conventionId`: `Id`<br>`page`: `PageRequest` | `Page<InstructionRecord>` |
| `conventions.applicability` | **Command**<br>`conventionId`: `Id`<br>`expectedRevision`: `Revision`<br>`applicability`: `Items<Label>` | `InstructionRecord` |
| `conventions.retire` | **Command**<br>`conventionId`: `Id`<br>`expectedRevision`: `Revision`<br>`resourceId`: `?Id` | `MutationSummary` |
| `conventions.index` | **Command**<br>`conventionId`: `Id`<br>`expectedRevision`: `Revision`<br>`resourceId`: `?Id` | `MutationSummary` |
| `conventions.import` | **Command**<br>`scope`: `DataScope`<br>`source`: `ArtifactRef`<br>`schema`: `SchemaRef` | `InstructionRecord` |
| `conventions.delete` | **Command**<br>`conventionId`: `Id`<br>`expectedRevision`: `Revision`<br>`resourceId`: `?Id` | `MutationSummary` |
| `conventions.move` | **Command**<br>`conventionId`: `Id`<br>`expectedRevision`: `Revision`<br>`destination`: `DataScope` | `InstructionRecord` |
| `conventions.resources` | **Query**<br>`scope`: `DataScope`<br>`page`: `PageRequest`<br>`applicability`: `Items<Label>` | `Page<InstructionRecord>` |
| `conventions.delete_resource` | **Command**<br>`conventionId`: `Id`<br>`expectedRevision`: `Revision`<br>`resourceId`: `?Id` | `MutationSummary` |
| `conventions.search` | **Query**<br>`scope`: `DataScope`<br>`page`: `PageRequest`<br>`applicability`: `Items<Label>`<br>`text`: `Text` | `Page<InstructionRecord>` |

### Skills

**Owner:** SkillService. Shared files discovered in place, local overrides staged for exact approval and safe adoption. Package assets resolve from owning root.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `skills.discover` | **Query**<br>`scope`: `DataScope`<br>`rootIds`: `Items<Id>`<br>`page`: `PageRequest` | `Page<InstructionRecord>` |
| `skills.read` | **Query**<br>`instructionId`: `Id`<br>`revision`: `?Revision` | `InstructionRecord` |
| `skills.resources` | **Query**<br>`instructionId`: `Id`<br>`revision`: `Revision`<br>`page`: `PageRequest` | `Page<ArtifactRef>` |
| `skills.draft` | **Command**<br>`scope`: `DataScope`<br>`instructionId`: `?Id`<br>`expectedRevision`: `?Revision`<br>`name`: `Label`<br>`content`: `ArtifactRef` | `InstructionRecord` |
| `skills.propose` | **Command**<br>`instructionId`: `Id`<br>`expectedRevision`: `Revision`<br>`contentDigest`: `Digest` | `ProposalRecord` |
| `skills.history` | **Query**<br>`instructionId`: `Id`<br>`page`: `PageRequest` | `Page<InstructionRecord>` |
| `skills.rollback` | **Command**<br>`instructionId`: `Id`<br>`expectedRevision`: `Revision`<br>`targetRevision`: `Revision`<br>`proposalId`: `?Id` | `InstructionRecord` |

### Roles

**Owner:** InstructionService. Approved versioned prompts/restrictions only; role edits cannot expand access or let a reviewer mutate.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `roles.read` | **Query**<br>`instructionId`: `Id`<br>`revision`: `?Revision` | `InstructionRecord` |
| `roles.draft` | **Command**<br>`scope`: `DataScope`<br>`instructionId`: `?Id`<br>`expectedRevision`: `?Revision`<br>`name`: `Label`<br>`content`: `ArtifactRef` | `InstructionRecord` |
| `roles.propose` | **Command**<br>`instructionId`: `Id`<br>`expectedRevision`: `Revision`<br>`contentDigest`: `Digest` | `ProposalRecord` |
| `roles.history` | **Query**<br>`instructionId`: `Id`<br>`page`: `PageRequest` | `Page<InstructionRecord>` |
| `roles.rollback` | **Command**<br>`instructionId`: `Id`<br>`expectedRevision`: `Revision`<br>`targetRevision`: `Revision`<br>`proposalId`: `?Id` | `InstructionRecord` |

### Approvals

**Owner:** ApprovalService. Only authenticated host human interaction mints a human decision. Bind target/revision/content; approval does not clear pause.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `approvals.list` | **Query**<br>`scope`: `DataScope`<br>`page`: `PageRequest`<br>`state`: `?ApprovalState` | `Page<ProposalRecord>` |
| `approvals.inspect` | **Query**<br>`proposalId`: `Id` | `ProposalRecord` |
| `approvals.propose` | **Command**<br>`scope`: `DataScope`<br>`operationKind`: `Label`<br>`target`: `RevisionedRef`<br>`contentDigest`: `Digest`<br>`summary`: `Content`<br>`artifact`: `?ArtifactRef` | `ProposalRecord` |
| `approvals.record_human_decision` | **Command**<br>`proposalId`: `Id`<br>`expectedRevision`: `Revision`<br>`contentDigest`: `Digest`<br>`decision`: `ProposalDecision`<br>`rationale`: `Content` | `ProposalRecord` |

### Notifications

**Owner:** NotificationService. Acknowledgment is presentation state, never approval or root Continue. OS denial preserves in-app access.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `notifications.list` | **Query**<br>`scope`: `DataScope`<br>`page`: `PageRequest`<br>`unacknowledgedOnly`: `bool` | `Page<NotificationRecord>` |
| `notifications.read` | **Query**<br>`notificationId`: `Id` | `NotificationRecord` |
| `notifications.acknowledge` | **Command**<br>`notificationId`: `Id`<br>`expectedRevision`: `Revision` | `NotificationRecord` |
| `notifications.settings` | **Query**<br>No caller fields<br><br>**Command**<br>`expectedRevision`: `Revision`<br>`config`: `NotificationConfig` | `NotificationSettingsRecord` |

### Git

**Owner:** GitService. Exact repository/worktree/expected objects, journaled external effects, owned branch limits and no blind retry after unknown outcome.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `git.worktree` | **Command**<br>`repositoryId`: `Id`<br>`workId`: `Id`<br>`expectedRevision`: `Revision`<br>`action`: `GitActionKind`<br>`base`: `GitOid`<br>`registeredRootId`: `Id`<br>`worktreeId`: `?Id` | `GitInspection` |
| `git.inspect` | **Query**<br>`repositoryId`: `Id`<br>`worktreeId`: `?Id` | `GitInspection` |
| `git.diff` | **Query**<br>`repositoryId`: `Id`<br>`worktreeId`: `?Id`<br>`base`: `GitOid`<br>`head`: `GitOid`<br>`paths`: `Items<RelativePath>`<br>`page`: `PageRequest` | `Page<ArtifactRef>` |
| `git.commit` | **Command**<br>[Existing input schema](contracts/git-command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'Commit'} | `GitInspection` |
| `git.push` | **Command**<br>[Existing input schema](contracts/git-command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'Push'} | `GitInspection` |
| `git.pull_request` | **Command**<br>[Existing input schema](contracts/git-command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'OpenPullRequest'}<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/git-command-request.r2.schema.json)) | `GitLink` |
| `git.integrate` | **Command**<br>[Existing input schema](contracts/git-command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'Integrate'} | `DeliverySnapshot` |
| `git.cleanup` | **Command**<br>[Existing input schema](contracts/git-command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'Cleanup'} | `MutationSummary` |

### Reviews

**Owner:** ReviewService. Exact heads/policies and independent verified-read evidence. Actionable findings require resolution; changed heads invalidate readiness.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `reviews.list` | **Query**<br>`scope`: `DataScope`<br>`rootId`: `?RootId`<br>`page`: `PageRequest` | `Page<ReviewRecord>` |
| `reviews.inspect` | **Query**<br>`reviewId`: `Id` | `ReviewRecord` |
| `reviews.request` | **Command**<br>`rootId`: `RootId`<br>`expectedRevision`: `Revision`<br>`barrier`: `DeliveryBarrier`<br>`policyRevision`: `Revision` | `ReviewRecord` |
| `reviews.findings` | **Query**<br>`reviewId`: `Id`<br>`page`: `PageRequest` | `Page<ReviewFinding>` |
| `reviews.respond` | **Command**<br>`findingId`: `Id`<br>`expectedRevision`: `Revision`<br>`response`: `Content`<br>`evidence`: `Items<ArtifactRef>` | `ReviewFinding` |
| `reviews.adjudicate` | **Command**<br>`findingId`: `Id`<br>`expectedRevision`: `Revision`<br>`actionable`: `bool`<br>`rationale`: `Content`<br>`evidence`: `Items<ArtifactRef>` | `ReviewFinding` |
| `reviews.readiness` | **Query**<br>`rootId`: `RootId`<br>`barrier`: `DeliveryBarrier`<br>`policyRevision`: `Revision` | `ReviewReadiness` |

### Releases

**Owner:** DeliveryService. Publish/deploy authority remains independent of merged work. Exact artifacts/destination and ask-by-default policy.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `releases.inspect` | **Query**<br>`releaseId`: `Id` | `ReleaseRecord` |
| `releases.prepare` | **Command**<br>[Existing input schema](contracts/release-command-request.schema.json)<br>{'pointer': '/action', 'equals': 'Prepare'} | `ReleaseRecord` |
| `releases.publish` | **Command**<br>[Existing input schema](contracts/release-command-request.schema.json)<br>{'pointer': '/action', 'equals': 'Publish'} | `ReleaseRecord` |
| `releases.deploy` | **Command**<br>[Existing input schema](contracts/release-command-request.schema.json)<br>{'pointer': '/action', 'equals': 'Deploy'} | `ReleaseRecord` |

### Github

**Owner:** GitHubService. One GitHub.com account, conditional paginated polling, observed remote revisions and consumer cursors; no public webhook service.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `github.account` | **Query**<br>No caller fields<br><br>**Command**<br>`action`: `AccountAction`<br>`connectionId`: `?Id`<br>`authFlowId`: `?Id` | `GithubAccountState` |
| `github.repositories` | **Query**<br>`accountId`: `Id`<br>`page`: `PageRequest`<br>`organization`: `?Label` | `Page<RepositoryRecord>` |
| `github.poll_status` | **Query**<br>`repositoryId`: `Id` | `Page<GithubObservation>` |
| `github.issue_sync` | **Command**<br>`repositoryId`: `Id`<br>`expectedCursorRevision`: `Revision`<br>`remoteIssueNumber`: `?PositiveCounter` | `OperationRef` |
| `github.checks` | **Query**<br>`repositoryId`: `Id`<br>`pullRequestNumber`: `PositiveCounter`<br>`head`: `GitOid`<br>`page`: `PageRequest` | `Page<GithubObservation>` |
| `github.reviews` | **Query**<br>`repositoryId`: `Id`<br>`pullRequestNumber`: `PositiveCounter`<br>`head`: `GitOid`<br>`page`: `PageRequest` | `Page<GithubObservation>` |

### Resources

**Owner:** ResourceService. Canonical physical keys and host-owned leases/fences. Uncertain running resources cannot be released as free.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `resources.list` | **Query**<br>`scope`: `DataScope`<br>`page`: `PageRequest`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `Page<ResourceRecord>` |
| `resources.acquire` | **Command**<br>`resourceId`: `ResourceId`<br>`expectedRevision`: `Revision`<br>`units`: `Limit`<br>`leaseMs`: `PositiveCounter`<br>`purpose`: `?Text`<br>`priority`: `?ResourcePriority`<br>`capacity`: `?Limit`<br>`labels`: `?Items<Label>`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `LeaseRecord` |
| `resources.renew` | **Command**<br>`leaseId`: `Id`<br>`expectedFence`: `Counter`<br>`leaseMs`: `PositiveCounter`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `LeaseRecord` |
| `resources.release` | **Command**<br>`leaseId`: `Id`<br>`expectedFence`: `Counter`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `LeaseRecord` |
| `resources.holders` | **Query**<br>`resourceId`: `ResourceId`<br>`page`: `PageRequest`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `Page<LeaseRecord>` |
| `resources.waiters` | **Query**<br>`resourceId`: `ResourceId`<br>`page`: `PageRequest`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `Page<LeaseRecord>` |

### Browser

**Owner:** BrowserBackend. Owned browser/session/frame/node revisions, authorized staged files and bounded untrusted evidence. Observe input uncertainty before retry.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `browser.sessions` | **Query**<br>`scope`: `DataScope`<br>`page`: `PageRequest`<br><br>**Command**<br>`action`: `BrowserSessionAction`<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/domain-request.r3.schema.json)) | `Page<BrowserSession>` |
| `browser.inspect` | **Query**<br>`target`: `BrowserTarget`<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/domain-request.r3.schema.json)) | `BrowserSnapshot` |
| `browser.navigate` | **Command**<br>`sessionId`: `Id`<br>`expectedRevision`: `Revision`<br>`targetId`: `?Id`<br>`location`: `BrowserLocation`<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/domain-request.r3.schema.json)) | `BrowserSnapshot` |
| `browser.input` | **Command**<br>`sessionId`: `Id`<br>`expectedRevision`: `Revision`<br>`action`: `BrowserInput`<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/domain-request.r3.schema.json)) | `BrowserObservation` |
| `browser.upload` | **Command**<br>`node`: `BrowserNode`<br>`files`: `NonEmpty<ArtifactRef>`<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/domain-request.r3.schema.json)) | `BrowserObservation` |
| `browser.screenshot` | **Query**<br>`target`: `BrowserTarget`<br>`fullPage`: `bool`<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/domain-request.r3.schema.json)) | `ArtifactRef` |
| `browser.console` | **Query**<br>`sessionId`: `Id`<br>`afterSequence`: `Counter`<br>`page`: `PageRequest`<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/domain-request.r3.schema.json)) | `Page<BrowserObservation>` |
| `browser.network` | **Query**<br>`sessionId`: `Id`<br>`afterSequence`: `Counter`<br>`page`: `PageRequest`<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/domain-request.r3.schema.json)) | `Page<BrowserObservation>` |

### Desktop

**Owner:** DesktopBackend. Canonical reservation, OS permissions and current OK/Wait consent at input boundary. Wait or stale consent denies input.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `desktop.inspect` | **Query**<br>No caller fields | `DesktopRecord` |
| `desktop.capture` | **Query**<br>`desktopId`: `Id`<br>`expectedRevision`: `Revision` | `DesktopCapture` |
| `desktop.request_control` | **Command**<br>`desktopId`: `Id`<br>`expectedRevision`: `Revision`<br>`reason`: `Content`<br>`leaseMs`: `PositiveCounter` | `DesktopRecord` |
| `desktop.input` | **Command**<br>`desktopId`: `Id`<br>`leaseId`: `Id`<br>`expectedFence`: `Counter`<br>`consentId`: `Id`<br>`expectedDesktopRevision`: `Revision`<br>`action`: `DesktopInput` | `DesktopRecord` |
| `desktop.release_control` | **Command**<br>`desktopId`: `Id`<br>`leaseId`: `Id`<br>`expectedFence`: `Counter` | `DesktopRecord` |

### Process

**Owner:** ProcessHost. Current Full/project/qualified confined profile. Full ambient user access is not a sandbox. Owned trees, bounded output and uncertain effects.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `process.execute` | **Command**<br>`spec`: `ProcessSpec` | `ProcessRecord` |
| `process.output` | **Query**<br>`processId`: `Id`<br>`afterBytes`: `Counter`<br>`maxBytes`: `ByteLimit` | `ProcessOutput` |
| `process.cancel` | **Command**<br>`processId`: `Id`<br>`expectedRevision`: `Revision` | `ProcessRecord` |
| `process.install_project_dependencies` | **Command**<br>`repositoryId`: `Id`<br>`expectedRevision`: `Revision`<br>`manifest`: `ArtifactRef`<br>`profile`: `ToolProfile` | `ProcessRecord` |
| `process.request_system_install` | **Command**<br>`packageSpec`: `ArtifactRef`<br>`contentDigest`: `Digest`<br>`reason`: `Content` | `ProposalRecord` |

### Web

**Owner:** WebAdapter. Native bounded untrusted retrieval with outbound/redirect/credential policy. Website content never authorizes tools.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `web.search` | **Query**<br>`scope`: `DataScope`<br>`query`: `Text`<br>`providerId`: `Id`<br>`page`: `PageRequest`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `Page<WebResult>` |
| `web.fetch` | **Query**<br>`scope`: `DataScope`<br>`url`: `Url`<br>`maxBytes`: `ByteLimit`<br>`timeoutMs`: `PositiveCounter`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `WebDocument` |

### Containers

**Owner:** ContainerBackend. Qualified optional engines only, owned resources, current mounts/ports/credential policy. Never stop preexisting engines/resources on app close.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `containers.capabilities` | **Query**<br>`engineId`: `?Id` | `Page<ContainerCapability>` |
| `containers.build` | **Command**<br>`engineId`: `Id`<br>`projectId`: `ProjectId`<br>`context`: `RegisteredPath`<br>`definition`: `ArtifactRef`<br>`tag`: `Text` | `ContainerImage` |
| `containers.pull` | **Command**<br>`engineId`: `Id`<br>`image`: `Text`<br>`credentialRef`: `?Id` | `ContainerImage` |
| `containers.create` | **Command**<br>`spec`: `ContainerSpec` | `ContainerRecord` |
| `containers.start` | **Command**<br>`containerId`: `Id`<br>`expectedRevision`: `Revision` | `ContainerRecord` |
| `containers.exec` | **Command**<br>`containerId`: `Id`<br>`expectedRevision`: `Revision`<br>`command`: `NonEmpty<Text>`<br>`environment`: `Items<EnvironmentEntry>`<br>`timeoutMs`: `PositiveCounter` | `ProcessRecord` |
| `containers.inspect` | **Query**<br>`containerId`: `Id` | `ContainerRecord` |
| `containers.logs` | **Query**<br>`containerId`: `Id`<br>`afterBytes`: `Counter`<br>`maxBytes`: `ByteLimit` | `ArtifactRef` |
| `containers.stop` | **Command**<br>`containerId`: `Id`<br>`expectedRevision`: `Revision` | `ContainerRecord` |

### Jobs

**Owner:** Scheduler. Durable occurrence identity; skip missed/overlap and reconcile interrupted runs. No job cap; linked features retain budgets. Restore requires reactivation.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `jobs.list` | **Query**<br>`scope`: `DataScope`<br>`page`: `PageRequest`<br>`enabled`: `?bool`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `Page<JobRecord>` |
| `jobs.create` | **Command**<br>`scope`: `DataScope`<br>`title`: `Label`<br>`schedule`: `Schedule`<br>`definition`: `JobDefinition`<br>`enabled`: `bool`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `JobRecord` |
| `jobs.edit` | **Command**<br>`jobId`: `Id`<br>`expectedRevision`: `Revision`<br>`title`: `Label`<br>`schedule`: `Schedule`<br>`definition`: `JobDefinition`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `JobRecord` |
| `jobs.enable` | **Command**<br>`jobId`: `Id`<br>`expectedRevision`: `Revision`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `JobRecord` |
| `jobs.run_now` | **Command**<br>`jobId`: `Id`<br>`expectedRevision`: `Revision`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `JobRun` |
| `jobs.inspect` | **Query**<br>`jobId`: `Id`<br>`runId`: `?Id`<br>`history`: `?PageRequest`<br>`usage`: `?PageRequest`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `JobInspection` |
| `jobs.cancel` | **Command**<br>`jobId`: `Id`<br>`runId`: `Id`<br>`expectedRevision`: `Revision`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `JobRun` |
| `jobs.delete` | **Command**<br>`jobId`: `Id`<br>`expectedRevision`: `Revision`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `JobRecord` |
| `jobs.disable` | **Command**<br>`jobId`: `Id`<br>`expectedRevision`: `Revision`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `JobRecord` |

### Settings

**Owner:** ConfigurationService. Known scoped schema revisions, explicit origin/effective values and no raw credentials. Unknown keys/schema versions fail closed.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `settings.read_effective` | **Query**<br>`scope`: `DataScope`<br>`keys`: `Items<Label>`<br>`page`: `PageRequest` | `Page<SettingEntry>` |
| `settings.validate` | **Query**<br>`scope`: `DataScope`<br>`expectedRevision`: `Revision`<br>`entries`: `NonEmpty<SettingAssignment>` | `ValidationReport` |
| `settings.save_revision` | **Command**<br>`scope`: `DataScope`<br>`expectedRevision`: `Revision`<br>`entries`: `NonEmpty<SettingAssignment>` | `Page<SettingEntry>` |
| `settings.explain_source` | **Query**<br>`scope`: `DataScope`<br>`key`: `Label` | `Page<SettingEntry>` |

### Router

**Owner:** InferenceRouter. Direct tiers/named/Auto and separate effort; durable pins, trusted capabilities, host attribution/price provenance and explicit unknown tails.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `router.catalog` | **Query**<br>`page`: `PageRequest`<br>`includeDisabled`: `bool`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `Page<RouteModel>` |
| `router.connections` | **Query**<br>`page`: `PageRequest`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `Page<ConnectionRecord>` |
| `router.route` | **Command**<br>`sessionId`: `Id`<br>`expectedContextEpoch`: `Counter`<br>`intent`: `RouteIntent`<br>`effort`: `EffortName`<br>`requiredCapabilities`: `Items<Label>` | `RouteChoice` |
| `router.pin` | **Command**<br>`sessionId`: `Id`<br>`expectedPinRevision`: `Revision`<br>`intent`: `RouteIntent`<br>`effort`: `EffortName` | `RouteChoice` |
| `router.availability` | **Query**<br>`intent`: `RouteIntent`<br>`effort`: `EffortName`<br>`requiredCapabilities`: `Items<Label>`<br>`sessionId`: `?Id`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `RouteAvailability` |
| `router.usage` | **Query**<br>`rootId`: `?RootId`<br>`sessionId`: `?Id`<br>`jobRunId`: `?Id`<br>`page`: `PageRequest` | `UsageReport` |
| `router.request_status` | **Query**<br>`requestId`: `Id` | `OperationRef` |
| `router.price_status` | **Query**<br>`modelIds`: `Items<Id>`<br>`page`: `PageRequest` | `Page<ModelPriceStatus>` |
| `router.price_refresh` | **Command**<br>`providerIds`: `NonEmpty<Id>`<br>`expectedPriceRevision`: `Revision` | `OperationRef` |
| `router.price_override` | **Command**<br>`modelId`: `Id`<br>`expectedRevision`: `Revision`<br>`price`: `PriceInput` | `PriceRecord` |
| `router.configure` | **Command**<br>`action`: `RouterConfigAction`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `MutationSummary` |
| `router.decision_models` | **Query**<br>`page`: `PageRequest`<br>`includeDisabled`: `bool` | `Page<DecisionModel>` |
| `router.decide` | **Command**<br>`modelId`: `Id`<br>`expectedModelRevision`: `Revision`<br>`state`: `DynamicJson`<br>`questions`: `NonEmpty<NamedQuestion>`<br>`images`: `Items<MediaRef>`<br>`videos`: `Items<MediaRef>`<br>`timeoutMs`: `PositiveCounter` | `DecisionResult` |

### Mcp

**Owner:** McpClient. Outgoing client; optional declared runtime. Remote hints are not verified-read proof; discovery never self-enables tools or expands host policy.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `mcp.servers` | **Query**<br>`scope`: `DataScope`<br>`page`: `PageRequest` | `Page<McpServer>` |
| `mcp.configure` | **Command**<br>`scope`: `DataScope`<br>`serverId`: `?Id`<br>`expectedRevision`: `?Revision`<br>`name`: `Label`<br>`transport`: `McpTransport` | `McpServer` |
| `mcp.connect` | **Command**<br>`serverId`: `Id`<br>`expectedRevision`: `Revision` | `McpServer` |
| `mcp.discover` | **Command**<br>`serverId`: `Id`<br>`expectedCatalogRevision`: `Revision` | `Page<McpCapability>` |
| `mcp.select_tools` | **Command**<br>`serverId`: `Id`<br>`expectedRevision`: `Revision`<br>`expectedCatalogRevision`: `Revision`<br>`toolIds`: `Items<Id>` | `McpServer` |
| `mcp.resources` | **Query**<br>`serverId`: `Id`<br>`catalogRevision`: `Revision`<br>`uri`: `?Text`<br>`page`: `PageRequest` | `Page<McpResource>` |
| `mcp.prompts` | **Query**<br>`serverId`: `Id`<br>`catalogRevision`: `Revision`<br>`name`: `?Label`<br>`arguments`: `?SchemaValue`<br>`page`: `PageRequest` | `Page<McpPrompt>` |
| `mcp.capabilities` | **Query**<br>`serverId`: `Id`<br>`catalogRevision`: `Revision`<br>`page`: `PageRequest` | `Page<McpCapability>` |

### Addons

**Owner:** PackageHost. Exact manifest/schema/version/trust and quiescent publication. Agent-authored contents always need exact approval regardless signature.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `addons.catalog` | **Query**<br>`page`: `PageRequest`<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/domain-request.r3.schema.json)) | `Page<PackageRecord>` |
| `addons.install` | **Command**<br>[Existing input schema](contracts/addon-command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'Install'}<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/addon-command-request.schema.json)) | `PackageRecord` |
| `addons.configure` | **Command**<br>`packageId`: `Id`<br>`expectedRevision`: `Revision`<br>`config`: `SchemaValue`<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/domain-request.r3.schema.json)) | `PackageRecord` |
| `addons.enable` | **Command**<br>[Existing input schema](contracts/addon-command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'Enable'}<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/addon-command-request.schema.json)) | `PackageRecord` |
| `addons.disable` | **Command**<br>[Existing input schema](contracts/addon-command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'Disable'}<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/addon-command-request.schema.json)) | `PackageRecord` |
| `addons.update` | **Command**<br>[Existing input schema](contracts/addon-command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'Update'}<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/addon-command-request.schema.json)) | `PackageRecord` |
| `addons.propose` | **Command**<br>`source`: `ArtifactRef`<br>`manifestDigest`: `Digest`<br>`basePackageId`: `?Id`<br>`expectedRevision`: `?Revision` | `ProposalRecord` |
| `addons.logs` | **Query**<br>`packageId`: `Id`<br>`afterSequence`: `Counter`<br>`page`: `PageRequest` | `Page<Diagnostic>` |
| `addons.rollback` | **Command**<br>[Existing input schema](contracts/addon-command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'Rollback'}<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/addon-command-request.schema.json)) | `PackageRecord` |
| `addons.uninstall` | **Command**<br>[Existing input schema](contracts/addon-command-request.schema.json)<br>{'pointer': '/command/kind', 'equals': 'Uninstall'}<br><br>**Schema revisions** 1, 2, 3 ([revision 3 input](contracts/addon-command-request.schema.json)) | `PackageRecord` |

### Help

**Owner:** HelpService. Version-matched searchable offline human/agent content. Deterministic capability docs do not require inference.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `help.search` | **Query**<br>`query`: `Text`<br>`page`: `PageRequest` | `Page<HelpTopic>` |
| `help.read` | **Query**<br>`topicId`: `Id`<br>`version`: `Label` | `HelpTopic` |
| `help.topic_for_screen` | **Query**<br>`screenId`: `Id`<br>`capabilityRevision`: `Revision` | `Page<HelpTopic>` |
| `help.package_guides` | **Query**<br>`packageId`: `Id`<br>`version`: `Label`<br>`page`: `PageRequest` | `Page<HelpTopic>` |
| `help.generate_capability_docs` | **Command**<br>`registryRevision`: `Revision`<br>`expectedHelpRevision`: `Revision` | `OperationRef` |

### Workspace

**Owner:** WorkspaceLifecycle. Dedicated human maintenance/update actions, exact immutable input and journaled transitions. No automatic backup/restore/restart.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `workspace.diagnostics` | **Query**<br>`page`: `PageRequest`<br>`component`: `?Label`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `DiagnosticPage` |
| `workspace.backup` | **Command**<br>`destinationGrantId`: `Id`<br>`expectedStoreSequence`: `Counter` | `BackupRecord` |
| `workspace.restore` | **Command**<br>`backup`: `ArtifactRef`<br>`expectedStoreId`: `StoreId`<br>`expectedRestoreEpoch`: `Counter`<br>`proposalId`: `Id` | `BackupRecord` |
| `workspace.migrate` | **Command**<br>`migrationManifest`: `ArtifactRef`<br>`expectedSchemaRevision`: `Revision`<br>`proposalId`: `?Id` | `MutationSummary` |
| `workspace.update_status` | **Query**<br>No caller fields | `Page<UpdateRecord>` |
| `workspace.approve_install` | **Command**<br>`updateId`: `Id`<br>`expectedRevision`: `Revision`<br>`binaryDigest`: `Digest`<br>`proposalId`: `Id` | `UpdateRecord` |
| `workspace.logs` | **Query**<br>`page`: `PageRequest`<br>`component`: `?Label`<br>`afterSequence`: `Counter`<br>`severity`: `?Severity`<br>`outcome`: `?DiagnosticOutcome`<br>`operation`: `?Label`<br>`operationId`: `?OperationId`<br>`correlationId`: `?CorrelationId`<br>`targetId`: `?Id`<br>`stage`: `?Label`<br>`projectId`: `?ProjectId`<br>`search`: `?Label`<br>`sinceMs`: `?Counter`<br>`untilMs`: `?Counter`<br>`order`: `?SortDirection`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `DiagnosticPage` |
| `workspace.record_diagnostic` | **Command**<br>`component`: `Label`<br>`code`: `Label`<br>`severity`: `Severity`<br>`message`: `Content`<br>`evidence`: `?ArtifactRef`<br>`operation`: `?Label`<br>`operationId`: `?OperationId`<br>`targetKind`: `?Label`<br>`targetId`: `?Id`<br>`stage`: `?Label`<br>`sourceRevision`: `?Label`<br>`attempt`: `?Counter`<br>`startedAtMs`: `?Counter`<br>`finishedAtMs`: `?Counter`<br>`progressCompleted`: `?Counter`<br>`progressTotal`: `?Counter`<br>`outcome`: `?DiagnosticOutcome`<br>`errorClass`: `?DiagnosticErrorClass`<br>`output`: `?Content`<br><br>**Schema revisions** 1, 2 ([revision 2 input](contracts/domain-request.r2.schema.json)) | `DiagnosticEntry` |

### Repositories

**Owner:** RepositoryService. Registered source identity and scope; removing index does not remove primary source or work history.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `repositories.list` | **Query**<br>`scope`: `DataScope`<br>`page`: `PageRequest`<br>`text`: `?Text` | `Page<RepositoryRecord>` |
| `repositories.get` | **Query**<br>`repositoryId`: `Id` | `RepositoryRecord` |
| `repositories.nodes` | **Query**<br>`repositoryId`: `Id`<br>`views`: `ViewSelection`<br>`page`: `PageRequest` | `GraphPage` |
| `repositories.remove_index` | **Command**<br>`repositoryId`: `Id`<br>`expectedRevision`: `Revision` | `MutationSummary` |
| `repositories.readme` | **Query**<br>`repositoryId`: `Id`<br>`views`: `ViewSelection` | `RepositoryText` |
| `repositories.summary` | **Query**<br>`repositoryId`: `Id`<br>`views`: `ViewSelection` | `RepositoryText` |
| `repositories.search` | **Query**<br>`scope`: `DataScope`<br>`query`: `Text`<br>`languageIds`: `Items<Label>`<br>`page`: `PageRequest` | `Page<RepositoryRecord>` |

### Queue

**Owner:** QueueDispatcher. Fenced claims/consumer state. Operator resolution cannot forge successful domain outcomes, retry unknown effects blindly, or clear pause.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `queue.inspect` | **Query**<br>`consumerId`: `?Id`<br>`operationId`: `?OperationId`<br>`page`: `PageRequest` | `Page<QueueDelivery>` |
| `queue.resolve` | **Command**<br>`deliveryId`: `Id`<br>`expectedRevision`: `Revision`<br>`expectedFence`: `Counter`<br>`resolution`: `QueueResolution`<br>`reason`: `Content` | `QueueDelivery` |
| `queue.retry` | **Command**<br>`deliveryId`: `Id`<br>`expectedRevision`: `Revision`<br>`expectedFence`: `Counter`<br>`resolution`: `QueueResolution`<br>`reason`: `Content` | `QueueDelivery` |
| `queue.consumer_status` | **Query**<br>`consumerId`: `Id` | `ConsumerStatus` |
| `queue.reconcile` | **Command**<br>`deliveryId`: `Id`<br>`expectedRevision`: `Revision`<br>`expectedFence`: `Counter`<br>`resolution`: `QueueResolution`<br>`reason`: `Content` | `QueueDelivery` |

### Imports

**Owner:** ImportService. Selected legacy work/history only, source snapshot/identity map and idempotent replay. Excluded knowledge/conventions/index migration remains excluded.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `imports.preview` | **Query**<br>`config`: `ImportConfig` | `ImportRun` |
| `imports.configure` | **Command**<br>`configId`: `?Id`<br>`expectedRevision`: `?Revision`<br>`config`: `ImportConfig` | `MutationSummary` |
| `imports.apply` | **Command**<br>`configId`: `Id`<br>`expectedRevision`: `Revision`<br>`sourceSnapshot`: `Digest`<br>`previewDigest`: `Digest` | `ImportRun` |
| `imports.runs` | **Query**<br>`page`: `PageRequest` | `Page<ImportRun>` |
| `imports.inspect` | **Query**<br>`runId`: `Id` | `ImportRun` |

### Maintenance

**Owner:** MaintenanceService. Exact preview/revisions and retention/reference holds; no pressure-driven deletion of retained documents or authored history.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `maintenance.prune_diagnostics` | **Command**<br>`previewOnly`: `bool`<br>`previewId`: `?Id`<br>`expectedRevision`: `?Revision`<br>`olderThanMs`: `Counter`<br>`maxItems`: `Limit` | `PrunePreview` |
| `maintenance.prune_index_runs` | **Command**<br>`previewOnly`: `bool`<br>`previewId`: `?Id`<br>`expectedRevision`: `?Revision`<br>`olderThanMs`: `Counter`<br>`maxItems`: `Limit` | `PrunePreview` |
| `maintenance.prune_messages` | **Command**<br>`previewOnly`: `bool`<br>`previewId`: `?Id`<br>`expectedRevision`: `?Revision`<br>`olderThanMs`: `Counter`<br>`maxItems`: `Limit` | `PrunePreview` |
| `maintenance.prune_sessions` | **Command**<br>`previewOnly`: `bool`<br>`previewId`: `?Id`<br>`expectedRevision`: `?Revision`<br>`olderThanMs`: `Counter`<br>`maxItems`: `Limit` | `PrunePreview` |

### Repository Docs

**Owner:** DocumentationService. Source/version-bound generated repository docs with scoped writes and ordinary router/approval/completion rules.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `repository_docs.generate` | **Command**<br>`repositoryId`: `Id`<br>`views`: `ViewSelection`<br>`expectedRevision`: `Revision`<br>`outputRootId`: `Id`<br>`topics`: `NonEmpty<Label>` | `OperationRef` |

### Mcp Server

**Owner:** McpServerAdapter and ConfigurationService. Incoming MCP adapter uses shared application services. Internal human configuration only; not automatically MCP-exposed. stdio/loopback HTTP, current principal/grant, explicit scope, read-default policy, receipts and publication revocation apply. Protocol/session/client labels never mint authority.

| Operation | Mode and input fields | Typed result value |
| --- | --- | --- |
| `mcp_server.status` | **Query**<br>No caller fields | `McpInboundStatus` |
| `mcp_server.configure` | **Command**<br>`expectedRevision`: `Revision`<br>`enabled`: `bool`<br>`bindings`: `Items<McpServerBinding>`<br>`httpPort`: `?Port` | `McpInboundStatus` |
| `mcp_server.clients` | **Query**<br>`page`: `PageRequest` | `Page<McpClientAccess>` |
| `mcp_server.set_client_access` | **Command**<br>`action`: `McpClientAccessAction` | `McpClientAccess` |

## Domain types

Reference names reused from earlier shared/provider/data contracts retain their existing schema definitions; they are not redeclared here. Other types below are closed records, enumerations or `kind`-tagged unions. Generic lists/pages and scalar bounds are native checked support types. A field marked *revision N+* was introduced at schema revision N: callers that negotiated an earlier revision never receive it and may not send it.

### LifeState

`Active`, `Archived`, `Deleted`.

### WorkKind

`Feature`, `Issue`, `Task`.

### AccessMode

`Full`, `Project`.

### ApprovalState

`Pending`, `Approved`, `Rejected`, `Superseded`, `Expired`.

### ReviewState

`Pending`, `Running`, `ChangesRequired`, `Ready`, `Blocked`.

### TrustLevel

`Trusted`, `Untrusted`, `Inferred`, `Unresolved`.

### ItemState

`Enabled`, `Disabled`, `Unavailable`, `Partial`, `Healthy`, `Failed`.

### UsagePriceKind

`Reported`, `Estimated`, `Free`, `Untracked`.

### Severity

`Info`, `Warning`, `Error`.

### DiagnosticOutcome

`Running`, `Succeeded`, `Partial`, `Stale`, `Cancelled`, `Retryable`, `Failed`.

### DiagnosticErrorClass

`Transient`, `Timeout`, `Cancelled`, `Configuration`, `Authentication`, `Permission`, `NotFound`, `InvalidInput`, `ResourceExhausted`, `Dependency`, `Corruption`, `Internal`, `Unknown`.

### HealthState

`Healthy`, `Degraded`, `Failed`, `Unavailable`.

### DiagnosticOrigin

`Host`, `Reported`.

### MetricAvailability

`Available`, `Unavailable`.

### TierName

`Max`, `High`, `Mid`, `Low`.

### EffortName

`Default`, `Minimal`, `Low`, `Medium`, `High`, `XHigh`, `Max`.

### DbEngine

`Sqlite`, `MariaDb`, `MySql`, `Postgres`, `SqlServer`.

### JobKind

`Native`, `Inference`.

### ProposalDecision

`Approve`, `Reject`.

### GitActionKind

`Create`, `Attach`, `Inspect`.

### LeaseState

`Waiting`, `Held`, `Releasing`, `Uncertain`, `Released`.

### ResourcePriority

`Low`, `Medium`, `High`, `Critical`.

### ResourceScope

`Machine`, `Project`.

### LeaseEnd

`Released`, `Expired`, `Cancelled`, `Reconciled`.

### DeliveryMode

`ReadOnly`, `WriteEnabled`.

### PriceOrigin

`OfficialSnapshot`, `ProviderReported`, `UserConfigured`, `LocalFree`, `Untracked`.

### DecisionQuestionType

`choice`, `noul`, `score`, `box`.

### AccountAction

`BeginSignIn`, `CompleteSignIn`, `Refresh`, `Disconnect`, `Inspect`.

### EndpointKind

`OpenAiResponses`, `CodexSubscriptionResponses`, `AnthropicMessages`, `ClaudeSubscriptionMessages`, `OpenRouterChat`, `CompatibleChat`, `Embedding`, `SystemOne`.

### ToolProfile

`VerifiedRead`, `Full`, `Project`, `Confined`.

### PruneKind

`Diagnostics`, `IndexRuns`, `Messages`, `Sessions`.

### QueueResolution

`Retry`, `Reconcile`, `AcknowledgePoison`, `CancelBeforeDispatch`.

### UpdateChannel

`Stable`, `Prerelease`.

### ReleasePolicy

`Automatic`, `AskMe`, `Disabled`.

### SortDirection

`Ascending`, `Descending`.

### MetadataObjectKind

`Catalog`, `Schema`, `Table`, `View`, `Column`, `Index`, `Constraint`, `Procedure`, `Type`, `GraphEntity`, `GraphEdge`.

### KnowledgeKind

`Fact`, `Decision`, `Lesson`, `Constraint`, `Finding`, `Proposal`.

### KnowledgeState

`Active`, `Challenged`, `Verified`, `Retracted`.

### MessageClass

`Instruction`, `Status`, `Question`, `Evidence`.

### PageRequest

| Field | Type |
| --- | --- |
| `limit` | `Limit` |
| `cursor` | `?Cursor` |

### RevisionedRef

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |

### RegisteredPath

| Field | Type |
| --- | --- |
| `rootId` | `Id` |
| `relative` | `RelativePath` |

### FileSource

| Field | Type |
| --- | --- |
| `artifact` | `ArtifactRef` |

### OperationRef

| Field | Type |
| --- | --- |
| `operationId` | `OperationId` |
| `state` | `OperationState` |

### MutationSummary

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `changed` | `bool` |

### PlanApproval

| Field | Type |
| --- | --- |
| `proposalId` | `Id` |
| `planRevision` | `Revision` |
| `planDigest` | `Digest` |
| `state` | `ApprovalState` |

### ProjectRecord

| Field | Type |
| --- | --- |
| `id` | `ProjectId` |
| `revision` | `Revision` |
| `name` | `Label` |
| `description` | `Content` |
| `repositoryIds` | `Items<Id>` |
| `lifecycle` | `LifeState` |
| `accessMode` | `AccessMode` |

### WorkRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `projectId` | `ProjectId` |
| `kind` | `WorkKind` |
| `title` | `Label` |
| `description` | `Content` |
| `statusId` | `Id` |
| `priority` | `i32` |
| `repositoryIds` | `Items<Id>` |
| `parentId` | `?Id` |
| `plan` | `?ArtifactRef` |
| `approval` | `?PlanApproval` |
| `dependencyIds` | `Items<Id>` |
| `assignedAgentId` | `?Id` |
| `callerRef` | `?Id` |
| `goals` | `Items<Content>` |
| `requirements` | `Items<RequirementItem>` |
| `acceptance` | `Items<AcceptanceCriterion>` |
| `inheritsRequirementIds` | `Items<Id>` |
| `touches` | `Items<TouchRef>` |
| `links` | `Items<WorkLink>` |
| `comments` | `Rows<WorkComment>` |
| `handoffs` | `Rows<HandoffRecord>` |
| `attempts` | `Rows<WorkAttempt>` |
| `claim` | `?ClaimRecord` |
| `mergedAwaitingAcceptance` | `bool` |
| `acceptanceReport` | `?ArtifactRef` |
| `detailCursor` | `?Cursor` |
| `detailCoverage` | `Coverage` |
| `commentCount` | `Counter` |
| `handoffCount` | `Counter` |
| `attemptCount` | `Counter` |
| `pendingPlan` | `?PendingFeaturePlan` |
| `dependentIds` | `Items<Id>` |
| `manualOrder` | `?Counter` |
| `githubIssue` | `?GithubIssueTracking` |
| `readiness` | `?ReadinessAssessment` · *revision 2+* |
| `queueRevision` | `?Revision` · *revision 2+* |
| `workflowRevision` | `?Revision` · *revision 2+* |
| `acceptanceTests` | `?Items<AcceptanceCriterion>` · *revision 2+* |
| `pendingReadiness` | `?ReadinessAssessment` · *revision 2+* |

### PlanDraft

| Field | Type |
| --- | --- |
| `content` | `ArtifactRef` |
| `acceptance` | `Items<AcceptanceCriterion>` |
| `repositoryIds` | `Items<Id>` |
| `goals` | `Items<Content>` |
| `requirements` | `Items<RequirementItem>` |
| `acceptanceTests` | `Items<AcceptanceCriterion>` |

### AcceptanceCriterion

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `text` | `Content` |
| `evidenceIds` | `Items<Id>` |
| `state` | `CriterionState` |

### ProjectPatch

| Field | Type |
| --- | --- |
| `name` | `?Label` |
| `description` | `?Content` |
| `accessMode` | `?AccessMode` |

### DependencyEdge

| Field | Type |
| --- | --- |
| `predecessor` | `Id` |
| `successor` | `Id` |

### DependencyReplacement

| Field | Type |
| --- | --- |
| `edges` | `Items<DependencyEdge>` |

### EvidenceLink

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `targetId` | `Id` |
| `artifact` | `ArtifactRef` |
| `summary` | `Content` |
| `actorId` | `?ActorId` · *revision 2+* |
| `occurredAtMs` | `?Counter` · *revision 2+* |
| `repositoryId` | `?Id` · *revision 2+* |
| `revision` | `?Text` · *revision 2+* |

### StatusRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `label` | `Label` |
| `category` | `Label` |
| `rank` | `Counter` |

### ClaimRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `workId` | `Id` |
| `ownerId` | `ActorId` |
| `fence` | `Counter` |
| `expiresAtMs` | `Counter` |
| `state` | `LeaseState` |

### LineageRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `parentId` | `?Id` |
| `kind` | `Label` |
| `relatedId` | `Id` |
| `summary` | `Content` |
| `actorId` | `?ActorId` · *revision 2+* |
| `occurredAtMs` | `?Counter` · *revision 2+* |
| `repositoryId` | `?Id` · *revision 2+* |
| `revision` | `?Text` · *revision 2+* |

### AgentRecord

| Field | Type |
| --- | --- |
| `assignmentId` | `AssignmentId` |
| `rootId` | `RootId` |
| `revision` | `Revision` |
| `roleId` | `Id` |
| `state` | `OperationState` |
| `checkpoint` | `?ArtifactRef` |
| `blockers` | `Items<Id>` |

### AgentMessage

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `fromAssignmentId` | `?AssignmentId` |
| `toAssignmentId` | `AssignmentId` |
| `messageClass` | `MessageClass` |
| `content` | `Content` |
| `artifactRefs` | `Items<ArtifactRef>` |
| `acknowledged` | `bool` |

### CheckpointRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `assignmentId` | `AssignmentId` |
| `transcriptSequence` | `Counter` |
| `content` | `ArtifactRef` |

### SessionRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `title` | `Label` |
| `rootId` | `?RootId` |
| `contextEpoch` | `Counter` |
| `closed` | `bool` |
| `createdAtMs` | `?Counter` · *revision 2+* |
| `lastActivityMs` | `?Counter` · *revision 2+* |

### ContextSource

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `kind` | `Label` |
| `sourceRevision` | `Revision` |
| `artifact` | `?ArtifactRef` |
| `tokenEstimate` | `Counter` |

### ScreenContext

| Field | Type |
| --- | --- |
| `screenId` | `Id` |
| `projectId` | `?ProjectId` |
| `rootId` | `?RootId` |
| `selectedIds` | `Items<Id>` |
| `helpTopicId` | `Id` |
| `revision` | `Revision` |

### RepositoryRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `name` | `Label` |
| `remoteUrl` | `?Url` |
| `projectIds` | `Items<ProjectId>` |
| `primary` | `bool` |
| `lifecycle` | `LifeState` |
| `coverage` | `Coverage` |
| `indexedViewIds` | `Items<Id>` |

### RepositoryText

| Field | Type |
| --- | --- |
| `repositoryId` | `Id` |
| `viewId` | `Id` |
| `content` | `ArtifactRef` |
| `coverage` | `Coverage` |

### IndexRunRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `repositoryIds` | `Items<Id>` |
| `stage` | `Label` |
| `state` | `OperationState` |
| `completedUnits` | `Counter` |
| `totalUnits` | `?Counter` |
| `outputViews` | `Items<Id>` |
| `diagnostics` | `Items<Diagnostic>` |

### IndexSourceConfig

| Field | Type |
| --- | --- |
| `structuralLanguages` | `Items<Label>` |
| `semanticPackageIds` | `Items<Id>` |
| `excludePatterns` | `Items<Text>` |
| `maxFileBytes` | `Counter` |

### AnalysisRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `views` | `ViewSelection` |
| `kind` | `Label` |
| `state` | `OperationState` |
| `report` | `?ArtifactRef` |
| `coverage` | `Coverage` |

### ClusterRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `viewId` | `Id` |
| `name` | `Label` |
| `memberNodeIds` | `Items<Id>` |
| `score` | `Finite` |

### MetricRecord

| Field | Type |
| --- | --- |
| `name` | `Label` |
| `value` | `Finite` |
| `unit` | `Label` |
| `sourceId` | `Id` |
| `inferred` | `bool` |

### SchemaConnection

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `name` | `Label` |
| `engine` | `DbEngine` |
| `credentialRef` | `?Id` |
| `state` | `ItemState` |
| `currentViewId` | `?Id` |
| `writesEnabled` | `bool` |
| `metadataOnly` | `bool` |

### ConnectionConfig

| Field | Type |
| --- | --- |
| `name` | `Label` |
| `engine` | `DbEngine` |
| `endpoint` | `Text` |
| `credentialRef` | `?Id` |
| `catalogs` | `Items<Label>` |
| `schemas` | `Items<Label>` |
| `includePatterns` | `Items<Text>` |
| `excludePatterns` | `Items<Text>` |
| `writesEnabled` | `bool` |

### SchemaObject

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `viewId` | `Id` |
| `kind` | `MetadataObjectKind` |
| `qualifiedName` | `Text` |
| `definition` | `?ArtifactRef` |
| `parentId` | `?Id` |
| `coverage` | `Coverage` |

### SchemaRelation

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `viewId` | `Id` |
| `fromObjectId` | `Id` |
| `toObjectId` | `Id` |
| `relation` | `Label` |
| `inferred` | `bool` |

### DbScalar

| Kind | Fields |
| --- | --- |
| `Null` | No caller fields |
| `Boolean` | `value`: `bool` |
| `Integer` | `value`: `SignedCounter` |
| `Decimal` | `value`: `Text` |
| `Text` | `value`: `Content` |
| `Bytes` | `value`: `ArtifactRef` |

### DbParameter

| Field | Type |
| --- | --- |
| `name` | `Label` |
| `value` | `DbScalar` |

### DbColumn

| Field | Type |
| --- | --- |
| `name` | `Label` |
| `dataType` | `Label` |
| `sensitive` | `bool` |

### DbRow

| Field | Type |
| --- | --- |
| `cells` | `Items<DbScalar>` |

### DbReadResult

| Field | Type |
| --- | --- |
| `columns` | `Items<DbColumn>` |
| `rows` | `Rows<DbRow>` |
| `truncated` | `bool` |
| `nextCursor` | `?Cursor` |
| `sourceRevision` | `?Revision` |

### DbWriteResult

| Field | Type |
| --- | --- |
| `connectionId` | `Id` |
| `affectedRows` | `?Counter` |
| `transactionId` | `?Id` |
| `state` | `OperationState` |
| `diagnostics` | `Items<Diagnostic>` |

### SourceEvidence

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `claimId` | `?Id` |
| `source` | `EvidenceLocator` |
| `snippet` | `Content` |
| `metadata` | `?SchemaValue` |
| `sourceRevision` | `?Text` · *revision 2+* |

### KnowledgeEntity

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `label` | `Label` |
| `entityType` | `Label` |
| `aliases` | `Items<Label>` |
| `scope` | `DataScope` |
| `canonicalName` | `?Label` · *revision 2+* |
| `externalId` | `?Text` · *revision 2+* |
| `source` | `?Label` · *revision 2+* |
| `authorId` | `?ActorId` · *revision 2+* |
| `createdAtMs` | `?Counter` · *revision 2+* |
| `updatedAtMs` | `?Counter` · *revision 2+* |
| `deleted` | `?bool` · *revision 2+* |

### KnowledgeClaim

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `kind` | `KnowledgeKind` |
| `subjectId` | `Id` |
| `predicate` | `Label` |
| `objectId` | `?Id` |
| `value` | `?Content` |
| `confidence` | `Probability` |
| `state` | `KnowledgeState` |
| `evidenceIds` | `Items<Id>` |
| `tags` | `Items<Label>` |
| `scope` | `DataScope` |
| `valueJson` | `?DynamicJson` |
| `normalizedText` | `?Content` |
| `effectiveAtMs` | `?Counter` |
| `supersedes` | `?Id` |
| `source` | `?Label` |
| `entryKind` | `?KnowledgeEntryKind` · *revision 2+* |
| `lifecycle` | `?KnowledgeLifecycle` · *revision 2+* |
| `origin` | `?KnowledgeOrigin` · *revision 2+* |
| `originRef` | `?Text` · *revision 2+* |
| `autonomous` | `?bool` · *revision 2+* |
| `authorId` | `?ActorId` · *revision 2+* |
| `sessionId` | `?Id` · *revision 2+* |
| `createdAtMs` | `?Counter` · *revision 2+* |
| `updatedAtMs` | `?Counter` · *revision 2+* |
| `verifiedAtMs` | `?Counter` · *revision 2+* |
| `verifiedSourceRevision` | `?Text` · *revision 2+* |
| `stale` | `?bool` · *revision 2+* |
| `supersededBy` | `?Id` · *revision 2+* |
| `lesson` | `?LessonRecord` · *revision 2+* |
| `challenges` | `?Items<LessonChallenge>` · *revision 3+* |

### ClaimDraft

| Field | Type |
| --- | --- |
| `kind` | `KnowledgeKind` |
| `subjectId` | `Id` |
| `predicate` | `Label` |
| `objectId` | `?Id` |
| `value` | `?Content` |
| `confidence` | `Probability` |
| `evidenceIds` | `Items<Id>` |
| `tags` | `Items<Label>` |
| `valueJson` | `?DynamicJson` |
| `normalizedText` | `?Content` |
| `effectiveAtMs` | `?Counter` |
| `supersedes` | `?Id` |
| `source` | `?Label` |
| `entryKind` | `?KnowledgeEntryKind` · *revision 2+* |
| `origin` | `?KnowledgeOrigin` · *revision 2+* |
| `originRef` | `?Text` · *revision 2+* |
| `lesson` | `?LessonDraft` · *revision 2+* |
| `sourceRevision` | `?Text` · *revision 2+* |

### KnowledgeBatch

| Field | Type |
| --- | --- |
| `entities` | `Items<KnowledgeEntityDraft>` |
| `claims` | `Items<ClaimDraft>` |
| `evidence` | `Items<SourceEvidence>` |

### KnowledgeEntityDraft

| Field | Type |
| --- | --- |
| `clientId` | `Id` |
| `label` | `Label` |
| `entityType` | `Label` |
| `aliases` | `Items<Label>` |
| `canonicalName` | `?Label` |
| `externalId` | `?Text` |
| `source` | `?Label` |

### KnowledgeBundle

| Field | Type |
| --- | --- |
| `entities` | `Rows<KnowledgeEntity>` |
| `claims` | `Rows<KnowledgeClaim>` |
| `evidence` | `Rows<SourceEvidence>` |
| `frontier` | `Items<Id>` |
| `nextCursor` | `?Cursor` |
| `truncated` | `bool` |
| `conflicts` | `Rows<KnowledgeConflict>` |
| `supersessionIds` | `Items<Id>` |
| `summary` | `?Content` |
| `relations` | `?Rows<KnowledgeRelation>` · *revision 2+* |
| `revisions` | `?Rows<KnowledgeRevisionEntry>` · *revision 2+* |
| `scopeRevision` | `?Revision` · *revision 2+* |

### KnowledgeQuery

| Field | Type |
| --- | --- |
| `entityIds` | `Items<Id>` |
| `claimIds` | `Items<Id>` |
| `predicates` | `Items<Label>` |
| `tags` | `Items<Label>` |
| `states` | `Items<KnowledgeState>` |
| `text` | `?Text` |
| `maxDepth` | `Depth` |
| `page` | `PageRequest` |
| `entityLimit` | `Limit` |
| `claimLimit` | `Limit` |
| `evidenceLimit` | `Limit` |
| `neighborLimit` | `Limit` |
| `includeSuperseded` | `bool` |
| `includeConflicts` | `bool` |
| `includeSupersessionChain` | `bool` |
| `includeEvidence` | `bool` |
| `confidenceMin` | `?Probability` |

### KnowledgeWriteReceipt

| Field | Type |
| --- | --- |
| `writeId` | `Id` |
| `state` | `OperationState` |
| `entityIds` | `Items<Id>` |
| `claimIds` | `Items<Id>` |
| `revision` | `Revision` |
| `diagnostics` | `Items<Diagnostic>` |
| `entityCount` | `?Counter` · *revision 2+* |
| `claimCount` | `?Counter` · *revision 2+* |
| `nextCursor` | `?Cursor` · *revision 2+* |

### DreamRun

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `scope` | `DataScope` |
| `state` | `OperationState` |
| `checkpoint` | `?ArtifactRef` |
| `report` | `?ArtifactRef` |
| `proposalIds` | `Items<Id>` |

### DreamConfig

| Field | Type |
| --- | --- |
| `enabled` | `bool` |
| `instructions` | `ArtifactRef` |
| `scheduleId` | `?Id` |
| `batchLimit` | `Limit` |

### DocumentRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `scope` | `DataScope` |
| `title` | `Label` |
| `currentVersion` | `ArtifactRef` |
| `retainedVersions` | `Rows<ArtifactRef>` |
| `retrievalEnabled` | `bool` |
| `extractionState` | `ItemState` |
| `coverage` | `Coverage` |

### InstructionRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `name` | `Label` |
| `scope` | `DataScope` |
| `content` | `ArtifactRef` |
| `sourcePath` | `?RegisteredPath` |
| `approved` | `bool` |
| `applicability` | `Items<Label>` |

### ProposalRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `contentDigest` | `Digest` |
| `state` | `ApprovalState` |
| `operationKind` | `Label` |
| `targetId` | `Id` |
| `targetRevision` | `Revision` |
| `summary` | `Content` |
| `artifact` | `?ArtifactRef` |

### NotificationRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `title` | `Label` |
| `body` | `Content` |
| `decisionId` | `?Id` |
| `rootId` | `?RootId` |
| `acknowledged` | `bool` |
| `createdAtMs` | `Counter` |

### NotificationConfig

| Field | Type |
| --- | --- |
| `desktopEnabled` | `bool` |
| `kinds` | `Items<Label>` |

### ReviewRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `rootId` | `RootId` |
| `state` | `ReviewState` |
| `barrier` | `?ArtifactRef` |
| `findingIds` | `Items<Id>` |
| `headDigest` | `Digest` |
| `sources` | `Items<ReviewSourceEvidence>` |

### ReviewFinding

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `reviewId` | `Id` |
| `summary` | `Content` |
| `location` | `?Text` |
| `severity` | `Severity` |
| `actionable` | `bool` |
| `response` | `?Content` |
| `evidence` | `Items<ArtifactRef>` |

### ReviewReadiness

| Field | Type |
| --- | --- |
| `ready` | `bool` |
| `reviewIds` | `Items<Id>` |
| `blockerIds` | `Items<Id>` |
| `headDigest` | `Digest` |
| `policyRevision` | `Revision` |

### GitInspection

| Field | Type |
| --- | --- |
| `repositoryId` | `Id` |
| `worktreeId` | `?Id` |
| `head` | `GitOid` |
| `branch` | `?Text` |
| `dirty` | `bool` |
| `state` | `OperationState` |
| `evidence` | `Items<ArtifactRef>` |

### GitLink

| Field | Type |
| --- | --- |
| `repositoryId` | `Id` |
| `workId` | `Id` |
| `worktreeId` | `?Id` |
| `branch` | `?Text` |
| `pullRequestUrl` | `?Url` |
| `head` | `GitOid` |

### AttributedGitLink

| Field | Type |
| --- | --- |
| `repositoryId` | `Id` |
| `workId` | `Id` |
| `worktreeId` | `?Id` |
| `branch` | `?Text` |
| `pullRequestUrl` | `?Url` |
| `head` | `GitOid` |
| `actorId` | `?ActorId` · *revision 2+* |
| `occurredAtMs` | `?Counter` · *revision 2+* |
| `worktreePath` | `?Text` · *revision 3+* |
| `worktreeRemoved` | `?bool` · *revision 3+* |
| `agentSessionId` | `?Id` · *revision 3+* |
| `repositoryUrl` | `?Url` · *revision 3+* |

### ReleaseRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `rootId` | `RootId` |
| `state` | `OperationState` |
| `version` | `Label` |
| `artifactIds` | `Items<Id>` |
| `releaseUrl` | `?Url` |
| `deploymentState` | `OperationState` |

### GithubAccount

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `login` | `Label` |
| `state` | `ItemState` |
| `permissionRevision` | `Revision` |

### GithubObservation

| Field | Type |
| --- | --- |
| `repositoryId` | `Id` |
| `subjectId` | `Id` |
| `kind` | `Label` |
| `state` | `Label` |
| `revision` | `Text` |
| `url` | `?Url` |
| `artifact` | `?ArtifactRef` |

### ResourceRecord

| Field | Type |
| --- | --- |
| `id` | `ResourceId` |
| `canonicalKey` | `Text` |
| `revision` | `Revision` |
| `capacity` | `Limit` |
| `holders` | `Items<Id>` |
| `waiters` | `Items<Id>` |
| `scope` | `?ResourceScope` · *revision 2+* |
| `projectId` | `?ProjectId` · *revision 2+* |
| `physical` | `?bool` · *revision 2+* |
| `labels` | `?Items<Label>` · *revision 2+* |
| `used` | `?Counter` · *revision 2+* |
| `available` | `?bool` · *revision 2+* |

### LeaseRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `resourceId` | `ResourceId` |
| `ownerId` | `ActorId` |
| `fence` | `Counter` |
| `expiresAtMs` | `Counter` |
| `state` | `LeaseState` |
| `units` | `?Limit` · *revision 2+* |
| `purpose` | `?Text` · *revision 2+* |
| `priority` | `?ResourcePriority` · *revision 2+* |
| `queuePosition` | `?Counter` · *revision 2+* |
| `requestedAtMs` | `?Counter` · *revision 2+* |
| `grantedAtMs` | `?Counter` · *revision 2+* |
| `renewedAtMs` | `?Counter` · *revision 2+* |
| `endedAtMs` | `?Counter` · *revision 2+* |
| `endReason` | `?LeaseEnd` · *revision 2+* |
| `ownerUnreachableSinceMs` | `?Counter` · *revision 2+* |
| `projectId` | `?ProjectId` · *revision 2+* |
| `rootId` | `?RootId` · *revision 2+* |
| `assignmentId` | `?AssignmentId` · *revision 2+* |
| `sessionId` | `?Id` · *revision 2+* |

### BrowserSession

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `state` | `ItemState` |
| `targetIds` | `Items<Id>` |
| `processOwnerId` | `Id` |
| `testedRepository` | `?TestedRepositoryRevision` · *revision 3+* |

### BrowserTarget

| Field | Type |
| --- | --- |
| `sessionId` | `Id` |
| `targetId` | `Id` |
| `frameId` | `Id` |
| `documentRevision` | `Revision` |

### BrowserNode

| Field | Type |
| --- | --- |
| `target` | `BrowserTarget` |
| `nodeId` | `Id` |
| `snapshotRevision` | `Revision` |

### BrowserInput

| Kind | Fields |
| --- | --- |
| `Click` | `node`: `BrowserNode`<br>`button`: `MouseButton` |
| `Text` | `node`: `BrowserNode`<br>`text`: `Content` |
| `Key` | `target`: `BrowserTarget`<br>`key`: `Label`<br>`modifiers`: `Items<KeyModifier>` |
| `Scroll` | `target`: `BrowserTarget`<br>`x`: `i32`<br>`y`: `i32` |

### MouseButton

`Primary`, `Secondary`, `Middle`.

### KeyModifier

`Shift`, `Control`, `Alt`, `Meta`.

### BrowserSnapshot

| Field | Type |
| --- | --- |
| `target` | `BrowserTarget` |
| `url` | `Text` |
| `title` | `Label` |
| `accessibility` | `ArtifactRef` |
| `screenshot` | `?ArtifactRef` |

### BrowserObservation

| Field | Type |
| --- | --- |
| `sessionId` | `Id` |
| `sequence` | `Counter` |
| `evidence` | `ArtifactRef` |
| `truncated` | `bool` |
| `testedRepository` | `?TestedRepositoryRevision` · *revision 3+* |

### DesktopRecord

| Field | Type |
| --- | --- |
| `desktopId` | `Id` |
| `revision` | `Revision` |
| `state` | `ItemState` |
| `permissionState` | `Label` |
| `leaseId` | `?Id` |
| `consentId` | `?Id` |

### DesktopCapture

| Field | Type |
| --- | --- |
| `desktop` | `DesktopRecord` |
| `image` | `ArtifactRef` |
| `capturedAtMs` | `Counter` |

### DesktopInput

| Kind | Fields |
| --- | --- |
| `Click` | `x`: `i32`<br>`y`: `i32`<br>`button`: `MouseButton` |
| `Text` | `text`: `Content` |
| `Key` | `key`: `Label`<br>`modifiers`: `Items<KeyModifier>` |
| `Scroll` | `x`: `i32`<br>`y`: `i32` |

### EnvironmentEntry

| Field | Type |
| --- | --- |
| `name` | `Label` |
| `value` | `?Text` |
| `credentialRef` | `?Id` |

### ProcessSpec

| Field | Type |
| --- | --- |
| `executableRef` | `Id` |
| `args` | `Items<Text>` |
| `workingDirectory` | `RegisteredPath` |
| `environment` | `Items<EnvironmentEntry>` |
| `profile` | `ToolProfile` |
| `timeoutMs` | `Counter` |
| `outputLimitBytes` | `Counter` |

### ProcessRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `state` | `OperationState` |
| `ownerId` | `Id` |
| `exitCode` | `?i32` |
| `output` | `?ArtifactRef` |
| `truncated` | `bool` |

### WebResult

| Field | Type |
| --- | --- |
| `title` | `Label` |
| `url` | `Url` |
| `snippet` | `Content` |
| `observedAtMs` | `Counter` |
| `trust` | `?TrustLevel` · *revision 2+* |
| `providerId` | `?Id` · *revision 2+* |

### WebRedirect

| Field | Type |
| --- | --- |
| `url` | `Url` |
| `status` | `i32` |

### WebDocument

| Field | Type |
| --- | --- |
| `url` | `Url` |
| `finalUrl` | `Url` |
| `content` | `ArtifactRef` |
| `mediaType` | `Label` |
| `observedAtMs` | `Counter` |
| `truncated` | `bool` |
| `trust` | `?TrustLevel` · *revision 2+* |
| `status` | `?i32` · *revision 2+* |
| `redirects` | `?Items<WebRedirect>` · *revision 2+* |
| `sourceSha256` | `?Digest` · *revision 2+* |
| `sourceByteLength` | `?Counter` · *revision 2+* |

### ContainerCapability

| Field | Type |
| --- | --- |
| `engineId` | `Id` |
| `state` | `ItemState` |
| `capabilities` | `Items<Label>` |
| `diagnostics` | `Items<Diagnostic>` |

### ContainerSpec

| Field | Type |
| --- | --- |
| `engineId` | `Id` |
| `projectId` | `ProjectId` |
| `image` | `Text` |
| `command` | `Items<Text>` |
| `mounts` | `Items<ContainerMount>` |
| `ports` | `Items<PortMapping>` |
| `environment` | `Items<EnvironmentEntry>` |
| `resourceIds` | `Items<ResourceId>` |

### ContainerMount

| Field | Type |
| --- | --- |
| `source` | `RegisteredPath` |
| `destination` | `Text` |
| `readOnly` | `bool` |

### PortMapping

| Field | Type |
| --- | --- |
| `containerPort` | `Port` |
| `hostPort` | `Port` |
| `protocol` | `PortProtocol` |

### PortProtocol

`Tcp`, `Udp`.

### ContainerRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `engineId` | `Id` |
| `owned` | `bool` |
| `state` | `ItemState` |
| `image` | `Text` |
| `processOwnerId` | `?Id` |

### Schedule

| Kind | Fields |
| --- | --- |
| `Manual` | No caller fields |
| `Interval` | `seconds`: `PositiveCounter` |
| `Calendar` | `expression`: `Text`<br>`timezone`: `Label` |

### JobDefinition

| Kind | Fields |
| --- | --- |
| `Native` | `capabilityId`: `Id`<br>`arguments`: `SchemaValue` |
| `Inference` | `prompt`: `ArtifactRef`<br>`route`: `RouteIntent`<br>`effort`: `EffortName`<br>`projectId`: `?ProjectId`<br>`featureId`: `?RootId` |

### JobRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `title` | `Label` |
| `schedule` | `Schedule` |
| `definition` | `JobDefinition` |
| `enabled` | `bool` |
| `interruptedRunId` | `?Id` |
| `lastRunId` | `?Id` |
| `scope` | `?DataScope` · *revision 2+* |
| `nextRunAtMs` | `?Counter` · *revision 2+* |
| `lastRunStartedAtMs` | `?Counter` · *revision 2+* |
| `lastRunFinishedAtMs` | `?Counter` · *revision 2+* |
| `lastRunStatus` | `?JobRunStatus` · *revision 2+* |
| `lastError` | `?Text` · *revision 2+* |
| `activeRunId` | `?Id` · *revision 2+* |
| `hold` | `?JobHold` · *revision 2+* |
| `authorized` | `?bool` · *revision 2+* |
| `createdAtMs` | `?Counter` · *revision 2+* |
| `updatedAtMs` | `?Counter` · *revision 2+* |
| `deletedAtMs` | `?Counter` · *revision 2+* |

### JobRun

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `jobId` | `Id` |
| `revision` | `Revision` |
| `occurrenceKey` | `Text` |
| `state` | `OperationState` |
| `operationId` | `?OperationId` |
| `usage` | `UsageReport` |
| `status` | `?JobRunStatus` · *revision 2+* |
| `trigger` | `?JobTrigger` · *revision 2+* |
| `occurrenceAtMs` | `?Counter` · *revision 2+* |
| `startedAtMs` | `?Counter` · *revision 2+* |
| `finishedAtMs` | `?Counter` · *revision 2+* |
| `reason` | `?Text` · *revision 2+* |
| `hold` | `?Label` · *revision 2+* |
| `cancelRequested` | `?bool` · *revision 2+* |
| `resumptions` | `?Counter` · *revision 2+* |
| `sessionId` | `?Id` · *revision 2+* |
| `linkedRootIds` | `?Items<RootId>` · *revision 2+* |
| `skippedForRunId` | `?Id` · *revision 2+* |

### SchemaValue

| Field | Type |
| --- | --- |
| `schema` | `SchemaRef` |
| `value` | `DynamicJson` |

### SettingEntry

| Field | Type |
| --- | --- |
| `key` | `Label` |
| `value` | `SchemaValue` |
| `sourceScope` | `DataScope` |
| `revision` | `Revision` |

### ValidationIssue

| Field | Type |
| --- | --- |
| `path` | `Text` |
| `code` | `Label` |
| `message` | `Content` |

### ValidationReport

| Field | Type |
| --- | --- |
| `valid` | `bool` |
| `issues` | `Rows<ValidationIssue>` |

### RouteIntent

| Kind | Fields |
| --- | --- |
| `Tier` | `tier`: `TierName` |
| `Model` | `modelId`: `Id` |
| `OpenRouterAuto` | No caller fields |

### RouteModel

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `displayName` | `Label` |
| `endpointProfile` | `EndpointKind` |
| `upstreamModelId` | `Text` |
| `enabled` | `bool` |
| `tierMemberships` | `Items<TierName>` |
| `efforts` | `Items<EffortName>` |
| `imageInput` | `bool` |
| `tools` | `bool` |
| `revision` | `Revision` |
| `contextTokens` | `?PositiveCounter` · *revision 2+* |
| `connectionId` | `?Id` · *revision 2+* |

### ConnectionRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `name` | `Label` |
| `profile` | `EndpointKind` |
| `state` | `ItemState` |
| `credentialRef` | `?Id` |
| `subscriptionRiskAccepted` | `bool` |
| `riskNoticeSha256` | `?Digest` |
| `endpoint` | `?Url` · *revision 2+* |
| `lifecycle` | `?ConnectionLifecycle` · *revision 2+* |
| `stateReason` | `?Text` · *revision 2+* |
| `credentialRevision` | `?Counter` · *revision 2+* |
| `healthy` | `?bool` · *revision 2+* |
| `checkedAtMs` | `?Counter` · *revision 2+* |
| `discoveredAtMs` | `?Counter` · *revision 2+* |
| `discoveredModelCount` | `?Counter` · *revision 2+* |
| `discoveredModelIds` | `?Items<Text>` · *revision 2+* |
| `accessExpiresAtMs` | `?Counter` · *revision 2+* |
| `signIn` | `?SignInChallenge` · *revision 2+* |
| `enabled` | `?bool` · *revision 2+* |
| `openRouter` | `?OpenRouterConstraints` · *revision 2+* |

### RouteChoice

| Field | Type |
| --- | --- |
| `modelId` | `Id` |
| `connectionId` | `Id` |
| `intent` | `RouteIntent` |
| `requestedEffort` | `EffortName` |
| `effectiveEffort` | `EffortName` |
| `pinId` | `Id` |
| `pinRevision` | `Revision` |
| `replacedPinId` | `?Id` |

### RouteAvailability

| Field | Type |
| --- | --- |
| `intent` | `RouteIntent` |
| `available` | `bool` |
| `eligibleModelIds` | `Items<Id>` |
| `blockedReasonCodes` | `Items<Label>` |
| `retryAtMs` | `?Counter` |
| `pin` | `?RouteChoice` · *revision 2+* |
| `pinEligible` | `?bool` · *revision 2+* |
| `pinReason` | `?Label` · *revision 2+* |

### UsageItem

| Field | Type |
| --- | --- |
| `requestId` | `Id` |
| `purpose` | `Label` |
| `modelId` | `Id` |
| `inputTokens` | `Counter` |
| `outputTokens` | `Counter` |
| `cacheReadTokens` | `Counter` |
| `cacheWriteTokens` | `Counter` |
| `amountUsd` | `?Money` |
| `priceKind` | `UsagePriceKind` |
| `priceOrigin` | `PriceOrigin` |
| `priceRevision` | `?Revision` |
| `incomplete` | `bool` |

### ToolInvocationOutcome

`Completed`, `Failed`, `ApprovalRequired`, `DependencyBlocked`, `CancelledBeforeDispatch`, `OutcomeUnknown`.

### ToolInvocation

| Field | Type |
| --- | --- |
| `sequence` | `Revision` |
| `callId` | `Id` |
| `sessionId` | `Id` |
| `toolName` | `Label` |
| `outcome` | `ToolInvocationOutcome` |
| `durationMs` | `?Counter` |
| `errorCode` | `?Label` |
| `operationId` | `?OperationId` |
| `actorId` | `ActorId` |
| `occurredAtMs` | `Counter` |

### UsagePage

| Field | Type |
| --- | --- |
| `items` | `Rows<UsageItem>` |
| `nextCursor` | `?Cursor` |
| `hasMore` | `bool` |
| `checkpoint` | `?Cursor` · *revision 2+* |
| `itemSequences` | `?Rows<Revision>` · *revision 2+* |
| `toolInvocations` | `?Rows<ToolInvocation>` · *revision 2+* |

### UsageReport

| Field | Type |
| --- | --- |
| `items` | `Rows<UsageItem>` |
| `totalTrackedUsd` | `Money` |
| `untrackedRequests` | `Counter` |
| `incompleteRequests` | `Counter` |
| `nextCursor` | `?Cursor` |

### PriceRecord

| Field | Type |
| --- | --- |
| `modelId` | `Id` |
| `revision` | `Revision` |
| `inputUsdPerMillion` | `Money` |
| `outputUsdPerMillion` | `Money` |
| `cacheReadUsdPerMillion` | `?Money` |
| `cacheWriteUsdPerMillion` | `?Money` |
| `origin` | `PriceOrigin` |
| `observedAtMs` | `Counter` |
| `sourceUrl` | `?Url` |

### DecisionModel

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `displayName` | `Label` |
| `connectionId` | `Id` |
| `upstreamModelId` | `Text` |
| `questionTypes` | `Items<DecisionQuestionType>` |
| `acceptsImages` | `bool` |
| `acceptsVideos` | `bool` |
| `enabled` | `bool` |
| `extensionQuestionSchemas` | `Items<SchemaRef>` |
| `resultSchema` | `SchemaRef` |

### DecisionCriterion

| Field | Type |
| --- | --- |
| `name` | `Label` |
| `description` | `?Content` |

### DecisionQuestion

| Kind | Fields |
| --- | --- |
| `Choice` | `instructions`: `Content`<br>`criteria`: `NonEmpty<DecisionCriterion>` |
| `Noul` | `instructions`: `Content` |
| `Score` | `instructions`: `Content`<br>`criteria`: `NonEmpty<Label>` |
| `Box` | `instructions`: `Content` |
| `Extension` | `schema`: `SchemaRef`<br>`payload`: `DynamicJson` |

### NamedQuestion

| Field | Type |
| --- | --- |
| `name` | `Label` |
| `question` | `DecisionQuestion` |

### MediaRef

| Kind | Fields |
| --- | --- |
| `Artifact` | `artifact`: `ArtifactRef` |
| `Remote` | `url`: `Url`<br>`mediaType`: `Label` |

### DecisionResult

| Field | Type |
| --- | --- |
| `modelId` | `Id` |
| `requestId` | `Id` |
| `output` | `SchemaValue` |
| `rawResponse` | `ArtifactRef` |
| `usage` | `UsageReport` |

### McpServer

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `name` | `Label` |
| `state` | `ItemState` |
| `transport` | `Label` |
| `selectedToolIds` | `Items<Id>` |
| `catalogRevision` | `Revision` |

### McpTransport

| Kind | Fields |
| --- | --- |
| `Stdio` | `process`: `ProcessSpec` |
| `Http` | `endpoint`: `Url`<br>`credentialRef`: `?Id` |

### McpCapability

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `serverId` | `Id` |
| `kind` | `Label` |
| `name` | `Label` |
| `description` | `Content` |
| `inputSchema` | `?SchemaRef` |
| `outputSchema` | `?SchemaRef` |
| `readOnly` | `bool` |
| `revision` | `Revision` |

### McpResource

| Field | Type |
| --- | --- |
| `serverId` | `Id` |
| `uri` | `Text` |
| `content` | `ArtifactRef` |
| `catalogRevision` | `Revision` |

### McpPrompt

| Field | Type |
| --- | --- |
| `serverId` | `Id` |
| `name` | `Label` |
| `messages` | `ArtifactRef` |
| `catalogRevision` | `Revision` |

### PackageRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `version` | `Label` |
| `digest` | `Digest` |
| `revision` | `Revision` |
| `publisherId` | `?Id` |
| `enabled` | `bool` |
| `trustState` | `Label` |
| `contributionIds` | `Items<Id>` |
| `registryRevision` | `?Revision` · *revision 2+* |
| `availability` | `?PackageAvailability` · *revision 2+* |
| `availabilityReason` | `?Text` · *revision 2+* |
| `packageKind` | `?Label` · *revision 2+* |
| `languages` | `?Items<Label>` · *revision 2+* |
| `targets` | `?Items<Label>` · *revision 2+* |
| `hostTargetSupported` | `?bool` · *revision 2+* |
| `prerequisites` | `?Items<PackagePrerequisite>` · *revision 2+* |
| `requestedAccess` | `?Items<Label>` · *revision 2+* |
| `effectiveAccess` | `?Items<Label>` · *revision 2+* |
| `configurationRevision` | `?Revision` · *revision 2+* |
| `configurationSchema` | `?SchemaRef` · *revision 2+* |
| `stateGeneration` | `?Counter` · *revision 2+* |
| `pendingVersion` | `?Label` · *revision 2+* |
| `pendingDigest` | `?Digest` · *revision 2+* |
| `pendingWaits` | `?Items<PackageWaitReason>` · *revision 2+* |
| `pendingProposalId` | `?Id` · *revision 2+* |
| `predecessorVersion` | `?Label` · *revision 2+* |
| `predecessorDigest` | `?Digest` · *revision 2+* |
| `publisherTrusted` | `?bool` · *revision 2+* |
| `signerKey` | `?Label` · *revision 2+* |
| `failureCode` | `?Label` · *revision 2+* |
| `contributions` | `?Rows<Label>` · *revision 2+* |
| `pendingBundle` | `?ArtifactRef` · *revision 2+* |
| `available` | `?Items<PackageOffer>` · *revision 2+* |
| `installed` | `?bool` · *revision 2+* |
| `disabledProjects` | `?Items<Id>` · *revision 3+* |
| `configuration` | `?SchemaValue` · *revision 3+* |

### HelpTopic

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `version` | `Label` |
| `title` | `Label` |
| `content` | `ArtifactRef` |
| `relatedCapabilityIds` | `Items<Id>` |

### DiagnosticMetric

| Field | Type |
| --- | --- |
| `name` | `Label` |
| `availability` | `MetricAvailability` |
| `value` | `?Counter` |
| `unit` | `?Label` |
| `reason` | `?Text` |

### RecoveryAction

| Field | Type |
| --- | --- |
| `operation` | `Label` |
| `label` | `Text` |
| `targetKind` | `?Label` |
| `targetId` | `?Id` |

### Diagnostic

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `severity` | `Severity` |
| `component` | `Label` |
| `code` | `Label` |
| `message` | `Content` |
| `correlationId` | `?CorrelationId` |
| `evidence` | `?ArtifactRef` |

### DiagnosticEntry

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `severity` | `Severity` |
| `component` | `Label` |
| `code` | `Label` |
| `message` | `Content` |
| `correlationId` | `?CorrelationId` |
| `evidence` | `?ArtifactRef` |
| `sequence` | `?Counter` · *revision 2+* |
| `recordedAtMs` | `?Counter` · *revision 2+* |
| `actorId` | `?ActorId` · *revision 2+* |
| `origin` | `?DiagnosticOrigin` · *revision 2+* |
| `projectId` | `?ProjectId` · *revision 2+* |
| `operation` | `?Label` · *revision 2+* |
| `operationId` | `?OperationId` · *revision 2+* |
| `targetKind` | `?Label` · *revision 2+* |
| `targetId` | `?Id` · *revision 2+* |
| `stage` | `?Label` · *revision 2+* |
| `sourceRevision` | `?Label` · *revision 2+* |
| `attempt` | `?Counter` · *revision 2+* |
| `startedAtMs` | `?Counter` · *revision 2+* |
| `finishedAtMs` | `?Counter` · *revision 2+* |
| `progressCompleted` | `?Counter` · *revision 2+* |
| `progressTotal` | `?Counter` · *revision 2+* |
| `outcome` | `?DiagnosticOutcome` · *revision 2+* |
| `errorClass` | `?DiagnosticErrorClass` · *revision 2+* |
| `health` | `?HealthState` · *revision 2+* |
| `output` | `?Content` · *revision 2+* |
| `outputOmittedBytes` | `?Counter` · *revision 2+* |
| `redactions` | `?Counter` · *revision 2+* |
| `metrics` | `?Items<DiagnosticMetric>` · *revision 2+* |
| `recovery` | `?Items<RecoveryAction>` · *revision 2+* |

### DiagnosticPage

| Field | Type |
| --- | --- |
| `items` | `Rows<DiagnosticEntry>` |
| `nextCursor` | `?Cursor` |
| `hasMore` | `bool` |
| `latestSequence` | `?Counter` · *revision 2+* |

### BackupRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `manifest` | `ArtifactRef` |
| `schemaVersion` | `Revision` |
| `createdAtMs` | `Counter` |
| `state` | `OperationState` |

### UpdateRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `version` | `Label` |
| `channel` | `UpdateChannel` |
| `digest` | `Digest` |
| `signatureVerified` | `bool` |
| `state` | `OperationState` |
| `migrationRequired` | `bool` |

### QueueDelivery

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `eventId` | `EventId` |
| `consumerId` | `Id` |
| `revision` | `Revision` |
| `state` | `OperationState` |
| `attempt` | `Counter` |
| `fence` | `Counter` |
| `leaseUntilMs` | `?Counter` |
| `error` | `?Label` |

### ImportRun

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `sourceSnapshot` | `Digest` |
| `state` | `OperationState` |
| `importedIds` | `Items<Id>` |
| `report` | `ArtifactRef` |

### ImportConfig

| Field | Type |
| --- | --- |
| `source` | `RegisteredPath` |
| `projectIds` | `Items<ProjectId>` |
| `workIds` | `Items<Id>` |
| `includeHistory` | `bool` |
| `includeGitLinks` | `bool` |

### PrunePreview

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `kind` | `PruneKind` |
| `candidateIds` | `Rows<RevisionedRef>` |
| `heldIds` | `Rows<Id>` |
| `expiresAtMs` | `Counter` |

### TaskPlanEntry

| Field | Type |
| --- | --- |
| `clientKey` | `Id` |
| `existingTaskId` | `?Id` |
| `expectedRevision` | `?Revision` |
| `title` | `Label` |
| `description` | `Content` |
| `repositoryIds` | `Items<Id>` |
| `dependencyKeys` | `Items<Id>` |
| `acceptance` | `Items<AcceptanceCriterion>` |
| `priority` | `?i32` |

### RouterConfigAction

| Kind | Fields |
| --- | --- |
| `Connection` | `connectionId`: `?Id`<br>`expectedRevision`: `?Revision`<br>`name`: `Label`<br>`profile`: `EndpointKind`<br>`endpoint`: `Url`<br>`credentialRef`: `?Id`<br>`enabled`: `?bool`<br>`openRouter`: `?OpenRouterConstraints` |
| `Model` | `model`: `RouteModel`<br>`expectedRevision`: `?Revision` |
| `Tier` | `tier`: `TierName`<br>`expectedRevision`: `Revision`<br>`memberIds`: `Items<Id>` |
| `DecisionModel` | `model`: `DecisionModel`<br>`expectedRevision`: `?Revision` |
| `SignIn` | `connectionId`: `Id`<br>`action`: `AccountAction`<br>`flowId`: `?Id` |
| `RemoveConnection` | `connectionId`: `Id`<br>`expectedRevision`: `Revision` |
| `SubscriptionRisk` | `connectionId`: `Id`<br>`expectedRevision`: `Revision`<br>`noticeSha256`: `Digest`<br>`accepted`: `bool` |

### RootSnapshot

| Field | Type |
| --- | --- |
| `root` | `RootControl` |
| `activation` | `?Activation` |
| `blockers` | `Blockers` |

### EligibilitySnapshot

| Field | Type |
| --- | --- |
| `target` | `EligibilityTarget` |
| `purpose` | `Purpose` |
| `eligible` | `bool` |
| `blockers` | `Blockers` |

### ReceiptObservation

| Field | Type |
| --- | --- |
| `requestKey` | `RequestKey` |
| `commandId` | `?CommandId` |
| `committedSequence` | `?Revision` |
| `operationId` | `?OperationId` |
| `replyArtifact` | `?ArtifactRef` |
| `replySchema` | `?SchemaRef` |
| `state` | `ReceiptLookupState` |

### ReceiptLookupState

`NotFound`, `Recorded`, `StatusUnknown`.

### SettingAssignment

| Field | Type |
| --- | --- |
| `key` | `Label` |
| `value` | `SchemaValue` |

### PriceInput

| Field | Type |
| --- | --- |
| `inputUsdPerMillion` | `Money` |
| `outputUsdPerMillion` | `Money` |
| `cacheReadUsdPerMillion` | `?Money` |
| `cacheWriteUsdPerMillion` | `?Money` |

### MessageDirection

`Inbound`, `Outbound`, `Both`.

### JobInspection

| Field | Type |
| --- | --- |
| `job` | `JobRecord` |
| `run` | `?JobRun` |
| `runs` | `?Rows<JobRun>` · *revision 2+* |
| `runsNextCursor` | `?Cursor` · *revision 2+* |

### RepositoryIndexStatus

| Field | Type |
| --- | --- |
| `repositoryId` | `Id` |
| `currentViewIds` | `Items<Id>` |
| `coverage` | `Coverage` |
| `lastRun` | `?IndexRunRecord` |
| `freshness` | `?IndexFreshness` · *revision 2+* |
| `languages` | `?Items<LanguageCoverage>` · *revision 2+* |
| `semanticPacks` | `?Items<SemanticPackStatus>` · *revision 2+* |
| `overlays` | `?Items<OverlayStatus>` · *revision 2+* |

### ConsumerStatus

| Field | Type |
| --- | --- |
| `consumerId` | `Id` |
| `revision` | `Revision` |
| `cursor` | `Counter` |
| `pendingCount` | `Counter` |
| `leasedCount` | `Counter` |
| `poisonCount` | `Counter` |
| `retryAtMs` | `?Counter` |

### BrowserSessionAction

| Kind | Fields |
| --- | --- |
| `Create` | `scope`: `DataScope`<br>`browserAssetId`: `Id` |
| `Close` | `sessionId`: `Id`<br>`expectedRevision`: `Revision` |

### GithubAccountState

| Field | Type |
| --- | --- |
| `account` | `?GithubAccount` |
| `authFlowId` | `?Id` |
| `state` | `ItemState` |

### EvidenceKind

`Conversation`, `Document`, `Code`, `Observation`, `External`.

### EvidenceLocator

| Field | Type |
| --- | --- |
| `kind` | `EvidenceKind` |
| `sourceRef` | `Text` |
| `artifact` | `?ArtifactRef` |
| `locator` | `?Text` |

### KnowledgeConflict

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `claimIds` | `NonEmpty<Id>` |
| `state` | `Label` |
| `explanation` | `Content` |

### RequirementStatus

`Confirmed`, `Proposed`, `Dropped`.

### CriterionState

`Unmet`, `Met`.

### RequirementItem

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `statement` | `Content` |
| `status` | `RequirementStatus` |
| `dropDecision` | `?ArtifactRef` |
| `coveredByTaskIds` | `Items<Id>` |

### WorkComment

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `content` | `Content` |
| `evidence` | `Items<ArtifactRef>` |

### HandoffRecord

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `summary` | `Content` |
| `nextSteps` | `Content` |
| `evidence` | `Items<ArtifactRef>` |

### WorkAttempt

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `assignmentId` | `?AssignmentId` |
| `state` | `OperationState` |
| `gitLinks` | `Items<GitLink>` |
| `checkpoint` | `?ArtifactRef` |

### WorkLink

| Field | Type |
| --- | --- |
| `kind` | `Label` |
| `targetId` | `?Id` |
| `url` | `?Url` |
| `artifact` | `?ArtifactRef` |

### TouchRef

| Field | Type |
| --- | --- |
| `repositoryId` | `Id` |
| `viewId` | `?Id` |
| `nodeId` | `?Id` |
| `path` | `?RelativePath` |

### FeaturePlanDraft

| Field | Type |
| --- | --- |
| `callerRef` | `Id` |
| `existingFeatureId` | `?RootId` |
| `expectedRevision` | `?Revision` |
| `title` | `Label` |
| `description` | `Content` |
| `repositoryIds` | `Items<Id>` |
| `plan` | `PlanDraft` |
| `tasks` | `PlanItems<TaskPlanEntry>` |

### AtomicWorkPlan

| Field | Type |
| --- | --- |
| `callerRef` | `Id` |
| `features` | `PlanFeatures<FeaturePlanDraft>` |
| `dependencyEdges` | `Rows<DependencyEdge>` |

### FeatureCreation

| Kind | Fields |
| --- | --- |
| `Single` | `title`: `Label`<br>`description`: `Content`<br>`repositoryIds`: `Items<Id>`<br>`statusId`: `Id`<br>`priority`: `i32`<br>`plan`: `?PlanDraft` |
| `AtomicPlan` | `plan`: `AtomicWorkPlan` |
| `StagedPlan` | `artifact`: `ArtifactRef` |

### BrowserLocation

| Kind | Fields |
| --- | --- |
| `Remote` | `url`: `Url` |
| `Artifact` | `artifact`: `ArtifactRef` |
| `Blank` | No caller fields |

### ContainerImage

| Field | Type |
| --- | --- |
| `engineId` | `Id` |
| `image` | `Text` |
| `digest` | `?Text` |
| `state` | `ItemState` |
| `owned` | `bool` |

### DetailLevel

`Summary`, `Full`.

### ProcessOutput

| Field | Type |
| --- | --- |
| `processId` | `Id` |
| `revision` | `Revision` |
| `state` | `OperationState` |
| `firstByte` | `Counter` |
| `nextByte` | `Counter` |
| `content` | `Content` |
| `encoding` | `OutputEncoding` |
| `truncated` | `bool` |
| `complete` | `bool` |

### OutputEncoding

`Utf8`, `Base64`.

### DreamSettingsRecord

| Field | Type |
| --- | --- |
| `scope` | `DataScope` |
| `revision` | `Revision` |
| `config` | `DreamConfig` |

### NotificationSettingsRecord

| Field | Type |
| --- | --- |
| `revision` | `Revision` |
| `config` | `NotificationConfig` |

### ModelPriceStatus

| Field | Type |
| --- | --- |
| `modelId` | `Id` |
| `priceKind` | `UsagePriceKind` |
| `price` | `?PriceRecord` |
| `stale` | `bool` |
| `nextCheckAtMs` | `?Counter` |

### ArtifactRecord

| Field | Type |
| --- | --- |
| `artifact` | `ArtifactRef` |
| `name` | `Label` |
| `createdAtMs` | `Counter` |
| `scope` | `DataScope` |
| `referenceCount` | `Counter` |
| `tags` | `?ArtifactTags` · *revision 2+* |
| `testedRepository` | `?TestedRepositoryRevision` · *revision 4+* |

### ArtifactReadContent

| Kind | Fields |
| --- | --- |
| `Text` | `artifact`: `ArtifactRef`<br>`firstByte`: `Counter`<br>`nextByte`: `Counter`<br>`content`: `Content`<br>`truncated`: `bool`<br>`testedRepository`: `?TestedRepositoryRevision` |
| `Binary` | `artifact`: `ArtifactRef`<br>`streamId`: `Id`<br>`firstByte`: `Counter`<br>`byteLength`: `Counter`<br>`expiresAtMs`: `Counter`<br>`testedRepository`: `?TestedRepositoryRevision` |

### ArtifactPreview

| Field | Type |
| --- | --- |
| `artifact` | `ArtifactRef` |
| `preview` | `ArtifactRef` |
| `page` | `?PositiveCounter` |
| `coverage` | `Coverage` |

### ArtifactExport

| Field | Type |
| --- | --- |
| `artifact` | `ArtifactRef` |
| `streamId` | `Id` |
| `expiresAtMs` | `Counter` |
| `suggestedName` | `Label` |

### FeaturePatch

| Field | Type |
| --- | --- |
| `title` | `?Label` |
| `description` | `?Content` |
| `statusId` | `?Id` |
| `repositoryIds` | `?Items<Id>` |
| `goals` | `?Items<Content>` |
| `addLinks` | `?Items<WorkLink>` |
| `removeLinkIds` | `?Items<Id>` |
| `addComment` | `?WorkComment` |
| `readiness` | `?ReadinessChange` · *revision 2+* |
| `acceptanceEvidence` | `?NonEmpty<AcceptanceCriterion>` · *revision 2+* |

### IssuePatch

| Field | Type |
| --- | --- |
| `title` | `?Label` |
| `description` | `?Content` |
| `statusId` | `?Id` |
| `repositoryIds` | `?Items<Id>` |
| `goals` | `?Items<Content>` |
| `acceptance` | `?Items<AcceptanceCriterion>` |
| `touches` | `?Items<TouchRef>` |
| `addLinks` | `?Items<WorkLink>` |
| `removeLinkIds` | `?Items<Id>` |
| `addComment` | `?WorkComment` |
| `handoff` | `?HandoffRecord` |
| `acceptanceReport` | `?ArtifactRef` |
| `priority` | `?i32` |

### TaskPatch

| Field | Type |
| --- | --- |
| `title` | `?Label` |
| `description` | `?Content` |
| `statusId` | `?Id` |
| `repositoryIds` | `?Items<Id>` |
| `goals` | `?Items<Content>` |
| `acceptance` | `?Items<AcceptanceCriterion>` |
| `inheritsRequirementIds` | `?Items<Id>` |
| `touches` | `?Items<TouchRef>` |
| `addLinks` | `?Items<WorkLink>` |
| `removeLinkIds` | `?Items<Id>` |
| `addComment` | `?WorkComment` |
| `handoff` | `?HandoffRecord` |
| `acceptanceReport` | `?ArtifactRef` |
| `priority` | `?i32` |

### McpServerBinding

`Stdio`, `LoopbackHttp`.

### McpInboundSettings

| Field | Type |
| --- | --- |
| `revision` | `Revision` |
| `enabled` | `bool` |
| `bindings` | `Items<McpServerBinding>` |
| `httpPort` | `?Port` |

### McpInboundStatus

| Field | Type |
| --- | --- |
| `settings` | `McpInboundSettings` |
| `state` | `ItemState` |
| `endpoint` | `?Url` |
| `protocolVersions` | `Items<Label>` |
| `publicationRevision` | `Revision` |
| `diagnostic` | `?Text` |

### McpClientAccess

| Field | Type |
| --- | --- |
| `clientId` | `Id` |
| `revision` | `Revision` |
| `name` | `Label` |
| `enabled` | `bool` |
| `credentialGeneration` | `Revision` |
| `projectIds` | `Items<ProjectId>` |
| `repositoryIds` | `Items<Id>` |
| `worktreeIds` | `Items<Id>` |
| `readOperations` | `Items<Label>` |
| `commandOperations` | `Items<Label>` |

### McpClientAccessAction

| Kind | Fields |
| --- | --- |
| `Register` | `name`: `Label` |
| `Replace` | `clientId`: `Id`<br>`expectedRevision`: `Revision`<br>`name`: `Label`<br>`enabled`: `bool`<br>`projectIds`: `Items<ProjectId>`<br>`repositoryIds`: `Items<Id>`<br>`worktreeIds`: `Items<Id>`<br>`readOperations`: `Items<Label>`<br>`commandOperations`: `Items<Label>` |
| `Revoke` | `clientId`: `Id`<br>`expectedRevision`: `Revision` |
| `RotateCredential` | `clientId`: `Id`<br>`expectedRevision`: `Revision` |

### PendingFeaturePlan

| Field | Type |
| --- | --- |
| `revision` | `Revision` |
| `draft` | `PlanDraft` |
| `approval` | `?PlanApproval` |

### GithubIssueIntakeState

`Queued`, `Triaging`, `AwaitingDecision`, `Working`, `Resolved`, `Closed`.

### GithubIssueTracking

| Field | Type |
| --- | --- |
| `repositoryId` | `Id` |
| `remoteIssueNumber` | `PositiveCounter` |
| `url` | `Url` |
| `remoteRevision` | `Text` |
| `remoteState` | `Label` |
| `intakeState` | `GithubIssueIntakeState` |
| `nextAdmissionAt` | `?Counter` |
| `resolutionOperationId` | `?Id` |

### ReviewSourceKind

`GitHubAutomation`, `InAppIndependent`, `InAppFallback`.

### ReviewObservationState

`Requested`, `Running`, `Completed`, `Failed`, `Unavailable`, `TimedOut`, `Stale`.

### ReviewSourceEvidence

| Field | Type |
| --- | --- |
| `sourceId` | `Id` |
| `sourceKind` | `ReviewSourceKind` |
| `repositoryId` | `Id` |
| `pullRequestNumber` | `PositiveCounter` |
| `head` | `GitOid` |
| `headDigest` | `Digest` |
| `policyRevision` | `Revision` |
| `cycle` | `Counter` |
| `runId` | `?Id` |
| `state` | `ReviewObservationState` |
| `completedAt` | `?Counter` |
| `findingCount` | `Counter` |
| `zeroFindingsConfirmed` | `bool` |
| `findingsCoverage` | `Coverage` |
| `evidence` | `Items<ArtifactRef>` |

### PackageAvailability

`NotInstalled`, `Disabled`, `MissingDependency`, `Incompatible`, `Ready`, `Failed`.

### PackageWaitReason

`TrustApproval`, `ExactVersionApproval`, `AccessApproval`, `Compatibility`, `Quiescence`.

### PackagePrerequisite

| Field | Type |
| --- | --- |
| `kind` | `Label` |
| `id` | `Label` |
| `requirement` | `Label` |
| `required` | `bool` |
| `satisfied` | `bool` |
| `state` | `?PrerequisiteState` · *revision 2+* |
| `detectedVersion` | `?Label` · *revision 2+* |

### PrerequisiteState

`Missing`, `Incompatible`, `Ready`.

### PackageOffer

| Field | Type |
| --- | --- |
| `version` | `Label` |
| `manifestDigest` | `Digest` |
| `bundleDigest` | `Digest` |
| `bundleBytes` | `Counter` |
| `publisherId` | `Id` |
| `source` | `Label` |
| `hostTargetSupported` | `bool` |
| `newer` | `bool` |

### IndexFreshnessState

`Current`, `Stale`, `Indexing`, `NotIndexed`, `Failed`.

### IndexFreshness

| Field | Type |
| --- | --- |
| `state` | `IndexFreshnessState` |
| `indexedCommit` | `?Label` |
| `headCommit` | `?Label` |
| `headRef` | `?Label` |
| `indexedAtMs` | `?Counter` |
| `observedAtMs` | `?Counter` |
| `configurationCurrent` | `bool` |
| `reasons` | `Items<Label>` |

### LanguageCoverageMode

`Structural`, `TextFallback`, `NotSelected`.

### LanguageCoverage

| Field | Type |
| --- | --- |
| `languageId` | `Label` |
| `name` | `Label` |
| `mode` | `LanguageCoverageMode` |
| `grammar` | `?Label` |
| `files` | `Counter` |
| `structuralFiles` | `Counter` |
| `textFiles` | `Counter` |
| `parseErrorFiles` | `Counter` |
| `skippedFiles` | `Counter` |
| `note` | `?Content` |

### SemanticPackState

`NotInstalled`, `Unavailable`, `Enabled`, `Applied`, `Disabled`.

### SemanticPackStatus

| Field | Type |
| --- | --- |
| `packId` | `Id` |
| `state` | `SemanticPackState` |
| `languages` | `Items<Label>` |
| `note` | `?Content` |

### OverlayState

`Queued`, `Indexing`, `Ready`, `Failed`, `Stale`, `Expired`, `Unavailable`.

### OverlayStatus

| Field | Type |
| --- | --- |
| `overlayId` | `Id` |
| `revision` | `Revision` |
| `worktreeId` | `?Id` |
| `state` | `OverlayState` |
| `viewId` | `?Id` |
| `baseCommit` | `?Label` |
| `headCommit` | `?Label` |
| `headRef` | `?Label` |
| `captureDigest` | `?Label` |
| `runId` | `?Id` |
| `reason` | `?Content` |
| `updatedAtMs` | `Counter` |
| `expiresAtMs` | `Counter` |

### IndexRunState

`Queued`, `Running`, `Succeeded`, `Failed`, `Cancelled`.

### IndexRunKind

`Index`, `Reindex`, `Overlay`.

### OpenRouterDataCollection

`allow`, `deny`.

### OpenRouterQuantization

`int4`, `int8`, `fp4`, `mxfp4`, `nvfp4`, `fp6`, `fp8`, `mxfp8`, `fp16`, `bf16`, `fp32`, `unknown`.

### OpenRouterSortBy

`price`, `throughput`, `latency`.

### OpenRouterPartition

`model`, `none`.

### OpenRouterCostTier

`low`, `medium`, `high`, `xhigh`, `max`.

### OpenRouterSort

| Field | Type |
| --- | --- |
| `by` | `OpenRouterSortBy` |
| `partition` | `?OpenRouterPartition` |

### OpenRouterPercentiles

| Field | Type |
| --- | --- |
| `p50` | `?Finite` |
| `p75` | `?Finite` |
| `p90` | `?Finite` |
| `p99` | `?Finite` |

### OpenRouterMaxPrice

| Field | Type |
| --- | --- |
| `prompt` | `?Money` |
| `completion` | `?Money` |
| `request` | `?Money` |
| `image` | `?Money` |

### OpenRouterProviderRouting

| Field | Type |
| --- | --- |
| `order` | `?Items<Label>` |
| `allowFallbacks` | `?bool` |
| `requireParameters` | `?bool` |
| `dataCollection` | `?OpenRouterDataCollection` |
| `zdr` | `?bool` |
| `enforceDistillableText` | `?bool` |
| `only` | `?Items<Label>` |
| `ignore` | `?Items<Label>` |
| `quantizations` | `?Items<OpenRouterQuantization>` |
| `sort` | `?OpenRouterSort` |
| `preferredMinThroughput` | `?OpenRouterPercentiles` |
| `preferredMaxLatency` | `?OpenRouterPercentiles` |
| `maxPrice` | `?OpenRouterMaxPrice` |

### OpenRouterConstraints

| Field | Type |
| --- | --- |
| `allowedModels` | `Items<Text>` |
| `excludedModels` | `Items<Text>` |
| `costTier` | `?OpenRouterCostTier` |
| `provider` | `?OpenRouterProviderRouting` |

### ConnectionLifecycle

`Disconnected`, `Authorizing`, `Connected`, `Refreshing`, `ReauthenticationRequired`, `Unavailable`, `Disabled`.

### SignInMethod

`DeviceCode`, `AuthorizationCode`.

### SignInChallenge

| Field | Type |
| --- | --- |
| `flowId` | `Id` |
| `method` | `SignInMethod` |
| `userCode` | `?Label` |
| `verificationUrl` | `?Url` |
| `authorizeUrl` | `?Url` |
| `expiresAtMs` | `Counter` |

### KnowledgeEntryKind

`Goal`, `Decision`, `Constraint`, `Fact`, `Gotcha`, `Attempt`, `Architecture`, `Lesson`, `Finding`, `Proposal`.

### KnowledgeLifecycle

`Active`, `Superseded`, `Retracted`, `Deleted`.

### KnowledgeOrigin

`User`, `Agent`.

### LessonSource

`Prompt`, `Review`, `RelayedPrompt`, `RelayedReview`, `Agent`, `Subagent`, `BotReview`.

### RuleState

`Candidate`, `Proposed`, `Effective`, `Rejected`, `Retired`.

### RuleEffect

`UserInstruction`, `Recurrence`, `Promoted`.

### LessonAddressKind

`Code`, `Instruction`, `Skill`, `Documentation`, `Test`.

### ChallengeOutcome

`Uphold`, `Retire`.

### ChallengeState

`Open`, `Upheld`, `Retired`.

### PromoteAction

`Effective`, `Propose`, `Reject`, `Address`.

### LessonView

`All`, `RecentlyLearned`, `Recurring`, `Unaddressed`, `Undelivered`.

### Applicability

| Field | Type |
| --- | --- |
| `activities` | `Items<Label>` |
| `paths` | `Items<RelativePath>` |
| `tools` | `Items<Label>` |
| `harness` | `?Label` |
| `repositoryIds` | `Items<Id>` |

### LessonDraft

| Field | Type |
| --- | --- |
| `source` | `LessonSource` |
| `sourceRef` | `?Text` |
| `quotedWords` | `?Content` |
| `applicability` | `Applicability` |
| `sameAs` | `?Id` |

### LessonAddress

| Field | Type |
| --- | --- |
| `kind` | `LessonAddressKind` |
| `name` | `Label` |
| `reference` | `?Text` |

### LessonChallenge

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `state` | `ChallengeState` |
| `reason` | `Content` |
| `evidenceIds` | `Items<Id>` |
| `authorId` | `ActorId` |
| `openedAtMs` | `Counter` |
| `resolvedAtMs` | `?Counter` |
| `resolution` | `?Content` |

### LessonRecord

| Field | Type |
| --- | --- |
| `ruleState` | `RuleState` |
| `effect` | `?RuleEffect` |
| `effectiveAtMs` | `?Counter` |
| `source` | `LessonSource` |
| `sourceRef` | `?Text` |
| `quotedWords` | `?Content` |
| `applicability` | `Applicability` |
| `recurrence` | `Counter` |
| `recurrenceThreshold` | `Counter` |
| `challenges` | `Items<LessonChallenge>` |
| `addressedBy` | `Items<LessonAddress>` |
| `deliveredSessions` | `Counter` |
| `lastDeliveredAtMs` | `?Counter` |
| `reason` | `?Content` |

### KnowledgeRelation

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `subjectId` | `Id` |
| `predicate` | `Label` |
| `objectId` | `Id` |
| `evidenceIds` | `Items<Id>` |
| `authorId` | `ActorId` |
| `createdAtMs` | `Counter` |

### KnowledgeRevisionEntry

| Field | Type |
| --- | --- |
| `revision` | `Revision` |
| `change` | `Label` |
| `authorId` | `ActorId` |
| `atMs` | `Counter` |
| `lifecycle` | `KnowledgeLifecycle` |
| `writeId` | `?Id` |
| `value` | `?Content` |

### JobRunStatus

`Queued`, `Running`, `Holding`, `Succeeded`, `Failed`, `Skipped`, `Cancelled`, `OutcomeUnknown`.

### JobTrigger

`Scheduled`, `Manual`.

### JobHold

`ReactivationRequired`, `AuthorizationRequired`, `ProjectClosed`.

### ReadinessDisposition

`ResolvedDecision`, `BoundedInvestigation`, `NeedsUserDecision`.

### FindingState

`Open`, `Resolved`, `RiskAccepted`.

### ConfidenceLevel

`High`, `Medium`, `Low`.

### ReadinessFreshness

`Current`, `Stale`.

### ReadinessFinding

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `disposition` | `ReadinessDisposition` |
| `blocking` | `bool` |
| `prerequisite` | `bool` |
| `state` | `FindingState` |
| `requirementIds` | `Items<Id>` |
| `scopeKey` | `?Id` |
| `affectedWorkIds` | `Items<Id>` |
| `summary` | `Content` |
| `missing` | `Content` |
| `consequence` | `Content` |
| `attempts` | `Items<Content>` |
| `insufficiency` | `Content` |
| `closureCondition` | `Content` |
| `decision` | `?Content` |
| `confidence` | `?ConfidenceLevel` |
| `evidence` | `Items<ArtifactRef>` |
| `objective` | `?Content` |
| `limits` | `?Content` |
| `question` | `?Content` |
| `recommendation` | `?Content` |
| `closureNote` | `?Content` |
| `closureEvidence` | `Items<ArtifactRef>` |

### ReadinessAssessment

| Field | Type |
| --- | --- |
| `id` | `Id` |
| `revision` | `Revision` |
| `planRevision` | `Revision` |
| `planDigest` | `Digest` |
| `reviewedPending` | `bool` |
| `freshness` | `ReadinessFreshness` |
| `staleScopeKeys` | `Items<Id>` |
| `ready` | `bool` |
| `summary` | `Content` |
| `findings` | `Items<ReadinessFinding>` |
| `assessedBy` | `ActorId` |
| `assessedAtMs` | `Counter` |

### ReadinessChange

| Kind | Fields |
| --- | --- |
| `Assess` | `expectedAssessmentRevision`: `?Revision`<br>`planRevision`: `Revision`<br>`planDigest`: `Digest`<br>`summary`: `Content`<br>`findings`: `Items<ReadinessFinding>` |
| `CloseFinding` | `expectedAssessmentRevision`: `Revision`<br>`findingId`: `Id`<br>`note`: `Content`<br>`evidence`: `Items<ArtifactRef>` |

### QueuePlacement

| Field | Type |
| --- | --- |
| `workId` | `Id` |
| `anchorId` | `Id` |
| `side` | `QueueSide` |

### QueueSide

`Before`, `After`.

### TestedRepositoryRevision

| Field | Type |
| --- | --- |
| `repositoryId` | `Id` |
| `worktreeId` | `Id` |
| `head` | `GitOid` |
| `dirty` | `bool` |
| `ownershipRevision` | `Revision` |
| `observedAtMs` | `Counter` |
| `repositoryUrl` | `?Url` |

## Machine sources

- [Input/result/mode/authority/effect/recovery bindings](contracts/family-coverage.json)
- [Schema manifest](contracts/manifest.json)
- [Native/domain schema verification](validation/domain-contracts/schema-model-results.json)
- [Frozen source bindings](validation/domain-contracts/source-binding-results.json)
- [Local implementation ownership](implementation-plan.json)
