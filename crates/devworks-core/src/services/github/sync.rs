//! An explicit issue sync is a journaled observation using the same source and cursor owner.
use super::issue_poll::{self, IssuePageResult, PollTarget};
use super::{GitHubService, reads};
use crate::context::InvocationContext;
use crate::gateway::AdmittedCommand;
use crate::operations::github::IssueSyncInput;
use crate::ports::storage::{
    NewEvent, NewOperation, NewQueueEntry, OperationRecord, ReadOptions, StorageError,
    WriteOptions, WriteTransaction, read_snapshot, run_write,
};
use crate::ports::{AuthorityFence, Authorization, Decided, IdKind, Preparation};
use crate::protocol::records::OperationRef;
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::{DomainErrorCode, EventId, Id, OperationId, OperationState, ProjectId};
use crate::services::effect::EffectOutcome;
use crate::services::fences::authorization_holds;
use crate::services::kernel::{Failure, Refusal, not_found};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub(super) const CAPABILITY: &str = "github.issue_sync";

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SyncRequest {
    local_repository_id: Id,
    repository_id: Id,
    binding_revision: i64,
    permission_revision: i64,
    cursor_revision: Option<i64>,
    issue_number: Option<i64>,
    project_id: Option<ProjectId>,
    actor_id: crate::protocol::ActorId,
    restore_epoch: i64,
    authority_fences: Vec<AuthorityFence>,
    delivery: Id,
}

pub(super) struct CheckedSync {
    target: PollTarget,
    project: Option<ProjectId>,
    number: Option<i64>,
}

pub(super) fn prepare(
    ctx: &InvocationContext,
    input: &IssueSyncInput,
    env: &Preparation<'_>,
) -> Result<CheckedSync, Failure> {
    let number = input
        .remote_issue_number
        .as_ref()
        .map(|number| {
            number
                .0
                .parse::<i64>()
                .map_err(|_| refused(DomainErrorCode::InvalidArgument))
        })
        .transpose()?;
    env.read(|snapshot| {
        Ok((|| {
            let project = reads::project(ctx, snapshot, &input.repository_id)?;
            let target = issue_poll::target(snapshot, &input.repository_id)?
                .ok_or_else(|| not_found("github_repository", &input.repository_id))?;
            Ok(CheckedSync {
                target,
                project,
                number,
            })
        })())
    })?
}

pub(super) fn admit(
    ctx: &InvocationContext,
    command: &AdmittedCommand,
    tx: &mut dyn WriteTransaction,
    input: &IssueSyncInput,
    checked: &CheckedSync,
) -> Result<Decided, Failure> {
    issue_poll::ensure_target(tx, &checked.target)?;
    if let Some(project) = &checked.project
        && !tx
            .projects()
            .members(project)?
            .contains(&input.repository_id)
    {
        return Err(not_found("github_repository", &input.repository_id).into());
    }
    let canonical = &checked.target.repository.id;
    let cursor = tx.github().github_poll_cursor(canonical, "issues")?;
    let issues_rev = cursor.as_ref().map_or(1, |row| row.revision);
    let status_rev = super::reads::status_scope_revision(tx.github(), &checked.target.repository)?;
    let expected = input.expected_cursor_revision.get();
    if expected != issues_rev && expected != status_rev {
        return Err(refused(DomainErrorCode::Conflict).into());
    }
    let operation = OperationId(tx.new_id(IdKind::Operation));
    let delivery = tx.new_id(IdKind::QueueEntry);
    let request = SyncRequest {
        local_repository_id: input.repository_id.clone(),
        repository_id: canonical.clone(),
        binding_revision: checked.target.binding.revision,
        permission_revision: checked.target.account.permission_revision,
        cursor_revision: cursor.as_ref().map(|row| row.revision),
        issue_number: checked.number,
        project_id: checked.project.clone(),
        actor_id: ctx.principal().clone(),
        restore_epoch: tx.store().restore_epoch,
        authority_fences: command.authorization().fences().to_vec(),
        delivery: delivery.clone(),
    };
    tx.operations().record_intent(NewOperation {
        id: operation.clone(),
        root_id: None,
        assignment_id: None,
        capability: CAPABILITY.into(),
        schema_version: i64::from(command.schema_revision().0),
        args_digest: command.fingerprint().as_str().into(),
        payload: serde_json::to_value(&request)
            .map_err(|_| StorageError::InvalidRecord("Invalid GitHub sync request".into()))?,
    })?;
    let event = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id: event.clone(),
        actor: request.actor_id,
        project_id: request.project_id,
        root_id: None,
        operation_id: Some(operation.clone()),
        aggregate_kind: "github_sync".into(),
        aggregate_id: operation.0.as_str().into(),
        aggregate_revision: 1,
        kind: "GithubIssueSyncRequested".into(),
        payload: json!({"repositoryId":input.repository_id,"issueNumber":request.issue_number}),
    })?;
    let now = tx.now_ms();
    tx.queue().enqueue(NewQueueEntry {
        id: delivery,
        consumer: super::EFFECT_CONSUMER.into(),
        event_id: event,
        root_id: None,
        operation_id: Some(operation.clone()),
        partition: Some(canonical.as_str().into()),
        ready_at_ms: cursor
            .as_ref()
            .map_or(now, |row| now.max(row.next_poll_at_ms)),
        priority: 0,
        payload: json!({}),
    })?;
    Ok(Decided {
        decision: DomainDecision::Accepted {
            operation_id: operation.clone(),
        },
        operation_id: Some(operation),
    })
}

