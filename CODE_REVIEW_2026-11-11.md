# Full Codebase Review — DevWorks — 2026-11-11

Reviewed commit: `f707b520` (`main`, also the tip of `origin/main` on the review date).

## Executive summary

- **Repository at a glance:** Rust 2024 workspace, five crates (`devworks-core`, `devworks-adapters`, `devworks-engine`, `devworks-extension-protocol`, `devworks-desktop`), 910 `.rs` files, about 570,000 lines of Rust. About 319,000 of those are hand-written non-test source; about 160,000 are tests; the rest is generated. `tools/` adds about 48,000 lines of Python and Rust; `docs/` holds the requirements, architecture and recorded validation evidence.
- **Coverage:** Thirteen reviewers read the code in parallel, one per subsystem, plus one cross-crate duplication census and two requirements-conformance checks against the CodeGraph tracker (project DW, features DW-F2 to DW-F14). I ran `cargo check --workspace --all-targets` and re-read the code behind the findings marked "Verified: lead". No tests were run. No reviewer read its whole scope line by line; the Coverage notes at the end say what was read, skimmed and not opened. Generated files (`protocol/records.rs`, `protocol/results.rs`, `operations/*.rs`, `registry/catalog.rs`, `domain_generated.rs`, lockfiles) were skipped.
- **Counts:** Critical: 1 | High: 18 | Medium: 46 | Low: 22 | Info: 4.

### The three questions

**1. Does it meet the requirements?** Partly. Confidence: high for features F2–F9, medium for F10–F14.

- Where agents wrote code, they built what was planned. The acceptance tests the items cite exist under the cited names, and neither requirements check found fabricated evidence or significant unrequested scope.
- The tracker overstates what is delivered. A shared criterion ("satisfy every listed source requirement; expose the same service to native UI") is marked met on items whose own acceptance reports disclaim parts of it. Dozens of requirements are recorded as covered by complete items when their real owners have not started (F-005, F-006).
- Work repeatedly stops at the service boundary. Capability exists in core but is not wired into the running app or its UI: Knowledge rule delivery, source staleness, automatic grading, scheduled inference, automatic worktree overlays, and about ten desktop surfaces (F-007, F-012 to F-015).
- Three behaviours differ from a stated requirement: manual Start is blocked by another root's claim (F-008), the readiness gate depends on a purpose the caller declares (F-009), and MCP-06 is reversed in this tree (F-010).
- 73 of 369 catalog operations have no handler. All belong to items that are not started, and the gateway answers `NoUseCase`, so they are unreachable rather than broken.

**2. Is the architecture clean?** At the crate level, yes. Inside the crates, less so. Confidence: high.

- Crate dependency direction holds everywhere. Core depends on no sibling; the engine never names SQLite; the desktop names adapters only in its composition root and one port adapter. The admission gateway, the single-writer SQLite store, the durable dispatcher, the provider wire codecs, the web egress policy and the extension protocol are well built.
- Inside `devworks-core` the layers are circular: `gateway`, `authority`, `settings` and `ports::storage` import from `services`, and `providers` and `services` import each other (F-041).
- About 115 non-test `run_write` call sites write domain state outside the gateway, with no receipt or authority fence (F-040).
- Authority rules live in four mechanisms and are keyed on operation-name strings, so a new operation defaults to allowed (F-027).
- Three composition and ownership problems recur: `runtime.rs` and `host.rs` split service construction with no stated rule (F-042); the agent turn has five owners (F-043); the desktop `Shell` is a 54-field object with a 164-variant slot enum (F-044).
- Business rules sit in adapters in three places (F-045).

**3. Is there unnecessary duplication or redundancy of purpose?** Yes, and it is the most consistent finding in this review. Confidence: high.

- The pattern is the one you suspected: each item's agent wrote its own copy of the shared plumbing. The copies have already drifted in ways that change behaviour.
- The worst cases: five storage-error-to-fault mappings (F-046), six cursor schemes (F-047), about 30 SQLite files re-declaring the same row helpers (F-048), nine engine effect-consumer wrappers and eleven supervisor skeletons (F-049, F-050), two owners for "what runs next" (F-051), two agent messaging stacks (F-052), five HTTP client stacks with three TLS trust policies (F-053), two tree-sitter extraction engines (F-054), and a second hand-maintained copy of the protocol types in `tools/contracts-eval` (F-055).
- I did not find text-level duplication across layers. Same-named SQLite and engine test files share 0–7% of their lines, and the engine's thin per-service files are glue, not a second implementation of the rule.

### Top risks

1. `main` does not compile (F-001). Known as DW-161; PR #73 is open.
2. The destructive-command guard can be bypassed with `find -exec rm` (F-002).
3. The files that decide what a project purge deletes are exempt from review (F-003).
4. The tracker cannot answer "is it built?" (F-005, F-006).

---

## Critical findings

#### [F-001] Restore a compiling `main` and stop merges that break it
- **Category:** bad-pattern
- **Severity:** Critical
- **Location:** `crates/devworks-engine/src/mcp_sampling.rs:L482`; `.github/workflows/ci.yml`
- **Finding:** `cargo check --workspace --all-targets` fails: `error[E0063]: missing field skill_stamp in initializer of ProviderContext`. Two PRs (DW-45 sampling and DW-34 skills) each passed CI alone and broke the build together. Tracked as DW-161; PR #73 is open and not merged.
- **Impact:** Nothing in the engine or desktop builds from `main`. Every branch cut from it starts broken.
- **Suggested fix:** Merge PR #73. Then require branches to be up to date before merge, or use a merge queue, so CI runs on the merge result. DW-45's handoff note records an earlier textual-merge break of the same kind (PR #58 with DW-45).
- **Confidence:** High.
- **Verified:** lead (ran the check).

---

## High findings

### Security and safety

#### [F-002] Preflight the operands of `find -exec rm`
- **Category:** security
- **Severity:** High
- **Location:** `crates/devworks-core/src/authority/shell.rs:L1768-L1815`
- **Finding:** For `-exec`, `-execdir`, `-ok` and `-okdir`, `find()` inspects only the first inner word (`"rm" | "rmdir" | "unlink" | "shred" => { deletes = true; c.claim(&n); }`) and then plans `EraseContents` on the `find` start points only. The inner command's own operands are never planned, and the claim silences the lexical backstop.
- **Impact:** `find . -maxdepth 0 -exec rm -rf /path/to/primary/repo ;` yields one plan for `.`. A plain `rm -rf` of the same path would be blocked. This defeats the guarantee that primary repositories, `.git`, home and filesystem roots are refused. The only existing test is `find . -exec rm {} ;`.
- **Suggested fix:** Allow only inner operands that are exactly `{}`. Otherwise run the removal planner on the inner words, or hold with `OpaqueCommand`. Add a test asserting `Blocked` for the argv above.
- **Confidence:** High for the missing operand check; not executed end to end.
- **Verified:** lead (read).

#### [F-003] Move purge deletion policy out of the review-exempt docs tree
- **Category:** security
- **Severity:** High
- **Location:** `crates/devworks-adapters/src/storage/sqlite/project_purge/classification.rs:L14-L57`; `project_purge/candidates.rs:L10-L13`; `.gitattributes:L18`; `AGENTS.md` (Review guidelines); `docs/architecture/validation/project-purge/*.json`
- **Finding:** The product compiles in the row selectors and per-table dispositions that decide which rows a project purge deletes, with `include_str!` from `docs/architecture/validation/project-purge/`. `.gitattributes` marks `docs/architecture/validation/**/*.json` as `linguist-generated`. No generator under `tools/` references these files; feature commits edit them by hand (about 9,200 lines).
- **Impact:** AGENTS.md tells automated review to skip generated files, GitHub collapses them in diffs, and the PR size check does not count them. A wrong `disposition` or `candidatePredicates` entry can ship unreviewed and delete the wrong data.
- **Suggested fix:** Move the policy files into the adapter crate (for example `project_purge/policy/`), outside the generated glob. Keep `docs/` for recorded evidence. Audit other `include_str!` targets under `docs/` the same way.
- **Confidence:** High on the mechanism; medium that no generator exists outside the repository.
- **Verified:** lead (read; `git check-attr` confirms the attribute).

### Requirements and tracker

#### [F-005] Correct three items that sit "in review" with no implementation
- **Category:** requirements
- **Severity:** High
- **Location:** DW-37, DW-39, DW-40; `crates/devworks-core/src/registry/catalog.rs` (`database.*`, `git.integrate`, `git.cleanup`, `github.*`, `issues.publish`, `issues.link`); `crates/devworks-core/src/services/mod.rs:L173-L186`, `L437-L461`
- **Finding:** All three items show `in_review` with `merged_awaiting_acceptance: true`. Their operations exist only as generated descriptors with no registered handler. DW-37 links PR #48 (DW-36's PR) and DW-39 links PR #21 (DW-38's PR). The project's own recorded gotcha explains the cause: the GitHub webhook links a PR to every item key written in its body.
- **Impact:** The board shows database data tools, final delivery and cleanup, and GitHub sync as awaiting sign-off. Accepting them would mark DB-04/05/08, GIT-06/15/16 and GH-01..12 delivered with no code.
- **Suggested fix:** Move DW-37, DW-39 and DW-40 back to `ready`, remove the foreign PR links, and name only a PR's own item key in its body.
- **Confidence:** High for the code state; medium for the cause.

#### [F-006] Stop counting requirements as covered by items that did not build them
- **Category:** requirements
- **Severity:** High
- **Location:** shared criterion A2 on DW-2, 3, 4, 7, 8, 10, 13, 17, 18, 21, 23, 28, 30, 36, 38, 43, 50, 108; `covered_by` on features DW-F2, F4–F11
- **Finding:** Every native item carries "Satisfy every primary and supporting source requirement listed on this item; expose the same service to native UI", marked met. The items' own reports disclaim parts of it. Overstated coverage found:
  - **Home chat** (UI-01–04, UI-08, UI-10–13, covered by DW-4/DW-10): `chat.rs:L45-L51` says the coordinator "is not connected in this build (DW-16)".
  - **Workspace coordinator** (COORD-01–08, covered by DW-13): nothing launches one.
  - **Backup and restore** (STORE-05/13, covered by DW-2/3/7): `workspace.backup`, `.restore` and `.migrate` have no use case; DW-57 is not started.
  - **SystemOne and Auto** (ROUTE-18/19, covered by DW-17/18/21/108): `router.decide` has no handler; "DevWorks Auto" appears nowhere in `crates/`.
  - **Database data tools** (DB-04/05/08, covered by DW-36): no code.
  - **Semantic packs** (CODE-03/04, covered by DW-23/28): `SemanticEnricher` has no implementation and no caller.
  - **Dreaming and documents** (DREAM-01..05, RAG-01..13, covered by DW-30): no service.
  - **Git delivery and GitHub** (GIT-06/07/09/12/15/16/17, GH-01..12, covered by DW-38): about a third built.
  - **SDK** (EXT-01..07, covered by DW-43): its report excludes SDK completion; DW-44 is not started.
- **Impact:** DW-F2 reads "complete, all 31 requirements covered". The feature completion view cannot flag the gaps, so the tracker cannot answer whether a requirement is built.
- **Suggested fix:** Trim each complete item's `inherits` to what it delivered and move the rest to the unbuilt owners (DW-16, 15, 22, 24–27, 32, 35, 37, 39–42, 44, 57, 107). Reword A2 so supporting requirements are not claimed, and so "expose to native UI" names the screen.
- **Confidence:** High.

#### [F-007] Wire Knowledge rule delivery and source staleness into the running app
- **Category:** requirements
- **Severity:** High
- **Location:** KNOW-06, KNOW-03, DW-30 (complete); `crates/devworks-core/src/services/knowledge/mod.rs:L1217-L1340`
- **Finding:** `KnowledgeService::rules_for`, `record_delivery` and `observe_source` have no caller outside `devworks-adapters/src/storage/sqlite/tests/knowledge.rs`. The module doc presents both as behaviour. Two reviewers found this independently.
- **Impact:** A lesson becomes `Effective` but is never put into a session's context. "Supplied to N sessions" always reads 0, and no claim ever turns stale. KNOW-06 is a Confirmed requirement on a complete item.
- **Suggested fix:** Call `rules_for` and `record_delivery` where the instruction epoch is adopted (`crates/devworks-engine/src/agent_turns.rs:L394-L399`), and `observe_source` from the indexing publish path. File the item.
- **Confidence:** High.

#### [F-008] Stop one claimed root from blocking every other Start in its project
- **Category:** requirements
- **Severity:** High
- **Location:** AGENT-16, WORK-01, WORK-11, DW-13; `crates/devworks-core/src/services/work/allocation.rs:L959-L977`, called from `crates/devworks-core/src/services/runs/mod.rs:L1362`
- **Finding:** `check_start` refuses any `runs.start` with `conflict("project_execution_owned")` when another root in the project holds a `Transferred` or `Yielding` claim on the project queue. The check does not require the root being started to hold a claim. AGENT-16 limits one-at-a-time execution to an optional mode, and `docs/architecture/devworks-architecture.md:471` says it "does not invent a project-wide prohibition on manually starting other features". No such mode or toggle exists.
- **Impact:** After anyone uses Next work and starts that item, the user cannot manually start a second feature or issue in the same project until the first quiesces.
- **Suggested fix:** Apply the single-root rule only to starts of roots that themselves hold a claim, or only under an enabled keep-working mode. Add a test that an unclaimed manual Start succeeds beside a transferred claim.
- **Confidence:** High on the code path; medium on intent.
- **Verified:** lead (read).

#### [F-009] Gate agent tool effects on feature readiness, not only inference requests
- **Category:** requirements
- **Severity:** High
- **Location:** AGENT-12, DW-9 A3, DW-F3 acceptance test 3; `crates/devworks-core/src/services/runs/mod.rs:L1050-L1098`, `L1413-L1423`; `crates/devworks-core/src/services/agents.rs:L743-L752`
- **Finding:** The late readiness amendment says "all implementation entry paths enforce current applicable feature readiness independently of approval". The readiness condition runs only under `if purpose == ActivationPurpose::Implementation && is_feature(root)`. Start on an unassessed feature creates a `Planning` activation, and child activation hard-codes `Planning`. Only inference admission, child activation, membership, jobs and approval replay call `begin_dispatch`; nothing under `services/git` or `services/process` does. The test passes the purpose by hand.
- **Impact:** An agent under a Planning activation on a feature whose readiness is missing, stale or blocked appears able to commit or run processes. The gate then depends on the model behaving.
- **Suggested fix:** Evaluate eligibility with `Implementation` for every mutating tool call by an agent bound to a feature root, as `approvals/replay.rs:L304-L309` already does. Add an engine test where a Planning-activation agent's `git.commit` and process run are refused.
- **Confidence:** Medium. A test driving a real agent tool call under a Planning activation would settle it.
- **Verified:** lead read the purpose selection; the missing tool-call gate is subagent only.

