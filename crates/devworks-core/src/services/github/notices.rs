//! Route persisted GitHub observations to work's current assignments, reconcile remote closures
//! against local obligations, preserve human pauses across remote reopenings, and escalate inbound
//! scope changes/conflicts without silently overwriting local plans.
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::kernel::notice::{NotificationDraft, publish};
use crate::operations::agents::MessagesInput;
use crate::operations::tasks::MoveInput;
use crate::ports::IdKind;
use crate::ports::storage::{
    ApplicationStore, ApprovalSource, ArtifactScope, ConfigScope, EventRecord, GithubIssueKey,
    GithubObligationRetirementReason, GithubScope, GithubWorkLink, ReadOptions, StorageError,
    StoredApproval, StoredGithubObligationRetirement, WORKFLOW_QUEUE, WorkFilter, WorkPosition,
    WorkSelection, WriteOptions, WriteTransaction, read_snapshot, run_write,
};
use crate::protocol::records::{MessageClass, NotificationLink};
use crate::protocol::support::{BoundedList, BoundedText, Url};
use crate::protocol::{ActorId, Counter, Id, OperationState, ProjectId, Revision, RootId};
use crate::services::agents::post_assignment;
use crate::services::artifacts::{ArtifactService, evidence::Evidence};
use crate::services::notifications::notify_approval;
use crate::services::work::{WorkService, workflow};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;

const PAGE: u16 = 128;

/// Reconciles remote closure obligations (requiring host evidence artifacts for unstarted
/// unreproducible bug retirement) and then delivers inbound notices and conflict escalations.
pub fn reconcile_and_deliver(
    store: &dyn ApplicationStore,
    artifacts: &ArtifactService,
    work_service: &Arc<WorkService>,
    event: &EventRecord,
) -> Result<(), StorageError> {
    if event.kind == "GithubIssueChanged" && event.payload["state"] == "closed" {
        reconcile_remote_closure(store, artifacts, work_service, event)?;
    }
    let event = event.clone();
    run_write(store, WriteOptions::host_state(), move |tx| {
        deliver(tx, &event)
    })
}

struct RetirementCandidate {
    key: GithubIssueKey,
    project_id: ProjectId,
    work_id: RootId,
    work_revision: i64,
    obligation_revision: i64,
    operation_id: crate::protocol::OperationId,
    actor_id: ActorId,
    observation_id: Id,
    closed_status_id: Id,
    workflow_revision: i64,
    case_revision: i64,
}

