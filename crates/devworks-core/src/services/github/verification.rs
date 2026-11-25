//! Binds a bug reproduction to a repository-committed check definition at the immutable indexed
//! commit OID. Untrusted GitHub issue text never supplies a container image or command line.
use super::triage_case::{self, BugVerificationProposal, TriageDestination, TriageInput};
use super::{GitHubService, issue_poll};
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::canonical_digest;
use crate::kernel::notice::{NotificationDraft, publish};
use crate::ports::IdKind;
use crate::ports::storage::{
    DesiredExecution, GithubIssueKey, NewEvent, NewOperation, NewQueueEntry, OperationRecord,
    ProcessFilter, ProcessPhase, ReadOptions, StorageError, StoredGithubTriageCase,
    WriteTransaction, read_snapshot,
};
use crate::protocol::records::NotificationLink;
use crate::protocol::support::{BoundedText, Url};
use crate::protocol::{Counter, EventId, Id, OperationId, OperationState, ProjectId, Revision};
use crate::services::runs::{ControlService, control_of};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const CAPABILITY: &str = "github.verify_issue";
pub const CONSUMER: &str = "github_verify_issue";
pub const CHECKS_MANIFEST_PATH: &str = ".devworks/github-checks.json";

/// Repository-committed check manifest read from `<oid>:.devworks/github-checks.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryCheckManifest {
    pub checks: Vec<RepositoryCheck>,
}

/// One repository-defined verification check with a fixed container image digest and argv.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryCheck {
    pub id: Id,
    pub description: String,
    pub image: String,
    pub argv: Vec<String>,
}

impl RepositoryCheck {
    pub fn validates(&self) -> bool {
        let image = !self.image.starts_with('-')
            && (self
                .image
                .split_once("@sha256:")
                .is_some_and(|(name, digest)| {
                    !name.is_empty()
                        && name.len() <= 256
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-_./:".contains(&b))
                        && digest.len() == 64
                        && digest
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                })
                || self
                    .image
                    .strip_prefix("sha256:")
                    .is_some_and(crate::fingerprint::is_sha256_hex));
        image
            && !self.description.is_empty()
            && self.description.len() <= 512
            && !self.description.chars().any(char::is_control)
            && !self.argv.is_empty()
            && self.argv.len() <= 32
            && self.argv.iter().all(|arg| {
                !arg.is_empty() && arg.len() <= 4096 && !arg.chars().any(char::is_control)
            })
            && !self.argv[0].starts_with('-')
    }
}

/// Parses and filters valid repository-committed checks from `.devworks/github-checks.json`.
pub fn parse_manifest(bytes: &[u8]) -> Vec<RepositoryCheck> {
    let Ok(manifest) = serde_json::from_slice::<RepositoryCheckManifest>(bytes) else {
        return Vec::new();
    };
    if manifest.checks.len() > 32 {
        return Vec::new();
    }
    manifest
        .checks
        .into_iter()
        .filter(RepositoryCheck::validates)
        .collect()
}

