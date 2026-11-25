//! Poll GitHub pull requests, retain exact head/base evidence and expose new review work.
use super::api::{ApiPage, Endpoint, Validators};
use super::issue_poll::{self, IssuePageResult, PollTarget};
use super::{GitHubService, pull_observations, repositories};
use crate::fingerprint::sha256_hex;
use crate::ports::storage::{
    ReadOptions, StorageError, StoredGithubObservation, StoredGithubPollCursor, WriteOptions,
    read_snapshot, run_write,
};
use crate::protocol::Id;
use crate::services::git::github::Fault;
use crate::services::kernel::{Failure, not_found};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(super) const ENDPOINT: &str = "pulls";
const MINIMUM_MS: i64 = 60_000;

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Watermark {
    through_ms: Option<i64>,
    cycle_start_ms: Option<i64>,
    permission_revision: Option<i64>,
    validator_request: Option<String>,
}

impl GitHubService {
    pub fn poll_pull_requests(&self, registration: &Id) -> Result<IssuePageResult, Failure> {
        let selected = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            let Some(target) = issue_poll::target(snapshot, registration)? else {
                return Ok(None);
            };
            let current = snapshot
                .github()
                .github_poll_cursor(&target.repository.id, ENDPOINT)?;
            Ok(Some((target, current)))
        })?
        .ok_or_else(|| not_found("github_repository", registration))?;
        let (target, current) = selected;
        let now = self.clock.now_ms().get();
        if current
            .as_ref()
            .is_some_and(|row| row.next_poll_at_ms > now)
        {
            return Ok(IssuePageResult::NotDue);
        }
        let mut water = current
            .as_ref()
            .and_then(|row| row.watermark.clone())
            .map(serde_json::from_value::<Watermark>)
            .transpose()
            .map_err(|_| invalid())?
            .unwrap_or_default();
        let bootstrapped = current.as_ref().is_some_and(|row| row.bootstrap_complete);
        let authorized = water.permission_revision == Some(target.account.permission_revision);
        if !authorized {
            water.cycle_start_ms = None;
            water.validator_request = None;
            water.permission_revision = Some(target.account.permission_revision);
        }
        let state = if bootstrapped { "all" } else { "open" };
        let repo = &target.repository;
        let uri = format!(
            "https://api.github.com/repos/{}/{}/pulls?state={state}&sort=updated&direction=desc&per_page=100",
            repo.owner, repo.name
        );
        let endpoint = Endpoint::new(
            &uri,
            Some(&format!("/repositories/{}/pulls", target.numeric_id)),
        )
        .map_err(repositories::api_fault)?;
        let next = current
            .as_ref()
            .filter(|_| authorized)
            .and_then(|row| row.next_page.as_deref());
        let validators = current
            .as_ref()
            .filter(|row| {
                next.is_none()
                    && water.validator_request.as_deref() == Some(&uri)
                    && (row.etag.is_some() || row.last_modified.is_some())
            })
            .map(|row| Validators {
                etag: row.etag.clone(),
                last_modified: row.last_modified.clone(),
            });
        let token = self
            .client
            .token_for_account(&*self.store, &target.account)
            .map_err(repositories::api_fault)?;
        let page = self.client.observe_authenticated_page(
            &endpoint,
            next,
            validators.as_ref(),
            now,
            MINIMUM_MS,
            token,
        );
        let reports = match &page {
            Ok(page) if page.status == 304 && validators.is_some() => Ok(Vec::new()),
            Ok(page) if page.status == 200 => decoded_page(&target, page, now),
            Ok(page) => Err(Fault::Rejected(page.status)),
            Err(_) => Err(Fault::Unavailable),
        };
        let throttle = page.as_ref().is_ok_and(|page| {
            page.status == 429
                || page.status == 403
                    && (page.control.remaining == Some(0) || page.control.retry_after_supplied)
        });
        let continuing = next.is_some();
        let result = run_write(&*self.store, WriteOptions::observed(), move |tx| {
            issue_poll::ensure_target(tx, &target)?;
            let revision = current.as_ref().map(|row| row.revision);
            let mut row = current.unwrap_or(StoredGithubPollCursor {
                repository_id: target.repository.id.clone(),
                endpoint: ENDPOINT.into(),
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
            let mut captured = 0;
            match (reports, page) {
                (Ok(reports), Ok(page)) => {
                    let boundary = water.through_ms.map(|time| time.saturating_sub(1000));
                    let cycle = *water.cycle_start_ms.get_or_insert_with(|| {
                        page.control
                            .server_date_ms
                            .map_or(now, |remote| now.min(remote))
                    });
                    let mut crossed = false;
                    for item in reports {
                        if bootstrapped
                            && boundary.is_some_and(|boundary| {
                                item.source_updated_at_ms
                                    .is_some_and(|time| time < boundary)
                            })
                        {
                            crossed = true;
                            continue;
                        }
                        captured += usize::from(pull_observations::publish_observation(
                            tx,
                            &target.binding.repository_id,
                            item,
                            bootstrapped,
                        )?);
                    }
                    row.next_page = if crossed { None } else { page.next_page };
                    if !continuing {
                        water.validator_request = Some(uri);
                        if page.status == 200 || page.control.etag.is_some() {
                            row.etag = page.control.etag;
                        }
                        if page.status == 200 || page.control.last_modified.is_some() {
                            row.last_modified = page.control.last_modified;
                        }
                    }
                    if row.next_page.is_none() {
                        row.bootstrap_complete = true;
                        water.through_ms =
                            Some(water.through_ms.map_or(cycle, |old| old.max(cycle)));
                        water.cycle_start_ms = None;
                        // A subsequent cycle must not reuse a validator for an identical page
                        // of a prior, interrupted pagination sequence.
                        if continuing || crossed {
                            water.validator_request = None;
                        }
                    }
                    row.next_poll_at_ms = pacing;
                    row.failure_count = 0;
                    row.last_error = None;
                    row.watermark = Some(serde_json::to_value(water).map_err(|_| invalid())?);
                }
                (error, _) => {
                    row.failure_count = row.failure_count.saturating_add(1);
                    let delay = MINIMUM_MS * (1_i64 << (row.failure_count - 1).min(6));
                    row.next_poll_at_ms = pacing.max(now.saturating_add(delay));
                    row.last_error = Some(
                        match error {
                            Err(Fault::Rejected(401)) => "GitHub authorization required",
                            _ if throttle => "GitHub request rate limited",
                            _ => "GitHub pull request observation unavailable",
                        }
                        .into(),
                    );
                }
            }
            tx.github().put_github_poll_cursor(&row, revision)?;
            Ok(if throttle {
                IssuePageResult::Throttled {
                    next_due_at_ms: row.next_poll_at_ms,
                }
            } else {
                IssuePageResult::Captured {
                    reports: captured,
                    next_due_at_ms: row.next_poll_at_ms,
                }
            })
        })?;
        Ok(result)
    }
}

