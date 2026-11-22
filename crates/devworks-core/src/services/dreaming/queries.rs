use super::{
    DreamingService, active, authorize, current_instruction, instruction, invalid, rev, settings,
};
use crate::context::InvocationContext;
use crate::gateway::AdmittedQuery;
use crate::operations::{OperationInput, dreaming as ops};
use crate::ports::QueryAnswer;
use crate::ports::storage::{
    DreamPhase, DreamRunRow, DreamTrigger, KnowledgeScope, ReadSnapshot, StorageError,
};
use crate::protocol::records::{
    DreamRun, DreamRunDetail, DreamRunStatus, DreamRunTrigger, PageRequest,
};
use crate::protocol::results::OperationResult;
use crate::protocol::support::{BoundedList, Limit};
use crate::protocol::{Id, OperationState, Revision};
use crate::services::cursor::{self, CursorBinding};
use crate::services::kernel::{Failure, counter, label, not_found};
use serde::de::DeserializeOwned;
use serde_json::Value;

impl DreamingService {
    pub(super) fn answer(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, Failure> {
        let OperationInput::Dreaming(input) = query.input() else {
            return Err(invalid("dreaming").into());
        };
        let repo = snapshot.dreaming()?;
        let (result, revision) = match input {
            ops::Input::Runs(input) => {
                let scope = KnowledgeScope::from(&input.scope);
                authorize(ctx, &scope)?;
                let revision = repo.dream_scope(&scope)?.revision;
                let binding = CursorBinding::new(ctx, "dreaming.runs", &[&scope.key()], revision);
                let page = cursor::page(
                    &binding,
                    &input.page,
                    |position, limit| {
                        let before = position.map(run_position).transpose()?;
                        repo.dream_runs(&scope, before.as_ref().map(|(at, key)| (*at, key)), limit)
                    },
                    |row| format!("{}:{}", row.started_at_ms, row.id.as_str()),
                )?
                .try_into_page(render_run)?;
                (OperationResult::DreamingRuns { value: page }, revision)
            }
            ops::Input::Report(input) => {
                let row = repo
                    .dream_run(&input.run_id)?
                    .filter(|row| authorize(ctx, &row.scope).is_ok())
                    .ok_or_else(|| not_found("dreaming_run", &input.run_id))?;
                let revision = row.revision;
                let digest = super::revert::journal_digest(repo, &row.id)?;
                let mut value = render_run(row)?;
                if let Some(detail) = &mut value.detail {
                    detail.journal_digest = Some(digest);
                }
                (OperationResult::DreamingReport { value }, revision)
            }
            ops::Input::SettingsQuery(input) => {
                let scope = KnowledgeScope::from(&input.scope);
                authorize(ctx, &scope)?;
                active(snapshot.projects(), &scope)?;
                let row = repo.dream_scope(&scope)?;
                let revision = row.revision;
                let artifact = current_instruction(repo, &scope)?
                    .ok_or(StorageError::ViewUnavailable)?
                    .artifact;
                (
                    OperationResult::DreamingSettings {
                        value: settings(&scope, row, artifact)?,
                    },
                    revision,
                )
            }
            ops::Input::InstructionsQuery(input) => {
                let scope = KnowledgeScope::from(&input.scope);
                authorize(ctx, &scope)?;
                active(snapshot.projects(), &scope)?;
                let current = repo.dream_cutoffs(&scope)?;
                let revision = match scope {
                    KnowledgeScope::Workspace => current.base_instruction_revision,
                    KnowledgeScope::Project(_) => current.project_instruction_revision,
                }
                .max(1);
                let binding =
                    CursorBinding::new(ctx, "dreaming.instructions", &[&scope.key()], revision);
                let default = PageRequest {
                    cursor: None,
                    limit: Limit::new(20).expect("default page"),
                };
                let page = cursor::page(
                    &binding,
                    input.page.as_ref().unwrap_or(&default),
                    |position, limit| {
                        let before = position
                            .map(str::parse::<i64>)
                            .transpose()
                            .map_err(|_| StorageError::CursorExpired)?;
                        repo.dream_instructions(&scope, before, limit)
                    },
                    |row| row.revision.to_string(),
                )?;
                (
                    OperationResult::DreamingInstructions {
                        value: page
                            .try_into_page(|row| Ok::<_, Failure>(instruction(&scope, row)))?,
                    },
                    revision,
                )
            }
            _ => return Err(invalid("dreaming").into()),
        };
        Ok(QueryAnswer {
            result,
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(revision)),
        })
    }
}

fn run_position(raw: &str) -> Result<(i64, Id), StorageError> {
    let (at, key) = raw.split_once(':').ok_or(StorageError::CursorExpired)?;
    Ok((
        at.parse().map_err(|_| StorageError::CursorExpired)?,
        Id::new(key).map_err(|_| StorageError::CursorExpired)?,
    ))
}

