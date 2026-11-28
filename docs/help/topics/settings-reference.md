# Complete settings reference

This local reference is maintained against the app’s actual settings catalog. Workspace-only keys cannot be overridden by a project. Inherited keys resolve Workspace → Project; clearing a project override restores inheritance. Delivery keys also resolve through a repository of the project (Workspace → Project → Repository), and a repository’s value applies to delivery in that repository only; the mandatory review is a harness protection that no scope, a repository included, can change. Defaults are defaults, not evidence that a runtime is installed.

Adoption: Immediate applies on save; NextSafeStep waits for the next safe context boundary; NextAdmission affects later admitted work; NewClonesOnly affects new repository clones. HarnessEnforced values are visible policies, not editable relaxations. Agent proposals and current authority still apply.

Use [Settings](help:settings) for safe edits and [Recovery](help:recovery) for conflicts or uncertain saves. Values rendered as Rust expressions retain their exact catalog default; byte limits use bytes.

## repositories.baseDirectory

Repository base directory.

- Default: `"~/DevWorks/Repositories"`
- Scope: Workspace → Project
- Adoption: NewClonesOnly
- Policy: Configurable
- Section: General

Default destination for new repository clones. Changing it never moves existing repositories or changes their recorded paths. Separate from app data and worktree placement. Engineering default.

## routing.managedAuto

DevWorks Auto for new managed sessions.

- Default: `false`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: Routing

Classify each new managed assignment once before its first reply. Existing pins and automatic tiers are retained.

## routing.tierMax.members

Tier Max members.

- Default: `["fable-latest", "astra-latest"]`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: Routing

Configured models allocated round-robin for Tier Max.

## routing.tierHigh.members

Tier High members.

- Default: `["opus-latest", "sol-latest"]`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: Routing

Configured models allocated round-robin for Tier High.

## routing.tierMid.members

Tier Mid members.

- Default: `["sonnet-latest", "terra-latest"]`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: Routing

Configured models allocated round-robin for Tier Mid.

## routing.tierLow.members

Tier Low members.

- Default: `["haiku-latest", "luna-latest"]`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: Routing

Configured models allocated round-robin for Tier Low.

## tools.overrides

Selected agent tools.

- Default: `serde_json::Map::<String, Value>::new()`
- Scope: Workspace only
- Adoption: NextSafeStep
- Policy: Configurable
- Section: Tools

Explicit per-tool choices shared by agents. Missing tools use their origin's default; reviewer read-only rules and other safety controls still apply.

## tools.webSearch

Web search.

- Default: `true`
- Scope: Workspace only
- Adoption: NextSafeStep
- Policy: Configurable
- Section: Tools

Allow agents the shared web search tool (code review agents included). Searches go to the provider configured below.

## tools.webFetch

Web page fetching.

- Default: `true`
- Scope: Workspace only
- Adoption: NextSafeStep
- Policy: Configurable
- Section: Tools

Allow agents the shared web page and documentation fetch tool (code review agents included). Fetched pages are kept as untrusted artifacts.

## web.searchProvider

Web search provider.

- Default: `"none"`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: Tools

The external search service web search uses. None turns searching off: a search is refused as unsupported rather than answered with no results. Brave Search needs an API key, kept in the OS secret store.

## web.searchCredential

Web search API key.

- Default: `Value::Null`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: Tools

Reference to the search provider's API key in the OS secret store. The key itself is never stored in configuration, history or logs, and is sent only to the provider.

## web.fetchMaxRedirects

Fetch redirect limit.

- Default: `5`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: Tools

Most redirects one fetch follows. Every hop is checked against the network policy again and recorded. Five covers ordinary http-to-https, canonical-host and documentation-version redirects. Engineering default.

## web.timeoutMs

Web request time limit (ms).

- Default: `30_000`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: Tools

Longest a web fetch or search may take end to end, whatever the caller asks: name resolution, connection, TLS, every redirect and the body. Thirty seconds fits slow documentation hosts while keeping a stuck page from holding an agent. Engineering default.

## web.fetchAllowPrivateNetwork

Fetch from private networks.

- Default: `false`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: Tools

Allow fetching from loopback and private network addresses, for example a local documentation server. Off by default so a page or agent cannot reach services on this machine or network. Link-local and cloud metadata addresses stay blocked either way.

## packages.indexSources

Package index sources.

- Default: `Vec::<serde_json::Value>::new()`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: Tools

Signed publisher indexes the Add-ons screen lists available packs from: an https index URL or a local directory holding devworks-index.json, each with the publisher's Ed25519 public key (hex) the index must be signed with. Only a person applies a change (an agent's change is a proposal). None by default.

## packages.connections

Package connections.

- Default: `Vec::<serde_json::Value>::new()`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: Tools