#### [F-010] Restore MCP-06: select a new server's read-only tools by default
- **Category:** requirements
- **Severity:** High
- **Location:** MCP-06 (Confirmed), DW-45, DW-153; `crates/devworks-core/src/services/mcp.rs:L1552-L1566`, `L1674-L1690`
- **Finding:** MCP-06 says "When an MCP server is added, enable its read-only tools by default." The code says "only mcp.select_tools can make a newly discovered tool executable". A new server starts with an empty selection. An automated review reversed the Confirmed requirement and it merged that way in PR #60. DW-153 (PR #69) restores it but is not in this tree.
- **Impact:** Every newly added server gives agents zero tools until a person ticks each one.
- **Suggested fix:** Merge DW-153, keeping its confinement (the hint sets default selection only and never counts as a verified read). Treat "bot review contradicts a Confirmed requirement" as a decision for the owner, not for the agent.
- **Confidence:** High for this tree.

#### [F-011] Compose a real scheduled-inference pipeline or reopen JOB-05/06/07
- **Category:** requirements
- **Severity:** High
- **Location:** JOB-05, JOB-07 (Confirmed), JOB-06, DW-50 (complete); `crates/devworks-core/src/services/jobs/runner.rs:L555-L575`; `crates/devworks-engine/src/runtime.rs:L101-L109`
- **Finding:** The only non-test `InferenceJobs` implementation is `NoInferencePipeline`, whose `start` always refuses with "Scheduled inference is not available in this build". The comment names DW-16 and DW-18 as missing; DW-18 is complete and `agent_supervisor.rs` now has an `AgentLauncher`, but nothing connects jobs to it.
- **Impact:** A user can create an inference schedule and every run fails before dispatch. No item tracks the wiring.
- **Suggested fix:** File an item to implement `InferenceJobs` over the agent turn runner and compose it in the desktop host. Remove JOB-05/06/07 from DW-50's covered set until then.
- **Confidence:** High that no implementation exists.

#### [F-012] Build agent worktree overlays automatically and give assigned agents their indexing tools
- **Category:** requirements
- **Severity:** High
- **Location:** CODE-09 (Confirmed), CODE-11, DW-23/DW-28; `crates/devworks-core/src/services/indexing/views.rs:L116-L151`; `indexing/overlay.rs:L416-L593`; `crates/devworks-engine/src/providers/schemas.rs:L111-L114`; `crates/devworks-desktop/src/shell/indexing.rs:L262-L290`
- **Finding:** CODE-09 says "Automatically include each agent's uncommitted worktree changes in its graph view." Only the `indexing.refresh_overlay` command creates an overlay, and nothing requests one. A later change skips every `indexing.*` tool when the context has an assignment. The UI can refresh an existing overlay but cannot request a first one.
- **Impact:** An agent on an assignment gets `ViewUnavailable` for its own graph view and has no tool to fix it.
- **Suggested fix:** Request the overlay when a worktree is bound to an agent. Allow assigned agents `indexing.refresh_overlay` and `indexing.status` for their own worktree. Add a "Request overlay" action. Retire the overlay with its worktree (today it stays `Ready` until the 72-hour expiry).
- **Confidence:** High on the code; medium on whether the tool exclusion was an owner decision.

#### [F-013] Build the desktop surfaces that complete items left as notes
- **Category:** requirements
- **Severity:** High
- **Location:** `crates/devworks-desktop/src/chat.rs:L1254-L1280`; `shell.rs:L4946-L5050`, `L5170-L5250`, `L4772`; `route.rs:L181-L195`; `shell/work.rs:L4654-L4676`; `knowledge_screen.rs`
- **Finding:** Items were marked complete on "expose the same service to native UI" while these surfaces were never built. No open item owns most of them.
  - **Current tab** (UI-01/03): three fixed `Unavailable` blocks naming DW-14, DW-12 and DW-15 as owners. DW-12 and DW-14 are complete.
  - **Project Artifacts view** (ART-01/02): `Unavailable`, owner "DW-11 (projects)", which is complete. The desktop never sends `artifacts.preview`, `.export`, `.reference` or `.tag`.
  - **Run control** (CONTROL-02/04/07/10): the desktop sends only start, pause, continue and inspect. `runs.redirect`, `runs.stop` and `runs.eligibility` have no caller; hold reasons are not shown; Continue is per item only.
  - **Agents and sessions** (CONTROL-01, UI-06): fully pending; no caller of `agents.inspect`, `agents.messages` or `agents.inbox`.
  - **Resources** (RES-01/03): counts only; no holder, waiter, purpose or expiry.
  - **Queue recovery and linked history** (OP-01, HISTORY-01): no desktop caller of any `queue.*` operation or of `history.work_lineage`, `history.evidence_links`, `history.tool_usage`.
  - **Knowledge graph reads and relation authoring** (DW-31 A2): `subgraph`, `entity_bundle`, `claim_bundle`, `expand_frontier`, `render_summary` and `knowledge.relate` are agent-only.
  - **Process, container, browser-action and desktop operations:** settings and status views only.
- **Impact:** The UI tells the user that working features do not exist and names finished items as the reason. A person cannot redirect running work, see why work is held, browse project artifacts, or recover a poisoned delivery.
- **Suggested fix:** File one item per surface. Until each lands, change the placeholder's owner text to the real open item.
- **Confidence:** High that the calls are absent.

### Defects

#### [F-004] Stop the Git reconcile loop before shutdown joins its task
- **Category:** bad-pattern
- **Severity:** High
- **Location:** `crates/devworks-engine/src/runtime.rs:L2683-L2710`, `L2818-L2851`; `crates/devworks-engine/src/git.rs:L67-L73`
- **Finding:** `git::run` loops `while !service.is_stopped()`. The only `git.stop()` call is at `runtime.rs:L2708`, after the loop that joins every boot task. The call sits in the middle of an unrelated comment about browser sessions, which suggests a merge artifact. `Runtime::stop()` stops the other services but not Git, and `Drop for Runtime` has the same gap.
- **Impact:** From the code, with Git composed (the desktop default), the join blocks until the whole shutdown bound (30 s default) elapses, then aborts. Every later step runs with zero budget: `dispatcher.idle()`, browser close, package helper shutdown, container shutdown and housekeeping. Helpers and owned process trees lose their cooperative stop.
- **Suggested fix:** Call `git.stop()` inside `Runtime::stop()`. Better, drive all background loops from one shared stop signal (F-050).
- **Confidence:** High on the code path. A timed `shutdown()` would confirm the 30 s wait.
- **Verified:** lead (read).

#### [F-016] Move blocking store and file I/O in the agent turn loop off the single-threaded runtime
- **Category:** bad-pattern
- **Severity:** High
- **Location:** `crates/devworks-engine/src/agent_turns.rs:L228-L499`; `crates/devworks-desktop/src/host.rs:L1333-L1337`; `crates/devworks-engine/src/runtime.rs:L1740-L1836`
- **Finding:** The desktop runs the engine on `tokio::runtime::Builder::new_current_thread()`. `run_native_loop` is `async` but calls synchronous store and file work inline (`ensure_membership_context`, `publish_continuation`, `refresh_native_tool_context`, `adopt_instruction_epoch`, `wait_for_linked_effects` and others). Only the provider call and tool dispatch go to `spawn_blocking`.
- **Impact:** Each blocking call stalls the one thread that drives the dispatcher, lease renewals, run supervision and every other agent's turn. DW-160 ("agent turn waits about 10 s before each provider request") is probably a symptom of the same contention.
- **Suggested fix:** Run the loop body on a blocking thread, or convert the helpers to the engine's own `store::read` / `store::write`.
- **Confidence:** High that the calls are synchronous on a single-threaded runtime; medium on stall length.

#### [F-017] Move store reads and writes off the desktop frame thread and behind the gateway
- **Category:** bad-pattern
- **Severity:** High
- **Location:** `crates/devworks-desktop/src/shell.rs:L485-L572`, `L2110-L2126`, `L1650-L1666`; `shell/indexing.rs:L530-L545`; `host.rs:L977-L1019`, `L1162-L1169`, `L1207-L1222`
- **Finding:** The shell makes four synchronous `Backend` calls on the UI thread, and three skip the gateway. `append_coordinator_context_if_changed` runs every frame and writes through `sessions::notice::append_screen_context(&**store, ...)` under a host principal. On error it leaves `last_coordinator_notice` unchanged, so the write is retried on every repaint. `read_coordinator_notice`, `index_sources` and `stage_pasted_image` are also inline.
- **Impact:** This contradicts the `lib.rs` claims "Never blocking the frame loop" and "admitted by the runtime's gateway". A busy SQLite writer freezes the window. `ui.current_screen` sits in the catalog with no handler while the host writes screen context directly.
- **Suggested fix:** Run all four through `Client::task` with slots. Record the failed key so a failure is not retried until the context changes. Register the notice and index-source reads as gateway operations.
- **Confidence:** High.

#### [F-018] Fix `ProcessToolService::observe` for dependency installs
- **Category:** bad-pattern
- **Severity:** High
- **Location:** `crates/devworks-core/src/services/process/tools.rs:L1465-L1490`
- **Finding:** `observe` returns `EffectOutcome::Failed("unsupported process effect")` for any capability other than `process.execute`. The branch below that builds `OperationResult::ProcessInstallProjectDependencies` can never run. `execute` accepts both capabilities.
- **Impact:** An install interrupted by a restart or an unknown outcome settles as a deterministic `Failed`, even when `npm ci` or `cargo build` ran or is still running. That contradicts the file's own rule that an unknown observation is never reported as a known result.
- **Suggested fix:** Accept both capabilities in the guard. Add a test that observes an install operation with a `Reaped` process record.
- **Confidence:** High.
- **Verified:** lead (read).

#### [F-019] Cancel the reservation when the dispatch write fails
- **Category:** bad-pattern
- **Severity:** High
- **Location:** `crates/devworks-core/src/services/usage/host.rs:L276-L286`; `usage/accounting.rs:L873-L874`, `L236-L284`; `crates/devworks-engine/src/usage.rs:L345-L362`
- **Finding:** `drive_with_sink` cancels the admitted attempt only when `mark_attempt_dispatched` returns `Ok(non-Admitted)`. On `Err` (for example `managed_owner` returning `ScopeDenied` because the turn or assignment changed after admission) the `?` skips `cancel_admitted`. The attempt stays `"Admitted"` with its reservation and gets a custody recovery reason.
- **Impact:** Every later turn on that work item is fenced with "an earlier work turn still has an unclosed inference transport". Recovery skips attempts of the current boot, so the block and the held budget last until the app restarts, although no transport was opened.
- **Suggested fix:** On any `Err` from the dispatch write, re-read the attempt and call `cancel_admitted` if it is still `"Admitted"`. Make `managed_owner` failures return `Admission::Fenced`.
- **Confidence:** Medium. Traced by reading only; a test that bumps the assignment generation between admission and dispatch would confirm it.

#### [F-020] Replace the per-node full edge scan in code-graph traversal with an adjacency index
- **Category:** efficiency
- **Severity:** High
- **Location:** `crates/devworks-core/src/services/graph/operators.rs:L203-L260`; `graph/impact.rs:L277-L330`; `graph/budget.rs:L50-L60`
- **Finding:** `walk` scans every materialized edge for each dequeued node and charges one budget visit per edge examined. The visited set is keyed by `(node, depth)`, so on cycles a node is re-expanded once per depth.
- **Impact:** Cost is O(expanded nodes × edges). Materialization already takes half of `max_visited`. With 100,000 edges in scope, the remaining budget covers about five node expansions before `graph_visited_limit`. `code.traverse`, `code.call_path` and `code.data_lineage` will return truncated results on any large repository.
- **Suggested fix:** Build outgoing and incoming adjacency maps once after `filtered_edges`. Charge visits per edge followed. Key `visited` by node alone.
- **Confidence:** High on the complexity; medium on the exact truncation point.

#### [F-021] Give tests a way to open the SQLite store without production fsync durability
- **Category:** efficiency
- **Severity:** High
- **Location:** `crates/devworks-adapters/src/storage/sqlite/conn.rs:L109-L113`, `L155-L159`; `sqlite/mod.rs:L100-L118`; `sqlite/tests.rs:L58-L90`; `crates/devworks-engine/tests/common/mod.rs:L73-L110`; `crates/devworks-desktop/tests/common/mod.rs:L88-L106`
- **Finding:** Every writer connection sets `synchronous=FULL`, and on macOS `fullfsync` and `checkpoint_fullfsync`. `verify_effective` refuses anything else, and `SqliteConfig` has no durability field. Every test store also applies 49 migrations, each followed by `PRAGMA quick_check`. Callers: 815 SQLite tests, about 616 engine tests, and 236 full boots across 295 desktop tests.
- **Impact:** This is the most likely reason the macOS desktop job runs 41–44 of 45 minutes, since macOS is the only platform that pays `F_FULLFSYNC`. The job has 1–4 minutes of headroom. The recurring "make test X deterministic" bugs (DW-86, 116, 125–148, 151, 152, 162, 163) are mostly this cost surfacing as timeouts. PR #67 addresses the migration half.
- **Suggested fix:** Add a crate-internal durability setting to `SqliteConfig` (production default unchanged; `verify_effective` checks the configured level). Set it to relaxed in the three shared harnesses and keep the dedicated durability tests on full. Shard the macOS desktop job with the existing `tools/ci/desktop-shard.py` now.
- **Confidence:** High that no setting exists; medium on the size of the saving, which nobody has timed.

---

## Medium findings

### Security

#### [F-022] Refuse `.git` and other dot-directories as agent container mount sources
- **Category:** security
- **Severity:** Medium
- **Location:** `crates/devworks-core/src/services/containers/service.rs:L113-L172`; `containers/effects.rs:L414-L437`
- **Finding:** `resolve_agent_mount_root` accepts any single-segment child of a member repository's primary checkout. Its only name check rejects empty names and path separators, so `.git` passes, and `read_only` is the caller's choice.
- **Impact:** An agent container can get a writable bind mount of `.git`, write `hooks/pre-commit` or `config`, and gain host code execution on the user's next Git command.
- **Suggested fix:** Refuse names starting with `.` in both mount resolvers. Consider forcing agent mounts of the primary checkout read-only.
- **Confidence:** Medium. `containers.create` needs a reviewed grant under Project scope; check what the reviewer is shown and whether Full scope skips it.

#### [F-023] Bind published container ports to loopback
- **Category:** security
- **Severity:** Medium
- **Location:** `crates/devworks-core/src/services/containers/backend.rs:L187-L199`
- **Finding:** The create plan emits `--publish {host}:{container}/{proto}` with no host address.
- **Impact:** Docker and Podman normally bind that form to all interfaces, so every port an agent publishes for a dev server or database would be reachable from the LAN.
- **Suggested fix:** Emit `127.0.0.1:{host}:{container}/{proto}` by default. Make wider binding an explicit, reviewed option.
- **Confidence:** High for the argv; medium for exposure (not tested against a running engine).
- **Verified:** lead (read).

