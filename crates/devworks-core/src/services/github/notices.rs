//! Route persisted GitHub observations to work's current assignments. The message and delivery
//! acknowledgement share the queue transaction, and posting evidence leaves execution controls alone.
use crate::context::{InvocationContext, PrincipalKind};
use crate::operations::agents::MessagesInput;
use crate::ports::IdKind;
use crate::ports::storage::{
    EventRecord, GithubIssueKey, StorageError, WorkFilter, WorkPosition, WorkSelection,
    WriteTransaction,
};
use crate::protocol::records::MessageClass;
use crate::protocol::support::{BoundedList, BoundedText};
use crate::protocol::{Counter, Id, Revision, RootId};
use crate::services::agents::post_assignment;
use serde_json::Value;
use std::collections::BTreeSet;

const PAGE: u16 = 128;

pub fn deliver(tx: &mut dyn WriteTransaction, event: &EventRecord) -> Result<(), StorageError> {
    let local = field_id(&event.payload, "repositoryId")?;
    let remote = field_id(&event.payload, "githubRepositoryId")?;
    if !tx
        .github()
        .github_repository_binding(&local)?
        .is_some_and(|binding| binding.state == "Active" && binding.github_repository_id == remote)
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
            .github_discovered_repository(&remote)?
            .is_some_and(|repo| {
                repo.permission_revision == account.permission_revision
                    && repo.account_id == account.id
            })
    {
        return Err(StorageError::ScopeDenied);
    }
    let (roots, url, description) = match event.kind.as_str() {
        "GithubIssueChanged" => {
            let number = event.payload["issueNumber"]
                .as_i64()
                .filter(|number| *number > 0)
                .ok_or_else(invalid)?;
            let roots = issue_roots(
                tx,
                GithubIssueKey {
                    repository_id: remote,
                    number,
                },
            )?;
            (
                roots,
                field(&event.payload, "url")?,
                format!("GitHub issue #{number} changed"),
            )
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
