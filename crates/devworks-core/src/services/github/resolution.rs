//! Publish resolution only from sealed, exact-head evidence for every linked work obligation.
use super::GitHubService;
use super::api::Endpoint;
use super::issue_poll::{self, PollTarget};
use crate::fingerprint::{canonical_digest, sha256_hex};
use crate::ports::IdKind;
use crate::ports::storage::{
    DesiredExecution, GithubIntakeFilter, GithubIssueKey, GithubObservationFilter, NewEvent,
    NewOperation, NewQueueEntry, OperationRecord, ReadOptions, SequenceKey, StorageError,
    StoredGithubIssueIntake, StoredGithubPollCursor, StoredGithubResolution,
    StoredGithubResolutionWitness, VERIFIED, WriteOptions, WriteTransaction, read_snapshot,
    run_write,
};
use crate::protocol::{EventId, Id, OperationId, OperationState, RootId};
use crate::services::effect::EffectOutcome;
use crate::services::git::github::Fault;
use crate::services::runs::control_of;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(super) const COMMENT: &str = "github.resolution.comment";
pub(super) const CLOSE: &str = "github.resolution.close";
const ENDPOINT: &str = "resolution-intake";
const PAGE: u16 = 64;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResolutionEffect {
    resolution_id: Id,
    repository_id: Id,
    registration_id: Id,
    issue_number: i64,
    remote_issue_id: String,
    binding_revision: i64,
    permission_revision: i64,
    remote_user_id: String,
    completion_digest: String,
    original: Value,
    marker: String,
    body: String,
}

