//! Explicit issue links use the same authenticated source snapshots as background polling.
use super::api::Endpoint;
use super::issue_poll::{self, IssueObservation, PollTarget};
use super::repositories::api_fault;
use super::{GitHubService, reads};
use crate::context::InvocationContext;
use crate::operations::issues::LinkInput;
use crate::ports::Preparation;
use crate::ports::storage::{GithubIssueKey, StorageError, StoredWork, WriteTransaction};
use crate::protocol::records::{GithubIssueLink, GithubIssueLinkResult};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedText, PositiveCounter, Url};
use crate::protocol::{DomainErrorCode, Id, RootId};
use crate::services::git::github::Fault;
use crate::services::kernel::{Failure, Refusal, not_found};

pub(super) struct CheckedLink {
    target: PollTarget,
    work: StoredWork,
    issue: IssueObservation,
}

impl GitHubService {
    pub(super) fn prepare_link(
        &self,
        ctx: &InvocationContext,
        input: &LinkInput,
        env: &Preparation<'_>,
    ) -> Result<CheckedLink, Failure> {
        let (target, work) = env.read(|snapshot| {
            Ok((|| {
                let project = reads::project(ctx, snapshot, &input.repository_id)?;
                let work = snapshot
                    .work()
                    .work(&input.issue_id.0)?
                    .filter(|work| {
                        !work.is_feature()
                            && project
                                .as_ref()
                                .is_none_or(|project| project == &work.project_id)
                    })
                    .ok_or_else(|| not_found("issue", &input.issue_id.0))?;
                if work.revision != input.expected_revision.get() {
                    return Err(refusal(DomainErrorCode::Conflict, &input.issue_id.0).into());
                }
                if !snapshot
                    .projects()
                    .members(&work.project_id)?
                    .contains(&input.repository_id)
                {
                    return Err(not_found("github_repository", &input.repository_id).into());
                }
                let target = issue_poll::target(snapshot, &input.repository_id)?
                    .ok_or_else(|| not_found("github_repository", &input.repository_id))?;
                Ok::<_, Failure>((target, work))
            })())
        })??;
        let number = input
            .remote_issue_number
            .0
            .parse::<i64>()
            .map_err(|_| refusal(DomainErrorCode::InvalidArgument, &input.repository_id))?;
        let now = self.clock.now_ms().get();
        if env.stopped(now) {
            return Err(StorageError::Cancelled.into());
        }
        let issue = self
            .observe_issue(&target, number, now)
            .map_err(api_fault)?;
        Ok(CheckedLink {
            target,
            work,
            issue,
        })
    }

    pub(super) fn observe_issue(
        &self,
        target: &PollTarget,
        number: i64,
        now: i64,
    ) -> Result<IssueObservation, Fault> {
        let repo = &target.repository;
        let endpoint = Endpoint::new(
            &format!(
                "https://api.github.com/repos/{}/{}/issues/{number}",
                repo.owner, repo.name
            ),
            Some(&format!(
                "/repositories/{}/issues/{number}",
                target.numeric_id
            )),
        )?;
        let token = self
            .client
            .token_for_account(&*self.store, &target.account)?;
        let page = self
            .client
            .observe_authenticated_page(&endpoint, None, None, now, 0, token)?;
        if page.status != 200 {
            return Err(Fault::Rejected(page.status));
        }
        let row = page.value.as_ref().ok_or(Fault::InvalidResponse)?;
        if row.get("pull_request").is_some() {
            return Err(Fault::Rejected(404));
        }
        let issue = issue_poll::decode_issue(repo, row)?;
        if issue.number != number {
            return Err(Fault::InvalidResponse);
        }
        Ok(issue)
    }
}

pub(super) fn link(
    input: &LinkInput,
    checked: &CheckedLink,
    tx: &mut dyn WriteTransaction,
) -> Result<DomainDecision, Failure> {
    issue_poll::ensure_target(tx, &checked.target)?;
    let work = tx
        .work()
        .work(&input.issue_id.0)?
        .filter(|work| !work.is_feature() && work.project_id == checked.work.project_id)
        .ok_or_else(|| not_found("issue", &input.issue_id.0))?;
    if work.revision != input.expected_revision.get() {
        return Err(refusal(DomainErrorCode::Conflict, &input.issue_id.0).into());
    }
    if !tx
        .projects()
        .members(&work.project_id)?
        .contains(&input.repository_id)
    {
        return Err(not_found("github_repository", &input.repository_id).into());
    }
    if super::publish::publication_slot(tx, &input.issue_id, &checked.target.repository.id)?
        .2
        .is_some_and(|state| !crate::ports::storage::is_terminal(state))
    {
        return Err(refusal(DomainErrorCode::Conflict, &input.issue_id.0).into());
    }
    let key = GithubIssueKey {
        repository_id: checked.target.repository.id.clone(),
        number: checked.issue.number,
    };
    issue_poll::capture(tx, &checked.target, checked.issue.clone())?;
    let value = link_value(tx, &checked.target, &input.issue_id, &key)?;
    tx.github()
        .link_github_work(&key, &input.issue_id, work.revision)?;
    Ok(DomainDecision::Applied {
        result: OperationResult::IssuesLink { value },
    })
}

pub(super) fn link_value(
    tx: &mut dyn WriteTransaction,
    target: &PollTarget,
    work: &RootId,
    key: &GithubIssueKey,
) -> Result<GithubIssueLinkResult, StorageError> {
    let local = &target.binding.repository_id;
    let scope = issue_poll::scope(local);
    let source = tx
        .github()
        .github_issue(key, &scope)?
        .ok_or_else(|| StorageError::InvalidRecord("Missing GitHub issue snapshot".into()))?;
    let observation = tx
        .github()
        .github_observation(&source.observation_id, &scope)?
        .ok_or_else(|| StorageError::InvalidRecord("Missing GitHub issue observation".into()))?;
    let remote_issue_id = observation.payload["remoteIssueId"]
        .as_str()
        .ok_or_else(|| StorageError::InvalidRecord("Missing GitHub issue identity".into()))?;
    Ok(GithubIssueLinkResult::Current(GithubIssueLink {
        repository_id: local.clone(),
        work_id: work.clone(),
        remote_repository_id: BoundedText(target.repository.remote_repository_id.clone()),
        remote_issue_id: BoundedText(remote_issue_id.to_owned()),
        remote_issue_number: PositiveCounter(key.number.to_string()),
        url: Url(observation.url),
        source_revision: BoundedText(source.remote_revision),
    }))
}

fn refusal(code: DomainErrorCode, target: &Id) -> Refusal {
    Refusal::new(code, "github_issue", Some(target.clone()))
}