fn reconcile_remote_closure(
    store: &dyn ApplicationStore,
    artifacts: &ArtifactService,
    work_service: &Arc<WorkService>,
    event: &EventRecord,
) -> Result<(), StorageError> {
    let local = field_id(&event.payload, "repositoryId")?;
    let remote = field_id(&event.payload, "githubRepositoryId")?;
    let number = event.payload["issueNumber"]
        .as_i64()
        .filter(|n| *n > 0)
        .ok_or_else(invalid)?;
    let key = GithubIssueKey {
        repository_id: remote.clone(),
        number,
    };
    let url = field(&event.payload, "url")?.to_owned();

    let (candidates, feature_retentions) =
        read_snapshot(store, &ReadOptions::default(), |snapshot| {
            check_source_snapshot(snapshot, &local, &remote)?;
            let scope = GithubScope {
                project_id: None,
                repositories: vec![local.clone()],
            };
            let Some(intake) = snapshot.github().github_issue(&key, &scope)? else {
                return Ok((Vec::new(), Vec::new()));
            };
            let Some(obs) = snapshot
                .github()
                .github_observation(&intake.observation_id, &scope)?
            else {
                return Ok((Vec::new(), Vec::new()));
            };
            if obs.payload["state"] != "closed" {
                return Ok((Vec::new(), Vec::new()));
            }
            let links = all_links(snapshot, &key)?;
            let mut retire = Vec::new();
            let mut retain_features = Vec::new();
            for link in links {
                let Some(work) = snapshot.work().work(&link.work_id.0)? else {
                    continue;
                };
                if work.project_id != link.project_id
                    || !snapshot
                        .projects()
                        .project(&work.project_id)?
                        .is_some_and(|p| p.lifecycle == "Active")
                {
                    continue;
                }
                let statuses = snapshot.work().statuses(&work.project_id)?;
                if work
                    .status_id
                    .as_ref()
                    .and_then(|s| workflow::semantic_of(&statuses, s))
                    .is_some_and(workflow::finished)
                {
                    continue;
                }
                let case = snapshot
                    .github()
                    .github_triage_case(&key, &link.project_id)?;
                let task_type = work.brief["taskType"].as_str();
                let is_feature_request = task_type == Some("Story")
                    || case
                        .as_ref()
                        .and_then(|c| c.evidence.as_ref())
                        .is_some_and(|e| e["kind"] == "FeatureRequest");
                if is_feature_request && !work.is_feature() {
                    retain_features.push((link.project_id, link.work_id));
                    continue;
                }
                // Started fixes, verified work, and feature-scoped work continue undisturbed.
                let started = work.is_feature()
                    || work.parent_feature_id.is_some()
                    || work.completion.is_some()
                    || snapshot
                        .runs()
                        .live_assignment_for_work(&link.work_id)?
                        .is_some()
                    || !snapshot
                        .sessions()
                        .links(
                            &link.work_id,
                            &["pull_request", "branch", "commit"],
                            None,
                            1,
                        )?
                        .is_empty();
                let Some(case) = case.filter(|c| !started && c.state == "AwaitingDecision") else {
                    continue;
                };
                if task_type.is_some_and(|t| t != "Bug") {
                    continue;
                }
                let Some(op) = snapshot.operations().operation(&case.triage_operation_id)? else {
                    continue;
                };
                if op.state != OperationState::ObservedComplete {
                    continue;
                }
                let Some(actor_str) = op.payload["actorId"].as_str() else {
                    continue;
                };
                let actor_id = ActorId(Id::new(actor_str.to_owned()).map_err(|_| invalid())?);
                let closed_status_id = statuses
                    .iter()
                    .find(|s| s.semantic == workflow::CLOSED)
                    .map(|s| s.id.clone())
                    .unwrap_or_else(|| workflow::default_status_id(&work.project_id, "closed"));
                let workflow_revision = snapshot
                    .work()
                    .queue_revision(&work.project_id, WORKFLOW_QUEUE)?;
                let obligation_revision = snapshot.github().github_obligation_revision(&key)?;
                retire.push(RetirementCandidate {
                    key: key.clone(),
                    project_id: link.project_id,
                    work_id: link.work_id,
                    work_revision: work.revision,
                    obligation_revision,
                    operation_id: case.triage_operation_id,
                    actor_id,
                    observation_id: intake.observation_id.clone(),
                    closed_status_id,
                    workflow_revision,
                    case_revision: case.revision,
                });
            }
            Ok((retire, retain_features))
        })?;

    if !feature_retentions.is_empty() {
        let event_id = event.id.0.clone();
        let url_copy = url.clone();
        run_write(store, WriteOptions::host_state(), move |tx| {
            for (project_id, work_id) in feature_retentions {
                publish(
                    tx,
                    &NotificationDraft {
                        source: format!(
                            "github-remote-close-feature:{}:{}",
                            event_id.as_str(),
                            work_id.0.as_str()
                        ),
                        category: "GitHub".into(),
                        decision_id: None,
                        project_id: Some(project_id.clone()),
                        root_id: Some(work_id.clone()),
                        title: "GitHub feature request closed remotely — manual decision retained"
                            .into(),
                        body: format!(
                            "Remote issue #{number} closed; local feature request stays open for manual review."
                        ),
                        links: vec![NotificationLink {
                            kind: BoundedText("GitHub issue".into()),
                            target_id: Some(work_id.0),
                            url: Some(Url(url_copy.clone())),
                            artifact: None,
                            project_id: Some(project_id),
                        }],
                        requires_input: true,
                        priority: 0,
                    },
                )?;
            }
            Ok(())
        })?;
    }

    for candidate in candidates {
        let host = store.info().host_actor;
        let next_work_revision = candidate.work_revision + 1;
        let source_meta = json!({
            "kind": "GithubObligationAdjudication",
            "repositoryId": candidate.key.repository_id,
            "issueNumber": candidate.key.number,
            "workId": candidate.work_id,
            "workRevision": next_work_revision,
            "obligationRevision": candidate.obligation_revision,
            "operationId": candidate.operation_id,
            "observationId": candidate.observation_id,
            "reason": GithubObligationRetirementReason::UnstartedUnreproducible,
            "actorId": candidate.actor_id,
        });
        let evidence_bytes = serde_json::to_vec(&source_meta).map_err(|_| invalid())?;
        let local_repo = local.clone();
        let remote_repo = remote.clone();
        let work_service = work_service.clone();
        let _ = artifacts
            .publish_evidence(
                store,
                Evidence {
                    scope: ArtifactScope::Workspace,
                    producer: host,
                    principal_kind: "Host".into(),
                    agent_session: None,
                    root: None,
                    assignment: None,
                    title: format!(
                        "Remote closure adjudication for GitHub issue #{}",
                        candidate.key.number
                    ),
                    media_type: "application/json".into(),
                    tags: vec!["github-adjudication".into()],
                    source: source_meta,
                },
                &evidence_bytes,
                move |tx, reference| {
                    check_source_tx(tx, &local_repo, &remote_repo)?;
                    workflow::ensure_workflow(tx, &candidate.project_id)?;
                    let info = tx.store().clone();
                    let host_ctx = InvocationContext::new(
                        info.host_actor,
                        PrincipalKind::Host,
                        tx.new_id(IdKind::Invocation),
                        Id::new("github-remote-closure").expect("static ingress"),
                        info.store_id,
                        Counter::new(info.restore_epoch).map_err(|_| invalid())?,
                        Revision::new(1).expect("positive policy revision"),
                    );
                    work_service
                        .move_task(
                            tx,
                            &host_ctx,
                            &MoveInput {
                                task_id: candidate.work_id.0.clone(),
                                expected_revision: Revision::new(candidate.work_revision)
                                    .map_err(|_| invalid())?,
                                expected_workflow_revision: Revision::new(
                                    candidate.workflow_revision,
                                )
                                .map_err(|_| invalid())?,
                                status_id: candidate.closed_status_id.clone(),
                            },
                        )
                        .map_err(|f| f.into_storage())?;
                    if let Some(mut case) = tx
                        .github()
                        .github_triage_case(&candidate.key, &candidate.project_id)?
                        && case.revision == candidate.case_revision
                    {
                        case.revision += 1;
                        case.state = "Closed".into();
                        tx.github()
                            .put_github_triage_case(&case, Some(candidate.case_revision))?;
                    }
                    let adjudication_ref =
                        serde_json::to_value(reference).map_err(|_| invalid())?;
                    let retirement = StoredGithubObligationRetirement {
                        id: tx.new_id(IdKind::Operation),
                        key: candidate.key,
                        project_id: candidate.project_id,
                        work_id: candidate.work_id,
                        work_revision: next_work_revision,
                        obligation_revision: candidate.obligation_revision,
                        reason: GithubObligationRetirementReason::UnstartedUnreproducible,
                        operation_id: candidate.operation_id,
                        actor_id: candidate.actor_id,
                        observation_id: candidate.observation_id,
                        adjudication_ref,
                        created_at_ms: tx.now_ms(),
                    };
                    tx.github().retire_github_work_link(&retirement)?;
                    super::triage_case::refresh_intake_state(tx, &local_repo, &retirement.key)
                },
            )
            .map_err(|e| match e {
                crate::services::artifacts::evidence::EvidenceFailure::Storage(s) => s,
                crate::services::artifacts::evidence::EvidenceFailure::Refused(_) => {
                    StorageError::InvalidRecord("GitHub adjudication artifact refused".into())
                }
            })?;
    }
    Ok(())
}

