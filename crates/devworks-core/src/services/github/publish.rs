//! Durable, explicit publication of local issues. The saved marker is scoped to one work item
//! and immutable GitHub repository. Lost mutation replies are followed by reads only.
use super::GitHubService;
use super::api::Endpoint;
use super::issue_poll::{self, IssueObservation, PollTarget};
use super::issues;
use crate::authority::authorization_holds;
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::gateway::AdmittedCommand;
use crate::operations::issues::PublishInput;
use crate::ports::storage::{
    DesiredExecution, GithubIssueKey, NewEvent, NewOperation, NewQueueEntry, OperationRecord,
    ReadOptions, ReadSnapshot, StorageError, WriteOptions, WriteTransaction, read_snapshot,
    run_write,
};
use crate::ports::{AuthorityFence, Authorization, Decided, IdKind, Preparation};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::{
    AssignmentId, DomainErrorCode, EventId, Id, OperationId, OperationState, ProjectId, RootId,
};
use crate::services::effect::EffectOutcome;
use crate::services::git::github::Fault;
use crate::services::kernel::{Failure, Refusal, not_found};
use crate::services::runs::control_of;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(super) const CAPABILITY: &str = "issues.publish";
pub const CONSUMER: &str = "github_effects";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Publication {
    repository_id: Id,
    remote_repository_id: Id,
    github_node_id: String,
    binding_revision: i64,
    permission_revision: i64,
    remote_user_id: String,
    work_id: RootId,
    work_revision: i64,
    project: ProjectId,
    project_scope_revision: i64,
    stop_epoch: i64,
    assignment: Option<(AssignmentId, i64)>,
    restore_epoch: i64,
    authority_fences: Vec<AuthorityFence>,
    delivery: Id,
    title: String,
    body: String,
    marker: String,
}

pub(super) struct PreparedPublish {
    target: PollTarget,
    project: ProjectId,
}

pub(super) fn prepare(
    ctx: &InvocationContext,
    input: &PublishInput,
    env: &Preparation<'_>,
) -> Result<PreparedPublish, Failure> {
    env.read(|snapshot| {
        Ok((|| {
            let project = super::reads::project(ctx, snapshot, &input.repository_id)?;
            let work = snapshot
                .work()
                .work(&input.issue_id.0)?
                .filter(|work| {
                    !work.is_feature() && project.as_ref().is_none_or(|p| p == &work.project_id)
                })
                .ok_or_else(|| not_found("issue", &input.issue_id.0))?;
            let target = issue_poll::target(snapshot, &input.repository_id)?
                .ok_or_else(|| not_found("github_repository", &input.repository_id))?;
            Ok(PreparedPublish {
                target,
                project: work.project_id,
            })
        })())
    })?
}

