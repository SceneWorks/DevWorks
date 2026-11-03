# Review record: evaluation and validated self-improvement (EVAL)

Status: **decided 3 November 2026.** The user accepted every recommended option for D1–D9. This file
is the review record: the rationale, options and prototype evidence behind the decisions. The
requirements themselves now live in `tools/planning/requirements_expansion.py` (section 20,
"Evaluation and validated self-improvement") and the generated requirement documents, with 6
Confirmed and 8 Proposed. The decisions are in `requirements-interview.json` (ids `eval_*_20261103`),
and the tracked feature is CodeGraph **DW-F13**. Where this file and those sources differ, the sources
win. The requirement text below is the reviewed draft, kept for the record.

## Why

Harness self-improvement is a confirmed goal (SKILL-07, SKILL-09, SELF-01). Agents may draft
improvements to skills, role instructions, overrides and harness code; a person approves them. What
is missing is proof that a change helps. SKILL-08 says an agent's claim that a skill is improved must
not be presented as demonstrated effectiveness, and leaves "validation and recovery mechanics" to be
defined. KNOW-05 says the same of lessons: recurrence alone is not proof of effectiveness.

This draft defines that mechanism as **evals**: graded measurements of how the harness behaves,
collected from real work and from repeatable trials, compared across revisions of what produced the
behavior, and attached to every improvement proposal.

The design is grounded in a working prototype for one skill (`codegraph-feature`, in
`~/.claude/skills/codegraph-feature/evals/`). On 3 real runs:
- 11 of 39 completed items got a `claim_lost` refusal on a completion write.
- 5 of 15 merges had no recognized paginated read of review bodies beforehand. A manual search of
  those runs, subagents included, found no other command that read them. The grader matches command
  patterns, so this is a proxy measure.
- The 3.3 pushes per PR looked high, but a three-vote judge found 28 of the 29 it could decide
  justified.

Four prototype results bear directly on this draft.
- Trials against a frozen feature snapshot first scored 0 because the stand-in tracker had no tool
  schemas, so the agent guessed the argument names. That was a case defect, not skill behavior. With
  the real schemas, 3 of 3 runs passed at $0.74 to $0.87 each (EVAL-04).
- Two judge models given the same nine acceptance reports agreed on only five verdicts (EVAL-03,
  EVAL-05).
- With the same model and rubric, re-judging moved a rubric from 29/30 to 23/29. With three votes
  per instance, 17 of 89 instances still had split votes (EVAL-03).
- Tightening one rubric moved its result from 23/25 to 14/25 on unchanged evidence. A measurement
  change alone can look like a regression (EVAL-06, EVAL-09).

## Existing requirements this builds on

| Requirement | What EVAL relies on |
|---|---|
| HISTORY-01/02 | Durable, queryable record of transcripts, tool actions, Git, reviews and outcomes: the input to production grading |
| SKILL-03, KNOW-06, CONV-02 | A session records the skill, rule and convention revisions it received: the key for comparing revisions |
| SELF-02 | Running version vs candidate build: the key for harness-code changes |
| SKILL-07/08/09, SELF-01/03 | Agent-authored changes need human approval; SKILL-08 needs validation evidence |
| KNOW-04/05, DREAM-01..05 | Lessons, recurrence and dreaming proposals: inputs to diagnosis |
| TEST-02/03 | Unit and integration tests never use real inference; real inference belongs in a separate live suite |
| ROUTE-08/17, JOB-06/09 | Routed inference with attributed usage; feature budgets; schedules without spending limits |
| REVIEW-01, NOTIFY-03 | The approval queue and notifications |
| EXT-01 | Packages may contribute skills, role instructions and verification gates |

## Proposed requirements

### Suites and grading

**EVAL-01 (Proposed).** The harness must support eval suites that measure the behavior produced by
harness skills, role instructions, harness-specific overrides, conventions and lesson rules delivered
to sessions, harness code builds, and model/tier routing assigned to roles. A suite is a versioned
asset with its own cases, graders, rubrics, fixtures and accepted exceptions, stored separately from
the subject it measures, with origin, location and active revision shown as for skills under
SKILL-04. Shared suites, harness-specific suites and package-contributed suites (for what the package
contributes) are supported. *Scope of subjects is decision D1.*

**EVAL-02 (Proposed).** Completed and stopped work must be graded automatically from durable history
without re-running it: transcripts and tool calls of coordinators and workers, Git, PR and review
records, and the resulting work state. This applies to every harness adapter's normalized session
record, including external harnesses. Attribute what is graded to the revisions in effect when it
happened (skills, roles, overrides, conventions, rules, routing, running app version) at the step
level. Safe-step adoption (SKILL-05) means one session can span several revisions; a result that
cannot be attributed to a single revision belongs to an explicit mixed-revision group, never silently
to one side. Grading reads only; it never changes work, repositories, remotes or Knowledge.