#### [F-024] Apply an egress policy to `browser.navigate`
- **Category:** security
- **Severity:** Medium
- **Location:** `crates/devworks-core/src/services/browser/mod.rs:L1677-L1679`; `browser/live.rs:L1602-L1611`; compare `web/egress.rs:L192-L198`
- **Finding:** A remote location goes straight to `Page.navigate`. The only check is the contract regex `^https?://[^\s]+$`. `web.fetch` classifies every resolved address and refuses private, link-local and metadata ranges.
- **Impact:** The same agent that `web.fetch` refuses can load `http://169.254.169.254/` or `http://192.168.1.1/` in the browser and read it back through `browser.inspect`. The private-network setting is therefore not a real boundary.
- **Suggested fix:** Move address classification into one shared egress module. For the browser, always block link-local and metadata classes, apply the private-network setting to private ranges, and decide loopback explicitly. Enforce on every request through the existing `Fetch.requestPaused` interception.
- **Confidence:** High that no check exists; medium on intended policy for loopback.

#### [F-025] Stop auto-approving on-disk skill changes without knowing who wrote them
- **Category:** security
- **Severity:** Medium
- **Location:** `crates/devworks-core/src/services/instructions/sync.rs:L342-L439`; `crates/devworks-desktop/src/host.rs:L490-L493`, `L517-L521`
- **Finding:** Any changed directory under `~/.agents/skills` or `<data-root>/skills` is recorded `Approved` and made the active revision. The host cannot tell a user edit from an agent write, and `~/.agents/skills` is outside the protected roots.
- **Impact:** The module says skill activation is "approval-required for every non-human principal". An agent with `process.execute` can write a `SKILL.md` to disk, and the next sync makes it the active instruction for all agents.
- **Suggested fix:** Record observed changes as pending and require a human confirmation. As a smaller step, add both skill roots to the write-protected roots and show observed activations to the user.
- **Confidence:** Medium. The auto-approval is documented design; the gap needs a native execution grant.

#### [F-026] Make redaction cover JSON-shaped text and real secret values
- **Category:** security
- **Severity:** Medium
- **Location:** `crates/devworks-core/src/authority/redact.rs:L6-L7`, `L106-L132`, `L183-L236`, `L279-L283`; `crates/devworks-core/src/services/diagnostics/mod.rs:L20-L22`, `L794`, `L814-L819`
- **Finding:** Four gaps. `redact_line` splits on `"`, so `{"password":"hunter2"}` passes. `redact_json` replaces a sensitive key only when its value is a string, so nested values pass. The prefix list rejects tokens containing `.`, `/` or `+`, so JWTs and AWS secret keys pass. `DiagnosticService::register_secret` has no caller outside tests, and 24 call sites build a fresh `Redactor::new()` with no registered values.
- **Impact:** Provider error bodies, stderr and diagnostics that echo JSON carrying a credential are persisted and exported in clear text. The module doc promises more than the code does.
- **Suggested fix:** Give the host one shared redactor and register each value when `SecretResolver` resolves it. Redact whole subtrees under sensitive keys. Treat a sensitive word followed by a bare `:` or `=` as pending. Correct the doc.
- **Confidence:** High (hand-traced; two reviewers).

#### [F-027] Derive authority lists from the registry instead of operation-name strings
- **Category:** bad-pattern
- **Severity:** Medium
- **Location:** `crates/devworks-core/src/authority/policy.rs:L48-L82`, `L399-L437`, `L474-L503`; `authority/service.rs:L279-L744`
- **Finding:** Policy is keyed on string literals: `NATIVE_EXECUTION: [&str; 17]`, `APPROVAL_REQUIRED: [&str; 5]` and inline lists. `agent_assignment_scope` ends in `_ => true`. Registered operations absent from `NATIVE_EXECUTION` include `git.integrate`, `git.pull_request`, `git.cleanup`, `desktop.input`, `browser.sessions` and `database.execute_write`. Separately, about 260 `principal_kind()` checks inside services re-verify what `authority/service.rs:L759` already resolves.
- **Impact:** A new or renamed operation defaults to allowed. No test ties these lists to the registry.
- **Suggested fix:** Add registry attributes (native execution, approval required, human only, assignment scope rule), generate them from the spec, and make the assignment match exhaustive with a deny default. Expose `authority::require_human_decision(ctx)`.
- **Confidence:** High for the structure; medium that each listed omission is wrong.

#### [F-028] Give `PrincipalKind` one serializer and one fail-closed parser
- **Category:** security
- **Severity:** Medium
- **Location:** `crates/devworks-core/src/context.rs:L18-L27`; `services/process/tools.rs:L582-L589`; `services/knowledge/writes.rs:L2922-L2931`; `services/git/commands.rs:L112-L119`; `services/browser/mod.rs:L405-L414`; `services/git/context.rs:L206-L214`; `services/mcp.rs:L630-L638`, `L882-L886`
- **Finding:** The kind is persisted as `format!("{:?}", ctx.principal_kind())` at 47 sites. Nine hand-copied `match` blocks parse it back when rebuilding a context for deferred effects. Three end in `_ => PrincipalKind::Host`, one in `_ => PrincipalKind::Agent`, and five fail closed. `containers/commands.rs` collapses Package, McpClient and Schedule into `"Agent"`.
- **Impact:** An unknown, renamed or new kind silently becomes Host in the process, knowledge and Git effect paths. Host gets `Allow` for everything except human-only classes. Debug output is also the storage format, so renaming a variant changes stored data. Package bundle trust compares `provenance["principalKind"] == "Agent"`.
- **Suggested fix:** Add `PrincipalKind::as_str` and `parse -> Option` in `context.rs`. Replace all nine parsers and all `{:?}` uses. Fail closed everywhere.
- **Confidence:** High.
- **Verified:** lead (read the three Host fallbacks).

#### [F-029] Stop byte-slicing untrusted strings
- **Category:** bad-pattern
- **Severity:** Medium
- **Location:** `crates/devworks-core/src/services/cursor.rs:L50-L62`; `crates/devworks-core/src/authority/shell.rs:L1345-L1349`; `crates/devworks-core/src/services/mcp.rs:L364`
- **Finding:** `CursorBinding::decode` checks `hex.len() % 2` in bytes and then slices `&hex[i..i + 2]`, so a cursor such as `v1.aéa.x` panics on a non-character boundary. The `nice` branch evaluates `w.text[1..]` before `starts_with('-')`, so `["nice", "é1"]` panics. `&digest[..60]` panics on a short stored MCP capability id.
- **Impact:** A caller-supplied page cursor or an agent-supplied argv panics the calling thread. Only the SQLite writer wraps jobs in `catch_unwind`.
- **Suggested fix:** Reject non-ASCII before slicing in `decode`; put `starts_with('-')` first; use `digest.get(..60)`.
- **Confidence:** High for the panics; medium for blast radius.
- **Verified:** lead (read the first two).

#### [F-030] Bind MCP list cursors the way every other cursor is bound
- **Category:** security
- **Severity:** Medium
- **Location:** `crates/devworks-core/src/services/mcp.rs:L209-L238`
- **Finding:** The MCP cursor is `format!("{revision}:{end}")`, checked only by `version.parse::<i64>().ok() != Some(revision)`. It has none of the principal, project, store or restore-epoch binding `services/cursor.rs` says must expire a cursor.
- **Impact:** A cursor from one principal or project continues another's listing if the revision matches.
- **Suggested fix:** Use `CursorBinding::encode/decode` (see F-047).
- **Confidence:** High on the missing binding; medium on exploitability, which depends on whether the item list is already filtered per caller.
- **Verified:** lead (read).

#### [F-031] Restrict the executables registered for agent process tools
- **Category:** security
- **Severity:** Medium
- **Location:** `crates/devworks-desktop/src/host.rs:L81-L137`, `L497-L548`
- **Finding:** `process_executables_on_path()` registers every executable file in every absolute `PATH` directory as agent-selectable, first match wins. It computes the canonical path for the file check but stores the uncanonicalised one.
- **Impact:** The "host-owned executable registration" control places no limit: `sh`, `curl`, `ssh` and anything in a user-writable `PATH` directory are selectable. The only remaining gate is the authority and approval policy. The scan also stats thousands of files before the window appears.
- **Suggested fix:** Register a named allowlist from settings or installed packages. Store the canonical path. Run the scan off the boot path.
- **Confidence:** Medium. The reviewer did not check how strictly core gates each launch.

#### [F-032] Check the AXChildren value is an array before calling `CFArrayGetCount`
- **Category:** security
- **Severity:** Medium
- **Location:** `crates/devworks-adapters/src/desktop_ax_macos.rs:L207-L211` (compare `L94-L98`)
- **Finding:** `observe()` calls `CFArrayGetCount` on whatever the focused application returned for `AXChildren`. The helper `array_values` guards with a type check, but this call runs before it.
- **Impact:** An application with a custom or buggy accessibility implementation can return a non-array, and `CFArrayGetCount` on a non-array is undefined behaviour. The foreground app is not trusted, and an agent can drive it.
- **Suggested fix:** Have `array_values` return `(values, total_len)` after its type check and delete the raw call.
- **Confidence:** High that the check is missing; medium that a real app can trigger it.

#### [F-033] Give the purge schema gate and migrations real integrity checks
- **Category:** bad-pattern
- **Severity:** Medium
- **Location:** `crates/devworks-adapters/src/storage/sqlite/migrations.rs:L535-L593`, `L820-L882`; `project_purge/schema.rs:L20-L52`; `project_purge/classification.rs:L134-L159`, `L736-L746`
- **Finding:** Three gaps in the store's self-checks.
  - The `MIGRATION_00NN_SHA256` constants stop at 35. Migrations 36–49 have no recorded checksum.
  - The purge gate is `if MIGRATIONS.last().map(|m| m.version) != Some(49)`. A new table is still caught by `policies()`, but a new project-owning column on an existing table is not; migrations 43, 44 and 45 added columns with no policy file.
  - `policies()` loads eleven policy documents; the assessment digest hashes eight (schema 42, 46 and 47 extensions are missing).
- **Impact:** An agent can edit migrations 36–49 and all tests still pass; every existing store then refuses to open. Adding migration 50 breaks every purge at runtime until someone changes `49` to `50`, which is all the gate requires. A prepared purge manifest stays valid after someone changes the disposition of three tables.
- **Suggested fix:** Add `expected_sha256` to `Migration` and one test that loops over `MIGRATIONS`. Replace the version constant with a recorded digest of the reference schema. Build the assessment digest by iterating the same list `policies()` loads.
- **Confidence:** High.
- **Verified:** lead (read the first two).

#### [F-034] Stop discarding the result of the skill rollback rename
- **Category:** bad-pattern
- **Severity:** Medium
- **Location:** `crates/devworks-adapters/src/fs/skills.rs:L360-L368`
- **Finding:** When `rename(&staging, &live)` fails after the live tree was moved aside, the rollback is `let _ = rename(&retired, &live); let _ = fs::remove_file(&journal);`. If the restoring rename also fails, the code still deletes the journal that lets the next use of the root complete or undo the swap.
- **Impact:** A double failure (for example a Windows sharing violation) leaves no live skill, an orphaned retired tree and no journal to recover from.
- **Suggested fix:** Remove the journal only when the restoring rename succeeds; otherwise keep it and return the fault.
- **Confidence:** Medium. The journal recovery routine was not read.
- **Verified:** lead (read).

### Defects and behaviour

#### [F-035] Do not return a boot error after tasks have started
- **Category:** bad-pattern
- **Severity:** Medium
- **Location:** `crates/devworks-engine/src/runtime.rs:L1191-L1212`, `L1240-L1243`, `L1298-L1302`
- **Finding:** Delivery, package helpers, collection, housekeeping, run supervision and usage are spawned at L1191–1212. The purge recovery step and `recover_os_notification_deliveries` can then return `Err(BootError::Composition)` without going through `failed`.
- **Impact:** `BootError` is documented "nothing was started and no command can be admitted", but the detached tasks keep claiming and delivering with no `Runtime` to stop them, and they hold the store and its workspace lock.
- **Suggested fix:** Move both steps before the spawns, or stop and join the spawned tasks on error.
- **Confidence:** High.

#### [F-036] Give the turn engine its own error type
- **Category:** bad-pattern
- **Severity:** Medium
- **Location:** `crates/devworks-engine/src/agent_turns.rs:L89-L98`, `L281-L287`, `L452-L457`; `tool_turn.rs:L1716-L1911`; `crates/devworks-core/src/services/agents.rs:L39-L41`, `L260-L276`
- **Finding:** `turn_failure` maps `ScopeDenied | InvalidRecord | NotFound` to `ActivationFailure::Deterministic`. Artifact read failures are flattened to `ScopeDenied`, and any provider outcome other than Completed, ToolCallsReady or Cancelled becomes `StorageError::InvalidRecord`. The agents service likewise returns `Result<_, StorageError>` for input validation and authorization (67 `InvalidRecord` and 94 `ScopeDenied` sites).
- **Impact:** A transient artifact read error, or a provider failure without a rate-limit reset, ends the activation as deterministic instead of retrying. A corrupt stored row is reported to the caller as their invalid argument.
- **Suggested fix:** Give the turn path an error enum (Denied, Stale, Transient, NeedsReconciliation, Provider) and return `Failure`/`Refusal` from the agents use cases.
- **Confidence:** High on the mapping; medium on user-visible frequency.

#### [F-037] Round provider-reported cost instead of dropping it or failing the turn
- **Category:** bad-pattern
- **Severity:** Medium
- **Location:** `crates/devworks-core/src/services/usage/money.rs:L21-L35`; `usage/accounting.rs:L1006-L1016`, `L1057-L1078`; `crates/devworks-core/src/providers/decode/usage.rs:L58-L120`
- **Finding:** The decoder expands a reported cost to an exact decimal of up to 4,096 places. `observe` returns `Err("invalid reported amount")` if the string exceeds 128 characters, and marks the whole charge `Untracked` if it has more than 12 fractional digits.
- **Impact:** A cost such as `0.000035549999999999995` sets `tracked_micro_usd = None`, so the charge disappears from feature spend and the budget cap undercounts. A value such as `1e-200` fails the accounting write and the turn.
- **Suggested fix:** Round to micro-USD in one place, keeping the original lexeme as evidence. Treat an unrepresentable amount as an untracked component, never a write error.
- **Confidence:** High for the code path; medium for how often providers emit more than 12 fractional digits.

#### [F-038] Report the Databases screen's own tab and selection to the coordinator
- **Category:** bad-pattern
- **Severity:** Medium
- **Location:** `crates/devworks-desktop/src/shell.rs:L501-L518`
- **Finding:** The screen-context builder reads the code-graph state for the Databases route: `Route::Databases => self.graph.tab` and `Route::Databases => self.graph.selected...`.
- **Impact:** The host notice sent to the coordinator names the wrong subview and a code-graph node while the user is on Databases.
- **Suggested fix:** Use `self.databases.tab` and `self.databases.selected`. Better, have each screen supply its own context.
- **Confidence:** High.
- **Verified:** lead (read).