pub(super) fn admit(
    ctx: &InvocationContext,
    command: &AdmittedCommand,
    tx: &mut dyn WriteTransaction,
    input: &PublishInput,
    prepared: &PreparedPublish,
) -> Result<Decided, Failure> {
    let target = &prepared.target;
    issue_poll::ensure_target(tx, target)?;
    let work = tx
        .work()
        .work(&input.issue_id.0)?
        .filter(|work| !work.is_feature() && work.project_id == prepared.project)
        .ok_or_else(|| not_found("issue", &input.issue_id.0))?;
    let project = tx
        .projects()
        .project(&work.project_id)?
        .filter(|project| project.lifecycle == "Active")
        .ok_or_else(|| not_found("project", &work.project_id.0))?;
    if !tx
        .projects()
        .members(&work.project_id)?
        .contains(&input.repository_id)
    {
        return Err(not_found("github_repository", &input.repository_id).into());
    }
    if work.revision != input.expected_revision.get() {
        return Err(refusal(DomainErrorCode::Conflict).into());
    }

    match tx
        .github()
        .github_issues_for_work(&input.issue_id, &target.repository.id, 2)?
        .as_slice()
    {
        [existing]
            if input
                .expected_remote_revision
                .as_ref()
                .is_none_or(|expected| expected.0 == existing.remote_revision) =>
        {
            // An explicit link may already provide the requested GitHub issue. Never post a copy.
            let value = issues::link_value(tx, target, &input.issue_id, &existing.key)?;
            return Ok(Decided {
                decision: DomainDecision::NoChange {
                    result: OperationResult::IssuesPublish { value },
                },
                operation_id: None,
            });
        }
        [] if input.expected_remote_revision.is_none() => (),
        _ => return Err(refusal(DomainErrorCode::Conflict).into()),
    }

    let Some(user) = &target.account.remote_user_id else {
        return Err(refusal(DomainErrorCode::PermissionDenied).into());
    };
    // REST repository IDs are numeric. GitHub's createIssue operation targets the immutable
    // GraphQL node directly, so repository renames or reuse of an old name cannot redirect it.
    if target.repository.remote_repository_id == target.numeric_id.to_string() {
        return Err(refusal(DomainErrorCode::UnsupportedTarget).into());
    }
    let control = control_of(tx.runs(), &input.issue_id)?;
    if control.desired == DesiredExecution::Paused || control.stop_operation_id.is_some() {
        return Err(refusal(DomainErrorCode::Conflict).into());
    }
    let assignment = if let Some(assignment) = ctx.assignment_id() {
        let current = tx
            .runs()
            .assignment(assignment)?
            .filter(|current| current.state.is_live())
            .ok_or_else(|| refusal(DomainErrorCode::PermissionDenied))?;
        Some((assignment.clone(), current.generation))
    } else {
        None
    };
    let (logical_key, id, existing) = publication_slot(tx, &input.issue_id, &target.repository.id)?;
    if existing.is_some() {
        return Ok(accepted(id));
    }
    let marker = format!("<!-- devworks-issue:{logical_key} -->");
    let description = work.brief["description"].as_str().unwrap_or_default();
    let body = format!("{description}\n\n{marker}");
    if work.title.is_empty() || body.len() > 65_536 {
        return Err(refusal(DomainErrorCode::InvalidArgument).into());
    }
    let delivery = tx.new_id(IdKind::QueueEntry);
    let publication = Publication {
        repository_id: input.repository_id.clone(),
        remote_repository_id: target.repository.id.clone(),
        github_node_id: target.repository.remote_repository_id.clone(),
        binding_revision: target.binding.revision,
        permission_revision: target.account.permission_revision,
        remote_user_id: user.clone(),
        work_id: input.issue_id.clone(),
        work_revision: work.revision,
        project: work.project_id.clone(),
        project_scope_revision: project.scope_revision,
        stop_epoch: control.stop_epoch,
        assignment,
        restore_epoch: tx.store().restore_epoch,
        authority_fences: command.authorization().fences().to_vec(),
        delivery: delivery.clone(),
        title: work.title,
        body,
        marker,
    };
    tx.operations().record_intent(NewOperation {
        id: id.clone(),
        root_id: Some(input.issue_id.clone()),
        assignment_id: ctx.assignment_id().cloned(),
        capability: CAPABILITY.into(),
        schema_version: 2,
        args_digest: command.fingerprint().as_str().into(),
        payload: serde_json::to_value(&publication).map_err(|_| invalid())?,
    })?;
    let event = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id: event.clone(),
        actor: ctx.principal().clone(),
        project_id: Some(work.project_id),
        root_id: Some(input.issue_id.clone()),
        operation_id: Some(id.clone()),
        aggregate_kind: "github_issue_publication".into(),
        aggregate_id: id.0.as_str().into(),
        aggregate_revision: 1,
        kind: "GithubIssuePublicationRequested".into(),
        payload: json!({"repositoryId": input.repository_id}),
    })?;
    let now = tx.now_ms();
    tx.queue().enqueue(NewQueueEntry {
        id: delivery,
        consumer: CONSUMER.into(),
        event_id: event,
        root_id: Some(input.issue_id.clone()),
        operation_id: Some(id.clone()),
        partition: Some(format!("publication:{logical_key}")),
        priority: 0,
        ready_at_ms: now,
        payload: json!({}),
    })?;
    Ok(accepted(id))
}

fn accepted(id: OperationId) -> Decided {
    Decided {
        decision: DomainDecision::Accepted {
            operation_id: id.clone(),
        },
        operation_id: Some(id),
    }
}

pub(super) fn publication_slot(
    tx: &mut dyn WriteTransaction,
    work: &RootId,
    repository: &Id,
) -> Result<(String, OperationId, Option<OperationState>), StorageError> {
    let logical_key = sha256_hex(
        &serde_json::to_vec(&(&tx.store().store_id.0, work, repository)).map_err(|_| invalid())?,
    );
    let mut id = op_id(&logical_key);
    loop {
        let Some(prior) = tx.operations().operation(&id)? else {
            return Ok((logical_key, id, None));
        };
        if prior.capability != CAPABILITY {
            return Err(invalid());
        }
        if !matches!(
            prior.state,
            OperationState::Failed | OperationState::CancelledBeforeDispatch
        ) {
            return Ok((logical_key, id, Some(prior.state)));
        }
        // A definitively failed or cancelled operation can be submitted again. Every
        // concurrent caller follows the same IDs. An unknown operation retains its slot.
        id = op_id(&sha256_hex(prior.id.0.as_str().as_bytes()));
    }
}