impl GitHubService {
    pub fn queue_one_resolution(&self, registration: &Id) -> Result<(), StorageError> {
        let now = self.clock.now_ms().get();
        let selected = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            let Some(target) = issue_poll::target(snapshot, registration)? else {
                return Ok(None);
            };
            let cursor = snapshot
                .github()
                .github_poll_cursor(&target.repository.id, ENDPOINT)?;
            if cursor.as_ref().is_some_and(|row| row.next_poll_at_ms > now) {
                return Ok(None);
            }
            let after = cursor
                .as_ref()
                .and_then(|row| row.watermark.as_ref())
                .and_then(Value::as_i64)
                .filter(|number| *number > 0)
                .map(|number| GithubIssueKey {
                    repository_id: target.repository.id.clone(),
                    number,
                });
            let filter = GithubIntakeFilter {
                scope: issue_poll::scope(registration),
                states: Vec::new(),
            };
            let rows = snapshot
                .github()
                .github_issues(&filter, after.as_ref(), 16)?;
            Ok(Some((target, cursor, rows)))
        })?;
        let Some((target, cursor, rows)) = selected else {
            return Ok(());
        };
        run_write(&*self.store, WriteOptions::host_state(), move |tx| {
            issue_poll::ensure_target(tx, &target)?;
            let last = rows.last().map_or(0, |row| row.key.number);
            for row in rows {
                // The source and all completion evidence are checked inside the same transaction.
                let Some(latest) = tx
                    .github()
                    .github_issue(&row.key, &issue_poll::scope(&target.binding.repository_id))?
                else {
                    continue;
                };
                if latest.remote_revision == row.remote_revision {
                    admit(tx, &target, &latest)?;
                }
            }
            let revision = cursor.as_ref().map(|row| row.revision);
            tx.github().put_github_poll_cursor(
                &StoredGithubPollCursor {
                    repository_id: target.repository.id,
                    endpoint: ENDPOINT.into(),
                    revision: revision.unwrap_or(0) + 1,
                    etag: None,
                    last_modified: None,
                    next_page: None,
                    watermark: Some(json!(last)),
                    bootstrap_complete: true,
                    next_poll_at_ms: now.saturating_add(if last == 0 { 60_000 } else { 1000 }),
                    last_polled_at_ms: Some(now),
                    failure_count: 0,
                    last_error: None,
                },
                revision,
            )
        })
    }

    pub(super) fn resolution_effect(
        &self,
        operation: &OperationRecord,
        reconcile: bool,
    ) -> EffectOutcome {
        let Ok(job) = serde_json::from_value::<ResolutionEffect>(operation.payload.clone()) else {
            return EffectOutcome::Failed("GitHub resolution payload is invalid".into());
        };
        if reconcile {
            return match self.reconcile_resolution_effect(operation, &job) {
                Ok(evidence) => EffectOutcome::Completed(evidence),
                Err(_) => EffectOutcome::Unknown(
                    "GitHub resolution awaits attributable observation".into(),
                ),
            };
        }
        let target = match read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            let Some(target) = issue_poll::target(snapshot, &job.registration_id)? else {
                return Ok(None);
            };
            if !job.matches(&target) {
                return Ok(None);
            }
            Ok(Some(target))
        }) {
            Ok(Some(target)) => target,
            Ok(None) => {
                self.cancel_companion_close(operation, &job);
                return EffectOutcome::Failed(
                    "GitHub resolution scope or credentials changed".into(),
                );
            }
            Err(_) => {
                return EffectOutcome::NotApplied(
                    "GitHub resolution storage temporarily unavailable".into(),
                );
            }
        };
        let now = self.clock.now_ms().get();
        let issue = match self.observe_issue(&target, job.issue_number, now) {
            Ok(issue) => issue,
            Err(Fault::Rejected(401 | 403 | 404 | 422) | Fault::CredentialsRequired) => {
                self.cancel_companion_close(operation, &job);
                return EffectOutcome::Failed(
                    "GitHub refused the verified resolution update".into(),
                );
            }
            Err(Fault::InvalidResponse) => {
                self.cancel_companion_close(operation, &job);
                return EffectOutcome::Failed("GitHub resolution observed invalid issue".into());
            }
            Err(_) => {
                return EffectOutcome::NotApplied(
                    "GitHub resolution deferred by transient transport error".into(),
                );
            }
        };
        if issue.payload["remoteIssueId"].as_str() != Some(&job.remote_issue_id) {
            self.cancel_companion_close(operation, &job);
            return EffectOutcome::Failed("GitHub issue identity changed".into());
        }
        let close = operation.capability == CLOSE;
        if close && issue.payload["state"] == "closed" {
            return EffectOutcome::Completed(json!({"observedClosed":true,"url":issue.url}));
        }
        // A text/label/author change or an unexpected comment is retained by polling. A newer
        // source needs a fresh explicit adjudication before the publisher changes GitHub.
        if !same_issue(&job.original, &issue.payload, close) {
            self.cancel_companion_close(operation, &job);
            return EffectOutcome::Failed(
                "GitHub resolution needs current scope or evidence".into(),
            );
        }
        let expected = target.clone();
        let request = job.clone();
        let operation_id = operation.id.clone();
        let permitted = match run_write(&*self.store, WriteOptions::host_state(), move |tx| {
            issue_poll::ensure_target(tx, &expected)?;
            resolution_permitted(tx, &request, &operation_id, close)
        }) {
            Ok(permitted) => permitted,
            Err(_) => {
                return EffectOutcome::NotApplied(
                    "GitHub resolution permission check deferred".into(),
                );
            }
        };
        if !permitted || self.is_stopped() {
            if !permitted {
                self.cancel_companion_close(operation, &job);
                return EffectOutcome::Failed(
                    "GitHub resolution obligation changed before dispatch".into(),
                );
            }
            return EffectOutcome::NotApplied("GitHub resolution stopped before mutation".into());
        }
        let token = match self.client.token_for_account(&*self.store, &target.account) {
            Ok(token) => token,
            Err(_) => {
                self.cancel_companion_close(operation, &job);
                return EffectOutcome::Failed("GitHub credentials are unavailable".into());
            }
        };
        let mutation = if close {
            self.client.graphql_mutation(
                token,
                "mutation CloseIssue($input:CloseIssueInput!){closeIssue(input:$input){clientMutationId issue{id closed}}}",
                json!({"issueId":job.remote_issue_id, "clientMutationId":job.marker}),
            )
        } else {
            self.client.graphql_mutation(
                token,
                "mutation AddComment($input:AddCommentInput!){addComment(input:$input){clientMutationId subject{id} commentEdge{node{id url}}}}",
                json!({"subjectId":job.remote_issue_id, "body":job.body, "clientMutationId":job.marker}),
            )
        };
        match mutation {
            Ok(result) if close => {
                let result = &result["data"]["closeIssue"];
                if result["clientMutationId"].as_str() == Some(&job.marker)
                    && result["issue"]["id"].as_str() == Some(&job.remote_issue_id)
                    && result["issue"]["closed"] == true
                {
                    EffectOutcome::Completed(json!({"observedClosed":true,"url":issue.url}))
                } else {
                    EffectOutcome::Unknown(
                        "GitHub resolution awaits attributable observation".into(),
                    )
                }
            }
            Ok(result) => {
                let result = &result["data"]["addComment"];
                if result["clientMutationId"].as_str() == Some(&job.marker)
                    && result["subject"]["id"].as_str() == Some(&job.remote_issue_id)
                    && let Some(comment) = result["commentEdge"]["node"]["id"].as_str()
                {
                    EffectOutcome::Completed(json!({"commentId":comment,"issueUrl":issue.url}))
                } else {
                    EffectOutcome::Unknown(
                        "GitHub resolution awaits attributable observation".into(),
                    )
                }
            }
            Err(Fault::Rejected(401 | 403 | 404 | 422) | Fault::CredentialsRequired) => {
                self.cancel_companion_close(operation, &job);
                EffectOutcome::Failed("GitHub refused the verified resolution update".into())
            }
            Err(_) => {
                EffectOutcome::Unknown("GitHub resolution awaits attributable observation".into())
            }
        }
    }

    fn cancel_companion_close(&self, operation: &OperationRecord, job: &ResolutionEffect) {
        if operation.capability != COMMENT {
            return;
        }
        let resolution_id = job.resolution_id.clone();
        let comment_id = operation.id.clone();
        let _ = run_write(&*self.store, WriteOptions::settlement(), move |tx| {
            cancel_close_if_unqueued(tx, &resolution_id, &comment_id)
        });
    }

    fn reconcile_resolution_effect(
        &self,
        operation: &OperationRecord,
        job: &ResolutionEffect,
    ) -> Result<Value, Fault> {
        let target = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            let Some(target) = issue_poll::target(snapshot, &job.registration_id)? else {
                return Ok(None);
            };
            if !job.matches(&target) {
                return Ok(None);
            }
            Ok(Some(target))
        })
        .map_err(|_| Fault::Unavailable)?
        .ok_or(Fault::CredentialsRequired)?;
        let now = self.clock.now_ms().get();
        let issue = self.observe_issue(&target, job.issue_number, now)?;
        if issue.payload["remoteIssueId"].as_str() != Some(&job.remote_issue_id) {
            return Err(Fault::InvalidResponse);
        }
        if operation.capability == CLOSE {
            return if issue.payload["state"] == "closed" {
                Ok(json!({"observedClosed":true,"url":issue.url}))
            } else {
                Err(Fault::Ambiguous)
            };
        }
        self.find_resolution_comment(&target, job, now)
    }

    fn find_resolution_comment(
        &self,
        target: &PollTarget,
        job: &ResolutionEffect,
        now: i64,
    ) -> Result<Value, Fault> {
        let repo = &target.repository;
        let suffix = format!("issues/{}/comments", job.issue_number);
        let endpoint = Endpoint::new(
            &format!(
                "https://api.github.com/repos/{}/{}/{suffix}?per_page=100",
                repo.owner, repo.name
            ),
            Some(&format!("/repositories/{}/{suffix}", target.numeric_id)),
        )?;
        let mut next = None;
        for _ in 0..25 {
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
            if let Some(comment) = rows.iter().rev().find(|row| {
                row["user"]["id"]
                    .as_u64()
                    .is_some_and(|id| id.to_string() == job.remote_user_id)
                    && row["body"]
                        .as_str()
                        .is_some_and(|body| body.lines().any(|line| line == job.marker))
            }) && let Some(id) = comment["node_id"].as_str()
            {
                return Ok(
                    json!({"commentId":id,"issueUrl":format!("{}/issues/{}", repo.url, job.issue_number)}),
                );
            }
            next = page.next_page;
            if next.is_none() {
                break;
            }
        }
        Err(Fault::Ambiguous)
    }
}