#### [F-039] Complete the restorable-command list and make its test scan every file
- **Category:** bad-pattern
- **Severity:** Medium
- **Location:** `crates/devworks-desktop/src/backend.rs:L642-L727`, `L746-L779`, `L1322-L1385`
- **Finding:** `Unresolved::restored_dispatch` drops any operation not in `COMMANDS`. The guard test scans only four files. The shell sends `roles.draft`, `skills.draft`, `roles.rollback`, `skills.rollback`, `agents.post`, `agents.ack`, `schemas.configure`, `schemas.refresh`, `schemas.remove_index` and `conventions.*`, none of which are in the list.
- **Impact:** A command in flight at close is written to `ui-state.json` and discarded at the next launch, so its outcome is never confirmed with the original request key.
- **Suggested fix:** Derive the list from `devworks_core::registry`, or make the test scan all of `src/**`.
- **Confidence:** High for instructions, messages and databases; medium for conventions.

### Architecture

#### [F-040] Decide which domain writes may bypass the gateway
- **Category:** bad-pattern
- **Severity:** Medium
- **Location:** about 115 non-test `run_write` call sites across services, engine, adapters and desktop; examples: `crates/devworks-engine/src/runtime.rs:L1652-L1714` (`record_skill_adoption`), `crates/devworks-desktop/src/host.rs:L977-L1019`, `crates/devworks-adapters/src/fs/client_staging.rs:L36-L105`; `crates/devworks-core/src/ports/storage.rs:L456-L591`; `crates/devworks-core/src/gateway.rs:L1231-L1910`
- **Finding:** The admission path is single and well sealed for registered operations. But about 115 sites write domain state on a raw transaction with no receipt, no authority fence and no `CommandDecided` history. `WriteTransaction` exposes 31 repositories and about 900 methods; the gateway's `HandlerTx` is 680 lines of manual delegation that restricts five of them. On a raw `run_write` transaction, `tx.authority().issue_grant(..)` needs no capability.
- **Impact:** The `lib.rs` claim that the gateway is "the only way to reach a use case" holds for registered operations only. A new repository is unrestricted by default. No current violation was found by grep.
- **Suggested fix:** Classify the raw writes (host-observed records, effect settlement, recovery) and name each class in the port. Split read and write ledger traits so authority writes exist only behind the capability. Generate the delegation.
- **Confidence:** High.

#### [F-041] Break the dependency cycles inside `devworks-core`
- **Category:** bad-pattern
- **Severity:** Medium
- **Location:** `crates/devworks-core/src/gateway.rs:L1102`, `L1202`, `L1676`; `authority/service.rs:L907`, `L921`, `L1409-L1438`; `ports/storage/projects.rs:L492`; `providers/encode.rs:L157`, `L229`, `L710`; `providers/decode/usage.rs:L2`; `providers/materialize.rs:L4-L5`; `services/indexing/git.rs:L394-L1480`; `services/sessions/mod.rs:L100-L236`; `services/process/tools.rs:L176-L181`, `L980-L991`, `L1143-L1151`
- **Finding:** Five layering leaks inside one crate.
  - `gateway`, `authority`, `settings` and a storage port signature import from `services`.
  - `providers/encode.rs` says "Pure profile encoders" but imports router connection profiles; `services/usage` imports `providers::decode`.
  - The general Git client lives in `indexing/git.rs`; 19 of its methods are called only by `services/git/**`.
  - The service kernel (`Failure`, `Refusal`, `decided`, `query_fault`, `project_visible`) lives in `sessions`; about 50 files import it from there.
  - `process/tools.rs` and `containers/service.rs` call `std::fs`, `std::env` and `canonicalize()` directly, despite the purity claim in `lib.rs`. Eighteen core files call `Instant::now` or `SystemTime::now` despite the `Clock` port.
- **Impact:** Gateway and authority cannot be compiled or reasoned about without the 147,000-line services tree. The manifest read bypasses the `VerifiedReader` no-follow port. It works only because everything is one crate, so the compiler enforces nothing.
- **Suggested fix:** Move the shared types down into `ports` or a small domain module. Move `Git` to `services/git/client.rs`. Create `services/support.rs` for the kernel. Route file and environment access through ports. Longer term, split `services` into its own crate.
- **Confidence:** High.

#### [F-042] Split `runtime.rs` and give service construction one owner
- **Category:** bad-pattern
- **Severity:** Medium
- **Location:** `crates/devworks-engine/src/runtime.rs:L435-L505`, `L531-L1471`, `L1840-L2127`; `crates/devworks-desktop/src/host.rs:L330-L680`
- **Finding:** `Runtime` has 55 fields and 12 `Mutex<Option<JoinHandle>>`; `boot` is one 940-line function. `host.rs` builds about twelve services; `boot` builds about twenty more, "replacing any the caller registered". Wiring depends on late setters and order (`install_remote`, `install_sampler`, `set_activity`, `install_launcher`, and `if let Some(s) = Arc::get_mut(&mut run_supervisor)`, which silently skips configuration if it fails).
- **Impact:** Adding a service means editing `Composition` or `RuntimeConfig`, `boot`, `Runtime`, `stop`, `shutdown`, `Drop` and `host.rs`. Missing one is silent, as F-004 shows.
- **Suggested fix:** Build all services in one place and pass `boot` a finished set. Register background workers through one trait with `run`, `stop` and `drain`, so shutdown iterates a list.
- **Confidence:** High.

#### [F-043] Give the agent turn one owner
- **Category:** bad-pattern
- **Severity:** Medium
- **Location:** `crates/devworks-engine/src/runtime.rs:L1647-L1731`, `L1740-L2261`, `L3198-L3256`; `tool_turn.rs:L75-L112`, `L1619-L1661`, `L1999-L2029`, `L2074-L2083`; `agent_turns.rs:L47`, `L228`
- **Finding:** Turn state is owned by core `ControlService::begin_turn/end_turn`, core `sessions::turn`, `tool_turn`, `agent_turns` and `Runtime` together. `tool_turn::dispatch_one` takes `&Runtime` to call back into it. The twelve-condition session authorization block exists as `authorized_session` and is pasted inline twice; a fourth copy, `cancel_before_dispatch`, checks only three conditions.
- **Impact:** The weaker copy skips the restore-epoch, actor-kind and deactivated-binding checks before it writes a final tool response. `record_skill_adoption` appends a history event with hand-built JSON outside the gateway.
- **Suggested fix:** Introduce a `TurnEngine` holding store, artifacts, providers and a narrow command port. Make `authorized_session` generic over read and write transactions and call it everywhere.
- **Confidence:** High on the drift; medium on exploitability.

#### [F-044] Break up the desktop `Shell` object and pick one screen layout
- **Category:** bad-pattern
- **Severity:** Medium
- **Location:** `crates/devworks-desktop/src/shell.rs:L178-L247`, `L1881-L3016`; `backend.rs:L245-L485`; all `shell/*.rs`, `*_screen.rs`, `*_ops.rs`
- **Finding:** `Shell` has 54 fields. `apply` is about 1,130 lines. All 14 sub-modules are `impl Shell` with `use super::*`. `Slot` is one enum of 164 variants. Three layouts coexist:
  - six screens split into `<x>_screen.rs` (view), `<x>_ops.rs` (frames) and `shell/<x>.rs` (controller);
  - four keep everything in one `shell/<x>.rs`;
  - four keep view and frames in one file with the controller inside `shell.rs`.

  The same-named pairs (for example `shell/conventions.rs` and `conventions_screen.rs`) are a principled split, not duplicates.
- **Impact:** A new screen touches about ten central lists by hand, and each silently goes wrong when an entry is forgotten (F-038 and F-039 are two examples). A reader cannot predict where a screen's request code lives.
- **Suggested fix:** Define a `Screen` trait with a per-screen slot type and have `Shell` iterate a registry. Adopt the three-file layout everywhere and split `shell/work.rs` (4,831 lines).
- **Confidence:** High.

#### [F-045] Move business rules out of adapters
- **Category:** bad-pattern
- **Severity:** Medium
- **Location:** `crates/devworks-adapters/src/fs/client_staging.rs:L36-L105`, `L138-L263`; `fs/artifacts.rs:L843-L946`; `fs/purge.rs:L120-L190`; `src/prices.rs:L197-L271`, `L297-L313`; `crates/devworks-core/src/ports/storage/projects.rs:L167-L235`; `crates/devworks-desktop/src/memory/purge/frames.rs:L88-L135`
- **Finding:** Three leaks.
  - Filesystem adapters hold an `ApplicationStore`, run their own transactions and decide a session/work/project scope check.
  - The price adapter derives tariffs the fetched page never states (cache-rate scaling for Batch and Fast tiers, a 1.1× `us` regional tariff keyed on model version).
  - Purge port types carry SQLite table and column names as strings, and the desktop keys on physical table names, two of which (`approvals`, `resource_grants`) no code writes.
- **Impact:** Ownership rules and cost accounting sit outside the admission gateway and core services. Renaming a table silently changes desktop request-affinity results.
- **Suggested fix:** Put one managed-file service in core. Keep the price adapter to "fetch and parse rows" and move derivation into a core pricing service. Map tables to a core enum of record kinds before returning.
- **Confidence:** High.

### Duplication and redundancy of purpose

#### [F-046] Unify the storage-error-to-fault mappings and keep one `Refusal`/`Failure` pair
- **Category:** redundant
- **Severity:** Medium
- **Location:** `crates/devworks-core/src/gateway.rs:L858-L874`; `services/sessions/mod.rs:L104-L179`; `settings/service.rs:L114-L162`, `L1069-L1085`; `services/artifacts/mod.rs:L209-L263`, `L2427-L2446`; `services/web/mod.rs:L258-L269`
- **Finding:** Five hand-written matches turn a `StorageError` into a `DomainErrorCode` plus `RetryAdvice`, and they have drifted. `sessions::query_fault` has no `ViewUnavailable` arm. Settings keeps `RetryAdvice::Never` for `CursorExpired`. Web gives `Cancelled` a `SameCommand` retry. Three structurally identical `Refusal` structs and three `Failure` enums force the duplication, along with 30 per-service copies of `invalid`, 7 of `refuse` and 21 of `conflict`.
- **Impact:** The same storage failure reaches a client with different codes and retry advice depending on the service. A client is told to retry a cancelled web request and never to retry an expired settings cursor.
- **Suggested fix:** Make the gateway's `storage_fault` the only mapping. Move the sessions pair to `services/support.rs` with an optional `blockers` field and re-export it.
- **Confidence:** High on the drift.

#### [F-047] Use one cursor format and one pagination helper
- **Category:** redundant
- **Severity:** Medium
- **Location:** `crates/devworks-core/src/services/cursor.rs:L14-L70`; `queue.rs:L167-L221`; `diagnostics/mod.rs:L1545-L1570`; `usage/reports.rs:L859-L873`; `agents/retained.rs:L31-L49`; `mcp.rs:L209-L238`; `instructions/queries.rs:L21-L43`; `approvals/service.rs:L243-L277`; `jobs/mod.rs:L1274-L1310`; `notifications/mod.rs:L391-L405`
- **Finding:** Core has one shared cursor type and six private schemes (`q1.`, `l2.`, `usage1.`, `ib:`, `revision:offset`, `end:binding`). Every list hand-writes decode, fetch, has-more and encode, with three has-more strategies. `PAGE` is defined 19 times with seven values.
- **Impact:** Three concrete defects follow from the copies. `approvals.list` reports `has_more: true` with an empty next page when the row count equals the limit. A storage error in the `jobs.list` probe silently truncates the list. The `notifications.list` cursor binds the store-wide commit counter, so any commit between two page reads expires it.
- **Suggested fix:** Add `cursor::page(binding, request, fetch, position_of)` with one probe rule, a `clip_to_frame` helper and a `Paged { items, has_more }` return type. Use `ports::storage::MAX_PAGE`.
- **Confidence:** High; medium on the notifications cursor until confirmed that `committed_sequence` advances on every commit.

#### [F-048] Extract the SQLite repository helper prelude
- **Category:** redundant
- **Severity:** Medium
- **Location:** `crates/devworks-adapters/src/storage/sqlite/repos.rs:L22-L58` and about 30 sibling files (for example `agent_messages.rs:L54-L69`, `github.rs:L12-L27`, `embedding.rs:L11-L29`, `packages.rs:L11-L46`)
- **Finding:** Each repository file re-declares `invalid` (32 copies), `corrupt` (26), `id_of` (16), page clamps (17), `conflict` (14), `opt_id` (10) and `json_of` (8). They have drifted: `agent_messages.rs` maps a corrupt row to `InvalidRecord` where 25 others return `Unavailable::Corrupt`; `embedding.rs` and `packages.rs` panic on a stored revision of 0 where others clamp or error. Three repositories carry a private `fn atomic` savepoint beside the port's own, and the rest have none.
- **Impact:** A damaged row is "store corrupt" in one table and "your input was invalid" in another. One table panics inside the store. Whether a failed repository method leaves partial writes depends on which agent wrote it.
- **Suggested fix:** Add `sqlite/support.rs` with one of each helper. Choose one policy for a non-positive stored revision (return `corrupt`). Choose one atomicity rule.
- **Confidence:** High.

#### [F-049] Collapse the per-effect glue and share one effect outcome
- **Category:** redundant
- **Severity:** Medium
- **Location:** `crates/devworks-engine/src/{git,process_tools,containers,help,desktop,conventions_import,mcp,browser,schemas}.rs`; `crates/devworks-core/src/services/{browser/mod.rs:L190, containers/effects.rs:L33, git/commands.rs:L31, process/tools.rs:L636, desktop/mod.rs:L178}`; `browser/mod.rs:L2259-L2371`; `desktop/mod.rs:L264-L356`; `process/tools.rs:L673-L799`, `L1360-L1429`; `containers/runner.rs:L109-L190`
- **Finding:** Nine engine modules each repeat a per-service outcome mapper, an `Effects` struct wrapping `spawn_blocking`, an identical `struct Settle`, and a `Consumer` literal. Core defines the same three-variant `EffectOutcome` four times, a completion reader three times, and the output-drain loop twice. `desktop.rs:L31` and `mcp.rs:L35` call `observe` on the async thread while the others use `spawn_blocking`.
- **Impact:** About 700 lines of pass-through. Recovery semantics differ per service for no stated reason; F-018 is the kind of defect this layout produces.
- **Suggested fix:** One `EffectOutcome` and a `JournaledEffect` trait in core; one generic `effect_consumer(name, service, policy, cap)` in the engine; one `ProcessSupervisor::run_captured`.
- **Confidence:** High.