fn op_id(digest: &str) -> OperationId {
    OperationId(Id::new(format!("github-publish-{digest}")).expect("valid publication ID"))
}

impl GitHubService {
    pub(super) fn publish(&self, operation: &OperationRecord, reconcile: bool) -> EffectOutcome {
        let job: Publication = match serde_json::from_value(operation.payload.clone()) {
            Ok(job) if operation.schema_version == 2 => job,
            _ => return EffectOutcome::Failed("Invalid GitHub publication".into()),
        };
        let target = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            let Some(target) = issue_poll::target(snapshot, &job.repository_id)? else {
                return Ok(None);
            };
            if !job.matches(&target) || !dispatch_preflight(snapshot, &job, reconcile)? {
                return Ok(None);
            }
            Ok(Some(target))
        });
        let Ok(Some(target)) = target else {
            return if reconcile {
                EffectOutcome::Unknown(
                    "Original GitHub account or repository scope is unavailable".into(),
                )
            } else {
                EffectOutcome::Failed(
                    "GitHub publication authorization or work revision changed".into(),
                )
            };
        };
        let outcome = if reconcile {
            self.reconcile_issue(&target, &job, operation.created_at_ms)
        } else {
            self.create_issue(&target, &job)
        };
        match outcome {
            Ok(issue) => {
                let number = issue.number;
                let result = run_write(&*self.store, WriteOptions::observed(), move |tx| {
                    issue_poll::ensure_target(tx, &target)?;
                    let work = tx
                        .work()
                        .work(&job.work_id.0)?
                        .ok_or(StorageError::ScopeDenied)?;
                    if work.project_id != job.project
                        || !tx
                            .projects()
                            .members(&job.project)?
                            .contains(&job.repository_id)
                    {
                        return Err(StorageError::ScopeDenied);
                    }
                    let key = GithubIssueKey {
                        repository_id: target.repository.id.clone(),
                        number,
                    };
                    issue_poll::capture(tx, &target, issue)?;
                    let value = issues::link_value(tx, &target, &job.work_id, &key)?;
                    tx.github()
                        .link_github_work(&key, &job.work_id, work.revision)?;
                    Ok(OperationResult::IssuesPublish { value })
                });
                match result {
                    Ok(result) => EffectOutcome::Completed(json!({"result":result})),
                    Err(_) => EffectOutcome::Unknown(
                        "GitHub issue observed; local link settlement is pending".into(),
                    ),
                }
            }
            Err(Fault::Rejected(401 | 403 | 404 | 422) | Fault::CredentialsRequired)
                if !reconcile =>
            {
                EffectOutcome::Failed("GitHub rejected issue publication".into())
            }
            Err(Fault::Rejected(429)) if !reconcile => {
                // Leave rate-limited writes under the caller's explicit retry control.
                EffectOutcome::Failed("GitHub rate limited issue publication".into())
            }
            Err(_) => EffectOutcome::Unknown(
                "GitHub issue publication awaits a matching observation".into(),
            ),
        }
    }

    fn create_issue(
        &self,
        target: &PollTarget,
        job: &Publication,
    ) -> Result<IssueObservation, Fault> {
        let token = self
            .client
            .token_for_account(&*self.store, &target.account)?;
        let document = self.client.graphql_mutation(
            token,
            "mutation PublishIssue($input:CreateIssueInput!){createIssue(input:$input){clientMutationId issue{id number repository{id}}}}",
            json!({"repositoryId":job.github_node_id, "title":job.title, "body":job.body, "clientMutationId":job.marker}),
        )?;
        let result = &document["data"]["createIssue"];
        if result["clientMutationId"].as_str() != Some(&job.marker)
            || result["issue"]["repository"]["id"].as_str() != Some(&job.github_node_id)
        {
            return Err(Fault::Ambiguous);
        }
        let number = result["issue"]["number"]
            .as_i64()
            .filter(|n| *n > 0)
            .ok_or(Fault::Ambiguous)?;
        let snapshot = self
            .observe_issue(target, number, self.clock.now_ms().get())
            .map_err(|_| Fault::Ambiguous)?;
        if snapshot.payload["remoteIssueId"] != result["issue"]["id"]
            || !job.witness(&snapshot.payload)
        {
            return Err(Fault::Ambiguous);
        }
        Ok(snapshot)
    }

    fn reconcile_issue(
        &self,
        target: &PollTarget,
        job: &Publication,
        created_at_ms: i64,
    ) -> Result<IssueObservation, Fault> {
        let since = jiff::Timestamp::from_millisecond(created_at_ms.saturating_sub(60_000).max(0))
            .map_err(|_| Fault::InvalidResponse)?;
        let repo = &target.repository;
        let mut uri = url::Url::parse(&format!(
            "https://api.github.com/repositories/{}/issues",
            target.numeric_id
        ))
        .map_err(|_| Fault::InvalidResponse)?;
        uri.query_pairs_mut()
            .append_pair("state", "all")
            .append_pair("sort", "created")
            .append_pair("direction", "desc")
            .append_pair("per_page", "100")
            .append_pair("since", &since.to_string());
        let endpoint = Endpoint::new(uri.as_str(), None)?;
        let now = self.clock.now_ms().get();
        let mut next = None;
        for _ in 0..20 {
            let token = self
                .client
                .token_for_account(&*self.store, &target.account)?;
            let page = self.client.observe_authenticated_page(
                &endpoint,
                next.as_deref(),
                None,
                now,
                0,
                token,
            )?;
            if page.status != 200 {
                return Err(Fault::Rejected(page.status));
            }
            let rows = page
                .value
                .as_ref()
                .and_then(Value::as_array)
                .ok_or(Fault::InvalidResponse)?;
            let mut matches = rows.iter().filter(|row| {
                row.get("pull_request").is_none()
                    && row["user"]["id"]
                        .as_u64()
                        .is_some_and(|id| id.to_string() == job.remote_user_id)
                    && row["body"]
                        .as_str()
                        .is_some_and(|body| body.lines().any(|line| line == job.marker))
            });
            if let Some(row) = matches.next() {
                if matches.next().is_some() {
                    return Err(Fault::Ambiguous);
                }
                return issue_poll::decode_issue(repo, row);
            }
            next = page.next_page;
            if next.is_none() {
                break;
            }
        }
        Err(Fault::Ambiguous)
    }
}