fn cancel_close_if_unqueued(
    tx: &mut dyn WriteTransaction,
    resolution_id: &Id,
    comment_operation_id: &OperationId,
) -> Result<(), StorageError> {
    let Some(binding) = tx.github().github_resolution(resolution_id)? else {
        return Ok(());
    };
    if binding.comment_operation_id != *comment_operation_id {
        return Ok(());
    }
    if let Some(close_op) = tx.operations().operation(&binding.close_operation_id)?
        && close_op.state == OperationState::IntentRecorded
    {
        tx.operations().transition(
            &binding.close_operation_id,
            close_op.revision,
            None,
            OperationState::CancelledBeforeDispatch,
            Some(json!({"reason": "companion comment operation failed before completion"})),
        )?;
    }
    Ok(())
}

/// Settles a completed GitHub journaled effect: once `github.resolution.comment` reaches
/// `ObservedComplete`, queues the companion `github.resolution.close` operation idempotently;
/// once `github.resolution.close` reaches `ObservedComplete`, marks intake `"Resolved"`.
pub fn settle_effect(
    tx: &mut dyn WriteTransaction,
    operation: &OperationRecord,
) -> Result<(), StorageError> {
    if operation.capability != COMMENT && operation.capability != CLOSE {
        return Ok(());
    }
    let job: ResolutionEffect =
        serde_json::from_value(operation.payload.clone()).map_err(|_| invalid())?;
    let Some(binding) = tx.github().github_resolution(&job.resolution_id)? else {
        return Ok(());
    };
    if operation.capability == CLOSE {
        if binding.close_operation_id == operation.id
            && operation.state == OperationState::ObservedComplete
        {
            let scope = issue_poll::scope(&job.registration_id);
            if let Some(mut intake) = tx.github().github_issue(&binding.key, &scope)?
                && intake.state != "Resolved"
            {
                let expected = intake.revision;
                intake.revision += 1;
                intake.state = "Resolved".into();
                tx.github().put_github_issue(&intake, Some(expected))?;
            }
        }
        return Ok(());
    }
    if binding.comment_operation_id != operation.id || binding.state != "Sealed" {
        return Ok(());
    }
    if matches!(
        operation.state,
        OperationState::Failed | OperationState::CancelledBeforeDispatch
    ) {
        return cancel_close_if_unqueued(tx, &binding.id, &operation.id);
    }
    if operation.state != OperationState::ObservedComplete {
        return Ok(());
    }
    let Some(close_op) = tx.operations().operation(&binding.close_operation_id)? else {
        return Ok(());
    };
    if close_op.state != OperationState::IntentRecorded {
        return Ok(());
    }
    let queue_id = Id::new(format!(
        "gh-close-{}",
        &sha256_hex(binding.close_operation_id.0.as_str().as_bytes())[..32]
    ))
    .expect("bounded close queue ID");
    if tx.queue().entry(&queue_id)?.is_some() {
        return Ok(());
    }
    let actor = tx.store().host_actor.clone();
    let event = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id: event.clone(),
        actor,
        project_id: None,
        root_id: None,
        operation_id: Some(binding.close_operation_id.clone()),
        aggregate_kind: "github_resolution".into(),
        aggregate_id: binding.id.as_str().into(),
        aggregate_revision: 2,
        kind: "GithubResolutionCloseRequested".into(),
        payload: json!({
            "repositoryId": job.registration_id,
            "issueNumber": job.issue_number,
            "resolutionId": binding.id,
        }),
    })?;
    let now = tx.now_ms();
    tx.queue()
        .enqueue(NewQueueEntry {
            id: queue_id,
            consumer: super::EFFECT_CONSUMER.into(),
            event_id: event,
            root_id: None,
            operation_id: Some(binding.close_operation_id),
            partition: Some(format!("resolution:{}", binding.id.as_str())),
            priority: 0,
            ready_at_ms: now,
            payload: json!({}),
        })
        .map(|_| ())
}