pub fn deliver(tx: &mut dyn WriteTransaction, event: &EventRecord) -> Result<(), StorageError> {
    let local = field_id(&event.payload, "repositoryId")?;
    let remote = field_id(&event.payload, "githubRepositoryId")?;
    check_source_tx(tx, &local, &remote)?;
    let (roots, url, description) = match event.kind.as_str() {
        "GithubIssueChanged" => {
            let number = event.payload["issueNumber"]
                .as_i64()
                .filter(|number| *number > 0)
                .ok_or_else(invalid)?;
            let key = GithubIssueKey {
                repository_id: remote.clone(),
                number,
            };
            let roots = issue_roots(tx, key.clone())?;
            let url = field(&event.payload, "url")?;
            record_inbound_clarification_if_needed(tx, &local, &key, event, &roots, url)?;
            escalate_inbound_conflict_if_needed(tx, &local, &key, event, &roots, url)?;
            (roots, url, format!("GitHub issue #{number} changed"))
        }
        "GithubPullEvidenceChanged" | "GithubPullRequestChanged" => {
            let pull_url = field(&event.payload, "pullUrl")?;
            let roots = tx
                .sessions()
                .links_to("pull_request", pull_url, crate::ports::storage::MAX_PAGE)?
                .into_iter()
                .filter(|link| link.repository_id.as_ref() == Some(&local))
                .map(|link| link.root_id)
                .collect();
            (
                roots,
                field(&event.payload, "url")?,
                "GitHub pull request evidence changed".into(),
            )
        }
        "GithubRefChanged" => {
            let roots = repository_roots(tx, &local)?;
            (
                roots,
                field(&event.payload, "headUrl")?,
                "GitHub branch advanced".into(),
            )
        }
        _ => return Err(invalid()),
    };
    let safe = url::Url::parse(url).map_err(|_| invalid())?;
    if safe.scheme() != "https"
        || safe.host_str() != Some("github.com")
        || !safe.username().is_empty()
        || safe.password().is_some()
        || safe.port().is_some()
    {
        return Err(invalid());
    }
    let info = tx.store().clone();
    let host = InvocationContext::new(
        info.host_actor,
        PrincipalKind::Host,
        tx.new_id(IdKind::Invocation),
        Id::new("github-observations").expect("static GitHub ingress"),
        info.store_id,
        Counter::new(info.restore_epoch).map_err(|_| invalid())?,
        Revision::new(1).expect("positive policy revision"),
    );
    let mut recipients = BTreeSet::new();
    for root in roots {
        let Some(work) = tx.work().work(&root.0)? else {
            continue;
        };
        if !tx
            .projects()
            .project(&work.project_id)?
            .is_some_and(|project| project.lifecycle == "Active")
            || !tx.projects().members(&work.project_id)?.contains(&local)
        {
            continue;
        }
        for work in std::iter::once(root).chain(work.parent_feature_id.map(RootId)) {
            if let Some(assignment) = tx.runs().live_assignment_for_work(&work)? {
                recipients.insert(assignment.id.0.as_str().to_owned());
            }
        }
    }
    let text = format!(
        "[{description}](<{safe}>)\nGitHub observation {}",
        event.id.0.as_str()
    );
    for recipient in recipients {
        let assignment = crate::protocol::AssignmentId(Id::new(recipient).map_err(|_| invalid())?);
        post_assignment(
            tx,
            &host,
            &MessagesInput {
                recipient_assignment_id: assignment,
                message_class: MessageClass::Evidence,
                content: BoundedText(text.clone()),
                attachments: BoundedList(Vec::new()),
            },
        )
        .map_err(|failure| failure.into_storage())?;
    }
    Ok(())
}