#### [F-050] Extract one background-worker skeleton
- **Category:** redundant
- **Severity:** Medium
- **Location:** engine: `approvals.rs:L68-L93`, `evals.rs:L61-L100`, `jobs.rs:L82-L389`, `prices.rs:L81-L155`, `notifications.rs:L81-L167`, `usage.rs:L67-L582`, `runs.rs:L179-L719`, `resources.rs:L41-L121`, `indexing.rs:L61-L124`, `diagnostics.rs:L410-L436`; `runtime.rs:L1245-L1391`; core: `conventions/indexing/mod.rs:L60-L110`, `conventions/importing/mod.rs:L40-L61`, `schemas/indexing.rs:L272-L307`, `help/generation.rs:L95-L135`, `indexing/worker.rs:L1385`; `usage/host.rs:L12-L86`, `prices/refresh.rs:L17-L45`
- **Finding:** Eleven engine types each reimplement a stop flag, `Notify`, `stop()`, `run(every)` and often `drain()`, with four different drain signatures. Boot also spawns six hand-written `sleep(1s)` polling loops with three different stop signals. Five core workers each re-implement the guarded write and job revalidation. Two write-fence types do the same job.
- **Impact:** An idle app makes roughly eight store passes a second. The lifecycle contract differs per type, which forces the hand-ordered sequence in `shutdown`, `stop` and `Drop`, and is how the Git loop was missed (F-004).
- **Suggested fix:** One `Ticker` (stop flag, wake, interval loop) and one `JobRuntime` guarded-write helper. Trigger wakes from the post-commit hook.
- **Confidence:** High.

#### [F-051] Give run eligibility and root activity one owner
- **Category:** redundant
- **Severity:** Medium
- **Location:** `crates/devworks-core/src/services/runs/mod.rs:L654-L735`, `L760-L1116`, `L239-L241`, `L2130-L2160`; `runs/recovery.rs:L735-L796`; `work/allocation.rs:L267-L425`, `L838-L935`; `runs/scope.rs:L122-L171`
- **Finding:** `ControlService::evaluate` derives prerequisite, repository-scope, finding-gate, plan, pause and blocking-question holds. `allocation::eligible` calls `evaluate` and then re-implements the same gates. `runs` calls into `work::` directly despite the `ExecutionScope` port. The copies already differ: `eligible` checks blocking questions for every purpose, `evaluate` only for `Implementation`. "Is this root still running" exists in six variants, each paging every unsettled process in the store and filtering in Rust.
- **Impact:** A rule change must be made in two modules that depend on each other. A claim or stop decision costs (linked roots + assignments) × (all unsettled processes + containers) row reads inside a write transaction. `eligible()` returns a bare `bool`, so the allocator always reports `no_ready_work` (WORK-10 asks for specific reasons).
- **Suggested fix:** Have `eligible` consume the conditions from `evaluate` and return them. Add root-filtered store queries and one `RootActivity` function.
- **Confidence:** High on the duplication.

#### [F-052] Merge the two agent messaging APIs
- **Category:** redundant
- **Severity:** Medium
- **Location:** `crates/devworks-core/src/services/agents.rs:L1028-L1153`, `L1274-L1960`, `L2117-L2221`; `agents/retained.rs`; `agents/retained/post.rs:L74-L112`
- **Finding:** Two full post/inbox/acknowledge stacks (`agents.messages`/`inbox`/`acknowledge` and `agents.post`/`receive`/`ack`) write the same message and delivery rows, one addressed by assignment, the other by session or human. Three copies decide whether the caller owns an assignment; the command copies require the current boot and an unexpired lease, the query copy checks neither. `prepare` for `agents.receive` blocks a thread for up to 55 s with `std::thread::sleep`.
- **Impact:** Two inboxes per agent with different cursors, acknowledgement semantics and authorization code. An agent context whose lease has lapsed is refused on commands but can still read the assignment inbox. Each waiting agent holds one OS thread.
- **Suggested fix:** Make assignment a recipient kind of one model. Write one active-assignment predicate. Move waiting into the engine as a notify-on-delivery wakeup.
- **Confidence:** Medium. Confirm whether both surfaces are required contract.

#### [F-053] Consolidate the HTTP client stacks and TLS trust policies
- **Category:** redundant
- **Severity:** Medium
- **Location:** `crates/devworks-adapters/Cargo.toml:L24`, `L31-L37`, `L45-L46`, `L53`, `L56`, `L60`, `L88`; `src/http.rs`; `src/packages.rs:L1208-L1251`; `src/provider_stream/mod.rs`; `src/web/transport.rs`; `src/mcp.rs:L941-L979`; `crates/devworks-core/src/ports/http.rs:L180`; `ports/web.rs:L161`; `ports/provider_http.rs:L88`; `ports/secrets.rs:L40`, `L90`
- **Finding:** The adapters link ureq 2, ureq 3, reqwest 0.12, reqwest 0.13, a hand-written HTTP/1.1 client over rustls, and hyper with the `server` feature for one parse helper. Core has two different traits both named `HttpTransport`, plus `StreamingHttp`, three failure types, and two secret-value types (one not zeroed on drop).
- **Impact:** MCP HTTPS and PostgreSQL trust the OS store through native-tls; providers, web, packages, SQL Server and MySQL use bundled Mozilla roots. `Cargo.toml:L34` calls the bundled roots "an explicit trust policy, not the OS store", which does not hold for MCP or PostgreSQL. Proxy and redirect behaviour differ per client. The package index fetch applies no egress policy and follows five redirects.
- **Suggested fix:** Move package index fetching onto `NativeHttp` and through the web egress check. Build one shared `rustls::ClientConfig` factory. Rename one `HttpTransport`. Delete `SecretValue`. Record the MCP TLS choice as a decision. The pinned-address web transport looks deliberate and should stay.
- **Confidence:** High on the inventory.

#### [F-054] Merge the two tree-sitter extraction engines
- **Category:** redundant
- **Severity:** Medium
- **Location:** `crates/devworks-adapters/src/index/treesitter.rs:L539-L542`, `L805-L1113`; `index/treesitter/spec.rs:L76-L250`, `L631+`
- **Finding:** Ten languages go through the table-driven `spec` engine. C#, JavaScript, TypeScript and TSX go through the older hand-coded `Walk::walk`. `props`, `dir_of`, `MAX_NODES` and `MAX_EDGES` are each defined twice.
- **Impact:** Definition, containment and call-edge rules exist in two implementations, so a fix lands in one language family only.
- **Suggested fix:** Add `Spec` entries for the four remaining languages and delete `Walk::walk`.
- **Confidence:** High on the two paths; medium on how far their outputs have drifted.

#### [F-055] Collapse `tools/contracts-eval` and the core protocol types into one source of truth
- **Category:** redundant
- **Severity:** Medium
- **Location:** `tools/contracts-eval/src/{lib,provider,data_delivery,domain_support,domain_rules,package}.rs`; `crates/devworks-core/src/protocol/{shared,provider,data,support,rules,addons}.rs`; `.github/workflows/ci.yml:L33-L49`
- **Finding:** The hand-written files are near-copies: `lib.rs` shares 464 of 492 unique lines with `shared.rs`; `data_delivery.rs` 873 of 1,047 with `data.rs`; `domain_rules.rs` 421 of 617 with `rules.rs`. Each of the last five commits to `domain_rules.rs` also edited core's `rules.rs`. Sixteen scalar types are also mirrored between core and `devworks-extension-protocol`, converted through strings.
- **Impact:** Every contract change is made twice by hand. CI regenerates only the core side; nothing compiles `contracts-eval`, runs its tests, or checks `domain_generated.rs` or the emitted schemas for freshness.
- **Suggested fix:** Have `devworks-core` emit the schemas from its own derives and delete the duplicated Rust. If the eval crate must stay, add a CI job that builds, tests and diffs it. For the extension scalars, either share them or add a test that both crates accept and reject the same corpus.
- **Confidence:** High on the duplication and the missing CI.

#### [F-056] Share small helpers that have drifted
- **Category:** redundant
- **Severity:** Medium
- **Location:** digest checks: `crates/devworks-core/src/protocol/provider.rs:L10-L25` (canonical) and 43 inline sites in 37 files; canonical-digest: `fingerprint.rs:L42-L47` and eight re-implementations; base64: `ports/http.rs:L212-L283`, `services/browser/cdp.rs:L354-L412`, `providers/encode.rs:L1081-L1102`, `providers/decode/embedding.rs:L3-L40`; protected branches: `authority/policy.rs:L33`, `L355-L370`, `protocol/data.rs:L1626-L1627`, `services/git/commands/push.rs:L243`, `git/approval.rs:L318`, `process/tools.rs:L1337`, `containers/runner.rs:L89`; URL redaction: `web/egress.rs:L331-L411`, `browser/live.rs:L342-L470`; scope conversion: ten `match` blocks; "authorized reach": `knowledge/mod.rs:L223-L237`, `artifacts/mod.rs:L329-L344`
- **Finding:** Seven helpers exist in several copies with different behaviour.
  - **SHA-256 hex check:** at least nine of 43 copies accept uppercase; core's `InstructionStamp::validate` accepts what the SQLite validator for the same value refuses.
  - **Canonical-JSON digest:** eight copies; one silently hashes non-canonical JSON on failure.
  - **base64:** four hand-written codecs although core depends on the `base64` crate; `base64url_decode` accepts both alphabets.
  - **Protected branches:** five restatements. `data.rs` checks `refs/heads/main` only. The desktop passes `protected_branches: &[]`, so a repository whose default branch is `develop` or `trunk` has no force-push or branch-delete protection.
  - **URL redaction:** two implementations with different parameter lists; `?nonce=` is hidden by the browser and kept by `web.fetch`.
  - **Scope enums:** `DataScope`, `KnowledgeScope` and `ConfigScope` are the same enum converted by ten `match` blocks.
  - **Authorized reach:** `knowledge::reach` fails closed; `artifacts::authorized_scope` returns `Workspace` for a non-human principal with no binding and never checks `active`. `instructions::draft_confinement` uses the permissive copy.
- **Impact:** A value core accepts can be refused at the store. The same value can hash differently by caller. An agent with a missing or inactive binding writes an unconfined workspace instruction draft.
- **Suggested fix:** One function each in `fingerprint`, `authority` and `protocol`; use the `base64` crate; resolve the repository's default branch and pass it in.
- **Confidence:** High on the drift.

#### [F-057] Merge parallel implementations inside the Knowledge, artifact and convention services
- **Category:** redundant
- **Severity:** Medium
- **Location:** `crates/devworks-core/src/services/knowledge/mod.rs:L1406-L1600`; `knowledge/graph.rs:L515-L729`; `artifacts/evidence.rs:L37-L135`; `artifacts/observed.rs:L36-L175`; `crates/devworks-adapters/src/fs/artifacts.rs:L421-L530`, `L701-L716`; `conventions/manifest.rs:L244-L275`; `conventions/manifest_writer.rs:L105-L170`; `conventions/importing/parser.rs:L291-L700`; `crates/devworks-adapters/src/storage/sqlite/conventions/imports.rs:L710-L713`; `ports/retrieval.rs:L1-L33`
- **Finding:** Five pairs do one job twice.
  - Two Knowledge bundle builders; supersession walks 128 steps in one and 5 in the other.
  - `publish_evidence` and `publish_observed` are the same pipeline; only the second re-checks authority inside the write.
  - Three "verify then serve" paths in the artifact store; one forgets a corrupt entry and one does not, and `open` re-hashes the whole file every call.
  - The convention manifest format has two hand-written writers, an 890-line hand-written parser and a JSON Schema; convention imports insert into `artifacts` with their own SQL.
  - Conventions has a complete hybrid retrieval path; Knowledge has full-text search plus a `VectorIndex` port whose only method reports a mode. There are three tokenizers.
- **Impact:** `knowledge.read` and `knowledge.subgraph` can report different conflicts for the same claim. Browser and desktop evidence can still be published after the principal was revoked mid-action. A field added to one manifest writer is silently missing from the other.
- **Suggested fix:** Express each pair as one function with a parameter. Decide whether Knowledge gets vector retrieval and remove the stub port if not.
- **Confidence:** High on the duplication.

#### [F-058] Replace string-typed domain values and untyped JSON with types
- **Category:** bad-pattern
- **Severity:** Medium
- **Location:** `crates/devworks-core/src/services/usage/accounting.rs:L149-L159`; `usage/reports.rs:L451-L458`, `L662-L668`; `providers/effort.rs:L20-L43`; `router/connections/discovery.rs:L197-L203`, `L398-L409`; `work/allocation.rs:L39-L41`, `L660-L680`; `services/mcp.rs:L549-L559`, `L602-L646`, `L829-L910`; `services/graph/` (39 `model::wire(json!(...))` calls); `crates/devworks-engine/src/runtime.rs:L62-L74`, `L3097-L3175`; `crates/devworks-desktop/src/ops.rs:L108-L126` and about 570 `json!` frame sites
- **Finding:** Attempt state, cost classification, effort, endpoint profile, claim state and delivery state are stored as `String` and matched by hand (`"Admitted"`, `"Transferred" | "Yielding"`). `reports.rs:L457` maps an unknown classification to `("Unknown", "Untracked")` while `L667` maps it to `("Untracked", "Untracked")`. MCP effect payloads are built with `json!` and re-parsed field by field by three functions. Typed graph results are built as JSON and deserialized back. The engine picks post-commit effects with `operation.starts_with("router.")`. Nine desktop modules each hard-code a schema revision; one reads it from the registry.
- **Impact:** A renamed variant or a typo compiles and silently changes accounting, reporting or admission. A renamed field in core surfaces as a runtime `Rejected` on one screen.
- **Suggested fix:** Parse enums at the storage boundary. Give MCP payloads `Serialize`/`Deserialize` structs. Build desktop frames from core's typed inputs with one `frame::<I: Serialize>(op, &input)` that reads the revision from the registry.
- **Confidence:** High.

#### [F-059] Replace re-implemented test doubles with one shared test-support crate
- **Category:** redundant
- **Severity:** Medium
- **Location:** `crates/devworks-engine/tests/common/mod.rs`; `crates/devworks-desktop/tests/common/mod.rs`; `crates/devworks-adapters/src/storage/sqlite/tests.rs`; `crates/devworks-core/tests/common/mod.rs:L739-L4540`
- **Finding:** 14 full `ArtifactFiles` pass-through decorators (about 970 lines), 11 "commit then lose the acknowledgement" store wrappers, 8 test clocks, 11 id sources, 8 hand-rolled temp-directory guards, and about 43 positional `INSERT INTO work_items VALUES(…13 columns…)`. Core's test store is a 4,540-line file with 725 methods that each return "not modelled"; the port traits have no default methods, so every new method needs a stub. 27 of 44 SQLite test files import core services, because core has no usable in-memory store.
- **Impact:** A new method on `ArtifactFiles` means editing 14 decorators. A new `work_items` column breaks about 43 inserts. Domain-service behaviour is tested in the adapter crate against real SQLite, which feeds F-021.
- **Suggested fix:** Add a dev-only `devworks-test-support` crate. Longer term, build a real in-memory `ApplicationStore` that passes the existing contract suite.
- **Confidence:** High on the counts.

