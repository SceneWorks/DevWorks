//! Deterministic, read-only grading of durable work. Evaluation persists only its own reports;
//! the evaluator receives read ports, never work, Git, Knowledge or external-service writers.
//! Supplied suite snapshots are measurements for this run, not activated or gating suites.
pub mod attribution;
pub mod automatic;
pub mod records;

use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, evals as ops};
use crate::ports::storage::{
    ApplicationStore, ReadOptions, ReadSnapshot, RevisionConflict, SessionRead, StorageError,
    StoredEntry, StoredEvalRun, StoredSession, WriteOptions, WriteTransaction, read_snapshot,
    run_write,
};
use crate::ports::{
    Clock, CommandHandler, Decided, HandlerFailure, IdKind, IdSource, Preparation, Prepared,
    QueryAnswer, QueryHandler,
};
use crate::protocol::provider::{Digest, TranscriptBody, UserContent};
use crate::protocol::records::*;
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, Check, DynamicJson, Probability};
use crate::protocol::{CorrelationId, DomainErrorCode, DomainFault, Id};
use crate::services::cursor::CursorBinding;
use crate::services::sessions::{
    Failure, Refusal, counter, decided, query_fault, rev, visible_session,
};
use sha2::{Digest as _, Sha256};
use std::sync::Arc;

pub const OPERATIONS: [&str; 2] = ["evals.grade", "evals.results"];
/// Bounds retained evidence, never silently removes instances from coverage.
const REPORT_BYTES: usize = 512 * 1024;
const RETAINED_BYTES: usize = 384 * 1024;
const RETAINED_INSTANCES: usize = 499;

pub struct EvalService {
    ids: Arc<dyn IdSource>,
    clock: Arc<dyn Clock>,
}
impl EvalService {
    pub fn new(ids: Arc<dyn IdSource>, clock: Arc<dyn Clock>) -> Self {
        Self { ids, clock }
    }
    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }

    /// Host-only automatic grading entry point. Selection/activation of suites is owned by
    /// suite management; the host supplies an already authorized immutable measurement.
    /// The deterministic identity makes crash recovery and repeated terminal observations safe.
    pub fn grade_automatically(
        &self,
        store: &dyn ApplicationStore,
        session_id: &Id,
        suite: &EvalSuiteSnapshot,
    ) -> Result<EvalReport, StorageError> {
        self.grade_automatically_cancellable(store, session_id, suite, &|| false)
    }

    /// Cancellation bounds retained grading work; the remaining population is explicit.
    pub fn grade_automatically_cancellable(
        &self,
        store: &dyn ApplicationStore,
        session_id: &Id,
        suite: &EvalSuiteSnapshot,
        stopped: &dyn Fn() -> bool,
    ) -> Result<EvalReport, StorageError> {
        suite.check().map_err(StorageError::InvalidRecord)?;
        let report = read_snapshot(store, &ReadOptions::default(), |snapshot| {
            let session = snapshot
                .sessions()
                .session(session_id)?
                .ok_or_else(|| StorageError::InvalidRecord("unknown eval session".into()))?;
            if !automatic::automatically_eligible(snapshot.runs(), &session)? {
                return Err(invalid("eval session is no longer terminal"));
            }
            build_report(
                snapshot,
                &session,
                suite,
                self.clock.now_ms().get(),
                stopped,
                true,
            )
        })?;
        // Interrupted automatic work remains retryable after restart. Persisting a cancelled
        // report under this source identity would prevent catch-up from ever grading it.
        if stopped() {
            return Err(invalid("automatic grading stopped"));
        }
        run_write(store, WriteOptions::default(), move |tx| {
            persist(tx, &report, true)
        })
    }
}