fn admit(
    tx: &mut dyn WriteTransaction,
    target: &PollTarget,
    source: &StoredGithubIssueIntake,
) -> Result<(), StorageError> {
    let mut witnesses = Vec::new();
    let mut after = None;
    loop {
        let page = tx
            .github()
            .github_work_links(&source.key, None, after.as_ref(), PAGE)?;
        let more = page.len() == usize::from(PAGE);
        after = page.last().map(|row| row.work_id.clone());
        for link in page {
            let Some(work) = tx.work().work(&link.work_id.0)? else {
                return Ok(());
            };
            if work.completion.as_deref() != Some(VERIFIED)
                || work.project_id != link.project_id
                || !tx
                    .projects()
                    .project(&work.project_id)?
                    .is_some_and(|project| project.lifecycle == "Active")
                || !permits_publication(tx, &link.work_id)?
            {
                return Ok(());
            }
            let Some(evidence) = merged_evidence(tx, target, &link.work_id)? else {
                return Ok(());
            };
            witnesses.push(StoredGithubResolutionWitness {
                resolution_id: Id::new("unsealed-github-resolution").expect("static ID"),
                work_id: link.work_id,
                project_id: link.project_id,
                work_revision: work.revision,
                evidence,
            });
        }
        if !more {
            break;
        }
    }
    if witnesses.is_empty() {
        return Ok(());
    }
    let obligation_revision = tx.github().github_obligation_revision(&source.key)?;
    let completion_digest = canonical_digest(&json!({
        "key":source.key, "obligationRevision":obligation_revision, "witnesses":witnesses
    }))
    .map_err(|_| invalid())?;
    let mut after_resolution = None;
    loop {
        let page = tx
            .github()
            .github_resolutions(&source.key, after_resolution.as_ref(), PAGE)?;
        for row in &page {
            if row.completion_digest == completion_digest {
                return Ok(());
            }
            if let Some(comment_op) = tx.operations().operation(&row.comment_operation_id)?
                && matches!(
                    comment_op.state,
                    OperationState::Failed | OperationState::CancelledBeforeDispatch
                )
            {
                cancel_close_if_unqueued(tx, &row.id, &row.comment_operation_id)?;
            }
            for op in [&row.comment_operation_id, &row.close_operation_id] {
                if tx
                    .operations()
                    .operation(op)?
                    .is_none_or(|op| !crate::ports::storage::is_terminal(op.state))
                {
                    return Ok(());
                }
            }
        }
        if page.len() < usize::from(PAGE) {
            break;
        }
        after_resolution = page.last().map(|row| row.id.clone());
    }
    let observation = tx
        .github()
        .github_observation(
            &source.observation_id,
            &issue_poll::scope(&target.binding.repository_id),
        )?
        .ok_or_else(invalid)?;
    let Some(remote_issue_id) = observation.payload["remoteIssueId"].as_str() else {
        return Ok(());
    };
    let Some(remote_user_id) = &target.account.remote_user_id else {
        return Ok(());
    };
    let resolution_id = tx.new_id(IdKind::Operation);
    let comment = OperationId(tx.new_id(IdKind::Operation));
    let close = OperationId(tx.new_id(IdKind::Operation));
    let marker = format!("<!-- devworks-resolution:{completion_digest} -->");
    let links: BTreeSet<_> = witnesses
        .iter()
        .filter_map(|w| w.evidence["pullUrl"].as_str())
        .collect();
    let summary = links
        .iter()
        .map(|url| format!("- {url}"))
        .collect::<Vec<_>>()
        .join("\n");
    let body = format!(
        "Verified and integrated the resolution in the following pull requests.\n\n{summary}\n\n{marker}"
    );
    let job = ResolutionEffect {
        resolution_id: resolution_id.clone(),
        repository_id: source.key.repository_id.clone(),
        registration_id: target.binding.repository_id.clone(),
        issue_number: source.key.number,
        remote_issue_id: remote_issue_id.into(),
        binding_revision: target.binding.revision,
        permission_revision: target.account.permission_revision,
        remote_user_id: remote_user_id.clone(),
        original: observation.payload,
        completion_digest: completion_digest.clone(),
        marker,
        body,
    };
    let payload = serde_json::to_value(&job).map_err(|_| invalid())?;
    for (id, capability) in [(&comment, COMMENT), (&close, CLOSE)] {
        tx.operations().record_intent(NewOperation {
            id: id.clone(),
            root_id: None,
            assignment_id: None,
            capability: capability.into(),
            schema_version: 1,
            args_digest: completion_digest.clone(),
            payload: payload.clone(),
        })?;
    }
    let resolution = tx
        .github()
        .insert_github_resolution(&StoredGithubResolution {
            id: resolution_id.clone(),
            key: source.key.clone(),
            state: "Draft".into(),
            completion_digest,
            source_revision: source.remote_revision.clone(),
            obligation_revision,
            summary_digest: sha256_hex(job.body.as_bytes()),
            comment_operation_id: comment.clone(),
            close_operation_id: close,
        })?;
    if resolution.id != resolution_id {
        return Err(invalid());
    }
    for witness in &mut witnesses {
        witness.resolution_id = resolution.id.clone();
        tx.github().insert_github_resolution_witness(witness)?;
    }
    tx.github().seal_github_resolution(
        &resolution_id,
        i64::try_from(witnesses.len()).map_err(|_| invalid())?,
    )?;
    // Enqueue only the comment operation initially; close is queued by settle_effect once the
    // comment operation reaches ObservedComplete (whether directly or via observation recovery).
    let actor = tx.store().host_actor.clone();
    let event = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id: event.clone(),
        actor,
        project_id: None,
        root_id: None,
        operation_id: Some(comment.clone()),
        aggregate_kind: "github_resolution".into(),
        aggregate_id: resolution_id.as_str().into(),
        aggregate_revision: 1,
        kind: "GithubResolutionCommentRequested".into(),
        payload: json!({
            "repositoryId": target.binding.repository_id,
            "issueNumber": source.key.number,
            "resolutionId": resolution_id,
        }),
    })?;
    let id = tx.new_id(IdKind::QueueEntry);
    let now = tx.now_ms();
    tx.queue().enqueue(NewQueueEntry {
        id,
        consumer: super::EFFECT_CONSUMER.into(),
        event_id: event,
        root_id: None,
        operation_id: Some(comment),
        partition: Some(format!("resolution:{}", resolution_id.as_str())),
        priority: 0,
        ready_at_ms: now,
        payload: json!({}),
    })?;
    Ok(())
}