**EVAL-03 (Proposed).** Support four grader kinds. Deterministic checks run over the event record.
Outcome checks run over work state. Model-graded rubrics run through the internal router. Human
verdicts are recorded by a person. Every grader names the requirement, skill section or rule it
enforces. Checks that can only measure a proxy (for example a command pattern rather than intent)
must say so.
- A model-graded result records the judge model and settings, rubric revision, sampled instances and
  their population, and each verdict with its reason. A judge verdict must never be presented as a
  deterministic result.
- Each judged instance gets several independent votes, and split votes are reported. A judge
  configuration is calibrated against human verdicts on a sample before its results can gate an
  approval under EVAL-05, and its agreement rate is shown with its results. A change of judge
  model, settings or rubric is a measurement change under EVAL-06.
- All model-based judging, including passive production grading, is live inference. It uses
  explicitly configured router connections under TEST-03 and never runs inside unit or integration
  tests. Tests of the eval machinery itself use scripted judges and fixtures under TEST-02.

**EVAL-04 (Proposed).** Support repeatable trials. A trial runs agent work against frozen inputs,
such as snapshots of work records and repository revisions, in an isolated workspace.
- Trials may write, but only to disposable copies: throwaway repositories and remotes, and stateful
  substitutes for the tracker, GitHub and other external services. Claiming work, implementing,
  reviewing and delivering can then be evaluated end to end. Production destinations, real accounts
  and real credentials are unreachable from a trial. A case may also refuse writes, as a plan-only
  case does.
- Substitute services expose the real services' tool schemas, so the agent sees the same interface
  as in production.
- Each case runs a configured number of times. Results report each run's score, the share of runs
  that passed, and whether every run passed.
- Trials use real inference, so they belong to the live suite under TEST-03 and never run as part of
  unit or integration tests.
- Each snapshot records its source, time, content identity and expected answer.

**EVAL-14 (Proposed).** Every result, at the level of instances, runs and suites, has one explicit
outcome: passed, failed, unclear, not applicable, evidence unavailable (for example a truncated
external transcript or a missing record), grader error, cancelled, or budget exhausted. Coverage is
reported alongside every rate: how many instances were eligible, graded and not graded, and why. Only
passed and failed outcomes enter pass and fail rates. Incomplete evidence must never count as a pass
or silently leave a denominator. Each suite declares, per metric, the minimum usable sample and the
required coverage a result needs to count as evidence under EVAL-05.

### The improvement gate

**EVAL-05 (Proposed).** Every change to an evaluable subject (EVAL-01) must carry eval evidence. The
subjects are skills, role instructions and overrides (SKILL-07, SKILL-09), conventions and lesson
rules, routing assigned to roles, and self-improvement builds (SELF-01).
- **When.** For changes that need approval, run the applicable trials against the baseline and the
  candidate before the proposal is presented. For changes that take effect without approval (a
  user-saved edit, a dreaming consolidation under DREAM-05, a routing change by the user), record the
  comparison when it can run and always monitor after adoption under EVAL-07. Eval evidence adds no
  approval step where none exists. *How this interacts with DREAM-05 is decision D9.*
- **What.** The comparison shows the target metric's change, every other metric's change with its
  method and uncertainty (EVAL-09), the number of runs, and coverage (EVAL-14). For behavior no trial
  exercises, the proposal states that validation happens after adoption under EVAL-07.
- **Status.** Evidence is labeled **improved**, **regressed**, **inconclusive** or **unvalidated**.
  Each suite declares, per metric, its minimum usable sample and required coverage (EVAL-14).
  Evidence is **unvalidated** when there is no applicable suite, the suite could not run, or either
  arm falls below those minima, for example after most results came back unavailable, errored or
  cancelled. **Inconclusive** is reserved for a comparison that met the minima in both arms but
  could not establish a meaningful difference. Eval evidence informs approval and never replaces
  it; a passing eval is not approval. *Whether failed or unvalidated evidence blocks approval is
  decision D2.*
- **Binding.** Evidence is bound to immutable identities: the candidate and baseline content, suite
  and case revisions, fixture snapshots, grader and judge configurations, accepted exceptions, and
  trial settings (models, tools, budgets). Both arms of a comparison use the same measurement
  revisions and settings, except for the declared subject under test. When the subject is a routing
  or model assignment, the worker models differ by design; every other setting, including the judge
  configuration, stays the same. Approval binds to the evaluated candidate identity. Any later
  change to the candidate or to a measurement input it depends on invalidates the evidence; the
  proposal returns to **unvalidated** until it is re-evaluated.