Named connections add-ons reach over HTTPS: each has a name, the network endpoint an add-on must be granted, a base URL, and optionally a credential kept in the OS secret store and sent only in its header to that URL. Requests follow the web fetch network rules. None by default.

## tools.shell

Shell commands.

- Default: `true`
- Scope: Workspace only
- Adoption: NextSafeStep
- Policy: Configurable
- Section: Tools

Allow agents the authorized process execution tools.

## tools.processExecutables

Process executables.

- Default: `[ "git", "sh", "bash", "make", "cargo", "rustc", "node", "npm", "npx", "pnpm", "yarn", "deno", "bun", "bunx", "python3", "python", "uv", "uvx", "pipx", "go", "bundle", "composer", "java", "dotnet", "swift", "docker", "podman", "container" ]`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: Tools

Command names that process tools, project dependency installs, container backends and local MCP servers may launch. Each name is looked up on PATH when it is first used and runs from its resolved real file; a program not named here cannot be selected wherever it is installed. The default covers the version control, build, package and container tools DevWorks itself runs. Shells and language runtimes run arbitrary code: access rules, approvals and command checks still decide what runs.

## safety.accessProfile

Agent access.

- Default: `"full"`
- Scope: Workspace → Project
- Adoption: Immediate
- Policy: Configurable
- Section: Safety

Full access, the initial default, lets agents use their selected tools with your ordinary user rights; it is not an OS sandbox. Project scope keeps brokered work inside registered repositories and task worktrees and refuses native shell, package and MCP execution unless you grant a bounded exception, because no confined launcher is qualified. Reviewer restrictions, approvals, main-branch protection and the destructive-operation guard apply in both.

## safety.reviewersReadOnly

Code review agents are read-only.

- Default: `true`
- Scope: Workspace only
- Adoption: Immediate
- Policy: HarnessEnforced
- Section: Safety

Reviewers receive read tools only; write/edit and indirect mutations are always denied.

## safety.protectMainBranch

Main branch protection.

- Default: `true`
- Scope: Workspace only
- Adoption: Immediate
- Policy: HarnessEnforced
- Section: Safety

Never force-push main; feature branch force-push requests require approval.

## safety.destructiveOperationGuard

Destructive operation guard.

- Default: `true`
- Scope: Workspace only
- Adoption: Immediate
- Policy: HarnessEnforced
- Section: Safety

Check tool calls before dispatch to prevent whole-drive or repository deletion.

## github.credential

GitHub credential.

- Default: `Value::Null`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: GitHub

Reference to the GitHub credential in the OS secret store. The credential value itself is never stored in configuration.

## github.pollForChanges

Poll GitHub for changes.

- Default: `true`
- Scope: Workspace → Project
- Adoption: NextAdmission
- Policy: Configurable
- Section: GitHub

Pushes, merges, reviews and checks notify agents through authenticated polling.

## github.importOpenIssues

Import open issues.

- Default: `true`
- Scope: Workspace → Project
- Adoption: NextAdmission
- Policy: Configurable
- Section: GitHub

Existing open issues are imported when a repository is connected, and newly reported ones thereafter.

## github.syncLinkedIssueFields

Receive linked issue updates.

- Default: `true`
- Scope: Workspace → Project
- Adoption: NextAdmission
- Policy: Configurable
- Section: GitHub

Remote issue updates are reflected locally; intermediate execution details stay local and a resolution is published only after completed work is verified.

## github.issueTriageIntervalSeconds

Issue triage interval (seconds).

- Default: `1800`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: GitHub

The app-wide minimum interval between automatic issue triage admissions.

## delivery.autoMerge

Auto-Merge.

- Default: `true`
- Scope: Workspace → Project → Repository
- Adoption: NextAdmission
- Policy: Configurable
- Section: Delivery

Allow DevWorks to submit a merge after the required checks, review, approval and repository policies pass for the exact PR head. Disable this setting to hold automatic submission while other authorized work continues.

## delivery.mandatoryReview

Independent local review.

- Default: `true`
- Scope: Workspace only
- Adoption: Immediate
- Policy: HarnessEnforced
- Section: Delivery

Every feature task requires one independent review before integration.

## delivery.finalPullRequestReview

Require final pull request review.

- Default: `true`
- Scope: Workspace → Project → Repository
- Adoption: NextAdmission
- Policy: Configurable
- Section: Delivery

Require exact-head review evidence for final GitHub pull requests. Local feature task review always applies.

## delivery.maxReviewFixCycles

Review rounds.

- Default: `5`
- Scope: Workspace → Project → Repository
- Adoption: NextAdmission
- Policy: Configurable
- Section: Delivery

Set a lower limit for automated reviews. Feature pull requests allow up to five rounds, and standalone pull requests up to three.

## watch.backupEnabled

Watcher backup check.

- Default: `true`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: Agents