impl GitHubService {
    pub(super) fn sync_issues(&self, operation: &OperationRecord) -> EffectOutcome {
        let Ok(request) = serde_json::from_value::<SyncRequest>(operation.payload.clone()) else {
            return EffectOutcome::Failed("Invalid GitHub sync request".into());
        };
        let target = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            let auth = Authorization::direct().fenced_by(request.authority_fences.clone());
            if snapshot.stamp().restore_epoch != request.restore_epoch
                || !authorization_holds(snapshot, &auth)?
                || !snapshot
                    .queue()
                    .entry(&request.delivery)?
                    .is_some_and(|row| !row.cancel_requested)
            {
                return Ok(None);
            }
            if let Some(project) = &request.project_id
                && !snapshot
                    .projects()
                    .members(project)?
                    .contains(&request.local_repository_id)
            {
                return Ok(None);
            }
            Ok(
                issue_poll::target(snapshot, &request.local_repository_id)?.filter(|target| {
                    target.account.permission_revision == request.permission_revision
                        && target.repository.id == request.repository_id
                        && target.binding.revision == request.binding_revision
                }),
            )
        });
        let Ok(Some(target)) = target else {
            return EffectOutcome::Failed(
                "GitHub authorization or repository binding changed".into(),
            );
        };
        let revision = target.cursor.as_ref().map(|row| row.revision);
        if request.issue_number.is_none() && revision > request.cursor_revision {
            // An automatic pass, or this operation before an interrupted acknowledgement,
            // already applied the next authorized page.
            return completed(operation);
        }
        if let Some(number) = request.issue_number {
            let issue = match self.observe_issue(&target, number, self.clock.now_ms().get()) {
                Ok(issue) => issue,
                Err(_) => return EffectOutcome::Failed("GitHub issue observation failed".into()),
            };
            let updated = run_write(&*self.store, WriteOptions::observed(), move |tx| {
                issue_poll::ensure_target(tx, &target)?;
                issue_poll::capture(tx, &target, issue)
            });
            return match updated {
                Ok(_) => completed(operation),
                Err(_) => {
                    EffectOutcome::NotApplied("Could not commit the GitHub observation".into())
                }
            };
        }
        match self.poll_target_page(target) {
            Ok(IssuePageResult::Captured { .. }) => completed(operation),
            Ok(IssuePageResult::Throttled { .. } | IssuePageResult::NotDue) => {
                EffectOutcome::Failed(
                    "GitHub is pacing requests; see repository polling status".into(),
                )
            }
            Err(_) => EffectOutcome::Failed("GitHub issue observation failed".into()),
        }
    }
}

fn completed(operation: &OperationRecord) -> EffectOutcome {
    let result = OperationResult::GithubIssueSync {
        value: OperationRef {
            operation_id: operation.id.clone(),
            state: OperationState::ObservedComplete,
        },
    };
    EffectOutcome::Completed(json!({"result":result}))
}

fn refused(code: DomainErrorCode) -> Refusal {
    Refusal::new(code, "github_issue_sync", None)
}