fn invalid(what: &str) -> StorageError {
    StorageError::InvalidRecord(what.into())
}
fn text(s: &str) -> BoundedText<1, 4096> {
    BoundedText(s.chars().take(4096).collect())
}
fn empty_revision(epoch: i64, reason: &str) -> EvalRevisionContext {
    EvalRevisionContext::Mixed {
        context_epoch: counter(epoch),
        sources: BoundedList(vec![]),
        reason: text(reason),
    }
}
fn outcome_name(outcome: &EvalOutcome) -> String {
    serde_json::to_value(outcome)
        .expect("outcome")
        .as_str()
        .expect("outcome string")
        .into()
}
/// Every candidate result is represented; only conclusive binary outcomes enter the rate.
fn coverage(outcomes: impl IntoIterator<Item = (EvalOutcome, i64)>) -> EvalCoverage {
    let mut counts = std::collections::BTreeMap::<String, (EvalOutcome, i64)>::new();
    for (outcome, n) in outcomes {
        counts
            .entry(outcome_name(&outcome))
            .or_insert((outcome, 0))
            .1 += n;
    }
    let passed = counts.get("passed").map_or(0, |x| x.1);
    let failed = counts.get("failed").map_or(0, |x| x.1);
    let eligible = counts.values().map(|x| x.1).sum::<i64>();
    let graded = passed + failed;
    EvalCoverage {
        eligible: counter(eligible),
        graded: counter(graded),
        ungraded: counter(eligible - graded),
        passed: counter(passed),
        failed: counter(failed),
        ungraded_reasons: BoundedList(
            counts
                .into_values()
                .filter(|(o, n)| *n > 0 && !matches!(o, EvalOutcome::Passed | EvalOutcome::Failed))
                .map(|(outcome, n)| EvalOutcomeCount {
                    outcome,
                    count: counter(n),
                })
                .collect(),
        ),
        pass_rate: if graded == 0 {
            None
        } else {
            Probability::new(passed as f64 / graded as f64)
        },
    }
}
fn aggregate(outcomes: &[EvalOutcome]) -> EvalOutcome {
    // A failure remains visible even when another instance lacks evidence. A pass requires
    // every instance to pass; coverage always discloses the ungraded population.
    for candidate in [
        EvalOutcome::Failed,
        EvalOutcome::GraderError,
        EvalOutcome::Cancelled,
        EvalOutcome::BudgetExhausted,
        EvalOutcome::EvidenceUnavailable,
        EvalOutcome::Unclear,
    ] {
        if outcomes
            .iter()
            .any(|o| outcome_name(o) == outcome_name(&candidate))
        {
            return candidate;
        }
    }
    if !outcomes.is_empty() && outcomes.iter().all(|o| matches!(o, EvalOutcome::Passed)) {
        EvalOutcome::Passed
    } else if outcomes
        .iter()
        .all(|o| matches!(o, EvalOutcome::NotApplicable))
    {
        EvalOutcome::NotApplicable
    } else {
        EvalOutcome::Unclear
    }
}
fn summarize(instances: &[EvalInstanceResult]) -> (EvalOutcome, EvalCoverage) {
    let outcome = aggregate(
        &instances
            .iter()
            .map(|i| i.outcome.clone())
            .collect::<Vec<_>>(),
    );
    let mut counts = Vec::new();
    for i in instances {
        counts.push((EvalOutcome::Passed, i.coverage.passed.get()));
        counts.push((EvalOutcome::Failed, i.coverage.failed.get()));
        counts.extend(
            i.coverage
                .ungraded_reasons
                .0
                .iter()
                .map(|r| (r.outcome.clone(), r.count.get())),
        );
    }
    (outcome, coverage(counts))
}
fn source(entry: &StoredEntry) -> EvalStepSource {
    EvalStepSource {
        session_id: entry.session_id.clone(),
        sequence: counter(entry.sequence),
        entry_id: entry.entry_id.clone(),
        kind: BoundedText(entry.kind.clone()),
    }
}
fn matched(check: &EvalDeterministicCheck, entry: &StoredEntry) -> bool {
    match &check.predicate {
        EvalPredicate::ToolSucceeded { operation } => {
            entry.kind == "ToolCallRecorded"
                && (operation.0 == "*"
                    || entry
                        .body
                        .pointer("/toolCall/capabilityId")
                        .and_then(|v| v.as_str())
                        == Some(operation.0.as_str()))
        }
        EvalPredicate::TranscriptContains { entry_kind, .. } => entry.kind == entry_kind.0,
        EvalPredicate::RecordFieldEquals { .. } => false,
    }
}
fn evaluate(
    read: &dyn SessionRead,
    session: &StoredSession,
    check: &EvalDeterministicCheck,
    entry: &StoredEntry,
) -> Result<(EvalOutcome, &'static str), StorageError> {
    match &check.predicate {
        EvalPredicate::ToolSucceeded { .. } => {
            let body: TranscriptBody = match serde_json::from_value(entry.body.clone()) {
                Ok(b) => b,
                Err(_) => return Ok((EvalOutcome::GraderError, "Invalid recorded tool call")),
            };
            let TranscriptBody::ToolCallRecorded { tool_call } = body else {
                return Ok((EvalOutcome::GraderError, "Tool-call kind mismatch"));
            };
            let Some(call) = read.tool_call(
                &session.id,
                &tool_call.call.attempt_id.0,
                &tool_call.call.native_call_id,
            )?
            else {
                return Ok((
                    EvalOutcome::EvidenceUnavailable,
                    "Durable tool-call record unavailable",
                ));
            };
            // The response can arrive after the call, but must belong to this captured session.
            if call
                .response_sequence
                .is_none_or(|seq| seq > session.last_durable_sequence())
            {
                return Ok((
                    EvalOutcome::EvidenceUnavailable,
                    "No recorded response at this boundary",
                ));
            }
            Ok(match call.outcome.as_deref() {
                Some("Completed") => (
                    EvalOutcome::Passed,
                    "Recorded tool response completed (proxy)",
                ),
                Some("Failed") => (EvalOutcome::Failed, "Recorded tool response failed (proxy)"),
                Some("CancelledBeforeDispatch") => (
                    EvalOutcome::Cancelled,
                    "Recorded call cancelled before dispatch",
                ),
                Some("ApprovalRequired" | "DependencyBlocked" | "OutcomeUnknown") => (
                    EvalOutcome::Unclear,
                    "Recorded response is not a conclusive result",
                ),
                _ => (
                    EvalOutcome::EvidenceUnavailable,
                    "Recorded response outcome unavailable",
                ),
            })
        }
        EvalPredicate::TranscriptContains { text: needle, .. } => {
            let body: TranscriptBody = match serde_json::from_value(entry.body.clone()) {
                Ok(b) => b,
                Err(_) => {
                    return Ok((
                        EvalOutcome::GraderError,
                        "Invalid recorded transcript entry",
                    ));
                }
            };
            let found = match body {
                TranscriptBody::AssistantAttempt {
                    visible_text,
                    interrupted,
                    ..
                } => {
                    if interrupted {
                        return Ok((
                            EvalOutcome::EvidenceUnavailable,
                            "Assistant text is interrupted",
                        ));
                    }
                    visible_text.contains(&needle.0)
                }
                TranscriptBody::UserMessage { content } => content
                    .iter()
                    .any(|c| matches!(c,UserContent::Text{text} if text.contains(&needle.0))),
                _ => {
                    return Ok((
                        EvalOutcome::GraderError,
                        "Text predicate supports only visible user and assistant text",
                    ));
                }
            };
            Ok(if found {
                (EvalOutcome::Passed, "Literal visible-text match (proxy)")
            } else {
                (EvalOutcome::Failed, "Literal visible-text mismatch (proxy)")
            })
        }
        EvalPredicate::RecordFieldEquals { .. } => Ok((
            EvalOutcome::GraderError,
            "Record predicate addressed as transcript",
        )),
    }
}