Periodically re-check every unfinished or unacknowledged watcher by a separate path, so an outcome whose first delivery was missed still reaches its agent, and an agent whose watcher stalled is reminded to check it. Engineering default.

## watch.backupIntervalMs

Watcher backup check interval (ms).

- Default: `900_000`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: Agents

How often the watcher backup check runs while some agent has an unfinished or unacknowledged watcher. Engineering default.

## watch.maxLifetimeMs

Polling watcher lifetime (ms).

- Default: `86_400_000`
- Scope: Workspace → Project
- Adoption: NextAdmission
- Policy: Configurable
- Section: Agents

Longest a polling watcher (a probe command) keeps checking before it ends TimedOut, unless the watcher sets its own. A day covers slow deploys and queues without leaving forgotten probes running. Engineering default.

## reviews.automationWaitSeconds

Automated review wait (seconds).

- Default: `900`
- Scope: Workspace → Project → Repository
- Adoption: NextAdmission
- Policy: Configurable
- Section: Delivery

After this time, DevWorks may use one independent in-app reviewer for the same round.

## reviews.githubBotId

GitHub review bot ID.

- Default: `0`
- Scope: Workspace → Project → Repository
- Adoption: NextAdmission
- Policy: Configurable
- Section: Delivery

The registered bot's numeric GitHub actor ID. Set to zero to use an independent in-app reviewer.

## agents.maxSubagents

Default maximum subagents.

- Default: `10`
- Scope: Workspace → Project
- Adoption: NextSafeStep
- Policy: Configurable
- Section: Agents

Applies independently per feature and issue coordinator.

## agents.compactionThresholdPercent

Context compaction threshold (%).

- Default: `80`
- Scope: Workspace only
- Adoption: NextSafeStep
- Policy: Configurable
- Section: Agents

Context usage at which conversations are compacted.

## containers.preferredRuntime

Preferred container runtime.

- Default: `"docker"`
- Scope: Workspace → Project
- Adoption: NextAdmission
- Policy: Configurable
- Section: Containers

Detect availability and capabilities; never silently switch to a different backend.

## knowledge.visionExtraction

Vision extraction fallback.

- Default: `true`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: Knowledge

Extracts existing text directly. An image-capable model can transcribe scanned PDF pages and eligible Word images. Turning this off keeps direct text available.

## knowledge.visionModels

Vision extraction models.

- Default: `Vec::<String>::new()`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: Knowledge

Choose model IDs in preference order for scanned documents. An empty list uses the session's eligible pinned model, then the configured Low pool.

## knowledge.lessonRecurrenceThreshold

Lesson recurrence threshold.

- Default: `3`
- Scope: Workspace → Project
- Adoption: NextAdmission
- Policy: Configurable
- Section: Knowledge

Distinct source events after which a lesson an agent or reviewer inferred takes effect as a rule, without approval. A rule from your own words takes effect at once. Engineering default.

## knowledge.searchTokenBudget

Knowledge search token budget.

- Default: `8000`
- Scope: Workspace → Project
- Adoption: NextAdmission
- Policy: Configurable
- Section: Knowledge

Approximate tokens of claim text (characters divided by 4) one Knowledge search page holds for an agent that names no budget. The best-ranked claims that fit are returned, always at least one, and the agent pages for more. Searching in the app has no token limit. Above the largest search token budget, that maximum applies. Engineering default.

## knowledge.searchTokenBudgetMax

Largest search token budget.

- Default: `32000`
- Scope: Workspace → Project
- Adoption: NextAdmission
- Policy: Configurable
- Section: Knowledge

The largest token budget any caller, a person or an agent, may name on one Knowledge search. A budget above it, or below 1000, is refused. Engineering default.

## power.keepAwake

Keep the computer awake while DevWorks is open.

- Default: `false`
- Scope: Workspace only
- Adoption: Immediate
- Policy: Configurable
- Section: Power

While DevWorks is open, ask the operating system not to sleep when idle, even with no active work; the display may still turn off. It does not prevent explicit sleep, closing the lid, shutdown or every power policy. Closing DevWorks or turning this off releases the request.

## storage.backupPolicy

Backups.

- Default: `"manual"`
- Scope: Workspace only
- Adoption: Immediate
- Policy: HarnessEnforced
- Section: Storage

Manual backup and restore only; there is no automatic backup.

## storage.artifactLocation

Artifact storage location.

- Default: `"applicationData"`
- Scope: Workspace only
- Adoption: Immediate
- Policy: HarnessEnforced
- Section: Storage

Artifact contents are managed files under the application data root (artifacts/, with in-flight files in staging/), outside repositories and disposable worktrees, with their metadata in the workspace store. Moving them is an explicit data-root relocation, never a side effect of a settings edit.

## storage.artifactRetention