**EVAL-06 (Proposed).** A subject under evaluation must not be able to change its own measurement.
Changes to suites, graders, rubrics, fixtures or accepted exceptions are separate proposals, with
their own history and approval. One proposal must not change both a subject and a suite that
measures it. Accepted exceptions (waivers) are authored by a person and record the check, the matched
instance, the reason and the approver. *Who may draft graders is decision D5.*

**EVAL-07 (Proposed).** After a change is adopted (at the next safe step under SKILL-05, on activation
of a convention, rule or routing revision, or on installation under SELF-03), production grading must
continue and compare the new revision's results with the previous revision's under EVAL-09. When a
monitored metric regresses past its configured criterion, notify the user with the evidence and
propose a rollback through the subject's own activation mechanism:
- **Skills, roles and overrides**: revision-aware rollback under SKILL-08.
- **Conventions and lesson rules**: their revision and lifecycle mechanisms (CONV-02, KNOW-03, DREAM-04).
- **Routing**: the settings revision for the affected role.
- **App builds**: reinstalling a prior build. This requires approval under SELF-03, a compatibility
  check of stored data and migrations made since that build, and the SELF-02 restart and rollback
  mechanics. A build whose data changes cannot be safely reversed is reported as not rollback-capable,
  with the reason.

Every rollback preserves later unrelated edits or reports the conflict without overwriting them, and
is itself a change that EVAL-07 monitors. *Whether rollback may happen without approval is decision
D3; default criteria are decision D7.*

### Diagnosis and honesty

**EVAL-08 (Proposed).** Failing checks must be traceable to the rule, skill section or lesson they
enforce, with links to the exact transcript, tool call or record (HISTORY-01). Recurring failures
should become candidate improvements in the Knowledge and dreaming flow, with the eval evidence
attached: a skill or role edit, a hook, a test, or a harness-code task. They should feed the lesson
recurrence views of KNOW-04/05. Before a failure becomes a proposal, sampled failing instances must
be checked so that a grader defect is not reported as agent behavior; that check and its outcome are
recorded.

**EVAL-09 (Proposed).** Comparisons must be fair and honestly reported.
- **Counts.** Results show counts with denominators, runs, coverage and revision groups, never a bare
  percentage.
- **Method.** Each metric declares its comparison method and uncertainty measure. For example, a
  binary outcome reports the difference in proportions with a confidence interval, and a judged
  rubric includes the judge's calibration (EVAL-03). A difference whose uncertainty includes no
  change, or that falls short of the metric's declared minimum effect, is **inconclusive**.
  Improvement is never claimed from a single run or a single case.
- **Matching.** Trial comparisons use matched settings: the same cases, fixtures, models, tools,
  budgets and measurement revisions in both arms, except for the declared subject under test
  (EVAL-05).
- **Common measurement.** Production comparisons use the same measurement revisions (graders,
  rubrics, judge configurations, accepted exceptions) for both cohorts. When a measurement changed
  between them, regrade the retained evidence of both cohorts under one revision where it is
  available. Where it is not, report the comparison as **non-comparable**; a non-comparable
  comparison never triggers a regression notice or rollback proposal under EVAL-07.
- **Production cohorts.** Production comparisons disclose how the cohorts differ: task mix and size,
  projects, models and routing, and other revisions that changed in the same period. A change that
  coincides with another change to the same behavior is reported as confounded rather than credited
  to either.
- **Held-out cases.** Where possible, a proposal is validated on cases other than the ones that
  motivated it, and results say which were which.

### Cost, control and access

**EVAL-10 (Proposed).** Trials and model-graded judging run through the internal router with
attributed usage and costs under ROUTE-17, linked to the suite run and to the proposal they validate.
Suites can run on demand, automatically when a proposal is created (EVAL-05), and on a schedule.
Before a trial run starts, show its expected run count and cost estimate. *Spending limits for eval
runs, and how they fit with JOB-09's confirmed rule that scheduled inference jobs have no spending
limits, are decision D4.*

**EVAL-11 (Proposed).** Provide a place to inspect suites and results: each suite's cases and graders,
the latest results by revision, trends across revisions, failing instances with links to their
evidence, accepted exceptions, and the eval comparison attached to each proposal in the approval queue
(REVIEW-01). *Where this lives in the UI is decision D6.*

**EVAL-12 (Proposed).** Agents must be able to list suites, read results and evidence, run permitted
suites and draft suite changes through the same operations the UI uses, under normal tool access,
project scope and authority. An agent cannot approve proposals, accept exceptions, or activate suite
changes without the approval EVAL-06 requires.

**EVAL-13 (Proposed).** Suites are documented under SELF-04: what each measures, what each grader
checks and cannot check, how to run it, and how to read its results. Suite documentation ships with
the suite's version.

## Changes to existing requirements

- **SKILL-08**: replace "Define validation and recovery mechanics" with a reference to EVAL-05
  (validation) and EVAL-07 (post-adoption monitoring and rollback proposal).
