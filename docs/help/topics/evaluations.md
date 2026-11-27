# Recorded evaluations and verification limits

Recorded evaluation measures durable evidence without running a model or mutating the work under test. The available `evals.grade` operation persists its own report; `evals.results` reads reports with current scope and bounded evidence. A supplied suite snapshot is the measurement for that run, not an activated gate or permission to rewrite acceptance criteria.

Choose the exact session/work, suite and evidence source supported by the current operation contract. Reports retain source identity, evaluator/suite versions, coverage and actual outcomes. Distinguish a failed behavior, a grader error, missing/unvalidated evidence, and an inconclusive result. A closed conversation or an agent's success statement is not verified completion.

Automatic recorded grading is not yet active: a suite can be versioned and activated (below), but activating one does not yet grade completed and stopped work automatically. Grade a session with `evals.grade`. When suites can be activated, automatic grading uses actual settled work/evidence boundaries and must not mistake an open durable session for a live owner. Do not repeatedly grade mutable evidence and present it as a comparable historical measurement. Use the report's source/suite identity when comparing results.

## Eval suites

An eval suite is a folder kept apart from the skills, roles and other subjects it measures. Shared suites are read in place from `~/.agents/evals` and never written. DevWorks keeps its own suites in the `evals` folder of its data folder. A suite folder holds:

- `SUITE.md`, its documentation, with four sections: `## What it measures`, `## Graders` (a `### <check id>` part for every check, with a `Checks:` line saying what it checks and a `Cannot check:` line saying what it cannot), `## How to run` and `## Reading results`;
- `suite.json`, the subjects it measures (`measures`) and its deterministic checks (`checks`);
- `exceptions.json`, the exceptions a person accepted, which only DevWorks writes;
- cases, graders, rubrics and fixtures under `cases/`, `graders/`, `rubrics/` and `fixtures/`. Any other file is a document.

A folder that is missing a section, a grader note or a valid `suite.json` is not recorded, and the listing does not show it. A suite never holds a skill (`SKILL.md`). A skill proposal whose files hold a suite (`SUITE.md` or `suite.json`) is refused, so one proposal never changes both a subject and the suite that measures it.

Every change makes a new, unchangeable revision. `evals.suites` lists each suite with its origin (shared or harness), its folder, its active revision and its latest revision. `evals.suite` reads one revision, and `evals.suite_history` lists every revision with its author, rationale and approval outcome.

- **A person's change** (`evals.draft_suite`) is approved and active at once. A change to a shared suite becomes a DevWorks suite of the same name.
- **An agent's change** is a draft that changes nothing. The agent proposes its exact revision with `evals.propose_suite`. Only after a person approves that proposal in Approvals does `evals.activate_suite` make it active.
- **A change made in a suite folder on disk** is recorded as a revision that waits for a person to confirm it with `evals.activate_suite`. The previous revision stays active until then. A folder change that alters `exceptions.json` is never confirmed.
- **A person can restore** any earlier approved revision with `evals.activate_suite`.

Only a person can accept an exception, with `evals.accept_exception`. It names the check, the matched instance (session, sequence, entry and kind) and a reason. DevWorks records the exception with the person as its approver in a new approved, active revision. An exception for an unknown check, or a second exception for the same instance, is refused. An agent's later draft keeps the accepted exceptions unchanged.

Live model judges, rubric calibration, inference trials and runtime gate activation are separate capabilities. Their schemas or planned UI do not make them available. Ordinary unit/integration tests must not execute learned model inference. A real inference trial needs its explicitly authorized isolated runtime and evidence; Help does not dispatch one.

See [Git and delivery](help:git-delivery), [Readiness](help:features), [History](help:overview), and [Capability reference](help:capability-reference).