### Requirements (medium)

#### [F-014] Wire the remaining capabilities that exist in core but not in the app
- **Category:** requirements
- **Severity:** Medium
- **Location:** `crates/devworks-engine/src/runtime.rs:L304`, `L355`, `L1310` (EVAL-02); `crates/devworks-core/src/services/usage/budget.rs:L217-L231` (ROUTE-11); `crates/devworks-engine/src/package_host/mod.rs:L2074-L2085`; `crates/devworks-core/src/settings/catalog.rs:L573-L653`; `crates/devworks-desktop/src/databases_screen.rs:L229-L237`; `crates/devworks-core/src/services/router/accounting.rs:L9-L16`, `L232-L268`
- **Finding:** Five more capabilities stop at the service boundary.
  - **Automatic grading:** the supervisor grades only suites in `RuntimeConfig.automatic_eval_suites`, which defaults to empty and is never set.
  - **75% budget warning:** recorded as a history event and shown as a chip; nothing delivers it to the coordinator.
  - **`broker.call` for packages:** every `PackageRequest::Call` returns "brokered host calls are not available in this host build".
  - **Settings nothing reads:** `github.pollForChanges` (default on), `delivery.autoMerge`, and the database data read/write switches.
  - **Report categories nothing produces:** `ReportCategory::CodeReview` and `RepositoryAnalysis` are never constructed.
- **Impact:** The shipped app grades nothing. A coordinator runs until the cap pauses it. A user can switch on polling, Auto-Merge or database writes and nothing happens.
- **Suggested fix:** Track each as an item. Hide or label the inert settings until their owners land.
- **Confidence:** High that the wiring is absent.