fn visit_snapshot(
    snapshot: &dyn ReadSnapshot,
    session: &StoredSession,
    source: &EvalRecordSource,
    visitor: &mut dyn FnMut(records::ObservedRecord) -> Result<(), StorageError>,
) -> Result<(), StorageError> {
    let read = match source {
        EvalRecordSource::WorkState => records::RecordRead::Work(snapshot.work()),
        EvalRecordSource::HistoryEvent => records::RecordRead::History(snapshot.history()),
        _ => records::RecordRead::Sessions(snapshot.sessions()),
    };
    records::visit_records(
        read,
        session,
        source,
        snapshot.stamp().committed_sequence,
        visitor,
    )
}
fn selected_sources(suite: &EvalSuiteSnapshot) -> Vec<EvalRecordSource> {
    let mut names = std::collections::BTreeSet::new();
    suite
        .checks
        .0
        .iter()
        .filter_map(|check| match &check.predicate {
            EvalPredicate::RecordFieldEquals { source, .. } => {
                let name = serde_json::to_string(source).expect("record source");
                names.insert(name).then(|| source.clone())
            }
            _ => None,
        })
        .collect()
}
fn hash_record(hash: &mut Sha256, record: records::ObservedRecord) -> Result<(), StorageError> {
    // The provenance watermark can move on unrelated commits. Only selected source identities
    // and their captured content belong in the measurement's immutable evidence identity.
    let body = crate::jcs::canonicalize(&serde_json::json!({"kind":record.source.kind,
        "id":record.source.entry_id,"body":record.body}))
    .map_err(|_| invalid("record canonicalization"))?;
    hash.update((body.len() as u64).to_be_bytes());
    hash.update(body.as_bytes());
    Ok(())
}
fn snapshot_evidence_digest(
    snapshot: &dyn ReadSnapshot,
    session: &StoredSession,
    suite: &EvalSuiteSnapshot,
    stopped: &dyn Fn() -> bool,
) -> Result<String, StorageError> {
    let mut hash = Sha256::new();
    hash.update(b"recorded-evidence-v1");
    for source in selected_sources(suite) {
        hash.update(serde_json::to_vec(&source).map_err(|_| invalid("record source"))?);
        visit_snapshot(snapshot, session, &source, &mut |record| {
            if stopped() {
                return Err(StorageError::Cancelled);
            }
            hash_record(&mut hash, record)
        })?;
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
fn transaction_evidence_digest(
    tx: &mut dyn WriteTransaction,
    session: &StoredSession,
    suite: &EvalSuiteSnapshot,
) -> Result<String, StorageError> {
    let mut hash = Sha256::new();
    hash.update(b"recorded-evidence-v1");
    let watermark = tx.committed_sequence()?;
    for source in selected_sources(suite) {
        hash.update(serde_json::to_vec(&source).map_err(|_| invalid("record source"))?);
        let read = match source {
            EvalRecordSource::WorkState => records::RecordRead::Work(tx.work()),
            EvalRecordSource::HistoryEvent => records::RecordRead::History(tx.history()),
            _ => records::RecordRead::Sessions(tx.sessions()),
        };
        records::visit_records(read, session, &source, watermark, &mut |record| {
            hash_record(&mut hash, record)
        })?;
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// The evaluator holds only coherent read interfaces and never replays the observed work.
fn build_report(
    snapshot: &dyn ReadSnapshot,
    session: &StoredSession,
    suite: &EvalSuiteSnapshot,
    created_at_ms: i64,
    stopped: impl Fn() -> bool,
    abort_on_cancel: bool,
) -> Result<EvalReport, StorageError> {
    let measurement = sha256_hex(
        crate::jcs::canonicalize(
            &serde_json::json!({"evaluator":"recorded-eval-v1","suite":suite}),
        )
        .map_err(|_| invalid("suite canonicalization"))?
        .as_bytes(),
    );
    let evidence = snapshot_evidence_digest(snapshot, session, suite, &stopped)?;
    let identity = sha256_hex(
        format!(
            "recorded-eval-v1|{}|{}|{}|{}|{}|{}",
            snapshot.stamp().store_id.0.as_str(),
            session.id.as_str(),
            session.revision,
            session.last_durable_sequence(),
            measurement,
            evidence
        )
        .as_bytes(),
    );
    if let Some(existing) = snapshot
        .evals()
        .eval_run(&Id::new(identity.clone()).map_err(|_| invalid("eval id"))?)?
    {
        return serde_json::from_value(existing.report).map_err(|_| invalid("stored eval report"));
    }
    let mut instances = Vec::new();
    let mut seen = vec![false; suite.checks.0.len()];
    let mut omitted = vec![0i64; suite.checks.0.len()];
    let mut cancelled = vec![0i64; suite.checks.0.len()];
    let mut missing_records = vec![0i64; suite.checks.0.len()];
    let invalid_checks: Vec<bool> = suite
        .checks
        .0
        .iter()
        .map(|check| match &check.predicate {
            EvalPredicate::RecordFieldEquals {
                pointer, expected, ..
            } => matches!(
                records::field_outcome(&serde_json::Value::Null, &pointer.0, &expected.0).0,
                EvalOutcome::GraderError
            ),
            EvalPredicate::TranscriptContains { entry_kind, .. } => {
                !matches!(entry_kind.0.as_str(), "AssistantAttempt" | "UserMessage")
            }
            _ => false,
        })
        .collect();
    for (n, invalid) in invalid_checks.iter().enumerate() {
        if *invalid {
            seen[n] = true;
        }
    }

    let mut retained_bytes = serde_json::to_vec(suite)
        .map_err(|_| invalid("suite"))?
        .len();
    let mut after = 0;
    loop {
        if abort_on_cancel && stopped() {
            return Err(StorageError::Cancelled);
        }
        let entries = snapshot.sessions().entries(&session.id, after, 500)?;
        if entries.is_empty() {
            break;
        }
        for entry in &entries {
            if abort_on_cancel && stopped() {
                return Err(StorageError::Cancelled);
            }
            after = entry.sequence;
            for (n, check) in suite.checks.0.iter().enumerate() {
                if invalid_checks[n] || !matched(check, entry) {
                    continue;
                }
                seen[n] = true;
                if stopped() {
                    cancelled[n] += 1;
                    continue;
                }
                // Reserve one summary per check and preserve all remaining coverage counts.
                if instances.len() + 2 * suite.checks.0.len() >= RETAINED_INSTANCES
                    || retained_bytes >= RETAINED_BYTES
                {
                    omitted[n] += 1;
                    continue;
                }
                let (outcome, detail) = evaluate(snapshot.sessions(), session, check, entry)?;
                let instance = EvalInstanceResult {
                    check_id: check.id.clone(),
                    source: Some(source(entry)),
                    revision: attribution::revision_context(snapshot.sessions(), session, entry)?,
                    coverage: coverage([(outcome.clone(), 1)]),
                    outcome,
                    detail: text(detail),
                    observed_value: None,
                };
                let bytes = serde_json::to_vec(&instance)
                    .map_err(|_| invalid("eval instance"))?
                    .len();
                if retained_bytes + bytes > RETAINED_BYTES {
                    omitted[n] += 1;
                    retained_bytes = RETAINED_BYTES;
                } else {
                    retained_bytes += bytes;
                    instances.push(instance);
                }
            }
        }
    }
    for (n, check) in suite.checks.0.iter().enumerate() {
        if invalid_checks[n] {
            continue;
        }
        let EvalPredicate::RecordFieldEquals {
            source,
            pointer,
            expected,
        } = &check.predicate
        else {
            continue;
        };
        visit_snapshot(snapshot, session, source, &mut |record| {
            seen[n] = true;
            if stopped() {
                if abort_on_cancel {
                    return Err(StorageError::Cancelled);
                }
                cancelled[n] += 1;
                return Ok(());
            }
            if instances.len() + 2 * suite.checks.0.len() >= RETAINED_INSTANCES
                || retained_bytes >= RETAINED_BYTES
            {
                omitted[n] += 1;
                return Ok(());
            }
            let (outcome, detail) = records::field_outcome(&record.body, &pointer.0, &expected.0);
            let observed_value = match record.body.pointer(&pointer.0).cloned() {
                None => None,
                Some(value) => match serde_json::from_value::<DynamicJson>(value) {
                    Ok(value) => Some(EvalObservedValue { value }),
                    Err(_) => {
                        omitted[n] += 1;
                        return Ok(());
                    }
                },
            };
            let instance = EvalInstanceResult {
                check_id: check.id.clone(),
                source: Some(record.source),
                revision: record.revision,
                coverage: coverage([(outcome.clone(), 1)]),
                outcome,
                detail: text(detail),
                observed_value,
            };
            let bytes = serde_json::to_vec(&instance)
                .map_err(|_| invalid("record instance"))?
                .len();
            if retained_bytes + bytes > RETAINED_BYTES {
                omitted[n] += 1;
                retained_bytes = RETAINED_BYTES;
            } else {
                retained_bytes += bytes;
                instances.push(instance);
            }
            Ok(())
        })?;
        if !seen[n] {
            seen[n] = true;
            missing_records[n] = 1;
        }
    }
    for (n, check) in suite.checks.0.iter().enumerate() {
        for (outcome, count, detail) in [
            (
                EvalOutcome::GraderError,
                i64::from(invalid_checks[n]),
                "Invalid deterministic predicate configuration",
            ),
            (
                EvalOutcome::EvidenceUnavailable,
                missing_records[n],
                "No retained record for this source",
            ),
            (
                EvalOutcome::NotApplicable,
                i64::from(!seen[n]),
                "No matching recorded steps",
            ),
            (
                EvalOutcome::BudgetExhausted,
                omitted[n],
                "Matching steps exceed the retained-report budget; counts remain in coverage",
            ),
            (
                EvalOutcome::Cancelled,
                cancelled[n],
                "Grading was cancelled; matching steps remain in coverage",
            ),
        ] {
            if count == 0 {
                continue;
            }
            instances.push(EvalInstanceResult {
                check_id: check.id.clone(),
                source: None,
                revision: empty_revision(0, "No single retained step for this population"),
                coverage: coverage([(outcome.clone(), count)]),
                outcome,
                detail: text(detail),
                observed_value: None,
            });
        }
    }
    let (outcome, coverage) = summarize(&instances);
    let report = EvalReport {
        id: Id::new(identity).map_err(|_| invalid("eval id"))?,
        project_id: session.project_id.clone(),
        session_id: session.id.clone(),
        suite: suite.clone(),
        created_at_ms: counter(created_at_ms),
        source_watermark: counter(snapshot.stamp().committed_sequence),
        session_revision: rev(session.revision),
        session_sequence: counter(session.last_durable_sequence()),
        evidence_digest: Digest::try_from(evidence).map_err(|_| invalid("evidence digest"))?,
        measurement_digest: Digest::try_from(measurement)
            .map_err(|_| invalid("measurement digest"))?,
        outcome: outcome.clone(),
        coverage: coverage.clone(),
        runs: BoundedList(vec![EvalRunResult {
            session_id: session.id.clone(),
            outcome,
            coverage,
            instances: BoundedList(instances),
        }]),
    };
    report.check().map_err(StorageError::InvalidRecord)?;
    if serde_json::to_vec(&report)
        .map_err(|_| invalid("report"))?
        .len()
        > REPORT_BYTES
    {
        return Err(invalid("report metadata exceeds budget"));
    }
    Ok(report)
}

fn persist(
    tx: &mut dyn WriteTransaction,
    report: &EvalReport,
    automatic: bool,
) -> Result<EvalReport, StorageError> {
    let session = tx
        .sessions()
        .session(&report.session_id)?
        .ok_or_else(|| invalid("missing eval session"))?;
    if automatic && !automatic::automatically_eligible(tx.runs(), &session)? {
        return Err(invalid("eval session is no longer terminal"));
    }
    if session.revision != report.session_revision.get()
        || session.last_durable_sequence() != report.session_sequence.get()
    {
        return Err(StorageError::RevisionConflict(Box::new(RevisionConflict {
            dimension: crate::protocol::RevisionDimension::Source,
            target_kind: "session",
            target_id: session.id.clone(),
            expected: rev(report.session_sequence.get() + 1),
            current: rev(session.next_sequence),
        })));
    }
    if transaction_evidence_digest(tx, &session, &report.suite)? != report.evidence_digest.as_str()
    {
        return Err(StorageError::RevisionConflict(Box::new(RevisionConflict {
            dimension: crate::protocol::RevisionDimension::Source,
            target_kind: "evaluation_evidence",
            target_id: session.id.clone(),
            expected: rev(report.source_watermark.get() + 1),
            current: rev(tx.committed_sequence()? + 1),
        })));
    }
    if let Some(existing) = tx.evals().eval_run(&report.id)? {
        return serde_json::from_value(existing.report).map_err(|_| invalid("stored eval report"));
    }
    let created_at_ms = tx.now_ms();
    tx.evals().insert_eval_run(&StoredEvalRun {
        id: report.id.clone(),
        suite_id: report.suite.id.clone(),
        suite_revision: report.suite.revision.get(),
        project_id: report.project_id.clone(),
        session_id: report.session_id.clone(),
        session_revision: report.session_revision.get(),
        session_sequence: report.session_sequence.get(),
        measurement_digest: report.measurement_digest.as_str().into(),
        created_at_ms,
        source_watermark: report.source_watermark.get(),
        report: serde_json::to_value(report).map_err(|_| invalid("report"))?,
    })?;
    Ok(report.clone())
}

impl CommandHandler for EvalService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        let OperationInput::Evals(ops::Input::Grade(input)) = command.input() else {
            return Ok(None);
        };
        let prepared = env.read(|snapshot| {
            let session = match visible_session(snapshot.sessions(), ctx, &input.session_id) {
                Ok(s) => s,
                Err(Failure::Refused(r)) => return Ok(Err(r)),
                Err(Failure::Storage(e)) => return Err(e),
            };
            build_report(
                snapshot,
                &session,
                &input.suite,
                self.clock.now_ms().get(),
                || env.stopped(self.clock.now_ms().get()),
                false,
            )
            .map(Ok)
        })?;
        Ok(Some(Prepared::new(prepared)))
    }
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let result = (|| {
            let OperationInput::Evals(ops::Input::Grade(input)) = command.input() else {
                return Err(
                    Refusal::new(DomainErrorCode::UnsupportedCapability, "eval", None).into(),
                );
            };
            visible_session(tx.sessions(), ctx, &input.session_id)?;
            let prepared = command
                .prepared::<Result<EvalReport, Refusal>>()
                .ok_or_else(|| invalid("missing prepared eval"))?;
            let report = prepared.clone().map_err(Failure::from)?;
            let persisted = persist(tx, &report, false)?;
            Ok(DomainDecision::Applied {
                result: OperationResult::EvalsGrade { value: persisted },
            })
        })();
        decided(tx, result)
    }
}
impl QueryHandler for EvalService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let result = (|| -> Result<_, Failure> {
            let OperationInput::Evals(ops::Input::Results(input)) = query.input() else {
                return Err(
                    Refusal::new(DomainErrorCode::UnsupportedCapability, "eval", None).into(),
                );
            };
            let session = visible_session(snapshot.sessions(), ctx, &input.session_id)?;
            let scope_revision = match &session.project_id {
                Some(p) => snapshot.sessions().project_scope_revision(p)?.unwrap_or(1),
                None => 1,
            };
            let binding =
                CursorBinding::new(ctx, "evals.results", &[session.id.as_str()], scope_revision);
            let after = input
                .page
                .cursor
                .as_ref()
                .map(|c| {
                    binding
                        .decode(&c.0)
                        .and_then(|s| Id::new(s).map_err(|_| StorageError::CursorExpired))
                })
                .transpose()?;
            let limit = input.page.limit.get().min(499) as u16;
            let mut rows = snapshot
                .evals()
                .eval_runs(&session.id, after.as_ref(), limit + 1)?;
            let more = rows.len() > usize::from(limit);
            rows.truncate(usize::from(limit));
            let next = if more {
                rows.last()
                    .map(|r| BoundedText(binding.encode(r.id.as_str())))
            } else {
                None
            };
            let reports = rows
                .into_iter()
                .map(|r| {
                    serde_json::from_value(r.report).map_err(|_| invalid("stored eval report"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok((
                OperationResult::EvalsResults {
                    value: EvalReportPage {
                        items: BoundedList(reports),
                        next_cursor: next,
                        has_more: more,
                    },
                },
                scope_revision,
            ))
        })()
        .map_err(|f| query_fault(f, "eval", self.correlation()))?;
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(result.1)),
            result: result.0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_outcome_has_explicit_coverage_and_only_binary_results_enter_rates() {
        let outcomes = [
            EvalOutcome::Passed,
            EvalOutcome::Failed,
            EvalOutcome::Unclear,
            EvalOutcome::NotApplicable,
            EvalOutcome::EvidenceUnavailable,
            EvalOutcome::GraderError,
            EvalOutcome::Cancelled,
            EvalOutcome::BudgetExhausted,
        ];
        for outcome in &outcomes {
            let c = coverage([(outcome.clone(), 1)]);
            c.check().unwrap();
            assert_eq!(c.eligible.get(), 1);
            let graded = matches!(outcome, EvalOutcome::Passed | EvalOutcome::Failed);
            assert_eq!(c.graded.get(), i64::from(graded));
            assert_eq!(c.ungraded.get(), i64::from(!graded));
            assert_eq!(c.pass_rate.is_some(), graded);
            assert_eq!(
                outcome_name(&aggregate(std::slice::from_ref(outcome))),
                outcome_name(outcome)
            );
        }
        let c = coverage(outcomes.into_iter().map(|o| (o, 1)));
        c.check().unwrap();
        assert_eq!(
            (c.eligible.get(), c.graded.get(), c.ungraded.get()),
            (8, 2, 6)
        );
        assert_eq!(c.pass_rate.unwrap().get(), 0.5);
        assert!(!matches!(
            aggregate(&[EvalOutcome::Passed, EvalOutcome::EvidenceUnavailable]),
            EvalOutcome::Passed
        ));
        assert!(!matches!(
            aggregate(&[EvalOutcome::Passed, EvalOutcome::NotApplicable]),
            EvalOutcome::Passed
        ));
    }
}