/// A bound verification recipe combining a trusted repository check with literal failure matchers.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Reproduction {
    pub project_id: ProjectId,
    pub check_id: Id,
    pub image: String,
    pub argv: Vec<String>,
    pub report_quote: String,
    pub test_identifier: String,
    pub expected_failure: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Job {
    pub operation_id: OperationId,
    pub issue: GithubIssueKey,
    pub issue_url: String,
    pub issue_title: String,
    pub case_id: Id,
    pub case_revision: i64,
    pub source_revision: String,
    pub permission_revision: i64,
    pub restore_epoch: i64,
    pub project_scope_revision: i64,
    pub repository_id: Id,
    pub repository_revision: i64,
    pub work_revision: i64,
    pub control_revision: i64,
    pub checkout: String,
    pub recipe: Reproduction,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Witness {
    pub checkout: String,
    pub argv_digest: String,
    pub exit_code: i64,
    pub evidence_digest: String,
}

pub(super) fn prepare_case(
    tx: &mut dyn WriteTransaction,
    case: &StoredGithubTriageCase,
    admission: &OperationRecord,
    input: &TriageInput,
    destination: &TriageDestination,
    proposal: Option<&BugVerificationProposal>,
) -> Result<(), StorageError> {
    if case.triage_operation_id != admission.id
        || case.state != "NeedsProof"
        || case.evidence.is_some()
    {
        return Ok(());
    }
    let repo = tx
        .projects()
        .repository(&destination.repository_id)?
        .ok_or(StorageError::ScopeDenied)?;
    let project = tx
        .projects()
        .project(&case.project_id)?
        .ok_or(StorageError::ScopeDenied)?;
    let work = tx
        .work()
        .work(&case.work_id.0)?
        .ok_or(StorageError::ScopeDenied)?;
    let control = control_of(tx.runs(), &case.work_id)?;
    let checkout = tx.indexing().published(&repo.id)?.map(|view| view.view_id);
    let checkout = if let Some(id) = checkout {
        tx.projects()
            .index_view(&id)?
            .map(|view| view.source_revision)
    } else {
        None
    };
    let trusted_recipe = checkout
        .as_ref()
        .filter(|oid| {
            super::indexing::valid_oid(oid) && destination.indexed_commit.as_ref() == Some(*oid)
        })
        .and_then(|oid| {
            let proposal = proposal.filter(|p| p.validates(&input.report))?;
            let check = destination
                .checks
                .iter()
                .find(|check| check.id == proposal.check_id && check.validates())?;
            Some((
                oid.clone(),
                Reproduction {
                    project_id: destination.project_id.clone(),
                    check_id: check.id.clone(),
                    image: check.image.clone(),
                    argv: check.argv.clone(),
                    report_quote: proposal.report_quote.clone(),
                    test_identifier: proposal.test_identifier.clone(),
                    expected_failure: proposal.expected_failure.clone(),
                },
            ))
        });
    let Some((checkout, recipe)) = trusted_recipe else {
        let mut next = case.clone();
        next.state = "AwaitingDecision".into();
        next.revision += 1;
        tx.github()
            .put_github_triage_case(&next, Some(case.revision))?;
        return triage_case::notify_for_work(
            tx,
            admission,
            input,
            Some(&case.project_id),
            Some(&case.work_id),
            "Review a GitHub issue whose reproduction is unavailable",
        );
    };
    let id = OperationId(tx.new_id(IdKind::Operation));
    let job = Job {
        operation_id: id.clone(),
        issue: case.key.clone(),
        issue_url: input.url.clone(),
        issue_title: input.title.clone(),
        case_id: case.id.clone(),
        case_revision: case.revision + 1,
        source_revision: case.source_revision.clone(),
        restore_epoch: tx.store().restore_epoch,
        permission_revision: admission.payload["permissionRevision"]
            .as_i64()
            .ok_or_else(invalid)?,
        repository_id: destination.repository_id.clone(),
        repository_revision: repo.revision,
        project_scope_revision: project.scope_revision,
        work_revision: work.revision,
        control_revision: control.revision,
        checkout,
        recipe,
    };
    let payload = serde_json::to_value(&job).map_err(|_| invalid())?;
    let fingerprint = canonical_digest(&payload).map_err(|_| invalid())?;
    tx.operations().record_intent(NewOperation {
        id: id.clone(),
        root_id: Some(case.work_id.clone()),
        assignment_id: None,
        capability: CAPABILITY.into(),
        schema_version: 1,
        args_digest: fingerprint,
        payload,
    })?;
    let mut next = case.clone();
    next.revision = job.case_revision;
    next.evidence = Some(serde_json::json!({"verificationOperationId":id}));
    tx.github()
        .put_github_triage_case(&next, Some(case.revision))?;
    let event = EventId(tx.new_id(IdKind::Event));
    let actor = tx.store().host_actor.clone();
    tx.history().append(NewEvent {
        id: event.clone(),
        actor,
        project_id: Some(case.project_id.clone()),
        root_id: Some(case.work_id.clone()),
        operation_id: Some(id.clone()),
        aggregate_kind: "github_triage_case".into(),
        aggregate_id: case.id.as_str().into(),
        aggregate_revision: next.revision,
        kind: "GithubIssueVerificationQueued".into(),
        payload: serde_json::json!({"checkout":job.checkout,"checkId":job.recipe.check_id}),
    })?;
    let delivery = tx.new_id(IdKind::QueueEntry);
    let now = tx.now_ms();
    tx.queue().enqueue(NewQueueEntry {
        id: delivery,
        consumer: CONSUMER.into(),
        event_id: event,
        root_id: Some(case.work_id.clone()),
        operation_id: Some(id),
        partition: None,
        priority: 0,
        ready_at_ms: now,
        payload: serde_json::json!({}),
    })?;
    Ok(())
}

pub fn job(operation: &OperationRecord) -> Result<Job, StorageError> {
    if operation.capability != CAPABILITY || operation.schema_version != 1 {
        return Err(invalid());
    }
    let job = serde_json::from_value::<Job>(operation.payload.clone()).map_err(|_| invalid())?;
    if job.operation_id != operation.id {
        return Err(invalid());
    }
    Ok(job)
}

impl GitHubService {
    /// Resolves the immutable checkout target after checking all saved authority again.
    pub fn verification_target(
        &self,
        operation: &OperationRecord,
        job: &Job,
    ) -> Result<Option<PathBuf>, StorageError> {
        let Some(root) = &operation.root_id else {
            return Ok(None);
        };
        read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            let Some(target) = issue_poll::target(snapshot, &job.repository_id)? else {
                return Ok(None);
            };
            if target.repository.id != job.issue.repository_id
                || job.permission_revision != target.account.permission_revision
                || job.restore_epoch != snapshot.stamp().restore_epoch
                || !snapshot
                    .projects()
                    .project(&job.recipe.project_id)?
                    .is_some_and(|row| {
                        row.lifecycle == "Active"
                            && row.scope_revision == job.project_scope_revision
                    })
                || !snapshot
                    .projects()
                    .members(&job.recipe.project_id)?
                    .contains(&job.repository_id)
                || !snapshot
                    .work()
                    .work(&root.0)?
                    .is_some_and(|row| row.revision == job.work_revision)
                || !snapshot
                    .github()
                    .github_issue(&job.issue, &issue_poll::scope(&job.repository_id))?
                    .is_some_and(|issue| issue.remote_revision == job.source_revision)
            {
                return Ok(None);
            }
            let control = control_of(snapshot.runs(), root)?;
            if control.desired != DesiredExecution::Enabled
                || control.revision != job.control_revision
                || control.stop_operation_id.is_some()
            {
                return Ok(None);
            }
            Ok(snapshot
                .projects()
                .repository(&job.repository_id)?
                .filter(|row| row.revision == job.repository_revision && row.lifecycle == "Active")
                .map(|row| PathBuf::from(row.location)))
        })
    }
}

