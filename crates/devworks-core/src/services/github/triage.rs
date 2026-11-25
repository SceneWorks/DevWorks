//! The app-wide intake gate. Recording an issue observation never spends an admission.
use super::GitHubService;
use super::credentials;
use crate::fingerprint::sha256_hex;
use crate::ports::IdKind;
use crate::ports::storage::{
    ConfigScope, GithubAdmission, GithubIntakeFilter, GithubScope, NewEvent, NewOperation,
    NewQueueEntry, RepositoryFilter, StorageError, StoredGithubIssueIntake, WriteOptions,
    WriteTransaction, is_terminal, run_write,
};
use crate::protocol::{ActorId, EventId, Id, OperationId};
use crate::services::runs::ControlService;
use crate::settings::{
    catalog::descriptor,
    model::{chain, current_layer},
};
use serde_json::json;
use std::sync::Arc;

pub const CONSUMER: &str = "github-triage";
pub(super) const CAPABILITY: &str = "github.triage";
const INTERVAL: &str = "github.issueTriageIntervalSeconds";
const PAGE: u16 = 100;

impl GitHubService {
    /// Atomically admit one real observation and enqueue a recoverable triage delivery.
    /// The runtime calls this only when it has a triage executor.
    pub fn admit_triage(
        &self,
        control: Arc<ControlService>,
    ) -> Result<Option<GithubAdmission>, StorageError> {
        let actor = self.store.info().host_actor;
        run_write(&*self.store, WriteOptions::host_state(), move |tx| {
            admit(tx, &actor, &control)
        })
    }
}

fn admit(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    control: &ControlService,
) -> Result<Option<GithubAdmission>, StorageError> {
    let Some(account) = tx.github().github_account()? else {
        return Ok(None);
    };
    if !credentials::account_is_current(tx.configuration(), &account)? {
        return Ok(None);
    }
    let mut clock = tx.github().github_intake_clock()?;
    let interval = interval_seconds(tx)?;
    if clock.interval_seconds != interval {
        clock = tx
            .github()
            .set_github_intake_interval(clock.revision, interval)?;
    }
    let now = tx.now_ms();
    if now < clock.next_admission_at_ms || clock.last_admitted_at_ms.is_some_and(|last| now < last)
    {
        return Ok(None);
    }
    let registrations = authorized_registrations(tx, account.permission_revision)?;
    let filter = GithubIntakeFilter {
        scope: GithubScope {
            project_id: None,
            repositories: registrations,
        },
        states: vec!["Queued".into()],
    };
    let Some(issue) = next_eligible(tx, &filter, control)? else {
        return Ok(None);
    };
    let op = OperationId(tx.new_id(IdKind::Operation));
    let payload = json!({
        "repositoryId": issue.key.repository_id,
        "actorId": actor,
        "issueNumber": issue.key.number,
        "sourceRevision": issue.remote_revision,
        "projectId": issue.project_id,
        "workId": issue.work_id,
        "permissionRevision": account.permission_revision,
    });
    tx.operations().record_intent(NewOperation {
        id: op.clone(),
        root_id: issue.work_id.clone(),
        assignment_id: None,
        capability: CAPABILITY.into(),
        schema_version: 1,
        args_digest: sha256_hex(payload.to_string().as_bytes()),
        payload: payload.clone(),
    })?;
    let admission = tx
        .github()
        .admit_github_issue(&issue.key, issue.revision, clock.revision, &op)?
        .ok_or(StorageError::Busy)?;
    let event = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id: event.clone(),
        actor: actor.clone(),
        project_id: issue.project_id,
        root_id: issue.work_id.clone(),
        operation_id: Some(op.clone()),
        aggregate_kind: "github_issue_intake".into(),
        aggregate_id: format!("{}-{}", issue.key.repository_id.as_str(), issue.key.number),
        aggregate_revision: admission.issue.revision,
        kind: "GithubTriageAdmitted".into(),
        payload: payload.clone(),
    })?;
    let delivery_id = tx.new_id(IdKind::QueueEntry);
    tx.queue().enqueue(NewQueueEntry {
        id: delivery_id,
        consumer: CONSUMER.into(),
        event_id: event,
        root_id: issue.work_id,
        operation_id: Some(op),
        partition: None,
        priority: 0,
        ready_at_ms: now,
        payload,
    })?;
    Ok(Some(admission))
}

fn interval_seconds(tx: &mut dyn WriteTransaction) -> Result<i64, StorageError> {
    let layer = current_layer(tx.configuration(), &ConfigScope::Workspace)?;
    let descriptor = descriptor(INTERVAL).expect("catalog GitHub triage interval");
    let entries = chain(descriptor, &[layer])
        .map_err(|_| StorageError::InvalidRecord("GitHub triage interval configuration".into()))?;
    entries
        .first()
        .and_then(|entry| entry.value.value.0.as_i64())
        .filter(|seconds| *seconds > 0 && seconds.checked_mul(1000).is_some())
        .ok_or_else(|| StorageError::InvalidRecord("GitHub triage interval is out of range".into()))
}

fn authorized_registrations(
    tx: &mut dyn WriteTransaction,
    permission_revision: i64,
) -> Result<Vec<Id>, StorageError> {
    let mut registrations = Vec::new();
    let mut after = None;
    loop {
        let page =
            tx.projects()
                .repositories(&RepositoryFilter::default(), after.as_ref(), PAGE)?;
        for repository in &page {
            if repository.lifecycle != "Active" {
                continue;
            }
            let Some(binding) = tx
                .github()
                .github_repository_binding(&repository.id)?
                .filter(|binding| binding.state == "Active")
            else {
                continue;
            };
            if tx
                .github()
                .github_discovered_repository(&binding.github_repository_id)?
                .is_some_and(|remote| remote.permission_revision == permission_revision)
            {
                registrations.push(repository.id.clone());
            }
        }
        if page.len() < usize::from(PAGE) {
            return Ok(registrations);
        }
        after = page.last().map(|repo| repo.id.clone());
    }
}

fn next_eligible(
    tx: &mut dyn WriteTransaction,
    filter: &GithubIntakeFilter,
    control: &ControlService,
) -> Result<Option<StoredGithubIssueIntake>, StorageError> {
    let mut after = None;
    let mut earliest = None;
    loop {
        let page = tx.github().github_issues(filter, after.as_ref(), PAGE)?;
        for issue in &page {
            if let Some(operation) = &issue.operation_id
                && tx
                    .operations()
                    .operation(operation)?
                    .is_none_or(|op| !is_terminal(op.state))
            {
                continue;
            }
            if let Some(root) = &issue.work_id
                && !control.root_permits_dispatch(tx, root)?
            {
                continue;
            }
            let Some(observation) = tx
                .github()
                .github_observation(&issue.observation_id, &filter.scope)?
            else {
                continue;
            };
            if issue.work_id.is_none() && observation.payload["state"] != "open" {
                continue;
            }
            let sort = (observation.observed_at_ms, observation.id);
            if earliest
                .as_ref()
                .is_none_or(|(at, _): &((i64, Id), StoredGithubIssueIntake)| {
                    (sort.0, sort.1.as_str()) < (at.0, at.1.as_str())
                })
            {
                earliest = Some((sort, issue.clone()));
            }
        }
        if page.len() < usize::from(PAGE) {
            return Ok(earliest.map(|(_, issue)| issue));
        }
        after = page.last().map(|issue| issue.key.clone());
    }
}