fn record_inbound_clarification_if_needed(
    tx: &mut dyn WriteTransaction,
    local: &Id,
    key: &GithubIssueKey,
    event: &EventRecord,
    roots: &[RootId],
    url: &str,
) -> Result<(), StorageError> {
    if roots.is_empty() {
        return Ok(());
    }
    let scope = GithubScope {
        project_id: None,
        repositories: vec![local.clone()],
    };
    let note_text = if let Some(comment_str) = event.payload["commentObservationId"].as_str() {
        let comment_id = Id::new(comment_str.to_owned()).map_err(|_| invalid())?;
        let Some(obs) = tx.github().github_observation(&comment_id, &scope)? else {
            return Ok(());
        };
        let body = obs.payload["body"].as_str().unwrap_or("").trim();
        if body.is_empty() || body.contains("<!-- devworks-resolution:") {
            return Ok(());
        }
        let author = obs.payload["author"].as_str().unwrap_or("github");
        format!("GitHub issue comment by @{author} ({}):\n\n{body}", obs.url)
    } else {
        let (Some(prev_str), Some(curr_str)) = (
            event.payload["previousObservationId"].as_str(),
            event.payload["observationId"].as_str(),
        ) else {
            return Ok(());
        };
        let prev_id = Id::new(prev_str.to_owned()).map_err(|_| invalid())?;
        let curr_id = Id::new(curr_str.to_owned()).map_err(|_| invalid())?;
        let (Some(prev), Some(curr)) = (
            tx.github().github_observation(&prev_id, &scope)?,
            tx.github().github_observation(&curr_id, &scope)?,
        ) else {
            return Ok(());
        };
        if curr.payload["state"] != "open" || prev.payload["state"] != "open" {
            return Ok(());
        }
        let title_changed = prev.payload["title"] != curr.payload["title"];
        let prev_body = prev.payload["body"].as_str().unwrap_or("");
        let curr_body = curr.payload["body"].as_str().unwrap_or("");
        let body_rewritten = !prev_body.is_empty() && !curr_body.starts_with(prev_body);
        let scope_label_added = curr.payload["labels"]
            .as_array()
            .is_some_and(|curr_labels| {
                let prev_labels = prev.payload["labels"].as_array();
                curr_labels.iter().filter_map(Value::as_str).any(|label| {
                    let lower = label.to_ascii_lowercase();
                    (lower.contains("feature")
                        || lower.contains("enhancement")
                        || lower.contains("breaking")
                        || lower.contains("scope"))
                        && !prev_labels
                            .is_some_and(|p| p.iter().any(|old| old.as_str() == Some(label)))
                })
            });
        if title_changed || body_rewritten || scope_label_added {
            return Ok(());
        }
        let appended = curr_body
            .strip_prefix(prev_body)
            .map(str::trim)
            .unwrap_or("");
        if appended.is_empty() {
            return Ok(());
        }
        format!(
            "Clarifying update appended on GitHub issue #{} ({url}):\n\n{appended}",
            key.number
        )
    };

    let host = tx.store().host_actor.clone();
    for root in roots {
        let Some(mut work) = tx.work().work(&root.0)? else {
            continue;
        };
        if !tx
            .projects()
            .project(&work.project_id)?
            .is_some_and(|project| project.lifecycle == "Active")
            || !tx.projects().members(&work.project_id)?.contains(local)
        {
            continue;
        }
        // Preserve work.revision while container reproduction is in flight for NeedsProof cases.
        if tx
            .github()
            .github_triage_case(key, &work.project_id)?
            .is_some_and(|case| case.state == "NeedsProof")
        {
            continue;
        }
        let comment_id = Id::new(format!(
            "gh-ev-{}",
            &sha256_hex(format!("{}:{}", event.id.0.as_str(), work.id.as_str()).as_bytes())[..24]
        ))
        .expect("bounded GitHub inbound comment ID");
        let mut brief = crate::kernel::work_brief::WorkBrief::of(&work)?;
        if brief.comments.len() >= 1024 || brief.comments.iter().any(|c| c.item.id == comment_id) {
            continue;
        }
        brief.comments.push(crate::kernel::work_brief::Noted {
            item: crate::protocol::records::WorkComment {
                id: comment_id,
                content: crate::services::kernel::content(&note_text),
                evidence: BoundedList(Vec::new()),
            },
            actor: host.clone(),
            at_ms: tx.now_ms(),
        });
        work.brief = brief.value();
        work.updated_by = Some(host.clone());
        work.updated_at_ms = tx.now_ms();
        let expected = work.revision;
        work.revision += 1;
        tx.work().update_work(&work, expected)?;
    }
    Ok(())
}

