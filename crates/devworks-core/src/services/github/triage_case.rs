//! Durable case routing. The classification result can prepare an investigation; independent
//! verification evidence from a repository-defined check is required before implementation starts.
use super::verification::RepositoryCheck;
use super::{GitHubService, credentials, issue_poll};
use crate::context::{InvocationContext, PrincipalKind};
use crate::kernel::notice::{NotificationDraft, publish};
use crate::ports::IdKind;
use crate::ports::storage::{
    GithubIssueKey, GithubRead, GithubScope, OperationRecord, ReadOptions, ReadSnapshot,
    RepositoryFilter, StorageError, StoredGithubIssueIntake, StoredGithubTriageCase,
    WriteTransaction, read_snapshot,
};
use crate::protocol::records::{NotificationLink, WorkItemType};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, Url};
use crate::protocol::{Counter, Id, OperationState, ProjectId, Revision, RootId};
use crate::services::kernel::{content, label};
use crate::services::work::{WorkService, workflow};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

const PAGE: u16 = 64;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TriageDestination {
    pub project_id: ProjectId,
    pub repository_id: Id,
    pub repository_location: String,
    pub indexed_commit: Option<String>,
    pub project_name: String,
    pub project_description: String,
    #[serde(default)]
    pub checks: Vec<RepositoryCheck>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TriageInput {
    pub title: String,
    pub report: String,
    pub url: String,
    pub destinations: Vec<TriageDestination>,
    pub registrations: Vec<Id>,
}

/// Classifier selection of one trusted repository check ID and literal failure matchers.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BugVerificationProposal {
    pub project_id: ProjectId,
    pub check_id: Id,
    pub report_quote: String,
    pub test_identifier: String,
    pub expected_failure: String,
}

impl BugVerificationProposal {
    pub fn validates(&self, report: &str) -> bool {
        [
            self.report_quote.as_str(),
            self.test_identifier.as_str(),
            self.expected_failure.as_str(),
        ]
        .into_iter()
        .all(|value| (8..=1024).contains(&value.len()) && !value.chars().any(char::is_control))
            && report.contains(&self.report_quote)
            && self.test_identifier != self.expected_failure
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum TriageClassification {
    FeatureRequest,
    Ambiguous,
    Unverifiable,
    Bugs {
        destinations: Vec<ProjectId>,
        #[serde(default)]
        verifications: Vec<BugVerificationProposal>,
    },
}

impl GitHubService {
    /// Exact observed source and currently authorized routing candidates for a journaled
    /// classification. None means the admission was superseded before any inference.
    pub fn triage_input(&self, op: &OperationRecord) -> Result<Option<TriageInput>, StorageError> {
        read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            load_input(snapshot, op)
        })
    }
}

fn load_input(
    snapshot: &dyn ReadSnapshot,
    op: &OperationRecord,
) -> Result<Option<TriageInput>, StorageError> {
    let key = issue_key(op)?;
    let Some(account) = snapshot.github().github_account()? else {
        return Ok(None);
    };
    if !credentials::account_is_current(snapshot.configuration(), &account)?
        || op.payload["permissionRevision"].as_i64() != Some(account.permission_revision)
    {
        return Ok(None);
    }

    let mut candidates = BTreeMap::new();
    let mut registrations = Vec::new();
    let mut after = None;
    loop {
        let page =
            snapshot
                .projects()
                .repositories(&RepositoryFilter::default(), after.as_ref(), PAGE)?;
        let more = page.len() == usize::from(PAGE);
        after = page.last().map(|repo| repo.id.clone());
        for repo in page {
            let Some(bound) = issue_poll::target(snapshot, &repo.id)? else {
                continue;
            };
            if bound.repository.id != key.repository_id {
                continue;
            }
            let indexed_commit = snapshot
                .indexing()
                .published(&repo.id)?
                .map(|published| published.view_id)
                .map(|view_id| snapshot.projects().index_view(&view_id))
                .transpose()?
                .flatten()
                .map(|view| view.source_revision)
                .filter(|oid| super::indexing::valid_oid(oid));
            for project in snapshot.projects().repository_projects(&repo.id)? {
                let Some(row) = snapshot
                    .projects()
                    .project(&project)?
                    .filter(|row| row.lifecycle == "Active")
                else {
                    continue;
                };
                candidates
                    .entry(project.0.as_str().to_owned())
                    .or_insert_with(|| TriageDestination {
                        project_id: project,
                        repository_id: repo.id.clone(),
                        repository_location: repo.location.clone(),
                        indexed_commit: indexed_commit.clone(),
                        project_name: row.name,
                        project_description: row.description,
                        checks: Vec::new(),
                    });
            }
            registrations.push(repo.id);
        }
        if !more {
            break;
        }
    }
    let scope = GithubScope {
        project_id: None,
        repositories: registrations,
    };
    let Some(row) = snapshot
        .github()
        .github_issue(&key, &scope)?
        .filter(|row| current_admission(row, op))
    else {
        return Ok(None);
    };
    let observation = snapshot
        .github()
        .github_observation(&row.observation_id, &scope)?
        .ok_or_else(invalid)?;
    if row.work_id.is_none() && observation.payload["state"] != "open" {
        return Ok(None);
    }
    Ok(Some(TriageInput {
        title: observation.payload["title"]
            .as_str()
            .ok_or_else(invalid)?
            .to_owned(),
        report: observation.payload["body"]
            .as_str()
            .ok_or_else(invalid)?
            .to_owned(),
        url: observation.url,
        destinations: candidates.into_values().collect(),
        registrations: scope.repositories,
    }))
}