- **TEST-03**: note that trials under EVAL-04 and all model-based judging under EVAL-03, including
  production grading, are live inference. They share the live suite's configuration, isolation and
  attribution rules.
- **JOB-09**: depends on decision D4.
- **KNOW-05**: note that a lesson rule's effectiveness may be shown by eval evidence under EVAL-08.
- **SELF-01**: note that self-improvement proposals carry eval evidence under EVAL-05.

## Decisions needed

Each decision lists the options and my recommendation with a confidence level.

**D1. What can be evaluated?**
- (a) Everything listed in EVAL-01: skills, roles, overrides, conventions and rules, harness code, and
  routing per role.
- (b) Skills, roles and overrides only.
- Recommendation: (a). Your requirements already let agents propose rule and code changes, and
  routing per role is where cost and quality trade off most. Confidence **medium**: routing evals are
  the least proven part.

**D2. Does failed or missing evidence block approval?** Evidence is improved, regressed,
inconclusive or unvalidated (EVAL-05).
- (a) Evidence only. You can approve anything; the proposal shows its evidence status.
- (b) Gate. Regressed or unvalidated evidence blocks approval unless you override it with a recorded
  reason. Inconclusive evidence is shown but does not block.
- (c) Gate (b) for agent-authored changes; evidence only for your own edits.
- Recommendation: (c). It enforces SKILL-08's point for agents without slowing your own edits.
  Inconclusive does not block: it now means an adequately measured comparison found no meaningful
  difference, and blocking it would stall harmless changes. Evidence below the suite's minimum sample
  or coverage is unvalidated (EVAL-05), so it blocks under (b) and (c). Confidence **medium**.

**D3. Can a regression roll back automatically?**
- (a) Notify and propose a rollback; you approve it.
- (b) Roll back automatically, then notify.
- Recommendation: (a). It matches SKILL-07: an agent-initiated change to the active skill needs your
  approval, and a rollback is such a change. Confidence **high**.

**D4. Do eval runs have spending limits?** JOB-09 is confirmed: scheduled inference jobs report
usage but have no spending limits.
- (a) Budgets for on-demand and proposal-triggered eval runs only. Scheduled eval runs follow JOB-09
  and report usage. No change to JOB-09.
- (b) Budgets for every eval run, including scheduled ones. JOB-09 gains an exception for eval runs.
- (c) No eval budgets anywhere; usage reporting only.
- Recommendation: (a). It keeps your confirmed JOB-09 decision intact, and the unattended spending
  risk comes from proposal-triggered runs, which (a) covers. Confidence **medium**. Measured cost so
  far: $0.74 to $0.87 per plan-only trial run on one six-item feature ($2.39 for 3 runs). Delivery
  trials will cost far more, and I have not measured them.

**D5. Who may draft graders and suites?**
- (a) Agents may draft them; activation needs your approval (EVAL-06).
- (b) People only.
- Recommendation: (a). The separation rule in EVAL-06 removes the conflict of interest, and agents
  are the ones who notice missing checks. Confidence **medium**.

**D6. Where do results live in the UI?**
- (a) A new Evals area, plus the comparison inside each proposal in the approval queue.
- (b) Inside the skill manager (per skill) and the approval queue, with no separate area.
- Recommendation: (a). Suites also cover harness code and routing, which have no skill page.
  Confidence **low**: this is a UI choice.

**D7. Default comparison criteria?**
- Recommendation: do not fix numbers in the requirements. Each metric declares its method and
  minimum effect (EVAL-09) and its minimum sample and coverage (EVAL-14), configurable per suite. The first suites start with
  provisional values labeled provisional (for example 3 runs per trial case), and measured results
  replace them. Provisional values are never acceptance thresholds.
- Confidence **medium**. My earlier numbers (5 runs, 10 points) came from general practice, not
  measurement.

**D8. Where does the work live in CodeGraph?**
- (a) A new feature, for example "Evaluation and validated self-improvement".
- (b) Inside DW-F8 (Knowledge, documents and instructions), next to DW-34, which owns SKILL-08.
- Recommendation: (a). EVAL spans skills, harness code, routing, history and the UI, which is wider
  than DW-F8. DW-34 then depends on its validation item. Confidence **medium**.

**D9. How does evaluation apply to dreaming's automatic consolidations?** DREAM-05 is confirmed:
dreaming applies non-conflicting consolidations without asking for approval.
- (a) Automatic consolidations still apply without approval. Changes to lesson rules are monitored
  after adoption (EVAL-07), and a regression proposes a revert through DREAM-04.
- (b) Changes to rules delivered to sessions become proposals with evidence, like skills. This
  narrows DREAM-05.
- Recommendation: (a). It keeps your confirmed DREAM-05 decision, and consolidation is mostly
  merging and retiring entries, not new behavior. Confidence **medium**.
