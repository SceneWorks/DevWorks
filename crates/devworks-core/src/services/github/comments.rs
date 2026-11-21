//! Conditional, restart-safe inbound issue comments for linked work.
use super::api::{Endpoint, Validators};
use super::issue_poll::{self, PollTarget};
use super::{GitHubService, repositories};
use crate::fingerprint::sha256_hex;
use crate::ports::IdKind;
use crate::ports::storage::{
    GithubIntakeFilter, GithubIssueKey, NewEvent, ReadOptions, StoredGithubObservation,
    StoredGithubPollCursor, WriteOptions, read_snapshot, run_write,
};
use crate::protocol::{EventId, Id};
use crate::services::git::github::Fault;
use crate::services::kernel::Failure;
use serde_json::{Value, json};

const MINIMUM_MS: i64 = 60_000;
const SWEEP: &str = "linked-comments";

impl GitHubService {
    pub(super) fn poll_scheduled_comments(
        &self,
        registration: &Id,
    ) -> Result<Option<i64>, Failure> {
        let now = self.clock.now_ms().get();
        let selected = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            let Some(target) = issue_poll::target(snapshot, registration)? else {
                return Ok(None);
            };
            let cursor = snapshot
                .github()
                .github_poll_cursor(&target.repository.id, SWEEP)?;
            if cursor.as_ref().is_some_and(|row| row.next_poll_at_ms > now) {
                return Ok(None);
            };
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
            let mut checked = 0;
            let mut next = None;
            for issue in snapshot
                .github()
                .github_issues(&filter, after.as_ref(), 32)?
            {
                checked = issue.key.number;
                if !snapshot
                    .github()
                    .github_work_links(&issue.key, None, None, 1)?
                    .is_empty()
                {
                    next = Some(issue.key);
                    break;
                }
            }
            Ok(Some((target, cursor, checked, next)))
        })?;
        let Some((target, sweep, checked, next)) = selected else {
            return Ok(None);
        };
        if let Some(issue) = next
            && let Some(until) = self.poll_comments(&target, &issue, now)?
        {
            return Ok(Some(until));
        }
        run_write(&*self.store, WriteOptions::observed(), move |tx| {
            issue_poll::ensure_target(tx, &target)?;
            let revision = sweep.as_ref().map(|row| row.revision);
            let mut cursor = cursor(&target, SWEEP, sweep);
            cursor.revision += 1;
            cursor.watermark = Some(json!(checked));
            cursor.next_poll_at_ms =
                now.saturating_add(if checked == 0 { MINIMUM_MS } else { 1000 });
            cursor.last_polled_at_ms = Some(now);
            cursor.bootstrap_complete = true;
            tx.github().put_github_poll_cursor(&cursor, revision)
        })?;
        Ok(None)
    }

    /// Explicitly polls comments for a linked issue on repository registration `registration`.
    pub fn poll_issue_comments(
        &self,
        registration: &Id,
        issue_number: i64,
    ) -> Result<Option<i64>, Failure> {
        let now = self.clock.now_ms().get();
        let Some(target) = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            issue_poll::target(snapshot, registration)
        })?
        else {
            return Ok(None);
        };
        let key = GithubIssueKey {
            repository_id: target.repository.id.clone(),
            number: issue_number,
        };
        self.poll_comments(&target, &key, now)
    }

    fn poll_comments(
        &self,
        target: &PollTarget,
        issue: &GithubIssueKey,
        now: i64,
    ) -> Result<Option<i64>, Failure> {
        let key = format!("issue-comments:{}", issue.number);
        let current = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            snapshot
                .github()
                .github_poll_cursor(&target.repository.id, &key)
        })?;
        if current
            .as_ref()
            .is_some_and(|row| row.next_poll_at_ms > now)
        {
            return Ok(None);
        }
        let repo = &target.repository;
        let suffix = format!("issues/{}/comments", issue.number);
        let endpoint = Endpoint::new(
            &format!(
                "https://api.github.com/repos/{}/{}/{suffix}?per_page=100",
                repo.owner, repo.name
            ),
            Some(&format!("/repositories/{}/{suffix}", target.numeric_id)),
        )
        .map_err(repositories::api_fault)?;
        let current_permission = current.as_ref().filter(|row| {
            row.watermark.as_ref().and_then(Value::as_i64)
                == Some(target.account.permission_revision)
        });
        let next = current_permission.and_then(|row| row.next_page.as_deref());
        let validators = current_permission
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
                    MINIMUM_MS,
                    token,
                )
            });
        let observations = match &page {
            Ok(page) if page.status == 304 && validators.is_some() => Ok(Vec::new()),
            Ok(page) if page.status == 200 => page
                .value
                .as_ref()
                .and_then(Value::as_array)
                .filter(|rows| rows.len() <= 100)
                .ok_or(Fault::InvalidResponse)
                .and_then(|rows| {
                    rows.iter()
                        .map(|row| decode(target, issue.number, row, now))
                        .collect()
                }),
            Ok(page) => Err(Fault::Rejected(page.status)),
            Err(_) => Err(Fault::Unavailable),
        };
        let throttle = page.as_ref().is_ok_and(|page| {
            page.status == 429
                || page.status == 403
                    && (page.control.remaining == Some(0) || page.control.retry_after_supplied)
        });
        let target = target.clone();
        let issue = issue.clone();
        let continuing = next.is_some();
        run_write(&*self.store, WriteOptions::observed(), move |tx| {
            issue_poll::ensure_target(tx, &target)?;
            if tx
                .github()
                .github_work_links(&issue, None, None, 1)?
                .is_empty()
            {
                return Ok(None);
            }
            let revision = current.as_ref().map(|row| row.revision);
            let mut row = cursor(&target, &key, current);
            row.revision += 1;
            row.last_polled_at_ms = Some(now);
            let pacing = page.as_ref().map_or(now, |page| page.control.poll_after_ms);
            match (observations, page) {
                (Ok(observations), Ok(page)) => {
                    let issue_scope = issue_poll::scope(&target.binding.repository_id);
                    let issue_baseline_ms = tx
                        .github()
                        .github_issue(&issue, &issue_scope)?
                        .and_then(|intake| {
                            tx.github()
                                .github_observation(&intake.observation_id, &issue_scope)
                                .transpose()
                        })
                        .transpose()?
                        .and_then(|obs| obs.source_updated_at_ms);
                    for mut observation in observations {
                        observation.id = tx.new_id(IdKind::Event);
                        let stored = tx.github().append_github_observation(&observation)?;
                        let post_baseline_comment = issue_baseline_ms
                            .zip(stored.source_updated_at_ms)
                            .is_some_and(|(base, cmt)| cmt > base);
                        if stored.id != observation.id
                            || (!row.bootstrap_complete && !post_baseline_comment)
                        {
                            continue;
                        }
                        let event = EventId(tx.new_id(IdKind::Event));
                        let actor = tx.store().host_actor.clone();
                        let payload = json!({
                            "repositoryId": target.binding.repository_id,
                            "githubRepositoryId": stored.repository_id,
                            "issueNumber": issue.number,
                            "url": stored.url,
                            "observationId": stored.id,
                            "commentObservationId": stored.id,
                        });
                        tx.history().append(NewEvent {
                            id: event.clone(),
                            actor,
                            project_id: None,
                            root_id: None,
                            operation_id: None,
                            aggregate_kind: "github_issue_comment".into(),
                            aggregate_id: stored.id.as_str().into(),
                            aggregate_revision: 1,
                            kind: "GithubIssueChanged".into(),
                            payload: payload.clone(),
                        })?;
                        super::references::enqueue(
                            tx,
                            &event,
                            &target.binding.repository_id,
                            super::references::NOTICE_CONSUMER,
                            payload,
                        )?;
                    }
                    row.next_page = page.next_page;
                    if !continuing && row.next_page.is_none() {
                        if page.status == 200 || page.control.etag.is_some() {
                            row.etag = page.control.etag;
                        }
                        if page.status == 200 || page.control.last_modified.is_some() {
                            row.last_modified = page.control.last_modified;
                        }
                    } else {
                        row.etag = None;
                        row.last_modified = None;
                    }
                    row.bootstrap_complete |= row.next_page.is_none();
                    row.next_poll_at_ms = pacing;
                    row.failure_count = 0;
                    row.last_error = None;
                    row.watermark = Some(json!(target.account.permission_revision));
                }
                _ => {
                    row.failure_count = row.failure_count.saturating_add(1);
                    let delay = MINIMUM_MS * (1_i64 << (row.failure_count - 1).min(6));
                    row.next_poll_at_ms = pacing.max(now.saturating_add(delay));
                    row.last_error = Some(
                        if throttle {
                            "GitHub request rate limited"
                        } else {
                            "GitHub issue comments are unavailable"
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

fn decode(
    target: &PollTarget,
    number: i64,
    row: &Value,
    now: i64,
) -> Result<StoredGithubObservation, Fault> {
    let id = row["id"]
        .as_u64()
        .filter(|id| *id > 0)
        .ok_or(Fault::InvalidResponse)?;
    let url = format!(
        "{}/issues/{number}#issuecomment-{id}",
        target.repository.url
    );
    if !row["html_url"]
        .as_str()
        .is_some_and(|actual| actual.eq_ignore_ascii_case(&url))
    {
        return Err(Fault::InvalidResponse);
    }
    let updated = row["updated_at"]
        .as_str()
        .and_then(|time| time.parse::<jiff::Timestamp>().ok())
        .map(|time| time.as_millisecond())
        .filter(|time| *time >= 0)
        .ok_or(Fault::InvalidResponse)?;
    let body = row["body"]
        .as_str()
        .filter(|body| body.len() <= 4 * 65_536)
        .ok_or(Fault::InvalidResponse)?;
    let payload = json!({
        "resourceId": id.to_string(),
        "nodeId": row["node_id"].as_str(),
        "body": body,
        "author": row["user"]["login"].as_str(),
        "authorId": row["user"]["id"].as_u64(),
        "createdAt": row["created_at"].as_str(),
        "updatedAtMs": updated,
        "state": "Observed",
    });
    Ok(StoredGithubObservation {
        id: Id::new("unpublished-github-comment").expect("static observation ID"),
        repository_id: target.repository.id.clone(),
        kind: "IssueComment".into(),
        subject_id: number.to_string(),
        remote_revision: sha256_hex(payload.to_string().as_bytes()),
        url,
        head: None,
        payload,
        source_updated_at_ms: Some(updated),
        observed_at_ms: now,
    })
}

fn cursor(
    target: &PollTarget,
    endpoint: &str,
    existing: Option<StoredGithubPollCursor>,
) -> StoredGithubPollCursor {
    existing.unwrap_or(StoredGithubPollCursor {
        repository_id: target.repository.id.clone(),
        endpoint: endpoint.into(),
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
    })
}
