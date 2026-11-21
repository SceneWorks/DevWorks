//! Exact-head GitHub checks, workflow runs and review evidence. Each endpoint owns its own
//! pagination and validators. Evidence from a previous head remains queryable as history.
use super::api::{ApiPage, Endpoint, Validators};
use super::issue_poll::{self, PollTarget};
use super::reads::ObservationKind;
use super::{GitHubService, repositories};
use crate::fingerprint::sha256_hex;
use crate::ports::IdKind;
use crate::ports::storage::{
    NewEvent, ReadOptions, StorageError, StoredGithubObservation, StoredGithubPollCursor,
    WriteOptions, WriteTransaction, read_snapshot, run_write,
};
use crate::protocol::{EventId, Id};
use crate::services::git::github::Fault;
use crate::services::kernel::{Failure, not_found};
use serde_json::{Value, json};

const INTERVAL_MS: i64 = 60_000;

#[derive(Clone, Copy)]
enum Evidence {
    CheckRun,
    CheckSuite,
    WorkflowRun,
    Status,
    Review,
    ReviewComment,
}

impl Evidence {
    fn kind(self) -> &'static str {
        match self {
            Self::CheckRun => "CheckRun",
            Self::CheckSuite => "CheckSuite",
            Self::WorkflowRun => "WorkflowRun",
            Self::Status => "CommitStatus",
            Self::Review => "Review",
            Self::ReviewComment => "ReviewComment",
        }
    }

    fn endpoint(self, number: &str, head: &str) -> (String, &'static str) {
        match self {
            Self::CheckRun => (
                format!("/commits/{head}/check-runs?filter=all"),
                "check_runs",
            ),
            Self::CheckSuite => (format!("/commits/{head}/check-suites?"), "check_suites"),
            Self::WorkflowRun => (format!("/actions/runs?head_sha={head}"), "workflow_runs"),
            Self::Status => (format!("/commits/{head}/statuses?"), ""),
            Self::Review => (format!("/pulls/{number}/reviews?"), ""),
            Self::ReviewComment => (format!("/pulls/{number}/comments?"), ""),
        }
    }
}

struct Pull<'a> {
    number: &'a str,
    head: &'a str,
}