fn merged_evidence(
    tx: &mut dyn WriteTransaction,
    target: &PollTarget,
    root: &RootId,
) -> Result<Option<Value>, StorageError> {
    let mut after = None;
    loop {
        let links = tx
            .sessions()
            .links(root, &["pull_request"], after.as_ref(), PAGE)?;
        for link in &links {
            if link.repository_id.as_ref() != Some(&target.binding.repository_id) {
                continue;
            }
            let spread = format!("{}/pull/", target.repository.url);
            let Some(number) = link.target_id.strip_prefix(&spread).filter(|num| {
                num.parse::<i64>()
                    .is_ok_and(|n| n > 0 && n.to_string() == *num)
            }) else {
                continue;
            };
            let filter = GithubObservationFilter {
                scope: issue_poll::scope(&target.binding.repository_id),
                kinds: vec!["PullRequest".into()],
                subject_id: Some(number.into()),
                head: link.revision.clone(),
            };
            let mut page_after = None;
            loop {
                let observations =
                    tx.github()
                        .github_observations(&filter, page_after.as_ref(), PAGE)?;
                for observation in &observations {
                    if observation.payload["state"] == "merged"
                        && observation.payload["mergeCommit"]
                            .as_str()
                            .is_some_and(super::indexing::valid_oid)
                        && observation.head.is_some()
                        && observation.head == link.revision
                    {
                        return Ok(Some(json!({
                            "localPullLink":link.id, "githubObservationId":observation.id,
                            "pullUrl":observation.url, "head":observation.head, "mergeCommit":observation.payload["mergeCommit"],
                        })));
                    }
                }
                if observations.len() < usize::from(PAGE) {
                    break;
                }
                page_after = observations.last().map(|row| row.id.clone());
            }
        }
        if links.len() < usize::from(PAGE) {
            return Ok(None);
        }
        after = links.last().map(|link| SequenceKey {
            sequence: link.sequence,
            source: 1,
            id: link.id.as_str().into(),
        });
    }
}