impl Publication {
    fn matches(&self, target: &PollTarget) -> bool {
        target.binding.revision == self.binding_revision
            && target.repository.id == self.remote_repository_id
            && target.repository.remote_repository_id == self.github_node_id
            && target.account.permission_revision == self.permission_revision
            && target.account.remote_user_id.as_deref() == Some(&self.remote_user_id)
    }
    fn witness(&self, payload: &Value) -> bool {
        payload["authorId"]
            .as_u64()
            .is_some_and(|id| id.to_string() == self.remote_user_id)
            && payload["body"]
                .as_str()
                .is_some_and(|body| body.lines().any(|line| line == self.marker))
    }
}

fn dispatch_preflight(
    snapshot: &dyn ReadSnapshot,
    job: &Publication,
    reconcile: bool,
) -> Result<bool, StorageError> {
    if snapshot.stamp().restore_epoch != job.restore_epoch
        || !snapshot.projects().project(&job.project)?.is_some_and(|p| {
            p.lifecycle == "Active" && p.scope_revision == job.project_scope_revision
        })
        || !snapshot
            .projects()
            .members(&job.project)?
            .contains(&job.repository_id)
        || !snapshot.work().work(&job.work_id.0)?.is_some_and(|work| {
            work.project_id == job.project && (reconcile || work.revision == job.work_revision)
        })
    {
        return Ok(false);
    }
    if reconcile {
        // The native owner is observing an already dispatched mutation, using the exact
        // original remote account, repository and effect marker. Settling that fact cannot
        // grant the initiating actor new dispatch or result access.
        return Ok(true);
    }
    let authorization = Authorization::direct().fenced_by(job.authority_fences.clone());
    if !authorization_holds(snapshot, &authorization)? {
        return Ok(false);
    }
    if let Some((id, generation)) = &job.assignment
        && !snapshot
            .runs()
            .assignment(id)?
            .is_some_and(|row| row.state.is_live() && row.generation == *generation)
    {
        return Ok(false);
    }
    let control = control_of(snapshot.runs(), &job.work_id)?;
    Ok(control.desired == DesiredExecution::Enabled
        && control.stop_operation_id.is_none()
        && control.stop_epoch == job.stop_epoch
        && snapshot
            .queue()
            .entry(&job.delivery)?
            .is_some_and(|entry| !entry.cancel_requested)
        && snapshot
            .github()
            .github_issues_for_work(&job.work_id, &job.remote_repository_id, 1)?
            .is_empty())
}

fn refusal(code: DomainErrorCode) -> Refusal {
    Refusal::new(code, "github_issue_publication", None)
}

fn invalid() -> StorageError {
    StorageError::InvalidRecord("Invalid GitHub issue publication".into())
}