impl GitHubService {
    pub(super) fn poll_pull_evidence(
        &self,
        registration: &Id,
        number: &str,
        head: &str,
        group: ObservationKind,
    ) -> Result<(), Failure> {
        if !number
            .parse::<i64>()
            .is_ok_and(|n| n > 0 && n.to_string() == number)
            || head.len() != 40
            || !head
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(repositories::api_fault(Fault::InvalidResponse));
        }
        let target = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            issue_poll::target(snapshot, registration)
        })?
        .ok_or_else(|| not_found("github_repository", registration))?;
        let pull = Pull { number, head };
        let current = self.observe_pull_head(&target, number)?;
        if current.as_deref() != Some(head) {
            // A superseded head reads retained history without attributing current evidence to it.
            return Ok(());
        }
        let endpoints: &[Evidence] = match group {
            ObservationKind::Checks => &[
                Evidence::CheckRun,
                Evidence::CheckSuite,
                Evidence::WorkflowRun,
                Evidence::Status,
            ],
            ObservationKind::Reviews => &[Evidence::Review, Evidence::ReviewComment],
        };
        for endpoint in endpoints {
            if self.is_stopped() {
                break;
            }
            if self
                .poll_evidence_endpoint(&target, &pull, *endpoint)?
                .is_some()
            {
                break;
            }
        }
        Ok(())
    }

    fn observe_pull_head(
        &self,
        target: &PollTarget,
        number: &str,
    ) -> Result<Option<String>, Failure> {
        let endpoint = Endpoint::new(
            &format!(
                "https://api.github.com/repos/{}/{}/pulls/{number}",
                target.repository.owner, target.repository.name
            ),
            Some(&format!(
                "/repositories/{}/pulls/{number}",
                target.numeric_id
            )),
        )
        .map_err(repositories::api_fault)?;
        let now = self.clock.now_ms().get();
        let token = self
            .client
            .token_for_account(&*self.store, &target.account)
            .map_err(repositories::api_fault)?;
        let page = self
            .client
            .observe_authenticated_page(&endpoint, None, None, now, INTERVAL_MS, token)
            .map_err(repositories::api_fault)?;
        if page.status != 200 {
            return Err(repositories::api_fault(Fault::Rejected(page.status)));
        }
        let value = page
            .value
            .as_ref()
            .ok_or_else(|| repositories::api_fault(Fault::InvalidResponse))?;
        let number_json = number.parse::<u64>().ok();
        let url = format!("{}/pull/{number}", target.repository.url);
        if value["base"]["repo"]["id"].as_u64() != Some(target.numeric_id)
            || value["number"].as_u64() != number_json
            || !value["html_url"]
                .as_str()
                .is_some_and(|raw| raw.eq_ignore_ascii_case(&url))
        {
            return Err(repositories::api_fault(Fault::InvalidResponse));
        }
        Ok(value["head"]["sha"].as_str().map(str::to_owned))
    }

    /// Reconcile one known open pull per pass, including when the pull list returned 304.
    pub(super) fn poll_scheduled_evidence(
        &self,
        registration: &Id,
    ) -> Result<Option<i64>, Failure> {
        const KEY: &str = "pull-evidence";
        let now = self.clock.now_ms().get();
        let selected = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            let Some(target) = issue_poll::target(snapshot, registration)? else {
                return Ok(None);
            };
            let cursor = snapshot
                .github()
                .github_poll_cursor(&target.repository.id, KEY)?;
            if cursor.as_ref().is_some_and(|row| row.next_poll_at_ms > now) {
                return Ok(None);
            }
            let after = cursor
                .as_ref()
                .and_then(|row| row.watermark.as_ref())
                .filter(|v| {
                    v["permissionRevision"].as_i64() == Some(target.account.permission_revision)
                })
                .and_then(|v| v["after"].as_i64())
                .unwrap_or(0);
            let pull = snapshot
                .github()
                .github_open_pulls(&target.repository.id, after, 1)?
                .into_iter()
                .next();
            Ok(Some((target, cursor, pull)))
        })?;
        let Some((target, cursor, pull)) = selected else {
            return Ok(None);
        };
        let mut after = 0;
        if let Some(pull) = pull {
            let number = &pull.subject_id;
            let head = pull
                .head
                .as_deref()
                .ok_or_else(|| StorageError::InvalidRecord("Missing pull request head".into()))?;
            let selected = Pull { number, head };
            let kinds = [
                Evidence::CheckRun,
                Evidence::CheckSuite,
                Evidence::WorkflowRun,
                Evidence::Status,
                Evidence::Review,
                Evidence::ReviewComment,
            ];
            for kind in kinds {
                if self.is_stopped() {
                    return Ok(None);
                }
                if let Some(until) = self.poll_evidence_endpoint(&target, &selected, kind)? {
                    return Ok(Some(until));
                }
            }
            after = number
                .parse::<i64>()
                .map_err(|_| StorageError::InvalidRecord("Invalid pull request number".into()))?;
        }
        run_write(&*self.store, WriteOptions::observed(), move |tx| {
            issue_poll::ensure_target(tx, &target)?;
            let revision = cursor.as_ref().map(|row| row.revision);
            let cursor = StoredGithubPollCursor {
                repository_id: target.repository.id.clone(),
                endpoint: KEY.into(),
                revision: revision.unwrap_or(0) + 1,
                etag: None,
                last_modified: None,
                next_page: None,
                bootstrap_complete: true,
                next_poll_at_ms: now.saturating_add(if after == 0 { INTERVAL_MS } else { 1000 }),
                last_polled_at_ms: Some(now),
                failure_count: 0,
                last_error: None,
                watermark: Some(
                    json!({"after":after,"permissionRevision":target.account.permission_revision}),
                ),
            };
            tx.github().put_github_poll_cursor(&cursor, revision)
        })?;
        Ok(None)
    }

    fn poll_evidence_endpoint(
        &self,
        target: &PollTarget,
        pull: &Pull<'_>,
        kind: Evidence,
    ) -> Result<Option<i64>, Failure> {
        let key = format!("pull:{}:{}:{}", pull.number, pull.head, kind.kind());
        let current = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            snapshot
                .github()
                .github_poll_cursor(&target.repository.id, &key)
        })?;
        let now = self.clock.now_ms().get();
        if current
            .as_ref()
            .is_some_and(|row| row.next_poll_at_ms > now)
        {
            return Ok(None);
        }
        let (suffix, container) = kind.endpoint(pull.number, pull.head);
        let path = suffix
            .split_once('?')
            .map_or(suffix.as_str(), |(path, _)| path);
        let join = if suffix.ends_with('?') { "" } else { "&" };
        let repo = &target.repository;
        let endpoint = Endpoint::new(
            &format!(
                "https://api.github.com/repos/{}/{}{}{join}per_page=100",
                repo.owner, repo.name, suffix,
            ),
            Some(&format!("/repositories/{}{path}", target.numeric_id)),
        )
        .map_err(repositories::api_fault)?;
        let prior = current.as_ref().filter(|row| {
            row.watermark.as_ref().and_then(Value::as_i64)
                == Some(target.account.permission_revision)
        });
        let next = prior.and_then(|row| row.next_page.as_deref());
        let validators = prior
            .filter(|row| next.is_none() && (row.etag.is_some() || row.last_modified.is_some()))
            .map(|row| Validators {
                etag: row.etag.clone(),
                last_modified: row.last_modified.clone(),
            });
        let page = self
            .client
            .token_for_account(&*self.store, &target.account)
            .and_then(|token| {
                self.client.observe_authenticated_page(
                    &endpoint,
                    next,
                    validators.as_ref(),
                    now,
                    INTERVAL_MS,
                    token,
                )
            });
        let rows = match &page {
            Ok(page) if page.status == 304 && validators.is_some() => Ok(Vec::new()),
            Ok(page) if page.status == 200 => {
                evidence_rows(target, pull, kind, container, page, now)
            }
            Ok(page) => Err(Fault::Rejected(page.status)),
            Err(_) => Err(Fault::Unavailable),
        };
        let throttle = page.as_ref().is_ok_and(|page| {
            page.status == 429
                || page.status == 403
                    && (page.control.remaining == Some(0) || page.control.retry_after_supplied)
        });
        let continuing = next.is_some();
        let selected = target.binding.repository_id.clone();
        let repo_id = target.repository.id.clone();
        let target = target.clone();
        run_write(&*self.store, WriteOptions::observed(), move |tx| {
            issue_poll::ensure_target(tx, &target)?;
            let revision = current.as_ref().map(|row| row.revision);
            let mut row = current.unwrap_or(StoredGithubPollCursor {
                repository_id: repo_id,
                endpoint: key,
                revision: 0,
                etag: None,
                last_modified: None,
                next_page: None,
                watermark: None,
                bootstrap_complete: false,
                next_poll_at_ms: 0,
                last_polled_at_ms: None,
                failure_count: 0,
                last_error: None,
            });
            row.revision += 1;
            row.last_polled_at_ms = Some(now);
            let pacing = page.as_ref().map_or(now, |page| page.control.poll_after_ms);
            match (rows, page) {
                (Ok(rows), Ok(page)) => {
                    for observation in rows {
                        publish_observation(tx, &selected, observation, row.bootstrap_complete)?;
                    }
                    row.next_page = page.next_page;
                    if !continuing {
                        if page.status == 200 || page.control.etag.is_some() {
                            row.etag = page.control.etag;
                        }
                        if page.status == 200 || page.control.last_modified.is_some() {
                            row.last_modified = page.control.last_modified;
                        }
                    }
                    row.bootstrap_complete |= row.next_page.is_none();
                    row.next_poll_at_ms = pacing;
                    row.failure_count = 0;
                    row.last_error = None;
                    row.watermark = Some(json!(target.account.permission_revision));
                }
                (error, _) => {
                    row.failure_count = row.failure_count.saturating_add(1);
                    let delay = INTERVAL_MS * (1_i64 << (row.failure_count - 1).min(6));
                    row.next_poll_at_ms = pacing.max(now.saturating_add(delay));
                    row.last_error = Some(
                        match error {
                            Err(Fault::Rejected(401)) => "GitHub authorization required",
                            _ if throttle => "GitHub request rate limited",
                            _ => "GitHub evidence endpoint unavailable",
                        }
                        .into(),
                    );
                }
            }
            tx.github().put_github_poll_cursor(&row, revision)?;
            Ok(throttle.then_some(row.next_poll_at_ms))
        })
        .map_err(Into::into)
    }
}