/// Apply only a completed classification receipt. The captured routing proposal never
/// changes an existing obligation's ownership or authorizes an implementation launch.
pub fn settle_classification(
    tx: &mut dyn WriteTransaction,
    work: &WorkService,
    operation: &OperationRecord,
    input: &TriageInput,
    classification: &TriageClassification,
) -> Result<(), StorageError> {
    let key = issue_key(operation)?;
    let Some(account) = tx.github().github_account()? else {
        return Err(StorageError::ScopeDenied);
    };
    if operation.state != OperationState::ObservedComplete
        || !credentials::account_is_current(tx.configuration(), &account)?
        || operation.payload["permissionRevision"].as_i64() != Some(account.permission_revision)
    {
        return Err(StorageError::ScopeDenied);
    }
    let scope = GithubScope {
        project_id: None,
        repositories: input.registrations.clone(),
    };
    let Some(mut intake) = tx
        .github()
        .github_issue(&key, &scope)?
        .filter(|row| current_admission(row, operation))
    else {
        return Ok(());
    };
    let ready = match classification {
        TriageClassification::Bugs { destinations, .. }
            if !destinations.is_empty()
                && destinations.len() <= 16
                && destinations.iter().all(|project| {
                    input
                        .destinations
                        .iter()
                        .any(|target| &target.project_id == project)
                })
                && !destinations
                    .iter()
                    .enumerate()
                    .any(|(index, project)| destinations[..index].contains(project)) =>
        {
            destinations
                .iter()
                .filter_map(|project| {
                    input
                        .destinations
                        .iter()
                        .find(|target| &target.project_id == project)
                })
                .collect::<Vec<_>>()
        }
        _ => Vec::new(),
    };
    let expected = intake.revision;
    intake.revision += 1;
    if ready.is_empty() {
        if matches!(classification, TriageClassification::FeatureRequest)
            && input.destinations.len() == 1
        {
            let destination = &input.destinations[0];
            let case = begin_case(
                tx,
                work,
                &intake,
                operation,
                input,
                destination,
                WorkItemType::Story,
                "AwaitingDecision",
                Some(json!({"kind": "FeatureRequest"})),
            )?;
            intake.work_id = Some(case.work_id.clone());
            intake.project_id = Some(case.project_id.clone());
            intake.state = "AwaitingDecision".into();
            notify_for_work(
                tx,
                operation,
                input,
                Some(&case.project_id),
                Some(&case.work_id),
                "Review a GitHub feature request",
            )?;
        } else {
            let reason = match classification {
                TriageClassification::FeatureRequest => "Review a GitHub feature request",
                TriageClassification::Unverifiable => {
                    "Review a GitHub issue that could not be reproduced"
                }
                _ => "Choose where to route a GitHub issue",
            };
            notify_for_work(tx, operation, input, None, None, reason)?;
            intake.state = "AwaitingDecision".into();
        }
    } else {
        for destination in ready {
            let case = begin_case(
                tx,
                work,
                &intake,
                operation,
                input,
                destination,
                WorkItemType::Bug,
                "NeedsProof",
                None,
            )?;
            let proposal = match classification {
                TriageClassification::Bugs { verifications, .. } => verifications
                    .iter()
                    .find(|recipe| recipe.project_id == destination.project_id),
                _ => None,
            };
            super::verification::prepare_case(tx, &case, operation, input, destination, proposal)?;
            if input.destinations.len() == 1 {
                intake.work_id = Some(case.work_id);
                intake.project_id = Some(case.project_id);
            }
        }
        // The admitted source stays in triage during its independent verification operations.
        // Storing a route is not trusted verification.
    }
    tx.github().put_github_issue(&intake, Some(expected))?;
    if let Some(local_repo) = input.registrations.first() {
        refresh_intake_state(tx, local_repo, &key)?;
    }
    Ok(())
}