Artifact retention.

- Default: `"untilExplicitDeletion"`
- Scope: Workspace only
- Adoption: Immediate
- Policy: HarnessEnforced
- Section: Storage

Artifacts are kept until they are explicitly deleted. Age, feature completion, worktree cleanup and storage pressure never remove them; only rebuildable previews are collected. Files still referenced elsewhere, being read or held by a backup stay until those references end.

## storage.artifactQuotaBytes

Artifact storage limit (bytes).

- Default: `0`
- Scope: Workspace → Project
- Adoption: NextAdmission
- Policy: Configurable
- Section: Storage

Bytes of artifact content a project may hold; 0 means no limit. Reaching it refuses new saves with a visible status and never deletes anything to make room. Engineering default.

## storage.artifactMaxFileBytes

Largest artifact file (bytes).

- Default: `67_108_864`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: Storage

Largest single file an artifact save or import reads. Larger sources are refused visibly. Engineering default.

## diagnostics.logRetentionDays

Diagnostic log retention (days).

- Default: `30`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: Storage

Diagnostic and log records older than this are removed by the hourly retention pass and at startup; records survive restarts until then. Thirty days keeps a month of history for recurring failures and post-incident review while the log stays bounded. Evidence artifacts a record names are kept under their own retention. Engineering default.

## diagnostics.logMaxRecords

Diagnostic log size (records).

- Default: `50_000`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: Storage

Most diagnostic and log records kept; the oldest beyond it are removed by the retention pass. With messages bounded to 8 KiB and helper output to 16 KiB per record, 50 000 records stay within about 1.2 GiB in the worst case and a few tens of megabytes typically. Engineering default.

## indexing.overlayRecordRetentionDays

Ended ref overlay retention (days).

- Default: `14`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: Storage

An ended ref overlay (expired, retired or failed) is pruned once it ended longer ago than this and is beyond the newest records kept per repository: its record, layers and graph data go, while its runs, history and logs stay. Live overlays and any still named by a view are never pruned. Separate from artifact retention. Engineering default.

## indexing.overlayRecordsKept

Ended ref overlays kept per repository.

- Default: `32`
- Scope: Workspace only
- Adoption: NextAdmission
- Policy: Configurable
- Section: Storage

The most recently ended ref overlays each repository keeps whatever their age; older ones past the retention window are pruned. Engineering default.

## roles.workspaceCoordinator.instructions

WorkspaceCoordinator.

- Default: `"Coordinate work across projects. Answer status questions and delegate to feature and issue coordinators."`
- Scope: Workspace → Project
- Adoption: NextSafeStep
- Policy: Configurable
- Section: Instructions

Editable role instructions; policies and selected tools still apply.

## roles.featureCoordinator.instructions

FeatureCoordinator.

- Default: `"Plan and coordinate one feature against its approved plan. Publish each owned feature branch before task checkout. Take the highest-priority task with verified prerequisites. Use a local task worktree and one independent review of its exact diff. Resolve findings, integrate locally and verify the candidate. For a conflict, verify that both the task and existing feature behaviors survive. Publish with git.push taskIntegration and advance only when DevWorks verifies publication. Open one final PR to main per affected repository after integrated acceptance."`
- Scope: Workspace → Project
- Adoption: NextSafeStep
- Policy: Configurable
- Section: Instructions

Editable role instructions; policies and selected tools still apply.

## roles.issueCoordinator.instructions

IssueCoordinator.

- Default: `"Triage, reproduce and coordinate one standalone issue."`
- Scope: Workspace → Project
- Adoption: NextSafeStep
- Policy: Configurable
- Section: Instructions

Editable role instructions; policies and selected tools still apply.

## roles.worker.instructions

Worker.

- Default: `"Implement the assigned task card and report verifiable evidence."`
- Scope: Workspace → Project
- Adoption: NextSafeStep
- Policy: Configurable
- Section: Instructions

Editable role instructions; policies and selected tools still apply.

## roles.codeReviewer.instructions

CodeReviewer.

- Default: `"Review the exact head for correctness and report findings."`
- Scope: Workspace → Project
- Adoption: NextSafeStep
- Policy: Configurable
- Section: Instructions

Editable role instructions; policies and selected tools still apply.

## roles.dreaming.instructions

Dreaming.

- Default: `"Consolidate Knowledge into checkpointed proposals."`
- Scope: Workspace → Project
- Adoption: NextSafeStep
- Policy: Configurable
- Section: Instructions

Editable role instructions; policies and selected tools still apply.

## roles.scheduled.instructions

Scheduled.

- Default: `"Run the scheduled job definition within its recorded scope."`
- Scope: Workspace → Project
- Adoption: NextSafeStep
- Policy: Configurable
- Section: Instructions

Editable role instructions; policies and selected tools still apply.
