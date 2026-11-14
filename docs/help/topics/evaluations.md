# Recorded evaluations and verification limits

Recorded evaluation measures durable evidence without running a model or mutating the work under test. The available `evals.grade` operation persists its own report; `evals.results` reads reports with current scope and bounded evidence. A supplied suite snapshot is the measurement for that run, not an activated gate or permission to rewrite acceptance criteria.

Choose the exact session/work, suite and evidence source supported by the current operation contract. Reports retain source identity, evaluator/suite versions, coverage and actual outcomes. Distinguish a failed behavior, a grader error, missing/unvalidated evidence, and an inconclusive result. A closed conversation or an agent's success statement is not verified completion.

Automatic recorded grading is not yet active: this build activates no suites, because suite versioning and approval are not built yet, so completed and stopped work is not graded automatically. Grade a session with `evals.grade`. When suites can be activated, automatic grading uses actual settled work/evidence boundaries and must not mistake an open durable session for a live owner. Do not repeatedly grade mutable evidence and present it as a comparable historical measurement. Use the report's source/suite identity when comparing results.

Live model judges, rubric calibration, inference trials and runtime gate activation are separate capabilities. Their schemas or planned UI do not make them available. Ordinary unit/integration tests must not execute learned model inference. A real inference trial needs its explicitly authorized isolated runtime and evidence; Help does not dispatch one.

See [Git and delivery](help:git-delivery), [Readiness](help:features), [History](help:overview), and [Capability reference](help:capability-reference).