pub(crate) fn refresh_intake_state(
    tx: &mut dyn WriteTransaction,
    local_repo: &Id,
    key: &GithubIssueKey,
) -> Result<(), StorageError> {
    let scope = super::issue_poll::scope(local_repo);
    let Some(mut intake) = tx.github().github_issue(key, &scope)? else {
        return Ok(());
    };
    if intake.state == "Resolved" {
        return Ok(());
    }
    let mut links = Vec::new();
    let mut after = None;
    loop {
        let page = tx
            .github()
            .github_work_links(key, None, after.as_ref(), 128)?;
        let more = page.len() == 128;
        after = page.last().map(|row| row.work_id.clone());
        links.extend(page);
        if !more {
            break;
        }
    }
    let mut any_working = false;
    let mut any_awaiting = false;
    for link in &links {
        if let Some(case) = tx.github().github_triage_case(key, &link.project_id)? {
            match case.state.as_str() {
                "NeedsProof" => return Ok(()),
                "Working" | "Verified" => any_working = true,
                "AwaitingDecision" => any_awaiting = true,
                _ => {}
            }
        }
    }
    let next_state = if links.is_empty()
        && tx
            .github()
            .github_observation(&intake.observation_id, &scope)?
            .is_some_and(|obs| obs.payload["state"] == "closed")
    {
        "Closed"
    } else if any_working {
        "Working"
    } else if any_awaiting {
        "AwaitingDecision"
    } else {
        return Ok(());
    };
    if intake.state != next_state {
        let expected = intake.revision;
        intake.revision += 1;
        intake.state = next_state.into();
        tx.github().put_github_issue(&intake, Some(expected))?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn begin_case(
    tx: &mut dyn WriteTransaction,
    service: &WorkService,
    intake: &StoredGithubIssueIntake,
    operation: &OperationRecord,
    input: &TriageInput,
    destination: &TriageDestination,
    item_type: WorkItemType,
    initial_state: &str,
    initial_evidence: Option<serde_json::Value>,
) -> Result<StoredGithubTriageCase, StorageError> {
    if !tx
        .projects()
        .project(&destination.project_id)?
        .is_some_and(|p| p.lifecycle == "Active")
        || !tx
            .projects()
            .members(&destination.project_id)?
            .contains(&destination.repository_id)
        || !tx
            .github()
            .github_repository_binding(&destination.repository_id)?
            .is_some_and(|binding| {
                binding.state == "Active"
                    && binding.github_repository_id == intake.key.repository_id
            })
    {
        return Err(StorageError::ScopeDenied);
    }
    if let Some(existing) = tx
        .github()
        .github_triage_case(&intake.key, &destination.project_id)?
    {
        // Changed reports do not rewrite the case or silently cause another launch. The
        // verification/approval owner records a new input revision against this same work.
        return Ok(existing);
    }
    let claimed =
        tx.github()
            .github_work_links(&intake.key, Some(&destination.project_id), None, 2)?;
    let work_id = match claimed.as_slice() {
        [link] => link.work_id.clone(),
        [] => {
            let who = tx.store().clone();
            let ctx = InvocationContext::new(
                who.host_actor,
                PrincipalKind::Host,
                tx.new_id(IdKind::Invocation),
                Id::new("github-triage").expect("static ingress identity"),
                who.store_id,
                Counter::new(who.restore_epoch).map_err(|_| invalid())?,
                Revision::new(1).expect("positive policy revision"),
            );
            let status = workflow::default_status(tx, &destination.project_id)?.id;
            let report_body = if input.report.trim().is_empty() {
                format!("Source report: {}", input.url)
            } else {
                format!("Source report: {}\n\n{}", input.url, input.report.trim())
            };
            let result = service
                .create_issue(
                    tx,
                    &ctx,
                    &crate::operations::issues::CreateInput {
                        project_id: destination.project_id.clone(),
                        title: label(&input.title, "GitHub issue"),
                        description: content(&report_body),
                        repository_ids: BoundedList(vec![destination.repository_id.clone()]),
                        status_id: status,
                        feature_id: None,
                        priority: 0,
                        task_type: Some(item_type),
                        labels: None,
                    },
                )
                .map_err(|failure| failure.into_storage())?;
            let DomainDecision::Applied {
                result: OperationResult::IssuesCreate { value },
            } = result
            else {
                return Err(invalid());
            };
            let work = RootId(value.id);
            tx.github()
                .link_github_work(&intake.key, &work, value.revision.get())?;
            work
        }
        _ => return Err(StorageError::ScopeDenied),
    };
    let row = StoredGithubTriageCase {
        id: tx.new_id(IdKind::Operation),
        revision: 1,
        key: intake.key.clone(),
        project_id: destination.project_id.clone(),
        work_id,
        source_revision: intake.remote_revision.clone(),
        triage_operation_id: operation.id.clone(),
        state: initial_state.into(),
        evidence: initial_evidence,
    };
    tx.github().put_github_triage_case(&row, None)?;
    Ok(row)
}

pub(crate) fn linked_report(
    github: &dyn GithubRead,
    root: &RootId,
    repositories: &[Id],
) -> Result<Option<(i64, String)>, StorageError> {
    for repository in repositories {
        if let Some(binding) = github.github_repository_binding(repository)?
            && let Some(issue) = github
                .github_issues_for_work(root, &binding.github_repository_id, 1)?
                .into_iter()
                .next()
            && let Some(remote) =
                github.github_discovered_repository(&binding.github_repository_id)?
        {
            let number = issue.key.number;
            return Ok(Some((number, format!("{}/issues/{number}", remote.url))));
        }
    }
    Ok(None)
}

pub(super) fn notify_for_work(
    tx: &mut dyn WriteTransaction,
    operation: &OperationRecord,
    input: &TriageInput,
    project_id: Option<&ProjectId>,
    root_id: Option<&RootId>,
    reason: &str,
) -> Result<(), StorageError> {
    publish(
        tx,
        &NotificationDraft {
            source: format!(
                "github-triage:{}:{}",
                operation.id.0.as_str(),
                project_id.map_or("workspace", |p| p.0.as_str())
            ),
            category: "GitHub".into(),
            decision_id: None,
            project_id: project_id.cloned(),
            root_id: root_id.cloned(),
            title: reason.into(),
            body: input.title.clone(),
            links: vec![NotificationLink {
                kind: BoundedText("GitHub issue".into()),
                target_id: root_id.map(|r| r.0.clone()),
                url: Some(Url(input.url.clone())),
                artifact: None,
                project_id: project_id.cloned(),
            }],
            requires_input: true,
            priority: 0,
        },
    )
}

fn issue_key(op: &OperationRecord) -> Result<GithubIssueKey, StorageError> {
    if op.capability != super::triage::CAPABILITY || op.schema_version != 1 {
        return Err(invalid());
    }
    let repository = op.payload["repositoryId"].as_str().ok_or_else(invalid)?;
    let repository_id = Id::new(repository.to_owned()).map_err(|_| invalid())?;
    let number = op.payload["issueNumber"]
        .as_i64()
        .filter(|number| *number > 0)
        .ok_or_else(invalid)?;
    Ok(GithubIssueKey {
        repository_id,
        number,
    })
}

fn current_admission(intake: &StoredGithubIssueIntake, operation: &OperationRecord) -> bool {
    intake.state == "Triaging"
        && intake.operation_id.as_ref() == Some(&operation.id)
        && operation.payload["sourceRevision"].as_str() == Some(&intake.remote_revision)
}

fn invalid() -> StorageError {
    StorageError::InvalidRecord("Invalid GitHub triage classification".into())
}