fn evidence_rows(
    target: &PollTarget,
    pull: &Pull<'_>,
    kind: Evidence,
    container: &str,
    page: &ApiPage,
    now: i64,
) -> Result<Vec<StoredGithubObservation>, Fault> {
    let value = page.value.as_ref().ok_or(Fault::InvalidResponse)?;
    let rows = if container.is_empty() {
        value
    } else {
        &value[container]
    };
    let rows = rows
        .as_array()
        .filter(|rows| rows.len() <= 100)
        .ok_or(Fault::InvalidResponse)?;
    let mut out = Vec::new();
    for row in rows {
        let observation_head = match kind {
            Evidence::Review | Evidence::ReviewComment => row["commit_id"].as_str(),
            Evidence::Status => Some(pull.head),
            _ => row["head_sha"].as_str(),
        };
        if observation_head != Some(pull.head) {
            // Review comments authored against another commit remain distinct evidence.
            continue;
        }
        let id = row["id"]
            .as_u64()
            .filter(|id| *id > 0)
            .ok_or(Fault::InvalidResponse)?;
        let stem = format!("{}/pull/{}", target.repository.url, pull.number);
        let url = match kind {
            Evidence::Review => format!("{stem}#pullrequestreview-{id}"),
            Evidence::ReviewComment => format!("{stem}#discussion_r{id}"),
            Evidence::WorkflowRun => format!("{}/actions/runs/{id}", target.repository.url),
            Evidence::CheckRun => format!("{stem}/checks?check_run_id={id}"),
            _ => format!("{}/commit/{}/checks", target.repository.url, pull.head),
        };
        let state = row["state"]
            .as_str()
            .or_else(|| row["status"].as_str())
            .filter(|state| !state.is_empty() && state.len() <= 128)
            .unwrap_or("Observed");
        let source_revision = json!({
            "resourceId": id.to_string(), "pullNumber":pull.number, "pullUrl":stem, "state": state,
            "name": row["name"].as_str().or(row["context"].as_str()).map(|v| v.chars().take(256).collect::<String>()),
            "conclusion": row["conclusion"].as_str(),
            "author": row["user"]["login"].as_str(), "appId": row["app"]["id"].as_u64(),
            "app": row["app"]["slug"].as_str(), "workflowId": row["workflow_id"].as_u64(),
            "runAttempt": row["run_attempt"].as_u64(), "reviewId": row["pull_request_review_id"].as_u64(),
            "submittedAt": row["submitted_at"].as_str(),
            "startedAt": row["started_at"].as_str(), "completedAt": row["completed_at"].as_str(),
            "updatedAt": row["updated_at"].as_str(), "body": row["body"].as_str().map(|v| v.chars().take(65_536).collect::<String>()),
        });
        out.push(StoredGithubObservation {
            id: Id::new("unpublished-github-evidence").expect("static GitHub observation ID"),
            repository_id: target.repository.id.clone(),
            kind: kind.kind().into(),
            subject_id: pull.number.into(),
            head: Some(pull.head.into()),
            remote_revision: sha256_hex(source_revision.to_string().as_bytes()),
            payload: source_revision,
            source_updated_at_ms: row["updated_at"]
                .as_str()
                .and_then(|time| time.parse::<jiff::Timestamp>().ok())
                .map(|time| time.as_millisecond())
                .filter(|time| *time >= 0),
            url,
            observed_at_ms: now,
        });
    }
    Ok(out)
}