fn escalate_inbound_conflict_if_needed(
    tx: &mut dyn WriteTransaction,
    local: &Id,
    key: &GithubIssueKey,
    event: &EventRecord,
    roots: &[RootId],
    url: &str,
) -> Result<(), StorageError> {
    if roots.is_empty() || event.payload.get("commentObservationId").is_some() {
        return Ok(());
    }
    let Some(prev_str) = event.payload["previousObservationId"].as_str() else {
        return Ok(());
    };
    let Some(curr_str) = event.payload["observationId"].as_str() else {
        return Ok(());
    };
    let prev_id = Id::new(prev_str.to_owned()).map_err(|_| invalid())?;
    let curr_id = Id::new(curr_str.to_owned()).map_err(|_| invalid())?;
    let scope = GithubScope {
        project_id: None,
        repositories: vec![local.clone()],
    };
    let (Some(prev), Some(curr)) = (
        tx.github().github_observation(&prev_id, &scope)?,
        tx.github().github_observation(&curr_id, &scope)?,
    ) else {
        return Ok(());
    };
    if curr.payload["state"] != "open" || prev.payload["state"] != "open" {
        return Ok(());
    }
    let title_changed = prev.payload["title"] != curr.payload["title"];
    let prev_body = prev.payload["body"].as_str().unwrap_or("");
    let curr_body = curr.payload["body"].as_str().unwrap_or("");
    // Clarifying details append to the existing report; rewriting or replacing existing
    // requirements or changing title/scope labels escalates for explicit human decision.
    let body_rewritten = !prev_body.is_empty() && !curr_body.starts_with(prev_body);
    let scope_label_added = curr.payload["labels"]
        .as_array()
        .is_some_and(|curr_labels| {
            let prev_labels = prev.payload["labels"].as_array();
            curr_labels.iter().filter_map(Value::as_str).any(|label| {
                let lower = label.to_ascii_lowercase();
                (lower.contains("feature")
                    || lower.contains("enhancement")
                    || lower.contains("breaking")
                    || lower.contains("scope"))
                    && !prev_labels.is_some_and(|p| p.iter().any(|old| old.as_str() == Some(label)))
            })
        });
    if !title_changed && !body_rewritten && !scope_label_added {
        return Ok(());
    }

    let host = tx.store().host_actor.clone();
    let restore_epoch = tx.store().restore_epoch;
    for root in roots {
        let Some(work) = tx.work().work(&root.0)? else {
            continue;
        };
        if !tx
            .projects()
            .project(&work.project_id)?
            .is_some_and(|project| project.lifecycle == "Active")
            || !tx.projects().members(&work.project_id)?.contains(local)
        {
            continue;
        }
        let approval_id = Id::new(format!(
            "gh-upd-{}",
            &sha256_hex(format!("{}:{}", event.id.0.as_str(), work.id.as_str()).as_bytes())[..32]
        ))
        .expect("bounded GitHub inbound approval ID");
        if tx.decisions().approval(&approval_id)?.is_some() {
            continue;
        }
        let reviewed = json!({
            "kind": "GithubInboundIssueConflict",
            "issueNumber": key.number,
            "url": url,
            "previousObservationId": prev.id,
            "currentObservationId": curr.id,
            "previousTitle": prev.payload["title"],
            "currentTitle": curr.payload["title"],
            "previousBody": prev_body,
            "currentBody": curr_body,
        });
        let content_digest = sha256_hex(reviewed.to_string().as_bytes());
        let approval_scope = ConfigScope::Project(work.project_id.clone());
        let scope_digest = sha256_hex(
            json!({
                "scope": format!("{approval_scope:?}"),
                "target": work.id.as_str(),
                "targetRevision": work.revision,
                "contentDigest": content_digest,
                "restoreEpoch": restore_epoch,
            })
            .to_string()
            .as_bytes(),
        );
        let summary = format!(
            "GitHub issue #{} changed scope or conflicted with retained description ({url})",
            key.number
        );
        let proposal = StoredApproval {
            id: approval_id.clone(),
            scope: approval_scope,
            revision: 1,
            state: "Pending".into(),
            operation_kind: "github.issue_update".into(),
            target_id: work.id.clone(),
            target_revision: work.revision,
            content_digest,
            scope_digest,
            summary: summary.clone(),
            artifact: None,
            payload: json!({"content": null, "reviewed": reviewed}),
            author: host.clone(),
            author_kind: PrincipalKind::Host,
            created_at_ms: tx.now_ms(),
            decided_by: None,
            decided_at_ms: None,
            rationale: None,
            restore_epoch,
        };
        tx.decisions().put_approval(&proposal, None)?;
        notify_approval(
            tx,
            ApprovalSource::Generic,
            &proposal.id,
            Some(&work.project_id),
            "Review conflicting or expanded GitHub issue update",
            &summary,
        )?;
    }
    Ok(())
}