fn decoded_page(
    target: &PollTarget,
    page: &ApiPage,
    now: i64,
) -> Result<Vec<StoredGithubObservation>, Fault> {
    let rows = page
        .value
        .as_ref()
        .and_then(Value::as_array)
        .filter(|rows| rows.len() <= 100)
        .ok_or(Fault::InvalidResponse)?;
    rows.iter()
        .map(|item| decode_pull(target, item, now))
        .collect()
}

pub(super) fn decode_pull(
    target: &PollTarget,
    item: &Value,
    now: i64,
) -> Result<StoredGithubObservation, Fault> {
    let number = item["number"]
        .as_i64()
        .filter(|number| *number > 0)
        .ok_or(Fault::InvalidResponse)?;
    let id = item["node_id"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 256)
        .ok_or(Fault::InvalidResponse)?;
    let url = format!("{}/pull/{number}", target.repository.url);
    let head = item["head"]["sha"]
        .as_str()
        .filter(|value| super::indexing::valid_oid(value))
        .ok_or(Fault::InvalidResponse)?;
    if item["base"]["repo"]["id"].as_u64() != Some(target.numeric_id)
        || !item["html_url"]
            .as_str()
            .is_some_and(|value| value.eq_ignore_ascii_case(&url))
    {
        return Err(Fault::InvalidResponse);
    }
    let updated = item["updated_at"]
        .as_str()
        .and_then(|value| value.parse::<jiff::Timestamp>().ok())
        .map(|value| value.as_millisecond())
        .filter(|value| *value >= 0)
        .ok_or(Fault::InvalidResponse)?;
    let state = item["state"]
        .as_str()
        .filter(|state| matches!(*state, "open" | "closed"))
        .ok_or(Fault::InvalidResponse)?;
    let merged = item["merged_at"].as_str().is_some();
    let payload = json!({
        "resourceId":id, "pullUrl":url, "state":if merged { "merged" } else { state },
        "headRef":item["head"]["ref"].as_str(), "baseRef":item["base"]["ref"].as_str(),
        "baseHead":item["base"]["sha"].as_str(), "mergeCommit":item["merge_commit_sha"].as_str(),
        "updatedAtMs":updated, "mergedAt":item["merged_at"].as_str(),
        "title":item["title"].as_str().map(|title| title.chars().take(256).collect::<String>()),
    });
    Ok(StoredGithubObservation {
        id: Id::new("unpublished-github-pull").expect("static GitHub observation ID"),
        repository_id: target.repository.id.clone(),
        kind: "PullRequest".into(),
        subject_id: number.to_string(),
        head: Some(head.into()),
        url,
        remote_revision: sha256_hex(payload.to_string().as_bytes()),
        payload,
        source_updated_at_ms: Some(updated),
        observed_at_ms: now,
    })
}

fn invalid() -> StorageError {
    StorageError::InvalidRecord("Invalid GitHub pull request checkpoint".into())
}