pub fn settle(
    tx: &mut dyn WriteTransaction,
    control: &ControlService,
    operation: &OperationRecord,
    witness: Option<Witness>,
) -> Result<(), StorageError> {
    if operation.state != OperationState::ObservedComplete {
        return Err(invalid());
    }
    let job = job(operation)?;
    let Some(mut case) = tx
        .github()
        .github_triage_case(&job.issue, &job.recipe.project_id)?
    else {
        return Ok(());
    };
    if case.id != job.case_id
        || case.revision != job.case_revision
        || case.state != "NeedsProof"
        || case
            .evidence
            .as_ref()
            .and_then(|row| row["verificationOperationId"].as_str())
            != Some(operation.id.0.as_str())
    {
        return Ok(());
    }
    let now = tx.store().clone();
    let account = tx
        .github()
        .github_account()?
        .ok_or(StorageError::ScopeDenied)?;
    if job.restore_epoch != now.restore_epoch
        || job.permission_revision != account.permission_revision
        || !super::credentials::account_is_current(tx.configuration(), &account)?
        || !tx
            .github()
            .github_repository_binding(&job.repository_id)?
            .is_some_and(|row| {
                row.state == "Active" && row.github_repository_id == job.issue.repository_id
            })
        || !tx
            .projects()
            .repository(&job.repository_id)?
            .is_some_and(|row| row.revision == job.repository_revision && row.lifecycle == "Active")
        || !tx
            .projects()
            .project(&job.recipe.project_id)?
            .is_some_and(|row| {
                row.scope_revision == job.project_scope_revision && row.lifecycle == "Active"
            })
        || !tx
            .projects()
            .members(&job.recipe.project_id)?
            .contains(&job.repository_id)
    {
        return Err(StorageError::ScopeDenied);
    }
    let work = tx
        .work()
        .work(&case.work_id.0)?
        .filter(|work| work.project_id == case.project_id)
        .ok_or(StorageError::ScopeDenied)?;
    let issue = tx
        .github()
        .github_issue(&job.issue, &issue_poll::scope(&job.repository_id))?
        .ok_or(StorageError::ScopeDenied)?;
    // A superseded result may settle the old case, but it cannot authorize new work.
    let current =
        issue.remote_revision == job.source_revision && work.revision == job.work_revision;
    let witness = witness.filter(|_| current);
    let root = control_of(tx.runs(), &case.work_id)?;
    let mut valid = false;
    if let Some(witness) = &witness {
        valid = witness.checkout == job.checkout
            && crate::fingerprint::is_sha256_hex(&witness.evidence_digest)
            && tx
                .resources()
                .processes(&ProcessFilter::Operation(operation.id.clone()), None, 10)?
                .iter()
                .any(|row| {
                    row.argv_sha256 == witness.argv_digest
                        && row.phase == ProcessPhase::Reaped
                        && row.exit_code == Some(witness.exit_code)
                        && witness.exit_code != 0
                });
    }
    let expected = case.revision;
    case.revision += 1;
    let permitted = valid
        && root.desired == DesiredExecution::Enabled
        && root.revision == job.control_revision
        && root.stop_operation_id.is_none();
    if permitted {
        let host = InvocationContext::new(
            now.host_actor,
            PrincipalKind::Host,
            tx.new_id(IdKind::Invocation),
            Id::new("github-verification").expect("static ingress"),
            now.store_id,
            Counter::new(now.restore_epoch).map_err(|_| invalid())?,
            Revision::new(1).expect("positive revision"),
        );
        if control.start_verified_issue(
            tx,
            &host,
            &case.work_id,
            job.work_revision,
            job.control_revision,
        )? {
            case.state = "Working".into();
        } else {
            case.state = "Verified".into();
        }
    } else {
        case.state = "AwaitingDecision".into();
        publish(
            tx,
            &NotificationDraft {
                source: format!("github-verify:{}", operation.id.0.as_str()),
                category: "GitHub".into(),
                decision_id: None,
                project_id: Some(case.project_id.clone()),
                root_id: Some(case.work_id.clone()),
                title: if current {
                    "Review a GitHub issue that could not be verified automatically"
                } else {
                    "Review a GitHub issue that changed during verification"
                }
                .into(),
                body: job.issue_title.clone(),
                links: vec![NotificationLink {
                    kind: BoundedText("GitHub issue".into()),
                    target_id: Some(case.work_id.0.clone()),
                    url: Some(Url(job.issue_url.clone())),
                    artifact: None,
                    project_id: Some(case.project_id.clone()),
                }],
                requires_input: true,
                priority: 0,
            },
        )?;
    }
    case.evidence = Some(serde_json::json!({
        "verificationOperationId": operation.id,
        "reproduction": witness,
        "sourceChanged": !current,
    }));
    tx.github().put_github_triage_case(&case, Some(expected))?;
    super::triage_case::refresh_intake_state(tx, &job.repository_id, &job.issue)?;
    Ok(())
}

fn invalid() -> StorageError {
    StorageError::InvalidRecord("Invalid GitHub issue reproduction witness".into())
}