fn report_field<T: DeserializeOwned>(
    report: Option<&Value>,
    member: &str,
) -> Result<Option<T>, Failure> {
    report
        .and_then(|report| report.get(member))
        .filter(|value| !value.is_null())
        .map(|value| {
            serde_json::from_value(value.clone()).map_err(|_| {
                StorageError::InvalidRecord(format!("invalid retained Dreaming {member}")).into()
            })
        })
        .transpose()
}

pub(super) fn render_run(row: DreamRunRow) -> Result<DreamRun, Failure> {
    let state = match row.phase {
        DreamPhase::Queued => OperationState::IntentRecorded,
        DreamPhase::Running => OperationState::Dispatching,
        DreamPhase::Completed | DreamPhase::ScheduledFanout => OperationState::ObservedComplete,
        DreamPhase::Failed => OperationState::Failed,
        DreamPhase::OutcomeUnknown => OperationState::OutcomeUnknown,
        DreamPhase::Cancelled
        | DreamPhase::Disabled
        | DreamPhase::Busy
        | DreamPhase::TooSoon
        | DreamPhase::NoSignal => OperationState::CancelledBeforeDispatch,
        DreamPhase::Blocked | DreamPhase::BudgetExhausted | DreamPhase::Partial => {
            OperationState::NeedsInput
        }
    };
    let status = match row.phase {
        DreamPhase::Queued => DreamRunStatus::Queued,
        DreamPhase::Running => DreamRunStatus::Running,
        DreamPhase::Disabled => DreamRunStatus::Disabled,
        DreamPhase::Busy => DreamRunStatus::Busy,
        DreamPhase::TooSoon => DreamRunStatus::TooSoon,
        DreamPhase::NoSignal => DreamRunStatus::NoSignal,
        DreamPhase::Blocked => DreamRunStatus::Blocked,
        DreamPhase::BudgetExhausted => DreamRunStatus::BudgetExhausted,
        DreamPhase::Partial => DreamRunStatus::Partial,
        DreamPhase::Failed => DreamRunStatus::Failed,
        DreamPhase::Cancelled => DreamRunStatus::Cancelled,
        DreamPhase::OutcomeUnknown => DreamRunStatus::OutcomeUnknown,
        DreamPhase::ScheduledFanout => DreamRunStatus::ScheduledFanout,
        DreamPhase::Completed => DreamRunStatus::Completed,
    };
    let trigger = match row.trigger {
        DreamTrigger::Manual => DreamRunTrigger::Manual,
        DreamTrigger::Scheduled => DreamRunTrigger::Scheduled,
        DreamTrigger::Resume => DreamRunTrigger::Resume,
    };
    let report = row.report.as_ref();
    let checkpoint = row.checkpoint.unwrap_or_default();
    let reason =
        report_field::<String>(report, "reason")?.map(|value| label(&value, "Unspecified"));
    let total = |name, value: u64| -> Result<_, Failure> {
        match report_field(report, name)? {
            Some(total) => Ok(total),
            None => Ok(counter(
                i64::try_from(value).map_err(|_| invalid("dreaming_usage"))?,
            )),
        }
    };
    let detail = DreamRunDetail {
        status,
        trigger,
        initiator: row.initiator,
        started_at_ms: counter(row.started_at_ms),
        ended_at_ms: row.ended_at_ms.map(counter),
        settings_revision: rev(row.settings_revision),
        base_instruction_revision: Revision::new(row.cutoffs.base_instruction_revision).ok(),
        project_instruction_revision: Revision::new(row.cutoffs.project_instruction_revision).ok(),
        model_id: report_field(report, "modelId")?,
        connection_id: report_field(report, "connectionId")?,
        input_tokens: total("inputTokens", checkpoint.input_tokens)?,
        output_tokens: total("outputTokens", checkpoint.output_tokens)?,
        cost_micro_usd: total("pricedMicroUsd", checkpoint.priced_micro_usd)?,
        usage_uncertain: row.phase == DreamPhase::OutcomeUnknown
            || report_field(report, "usageUncertain")?.unwrap_or(checkpoint.usage_uncertain),
        continued_from: row.continued_from,
        can_continue: row.phase.resumable() && checkpoint.continuation.is_some(),
        reason,
        journal_digest: report_field(report, "journalDigest")?,
        evidence: report_field(report, "evidence")?,
    };
    Ok(DreamRun {
        id: row.id,
        revision: rev(row.revision),
        scope: (&row.scope).into(),
        state,
        checkpoint: checkpoint.continuation,
        report: report_field(report, "artifact")?,
        proposal_ids: report_field(report, "proposalIds")?.unwrap_or_else(|| BoundedList(vec![])),
        detail: Some(detail),
    })
}