pub(super) fn publish_observation(
    tx: &mut dyn WriteTransaction,
    native: &Id,
    mut observation: StoredGithubObservation,
    notify: bool,
) -> Result<bool, StorageError> {
    observation.id = tx.new_id(IdKind::Event);
    let stored = tx.github().append_github_observation(&observation)?;
    if stored.id != observation.id || !notify {
        return Ok(stored.id == observation.id);
    }
    let event = EventId(tx.new_id(IdKind::Event));
    let payload = json!({
        "repositoryId":native,"githubRepositoryId":stored.repository_id,
        "observationId":stored.id,"pullNumber":stored.subject_id,"head":stored.head,
        "url":stored.url,"kind":stored.kind,"state":stored.payload["state"],
        "pullUrl":stored.payload["pullUrl"],
    });
    let actor = tx.store().host_actor.clone();
    tx.history().append(NewEvent {
        id: event.clone(),
        actor,
        project_id: None,
        root_id: None,
        operation_id: None,
        aggregate_kind: "github_observation".into(),
        aggregate_id: stored.id.as_str().into(),
        aggregate_revision: 1,
        kind: if stored.kind == "PullRequest" {
            "GithubPullRequestChanged"
        } else {
            "GithubPullEvidenceChanged"
        }
        .into(),
        payload: payload.clone(),
    })?;
    super::references::enqueue(
        tx,
        &event,
        native,
        super::references::NOTICE_CONSUMER,
        payload,
    )?;
    Ok(true)
}