fn check_source_snapshot(
    snapshot: &dyn crate::ports::storage::ReadSnapshot,
    local: &Id,
    remote: &Id,
) -> Result<(), StorageError> {
    if !snapshot
        .github()
        .github_repository_binding(local)?
        .is_some_and(|binding| binding.state == "Active" && &binding.github_repository_id == remote)
    {
        return Err(StorageError::ScopeDenied);
    }
    let account = snapshot
        .github()
        .github_account()?
        .ok_or(StorageError::ScopeDenied)?;
    if !super::credentials::account_is_current(snapshot.configuration(), &account)?
        || !snapshot
            .github()
            .github_discovered_repository(remote)?
            .is_some_and(|repo| {
                repo.permission_revision == account.permission_revision
                    && repo.account_id == account.id
            })
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
}

fn check_source_tx(
    tx: &mut dyn WriteTransaction,
    local: &Id,
    remote: &Id,
) -> Result<(), StorageError> {
    if !tx
        .github()
        .github_repository_binding(local)?
        .is_some_and(|binding| binding.state == "Active" && &binding.github_repository_id == remote)
    {
        return Err(StorageError::ScopeDenied);
    }
    let account = tx
        .github()
        .github_account()?
        .ok_or(StorageError::ScopeDenied)?;
    if !super::credentials::account_is_current(tx.configuration(), &account)?
        || !tx
            .github()
            .github_discovered_repository(remote)?
            .is_some_and(|repo| {
                repo.permission_revision == account.permission_revision
                    && repo.account_id == account.id
            })
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
}