#### [F-015] Finish outgoing MCP management
- **Category:** requirements
- **Severity:** Medium
- **Location:** MCP-02, MCP-04, DW-45, DW-154, DW-155, DW-157, DW-158; `crates/devworks-core/src/services/mcp.rs:L1650-L1662`, `L2183`; `crates/devworks-desktop/src/mcp_panel.rs:L228-L252`, `L349-L429`
- **Finding:** Connect, discover, select and a sampling toggle exist. No operation or control disables, stops or removes a server. `mcp.servers` returns only the transport label, so the form is never prefilled, and a rename sends an empty transport that resets the catalog and selection. Stdio servers are entered as raw JSON. `mcp.resources` and `mcp.prompts` have no Settings UI. On a transport change `configure` clears capabilities and schemas but leaves `resource_uris`.
- **Impact:** A user cannot remove a server once added and loses the tool selection on rename.
- **Suggested fix:** Merge DW-155 (PR #70), then deliver DW-154, DW-157 and DW-158. All are already tracked.
- **Confidence:** High.

#### [F-060] Bring Help and status documents up to date and run the Help check in CI
- **Category:** requirements
- **Severity:** Medium
- **Location:** HELP-02 (Confirmed), DW-52; `docs/help/check.py:L170-L183`; `docs/help/coverage.json`; `.github/workflows/`; `README.md:L13`; `docs/architecture/visual/README.md`; `docs/architecture/codegraph-work-map.json`; `docs/architecture/implementation-slices.json`; `crates/devworks-core/src/lib.rs:L6-L13`; `CODEGRAPH.md`
- **Finding:**
  - `check.py` requires Help coverage for every registered service; eight are missing (`agents`, `containers`, `conventions`, `desktop`, `embedding_spaces`, `mcp`, `process_tools`, `schemas`). No workflow runs the script, and by reading it fails today.
  - The README's "Not implemented yet" paragraph lists routing, the package host, indexing, Git, plans and readiness. All exist.
  - `docs/architecture/visual/README.md` says routing, packages, indexing, approvals and Git "have no implementation yet"; VIS-03 requires the diagrams to follow the code.
  - The work map and slices files show three complete items as pending.
  - `CODEGRAPH.md` describes a `devworks-contract-probe` crate and an `overgraph` dependency that are not in the workspace.
  - The watcher requirements WATCH-01..12 (DW-F14, agreed 2026-11-04) are in neither requirements file.
- **Impact:** Agents and users have no authored Help for desktop control, process tools, containers or MCP. Readers and agents under-estimate what runs.
- **Suggested fix:** Add the topics, extend the owner map, add `python3 docs/help/check.py` to `ci.yml`, rewrite the README paragraph, and add WATCH-01..12 to the planning sources.
- **Confidence:** High on the gaps; medium that the script fails exactly as read (not run).

#### [F-061] Resolve smaller requirement gaps on complete items
- **Category:** requirements
- **Severity:** Medium
- **Location:** `crates/devworks-core/src/authority/policy.rs:L399-L416` (WORK-02); `crates/devworks-core/src/operations/skills.rs:L84-L123`, `services/instructions/commands.rs:L437-L451` (SKILL-02/03/07); `crates/devworks-core/src/settings/catalog.rs:L623-L633` (DELIVERY-04); `crates/devworks-core/src/services/git/commands/pull_request.rs:L201-L236` (DW-38 A1); `crates/devworks-desktop/src/shell/work.rs:L3196-L3203` (UI-14); `crates/devworks-core/src/services/notifications/mod.rs:L119-L200` (NOTIFY-03); `crates/devworks-core/src/services/schemas/queries.rs` (SCHEMA-02); `crates/devworks-adapters/src/database/network/postgres.rs:L106-L108`
- **Finding:**
  - **WORK-02:** rooted agents are denied `tasks.prioritize`, `tasks.dependency_links` and `features.dependencies`; the desktop sends no `tasks.assign`.
  - **Skills:** `skills.draft` takes no rationale; history omits author and session; `enabled` is always true; nothing links skills to lessons; project-scope skills are refused by an autonomous decision.
  - **DELIVERY-04:** `delivery.mandatoryReview` is workspace-only and `ScopeKind` has no repository scope.
  - **DW-38:** the only Git lifecycle fixture has `parent_feature_id: None`, so the feature-branch PR path is untested.
  - **Overview Activity:** each row shows sequence, actor and raw epoch milliseconds; the event body is not rendered.
  - **Notifications:** only approvals and completions produce them; failures, budget pauses and authentication problems never reach the inbox.
  - **Schemas:** no code-to-schema links.
  - **Database versions:** the readers accept exactly PostgreSQL 17, SQL Server 16, MySQL 8.4 and MariaDB 11.4.
- **Impact:** Each leaves a stated behaviour unbuilt or unproven on an item marked complete.
- **Suggested fix:** File each as a bug or record an owner decision narrowing the requirement.
- **Confidence:** Medium to high per item.

### Efficiency

#### [F-062] Stop doing work proportional to history on hot paths
- **Category:** efficiency
- **Severity:** Medium
- **Location:** `crates/devworks-core/src/services/usage/reports.rs:L706-L754`, `L461-L589`, `L889-L900`; `usage/host.rs:L302-L311`, `L447-L521`, `L540-L560`; `services/graph/operators.rs:L52-L65`, `L579-L596`; `graph/scan.rs:L169-L380`; `knowledge/mod.rs:L426-L560`; `conventions/indexing/query.rs:L454-L497`; `work/allocation.rs:L559-L700`; `services/mcp.rs:L56-L70`, `L148-L163`
- **Finding:**
  - **Usage reports:** every page request re-reads every attempt in the project with four point reads each, then serializes and hashes the whole row set to bind the cursor.
  - **Streaming:** the drive loop runs up to six store queries before every streamed frame; the runtime tick already polls the same check.
  - **Attempt recovery:** one unresolved attempt makes the runtime rescan the full attempt history every interval.
  - **Graph reads:** a node-detail read loads all edges; page N recomputes the whole set; lineage does it twice.
  - **Knowledge:** each claim costs up to six store reads; a text query can issue about 12,000.
  - **Convention search:** every page pages through every convention in scope to report at most 32 gaps.
  - **`next_work`:** one claim costs roughly features × tasks × several reads inside the write transaction.
  - **MCP:** one configuration revision holds every server's full catalog and is rewritten on every state change.
- **Impact:** Cost on user-facing reads and on the single writer grows with total history or project size, not with what is requested.
- **Suggested fix:** Push filters into storage queries, cache per immutable pin, batch child reads, and store MCP catalogs as separate rows.
- **Confidence:** High on the structure; none of it was measured.

#### [F-063] Remove polling and per-frame rebuilds
- **Category:** efficiency
- **Severity:** Medium
- **Location:** `crates/devworks-engine/src/agent_turns.rs:L321-L329`, `L442-L499`; `crates/devworks-desktop/src/shell.rs:L4091`, `L3088-L3190`, `L3750-L3880`; `shell/project_purge.rs:L26-L183`; `backend.rs:L833-L885`; `crates/devworks-adapters/src/mcp.rs:L1022-L1050`; `provider_stream/mod.rs:L34`, `L268-L276`
- **Finding:**
  - A tool call awaiting human approval is re-dispatched every 500 ms: up to eight threads, an argument artifact re-read (up to 8 MB) and a re-submitted command, with no backoff.
  - The desktop `ui()` snapshots all UI state every frame (every draft, full convention markdown, a `Debug` rendering of each draft) and re-serializes it under a mutex.
  - Every runtime hint re-reads every feature and card page with no minimum interval.
  - Three desktop workers serve every query, command and slow network task.
  - Provider streams and database reads wake every 10 ms to poll cancellation; each stdio MCP server holds three blocking threads.
- **Impact:** An agent waiting hours for an approval re-admits its commands twice a second. Frame cost grows with draft size. Three slow calls stall every other request in the app.
- **Suggested fix:** Wait on the approval change with a bounded backoff. Publish the purge snapshot on a dirty flag. Debounce `refresh_visible`. Give network tasks their own pool. Give `Cancellation` a waker.
- **Confidence:** High on the code; medium on frequency under load.

#### [F-064] Tune the SQLite connection and drop dead schema
- **Category:** efficiency
- **Severity:** Medium
- **Location:** `crates/devworks-adapters/src/storage/sqlite/conn.rs:L88-L119`; `migrations/0001_initial.sql`; `project_purge/classification.rs:L190-L194`, `L420-L478`; `repos.rs:L533-L571`; `mod.rs:L199-L212`
- **Finding:**
  - 362 `prepare_cached` call sites against rusqlite's default cache of 16 statements.
  - About 25 tables from migration 0001 that no code writes (`approvals`, `provider_connections`, `knowledge_entries`, `tool_calls`, `usage_entries` and others). Several duplicate a live table's purpose. Purge code still classifies them and runs six partition queries against always-empty knowledge tables.
  - No index for `operations_for_capability`; operations are never deleted.
  - No `wal_checkpoint`, `journal_size_limit`, `PRAGMA optimize` or `ANALYZE` call for 232 indexes.
  - `session_usage` and `job_run_usage` are still written beside the canonical `usage_attempts`.
- **Impact:** Most "cached" statements are re-parsed on the single writer thread. The WAL file never shrinks. Three stores hold the same usage facts.
- **Suggested fix:** Set the cache to a few hundred. One migration drops the unused tables and the matching purge branches. Add `operations(capability,state,id)`. Run `PRAGMA optimize` on clean shutdown.
- **Confidence:** High on the settings; medium on the full dead-table list.

### Tests and CI

#### [F-065] Run or delete tests that never execute, and close CI gaps
- **Category:** dead-code
- **Severity:** Medium
- **Location:** `crates/devworks-adapters/tests/database_metadata.rs` (52 `#[ignore]`); `crates/devworks-desktop/tests/database_network.rs:L97-L119`; `crates/devworks-adapters/tests/processes.rs:L873`, `L914`, `L1017`; `crates/devworks-engine/tests/browser.rs:L4531-L4536`, `L5123-L5128`; `.github/workflows/container-qualification.yml:L4-L13`, `L37`, `L58`; `crates/devworks-engine/tests/conventions_import.rs:L28-L30`; `.github/workflows/ci.yml:L205-L211`, `L264-L281`
- **Finding:**
  - 42 PostgreSQL, MySQL and MariaDB adapter tests, 4 desktop tests and 3 Docker tests are `#[ignore]` and selected by no workflow. DB-06 requires MariaDB and MySQL to be "covered and verified".
  - Two browser tests return early unless an environment variable is set and report as passed.
  - The container qualification `paths:` filter omits the container and process adapters it qualifies, and pulls `alpine:latest`.
  - Two engine tests compile production source files into the test binary with `#[path]`, so they exercise a second copy of the code.
  - About 27 fixed sleeps of 100 ms or more assert "nothing happened" afterwards and can pass falsely on a slow runner.
  - `windows_foundation` runs engine Git and browser tests that `windows_engine` runs again.
- **Impact:** About 1,300 lines of metadata-reader tests can rot unnoticed. A change to the native container adapter does not trigger the only job that exercises it.
- **Suggested fix:** Add service containers to `schema-metadata.yml`. Use `#[ignore]` for the browser tests. Fix the path filter and pin the image. Import the production modules directly. Replace absence sleeps with a pass counter.
- **Confidence:** High.

---

## Low findings

#### [F-066] Guard pane-state persistence against a leftover temp file and a failed strict decode
- **Category:** bad-pattern
- **Severity:** Low
- **Location:** `crates/devworks-desktop/src/project_purge.rs:L52-L86`, `L143-L226`; `shell.rs:L4012-L4034`; `memory.rs:L231-L246`
- **Finding:** `save_checkpoint` opens `ui-state.purge.tmp` with `create_new(true)` and never removes it on a failed write. A failed strict decode sets `state.memory = None`, after which `publish` always errors. `Memory::save` and `save_checkpoint` are two atomic-save implementations.
- **Impact:** After one interrupted write the app may stop saving drafts and unconfirmed command frames.
- **Suggested fix:** Remove a stale temp file at startup; quarantine an undecodable file and continue from defaults; keep one save function.
- **Confidence:** Medium. `interrupted()` at L52–58 detects the temp file, so a recovery path may exist that the reviewer did not trace. I downgraded this from the reviewer's High.

#### [F-067] Make multi-part Windows credential updates atomic
- **Category:** security
- **Severity:** Low
- **Location:** `crates/devworks-adapters/src/secrets/chunked.rs:L44-L89`
- **Finding:** `put` overwrites parts in place and writes the header last; `get` concatenates with no checksum or generation.
- **Impact:** For a secret over 2,558 bytes, a crash during an update returns old and new chunks joined with no error.
- **Suggested fix:** Put a generation and a SHA-256 of the whole value in the header; name parts by generation.
- **Confidence:** High on the logic; Windows only.

#### [F-068] Decide whether dependency install should run package lifecycle scripts
- **Category:** security
- **Severity:** Low
- **Location:** `crates/devworks-core/src/services/process/tools.rs:L963-L1126`, `L295-L299`, `L815-L853`
- **Finding:** The install plan binds the digest of one manifest, then runs `npm install`, `cargo build`, `pip install -r` with scripts enabled. Approving `package.json` does not bind the lockfile; approving `Cargo.toml` does not bind `build.rs`. Separately, `bound_spec` lets any principal use a primary checkout as the working directory, while `install_plan` reserves it for Human/Host.
- **Impact:** The reviewed digest covers less than the code that executes.
- **Suggested fix:** Bind the lockfile when one exists; use `--ignore-scripts` and `cargo fetch --locked` for the install step; use one working-root rule.
- **Confidence:** Medium.

#### [F-069] Tighten what the package broker returns and retains
- **Category:** security
- **Severity:** Low
- **Location:** `crates/devworks-engine/src/package_host/mod.rs:L2152`, `L2169`, `L2233`; `package_host/session.rs:L408-L437`, `L615-L617`; `crates/devworks-extension-protocol/src/package.rs:L726-L746`, `L854-L877`
- **Finding:** Storage failures go to the helper as `format!("{e:?}")`. `forget()` never removes from `invokes`. Dropped events have no counter. The package schema declares length bounds that `validate()` does not check.
- **Impact:** Internal error detail reaches an untrusted process; memory grows per timed-out invocation.
- **Suggested fix:** Fixed message per error class; remove the entry in `forget`; validate each declared bound.
- **Confidence:** High on the code.

#### [F-070] Do not send the internal tool-result record to chat and Responses providers
- **Category:** bad-pattern
- **Severity:** Low
- **Location:** `crates/devworks-core/src/providers/encode.rs:L1104-L1108`
- **Finding:** For an error result, the tool output serializes the host's `ResultBlock` enum, including artifact ids and digests. The Messages profile sends a native `is_error` flag.
- **Impact:** Internal identifiers go to a third-party provider and into the model's context.
- **Suggested fix:** Emit `{"is_error":true,"content":"<joined text>"}`.
- **Confidence:** High.

#### [F-071] Hash picked bundle files with the same safe open as attachments
- **Category:** security
- **Severity:** Low
- **Location:** `crates/devworks-desktop/src/attachments.rs:L51-L80`; `addons.rs:L873-L895`
- **Finding:** Attachments open through `files::open_regular` (`O_NOFOLLOW`, rejects non-regular files); add-ons use `std::fs::File::open`.
- **Impact:** Picking a FIFO as an add-on bundle can block the worker; a symlink is followed.
- **Suggested fix:** One `sha256_file` in `files.rs` built on `open_regular`.
- **Confidence:** High on the drift.

#### [F-072] Replace the backtracking glob matcher
- **Category:** efficiency
- **Severity:** Low
- **Location:** `crates/devworks-adapters/src/fs/mod.rs:L304-L327`, `L389-L419`
- **Finding:** The `*` case is exponential in the number of stars, and `expand` drops non-UTF-8 names through `to_string_lossy()`.
- **Impact:** A crafted pattern stalls the destructive preflight; some files the shell would match are missing from the reviewed list.
- **Suggested fix:** Iterative two-pointer match on `OsStr` bytes.
- **Confidence:** High.

#### [F-073] Report a definite commit failure as a failure, not `CommitUnknown`
- **Category:** bad-pattern
- **Severity:** Low
- **Location:** `crates/devworks-adapters/src/storage/sqlite/writer.rs:L177-L188`
- **Finding:** `TxGuard::commit` maps every `COMMIT` error to `CommitUnknown`, including a deferred foreign-key failure, where the code rolls back and the outcome is known.
- **Impact:** A constraint violation is reported as "commit acknowledgment lost" and its message is discarded.
- **Suggested fix:** If the connection is still in a transaction after a failed `COMMIT`, roll back and return the database error.
- **Confidence:** High on behaviour.

#### [F-074] Decide which dropped write results are intentional
- **Category:** bad-pattern
- **Severity:** Low
- **Location:** `crates/devworks-engine/src/runs.rs:L667-L682`; `dispatch.rs:L925-L933`, `L1397`; `crates/devworks-core/src/services/usage/host.rs:L385-L393`; `crates/devworks-adapters/src/packages.rs:L533-L548`
- **Finding:** Of 311 `let _ =` and 22 `.ok();` sites, most are commented best-effort cleanup. These are not: the write that records an activation's end, the write that records a delivery failure, every dispatcher pass error, and a `sync_dir` that swallows the fsync error.
- **Impact:** A failed final write leaves an activation `Running` with no owner in memory and no diagnostic. A persistent pass failure stops all delivery while health shows the pass counter rising.
- **Suggested fix:** Route these results to diagnostics; keep the last dispatcher error and a consecutive-failure count.
- **Confidence:** Medium. Lease expiry or boot recovery may repair the activation.

#### [F-075] Guard unchecked arithmetic on stored and remote values
- **Category:** bad-pattern
- **Severity:** Low
- **Location:** `crates/devworks-engine/src/runtime.rs:L2168-L2174`; `crates/devworks-core/src/services/router/connections/auth.rs:L222-L230`
- **Finding:** `instructions.len() - old_count` subtracts a stored count with no check. `(seconds * 1000).min(MAX)` multiplies an issuer-supplied `i64` unchecked.
- **Impact:** Panic in debug builds; wrap in release.
- **Suggested fix:** `checked_sub` and `saturating_mul`.
- **Confidence:** Medium; no writer of an inconsistent context was found.

#### [F-076] Remove or wire unused ports, modules and public functions
- **Category:** dead-code
- **Severity:** Low
- **Location:** `crates/devworks-core/src/ports/indexing.rs:L271-L318` (`SemanticEnricher`); `services/github/api.rs`, `github/response.rs` (626 lines); `crates/devworks-engine/src/agent_inbox.rs:L14-L150`; `runtime.rs:L1567`, `L1642`, `L1840`, `L1960`, `L2082`, `L2273`, `L2409`; `crates/devworks-core/src/protocol/shared.rs:L325-L357`, `L503-L562`; `services/runs/recovery.rs:L37-L50`; `runs/substrate.rs:L1405`; `jobs/discovery.rs:L235`; `crates/devworks-desktop/src/backend.rs:L27-L243`; `crates/devworks-engine/src/bin/devworks-package-fixture.rs`
- **Finding:** `SemanticEnricher` has no implementor or caller anywhere. `services/github` is used only by its own files and tests. `RetainedInboxDriver` and seven `Runtime` methods (including `export_diagnostics`) are called only from tests. The early-draft reply types in `shared.rs` have no production user. About 28 `pub fn` have no reference and 51 more are referenced only from tests. A 447-line test fixture builds as a product binary.
- **Impact:** Because they are `pub`, the compiler cannot flag them. Some match the AGENTS.md "land unwired" policy but have no visible tracking. Readers cannot tell which path is live.
- **Suggested fix:** Change `pub` to `pub(crate)` across services so `dead_code` fires, then delete or link each to its item.
- **Confidence:** Medium. This is a name-token search; a macro-generated call would be missed.

#### [F-077] Remove dead branches and stale conditions
- **Category:** dead-code
- **Severity:** Low
- **Location:** `crates/devworks-core/src/authority/policy.rs:L474-L483`, `L555-L559`; `services/projects.rs:L484-L490`; `crates/devworks-adapters/src/storage/sqlite/sessions.rs:L1341-L1359`; `crates/devworks-core/src/services/usage/reports.rs:L501-L509`; `ports/usage.rs:L25`; `crates/devworks-desktop/src/shell.rs:L4973-L4980`
- **Finding:** A coordinator exemption sits inside a condition that makes it always true. `no_proposal` refuses every `proposalId` on three project operations. `create_session` branches on `schema_version < 44`. `attempt()` writes gaps into a throwaway collector. `PriceQuery.access_mode` is written with one vocabulary and overwritten with another. `pending_screen` has an unreachable Approvals branch.
- **Impact:** A reader believes workspace coordinators are exempt from the project-administration denial when they are not.
- **Suggested fix:** Decide the intended coordinator rule; delete the rest.
- **Confidence:** High that each is dead; low on the intended coordinator rule.

#### [F-078] Remove consumer-specific branches from generic services
- **Category:** bad-pattern
- **Severity:** Low
- **Location:** `crates/devworks-core/src/services/queue.rs:L74`, `L483-L500`, `L546-L553`; `services/approvals/service.rs:L138-L149`, `L441-L453`; `approvals/owner_preview.rs:L80-L195`; `services/artifacts/mod.rs` (Help, convention, reply and instruction methods)
- **Finding:** The generic queue hard-codes two consumers. `ApprovalService` special-cases three project operations although a `SavedOperationOwners` port exists; owners are registered in four hard-coded lists. `ArtifactService` keys retention on JSON strings such as `provenance["derived"]["kind"] == "HelpTopic"`.
- **Impact:** Adding one approvable operation needs edits in up to four places; missing one gives `UnsupportedCapability` at decision time.
- **Suggested fix:** Per-consumer hooks registered at composition; each owner implements the port.
- **Confidence:** High.

#### [F-079] Split oversized modules
- **Category:** readability
- **Severity:** Low
- **Location:** `crates/devworks-core/src/services/router/connections/mod.rs` (3,763 lines); `crates/devworks-adapters/src/storage/sqlite/repos.rs` (2,624); `crates/devworks-core/src/services/indexing/worker.rs` (3,600); `crates/devworks-desktop/src/shell/work.rs` (4,831); `crates/devworks-core/src/services/process/tools.rs`
- **Finding:** `connections/mod.rs` owns connection CRUD, OAuth, token refresh, MCP key staging, settings secret staging and model configuration, and re-declares helpers `router/mod.rs` already defines. `repos.rs` is both the composition point and the home of seven unrelated repositories, including the durable queue. `ToolBindings` in the process module also owns container credential and path resolution.
- **Impact:** A change to settings secrets means editing the connection owner. The store's most safety-critical code sits between unrelated code.
- **Suggested fix:** Extract secret staging, the queue repository and a `bindings` module.
- **Confidence:** High.

#### [F-080] Reformat compressed statements and reconsider counting lines in the PR size check
- **Category:** readability
- **Severity:** Low
- **Location:** `crates/devworks-core/src/services/git/commands.rs:L601`, `L604`, `L825`, `L833`; `git/mod.rs:L288` (682 characters); `help/generation.rs:L334-L337`, `L621`, `L634`; `services/graph/operators.rs:L121`; `tools/pr-size/check.py:L26-L43`
- **Finding:** 19 lines in `git/`, 14 in `help/` and 57 in the graph and conventions services exceed 160 characters; 10 exceed 400, some with no spacing at all. They hold history event payloads and artifact provenance. `rustfmt` leaves statements wrapped in `json!` unformatted. The PR size check counts lines, uses `--no-renames` (a pure move counts double), runs the PR's own `.gitattributes`, and has no test.
- **Impact:** The records a reviewer most needs to check are the hardest to read. The 5,000-line limit may be rewarding this style, and it will penalize the refactors this review recommends.
- **Suggested fix:** Build each payload in a named `let` and run rustfmt. Use `-M` in the size check and warn when a PR changes `.gitattributes`.
- **Confidence:** High for the lines; low for the cause.

#### [F-081] Name things for what they hold
- **Category:** readability
- **Severity:** Low
- **Location:** `crates/devworks-engine/src/runtime.rs:L462`, `L2450-L2452`, `L3038-L3081`; `crates/devworks-core/src/services/router/latest.rs:L4-L6`, `L47-L66`; `crates/devworks-adapters/src/storage/sqlite/mod.rs:L21-L22`; `crates/devworks-desktop/src/virtual_list.rs:L1-L5`, `L88-L100`; `crates/devworks-core/src/services/mod.rs:L196-L210`
- **Finding:** `admission: RwLock<bool>` holds `true` when closed, but every reader binds it as `open`. Model publisher is inferred from array position (`FAMILIES[..4]` is OpenAI). `mod.rs` says repositories "are the only place statements exist" while 30 sibling files contain SQL. `virtual_list` claims constant per-frame cost but sums all heights each frame. A doc comment for `with_help` is attached to `with_conventions`.
- **Impact:** The shutdown fence reads backwards at every use. Reordering a model family silently changes its publisher.
- **Suggested fix:** Rename the binding; store `(family, publisher)` pairs; correct the three comments.
- **Confidence:** High.

#### [F-082] Centralise poisoned-lock recovery
- **Category:** readability
- **Severity:** Low
- **Location:** 431 inline `.lock().unwrap_or_else(|p| p.into_inner())` sites (engine 194, core 176, adapters 41, desktop 20); 20 `lock().unwrap()/expect()` sites
- **Finding:** Two poison policies coexist with no visible rule for which applies where.
- **Impact:** Noise, and 20 sites that panic on poison where 431 recover.
- **Suggested fix:** A `LockExt::locked()` extension trait in core; convert both forms deliberately.
- **Confidence:** High on the counts.

#### [F-083] Use one desktop time formatter and one enum-label helper
- **Category:** redundant
- **Severity:** Low
- **Location:** `crates/devworks-desktop/src/routing.rs:L211`, `L403-L411`; `shell/jobs.rs:L318-L326`; `usage_screen.rs:L923-L931`; `shell/decisions.rs:L1467-L1475`; `shell/work/workflow.rs:L480-L488`; `knowledge_screen.rs:L61`
- **Finding:** Five "milliseconds to display string" copies with four formats and four fallback texts; the Usage screen formats in UTC with no zone label while the rest use local time. Six copies of a `wire` enum-label helper; the seventh, in the Knowledge screen, prints the Rust `Debug` name.
- **Impact:** Usage shows UTC unlabelled; Knowledge shows `PascalCase` debug names where other screens show the wire spelling.
- **Suggested fix:** One time helper in `widgets.rs`; one `wire_name` in `devworks_core::protocol`.
- **Confidence:** High.

#### [F-084] Reuse the DW-10 pane binding and give it paging
- **Category:** redundant
- **Severity:** Low
- **Location:** `crates/devworks-desktop/src/shell.rs:L160-L176`, `L5327-L5438`; `widgets.rs:L53-L165`; `chat.rs:L1380-L1382`
- **Finding:** DW-10 promised reusable pane bindings. `list_body` is used by five early lists; every later screen hand-rolls its own generation, cursor, loading and error fields (46 `generation: u64` occurrences). `list_body` cannot page; the chat sidebar prints "Older conversations are not listed yet." past 200 conversations.
- **Impact:** Loading, failed, stale and offline states look and behave differently per screen.
- **Suggested fix:** Extract a `Paged<T>` binding with request, apply and UI; add load-more.
- **Confidence:** High.

#### [F-085] Stop importing core service internals into desktop view code
- **Category:** bad-pattern
- **Severity:** Low
- **Location:** `crates/devworks-desktop/src/routing.rs:L296-L322`; `conventions_transfer.rs:L159-L190`; `shell/work.rs:L29-L30`, `L4775`, `L4804`; `shell/projects.rs:L2117-L2124`
- **Finding:** Fourteen desktop files import `devworks_core::services::*` items. `routing::models_of` re-derives which catalog models a tier pools, to decide which efforts to offer.
- **Impact:** The effort selector can disagree with the router's own pooling.
- **Suggested fix:** Have `router.catalog` return the offered efforts; re-export pure helpers from `protocol`.
- **Confidence:** Medium.

#### [F-086] Archive the one-off evaluation harnesses
- **Category:** dead-code
- **Severity:** Low
- **Location:** `tools/storage-eval/`, `tools/store-port-eval/`, `tools/native-boundary-eval/`, `tools/document-eval/`, `tools/architecture-gates-eval/`; `tools/contracts-eval/probe_*.py`; `tools/planning/probe_router_contracts.py`
- **Finding:** Each of the five directories has exactly one commit (the initial import). Nothing in `.github/`, `tools/ci/` or `crates/` references them. Their READMEs call them "disposable". About 5,800 hand-written lines plus 11,200 lockfile lines, including pinned dependencies on rejected engines.
- **Impact:** No CI cost. Noise for agents and reviewers.
- **Suggested fix:** Move them to an archive branch or tag and leave a pointer in the docs that cite them.
- **Confidence:** High that nothing runs them.

#### [F-087] Fix tracker bookkeeping on merged items
- **Category:** requirements
- **Severity:** Low
- **Location:** DW-159; DW-31 A1; DW-69 A1/A2; DW-83 AC1; `crates/devworks-core/src/authority/deletion.rs:L144-L330`
- **Finding:** DW-159 merged as PR #71 with all criteria still "unmet". DW-31 cites engine tests that live in the SQLite test tree. DW-69's report admits "briefing paths validate but scope nothing" yet marks both criteria met. DW-83 says an agent can prepare a purge preview, but `projects.prepare_purge` is denied to rooted agents. The SEC-04 repository-deletion confirmation is called only from tests.
- **Impact:** Minor traceability errors of the same kind as F-006.
- **Suggested fix:** Close out DW-159 with evidence; correct the rest.
- **Confidence:** High.

---

## Informational

#### [F-088] What is clean
- **Category:** readability
- **Severity:** Info
- **Location:** `crates/devworks-core/src/gateway.rs`, `admission.rs`; `crates/devworks-adapters/src/storage/sqlite/writer.rs`, `conn.rs`; `crates/devworks-engine/src/dispatch.rs`; `crates/devworks-core/src/providers/`; `services/web/egress.rs`, `web/fetch.rs`; `services/git/commands/push.rs`; `crates/devworks-extension-protocol/`; `.github/workflows/`
- **Finding:** These held up under review.
  - Crate dependency direction is correct everywhere; no `todo!`, `unimplemented!`, orphan modules or commented-out code.
  - The admission path is single: `Gateway` is constructed in two production places.
  - One writer thread owns the only write connection; every job runs in one `BEGIN IMMEDIATE`; readers are verified `query_only`. No SQL is built from caller input anywhere in the store or the database adapters.
  - The dispatcher re-checks fenced claims inside every transaction and detects sleep/wake.
  - One encoder, one decoder and one SSE decoder serve every provider profile. No credential was found in a log, error or stored record. No double release in budget arithmetic.
  - `web.fetch` pins the connection to the checked address and re-checks every redirect hop. Git push uses an isolated Git directory and an explicit lease.
  - Process launch has one `host.spawn` call in core; unix process-group kill and start-time verification read correctly.
  - Every third-party action is pinned to a commit; no `pull_request_target`; no `continue-on-error`; core codegen is checked for freshness.
- **Impact:** The foundations are sound. The problems are in what was built on top of them item by item.
- **Suggested fix:** None.
- **Confidence:** High for the files read.

#### [F-089] Capability built beyond the requirements
- **Category:** requirements
- **Severity:** Info
- **Location:** DW-67 to DW-70; DW-119; `crates/devworks-core/src/services/git/approval.rs:L93-L634`; `services/work/allocation.rs:L29-L36`, `L1234-L1356`; `services/mcp.rs:L51-L85`
- **Finding:** Agents added, without a source requirement: per-request schema-revision negotiation, durable receipt replies, `artifacts.tag`, embedding spaces, an explicit `idempotency_key` on `next_work` with its own replay ledger beside the gateway's, `notifications.mark_read`, a cap of 128 readiness findings, the reviewed-commit manifest with hook, filter and signer blocking, and the MCP sampling opt-in. Each is recorded as a tracker item or decision, and none contradicts a requirement.
- **Impact:** Extra surface and storage, mostly in the safer direction.
- **Suggested fix:** Confirm the second `next_work` replay ledger is wanted.
- **Confidence:** High for F2–F9; medium for F10–F12.

#### [F-090] Disclosed scope narrowing in agent tools
- **Category:** requirements
- **Severity:** Info
- **Location:** DW-47, DW-48, DW-49; `crates/devworks-core/src/services/containers/commands.rs:L52`; `crates/devworks-adapters/src/desktop.rs:L32-L36`
- **Finding:** Restricted process profiles are refused rather than enforced. Apple containers report `apple_container_lifecycle_unqualified`. Wayland capture and pointer input report unsupported. Each is disclosed in its item.
- **Impact:** None hidden. These are permanent gaps unless tracked.
- **Suggested fix:** File items for confined process profiles and Apple container lifecycle if still wanted.
- **Confidence:** Medium (sampled).

#### [F-091] Catalog operations with no handler
- **Category:** requirements
- **Severity:** Info
- **Location:** `crates/devworks-core/src/registry/catalog.rs`; `crates/devworks-core/src/services/mod.rs`
- **Finding:** 73 of 369 catalog operations have no registered handler: `database.*` 5, `github.*` 6, `git.integrate`, `git.cleanup`, `issues.publish`, `issues.link`, `reviews.*` 7, `releases.*` 4, `analysis.*` 13, `documents.*` 10, `dreaming.*` 7, `imports.*` 4, `mcp_server.*` 4, `repository_docs.generate`, `ui.current_screen`, `workspace.*` 4, `router.decide`, `router.decision_models`, and three `settings.*` reads.
- **Impact:** The gateway answers `NoUseCase` and agents are not offered them, so they are unreachable rather than broken. All belong to items that are not started.
- **Suggested fix:** None beyond F-005 and F-006.
- **Confidence:** High (string scan plus reading the registration code).

---

## Tracker items for the critical and high findings

Created in CodeGraph project DW on 2026-11-11, all in `backlog` with the label `code-review-2026-11-11`.

| Finding | Item | Type | Feature |
|---|---|---|---|
| F-001 compile fix | DW-161 (existing, PR #73) | bug | — |
| F-001 CI on the merge result | DW-180 | task | DW-F12 |
| F-002 `find -exec rm` guard | DW-164 | bug | DW-F3 |
| F-003 purge policy review gap | DW-165 | bug | DW-F4 |
| F-004 Git stop at shutdown | DW-166 | bug | DW-F2 |
| F-005 items in review with no code | DW-167 | task | DW-F12 |
| F-006 coverage overstated | DW-168 | task | DW-F12 |
| F-007 Knowledge rule delivery | DW-169 | bug | DW-F8 |
| F-008 manual Start blocked | DW-170 | bug | DW-F5 |
| F-009 readiness gate | DW-171 | bug | DW-F3 |
| F-010 MCP-06 | DW-153 (existing, PR #69) | story | — |
| F-011 scheduled inference | DW-172 | story | DW-F11 |
| F-012 worktree overlays | DW-173 | bug | DW-F7 |
| F-013 project Artifacts view | DW-181 | story | DW-F2 |
| F-013 run control | DW-182 | story | DW-F3 |
| F-013 Agents screen | DW-183 | story | DW-F5 |
| F-013 Resources detail | DW-184 | story | DW-F3 |
| F-013 queue recovery and history | DW-185 | story | DW-F2 |
| F-013 Knowledge graph views | DW-186 | story | DW-F8 |
| F-013 tool action UI | DW-187 | story | DW-F11 |
| F-013 Current tab | DW-16 (existing, not started) | story | — |
| F-016 agent turn blocking I/O | DW-174 | bug | DW-F5 |
| F-017 desktop frame thread | DW-175 | bug | DW-F4 |
| F-018 install observe | DW-176 | bug | DW-F11 |
| F-019 reservation leak | DW-177 | bug | DW-F6 |
| F-020 graph traversal | DW-178 | bug | DW-F7 |
| F-021 test durability | DW-179 | task | DW-F12 |

## Tracker items for the medium and low findings

Created on 2026-11-11 in four new features, all in `backlog` with the label `code-review-2026-11-11`. Each medium finding has one item; all low findings share one.

| Feature | Findings | Items |
|---|---|---|
| DW-F15 Security hardening | F-022 to F-034 | DW-188 to DW-200, in finding order |
| DW-F16 Defects and architecture | F-035 to F-045 | DW-201 to DW-211, in finding order |
| DW-F17 Shared plumbing and duplication | F-046 to F-059 | DW-212 to DW-225, in finding order |
| DW-F18 Unwired capability, documents, efficiency, tests and minor findings | F-014 | DW-226 |
| | F-060 to F-065 | DW-227 to DW-232, in finding order |
| | F-066 to F-087 (all low findings) | DW-233 |

F-015 (outgoing MCP management) has no new item: DW-154, DW-155, DW-157 and DW-158 already track it.

---

## Themes and systemic observations

1. **Each item built its own plumbing.** The plan's cost model (one implementation, one review against item-local criteria, merge on passing CI) leaves no step where anyone looks across items. Pagination, cursors, error mapping, row decoding, effect consumers, worker lifecycles, digests, base64, redaction and time formatting were each written between three and forty times. The copies have drifted in ways that change behaviour (F-046 to F-058). A short "shared kernel" feature would remove most of it: `services/support.rs`, `sqlite/support.rs`, one cursor helper, one effect trait, one `Ticker`, one test-support crate.

2. **"Done" stops at the service boundary.** The shared criterion "expose the same service to native UI and applicable agent tools" was marked met when the operation was registered. The desktop surface, the engine wiring or the context seam was left as a note in the acceptance report, and the note was never filed as an item (F-007, F-011 to F-015). Acceptance reports are honest; the status field is not.

3. **The tracker counts inheritance as coverage.** A complete item that inherits a requirement marks it covered, whether or not the item built it. Combined with the PR-linking webhook (F-005), the board overstates delivery in every feature (F-006).

4. **String-keyed rules fail open.** Authority lists, principal kinds, post-commit effects, attempt states and schema revisions are matched as strings, and several fall through to allow, to Host, or to a default (F-027, F-028, F-058).

5. **Lists that must be kept in sync by hand.** Shutdown order (F-004), the restorable-command list (F-039), the purge policy and version gate (F-033), the Help owner map (F-060), migration checksums (F-033) and screen registration (F-044) each silently go wrong when one entry is forgotten. Several already have.

6. **Late additions are the least settled.** MCP-06 is reversed pending an unmerged fix, the readiness gate depends on a declared purpose, outgoing MCP lacks stop and remove, Help has not followed later capabilities, and the watcher requirements are not in the planning documents.

7. **Test cost is structural.** Nearly every test opens an on-disk SQLite store at production durability, service behaviour is tested in the adapter crate because core has no in-memory store, and Cargo links 85 integration test binaries. The long tail of "make test X deterministic" bugs is the visible symptom (F-021, F-059).

---

## Coverage notes

**Method.** Thirteen reviewers ran in parallel, each briefed on one scope and told to verify claims against code and quote lines. I re-read the code behind 17 findings (marked "Verified: lead") and ran one workspace type-check. No tests ran. No timing was measured; every efficiency claim is an inference from code.

**Depth by scope.** No scope was read in full.

| Scope | Lines | Read |
|---|---|---|
| Core foundation (gateway, authority, settings, ports, protocol) | ~35k | Gateway, admission, context, authority and the HTTP and secrets ports in full; `ports/storage/*` sub-files and 15 other ports not opened |
| Work and coordination services | ~40k | Eight files in full, eleven in large part; `sessions/turn.rs`, most of `approvals/` not opened |
| Indexing, graph, schemas, conventions, knowledge | ~40k | About 9k in full; `indexing/` got a structural pass only (`worker.rs`, 3,600 lines, signatures only); `graph/analysis*`, `graph/search.rs` not opened |
| Router, usage, prices, MCP, evals, providers | ~32k | Encode, SSE, usage decode, accounting, budget, MCP in full; `router/select.rs`, `connections/lifecycle.rs`, `embedding_spaces/`, four decoders not opened |
| Artifacts, browser, packages, Git, containers, instructions, diagnostics, help, web, process, desktop | ~47k | About 12k. `packages/policy.rs`, `git/approval.rs`, `git/commands/pull_request.rs`, `instructions/commands.rs`, most of `help/` not opened. **Treat this scope as the least covered.** |
| SQLite store (non-test) | ~38k | Core files and purge gate in full; 20 repository files by grep only |
| Other adapters | ~32k | HTTP, MCP, process, filesystem, secrets, network databases in full; `browser.rs`, `desktop.rs`, `index/manifests.rs`, `index/endpoints.rs`, Linux accessibility not read |
| Engine | ~30k | Dispatch, runtime, package session in full; `providers/driver.rs`, `context_epoch.rs`, `instruction_guard.rs` not read |
| Desktop | ~50k | Backend, host, purge in full; about 40% of `shell.rs`; most `*_screen.rs` by grep only |
| Tests, tools, CI | ~160k tests | Workflows and harnesses in full; test bodies by grep and count |
| Requirements F2–F6 | — | 23 items criterion by criterion; DW-F6 shallower |
| Requirements F7–F14 | — | 18 items criterion by criterion; 11 sampled; F10–F14 shallower |

**Not reviewed.**
- Generated files (`protocol/records.rs`, `protocol/results.rs`, `operations/*.rs`, `registry/catalog.rs`, `tools/contracts-eval/src/domain_generated.rs`, lockfiles).
- `docs/` content other than the requirements, traceability and status files used for conformance. The HTML prototype (`docs/planning/workbench.js`) was not reviewed.
- `tools/planning/*` and the Rust sources of the five archived eval harnesses.
- Dependency CVEs. No `cargo audit` or `cargo deny` was run.
- Windows and Linux behaviour; everything was read on macOS.
- Open PRs #67–#70 and #73 were not reviewed; findings describe `main` at `f707b520`.

**Known limits on confidence.**
- Dead-code findings rest on name searches across `crates/`; a macro-generated call would be missed.
- The requirements checks trusted the tracker's item text as the statement of each criterion.
- Reviewer findings I did not re-read carry the reviewer's stated confidence. A reviewer handed a hypothesis tends to confirm it; the briefs named suspected patterns, so "redundant" findings in particular deserve a second look before large refactors.