fn resolution_permitted(
    tx: &mut dyn WriteTransaction,
    job: &ResolutionEffect,
    id: &OperationId,
    close: bool,
) -> Result<bool, StorageError> {
    let Some(binding) = tx.github().github_resolution(&job.resolution_id)? else {
        return Ok(false);
    };
    if binding.state != "Sealed"
        || binding.completion_digest != job.completion_digest
        || binding.obligation_revision != tx.github().github_obligation_revision(&binding.key)?
    {
        return Ok(false);
    }
    if close {
        if &binding.close_operation_id != id
            || !tx
                .operations()
                .operation(&binding.comment_operation_id)?
                .is_some_and(|row| row.state == OperationState::ObservedComplete)
        {
            return Ok(false);
        }
    } else if &binding.comment_operation_id != id {
        return Ok(false);
    }
    let mut after = None;
    loop {
        let page =
            tx.github()
                .github_resolution_witnesses(&binding.id, None, after.as_ref(), PAGE)?;
        for witness in &page {
            if !tx.work().work(&witness.work_id.0)?.is_some_and(|work| {
                work.revision == witness.work_revision
                    && work.completion.as_deref() == Some(VERIFIED)
                    && work.project_id == witness.project_id
            }) || !permits_publication(tx, &witness.work_id)?
            {
                return Ok(false);
            }
        }
        if page.len() < usize::from(PAGE) {
            return Ok(true);
        }
        after = page.last().map(|row| row.work_id.clone());
    }
}

fn permits_publication(tx: &mut dyn WriteTransaction, root: &RootId) -> Result<bool, StorageError> {
    let control = control_of(tx.runs(), root)?;
    Ok(control.desired == DesiredExecution::Enabled && control.stop_operation_id.is_none())
}

impl ResolutionEffect {
    fn matches(&self, target: &PollTarget) -> bool {
        self.repository_id == target.repository.id
            && self.binding_revision == target.binding.revision
            && self.permission_revision == target.account.permission_revision
            && target.account.remote_user_id.as_deref() == Some(&self.remote_user_id)
    }
}

fn same_issue(expected: &Value, current: &Value, after_comment: bool) -> bool {
    if expected["state"] == "closed" && current["state"] != "closed" {
        return false;
    }
    ["title", "body", "labels", "authorId", "remoteIssueId"]
        .iter()
        .all(|field| expected[*field] == current[*field])
        && expected["commentCount"].as_u64().is_some_and(|count| {
            current["commentCount"].as_u64() == count.checked_add(u64::from(after_comment))
        })
}

fn invalid() -> StorageError {
    StorageError::InvalidRecord("Invalid GitHub verified resolution".into())
}