fn repository_roots(tx: &mut dyn WriteTransaction, repo: &Id) -> Result<Vec<RootId>, StorageError> {
    let projects = tx.projects().repository_projects(repo)?;
    let mut roots = Vec::new();
    for project in projects {
        for selection in [WorkSelection::Tasks, WorkSelection::Features] {
            let filter = WorkFilter {
                project: project.clone(),
                selection,
                status_ids: Vec::new(),
                repository_ids: vec![repo.clone()],
                text: None,
            };
            let mut after = None;
            loop {
                let page = tx.work().works(&filter, after.as_ref(), PAGE)?;
                let more = page.len() == usize::from(PAGE);
                after = page.last().map(WorkPosition::of);
                roots.extend(page.into_iter().map(|row| RootId(row.id)));
                if !more {
                    break;
                }
            }
        }
    }
    Ok(roots)
}

fn all_links(
    snapshot: &dyn crate::ports::storage::ReadSnapshot,
    key: &GithubIssueKey,
) -> Result<Vec<GithubWorkLink>, StorageError> {
    let mut links = Vec::new();
    let mut after = None;
    loop {
        let page = snapshot
            .github()
            .github_work_links(key, None, after.as_ref(), PAGE)?;
        let more = page.len() == usize::from(PAGE);
        after = page.last().map(|row| row.work_id.clone());
        links.extend(page);
        if !more {
            return Ok(links);
        }
    }
}

fn issue_roots(
    tx: &mut dyn WriteTransaction,
    key: GithubIssueKey,
) -> Result<Vec<RootId>, StorageError> {
    let mut roots = Vec::new();
    let mut after = None;
    loop {
        let page = tx
            .github()
            .github_work_links(&key, None, after.as_ref(), PAGE)?;
        let more = page.len() == usize::from(PAGE);
        after = page.last().map(|row| row.work_id.clone());
        roots.extend(page.into_iter().map(|row| row.work_id));
        if !more {
            return Ok(roots);
        }
    }
}

fn field<'a>(payload: &'a Value, key: &str) -> Result<&'a str, StorageError> {
    payload[key]
        .as_str()
        .filter(|val| !val.is_empty())
        .ok_or_else(invalid)
}

fn field_id(payload: &Value, key: &str) -> Result<Id, StorageError> {
    Id::new(field(payload, key)?.to_owned()).map_err(|_| invalid())
}

fn invalid() -> StorageError {
    StorageError::InvalidRecord("Invalid GitHub notification observation".into())
}
