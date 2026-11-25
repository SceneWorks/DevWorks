//! An authenticated baseline of the remote default ref followed by durable source-change events.
use super::api::{ApiPage, Endpoint, Validators};
use super::issue_poll::{self, IssuePageResult, PollTarget};
use super::{GitHubService, repositories};
use crate::fingerprint::sha256_hex;
use crate::ports::IdKind;
use crate::ports::storage::{
    NewEvent, NewQueueEntry, ReadOptions, StorageError, StoredGithubPollCursor, WriteOptions,
    WriteTransaction, read_snapshot, run_write,
};
use crate::protocol::{EventId, Id};
use crate::services::git::github::Fault;
use crate::services::kernel::{Failure, not_found};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const NOTICE_CONSUMER: &str = "github_agent_notices";
pub const INDEX_CONSUMER: &str = "github_index_refresh";
const MINIMUM_MS: i64 = 60_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RefBaseline {
    oid: String,
    permission_revision: i64,
}

impl GitHubService {
    /// One bounded default-ref observation. A baseline does not represent a push. Subsequent
    /// changes append their event and create both deliveries with the new baseline.
    pub fn poll_default_ref(&self, registration: &Id) -> Result<IssuePageResult, Failure> {
        let selected = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            let Some(target) = issue_poll::target(snapshot, registration)? else {
                return Ok(None);
            };
            let Some(branch) = target.repository.default_branch.clone() else {
                return Ok(None);
            };
            let key = endpoint_key(&branch);
            let cursor = snapshot
                .github()
                .github_poll_cursor(&target.repository.id, &key)?;
            Ok(Some((target, branch, key, cursor)))
        })?
        .ok_or_else(|| not_found("github_repository", registration))?;
        let (target, branch, key, current) = selected;
        let now = self.clock.now_ms().get();
        if current
            .as_ref()
            .is_some_and(|row| row.next_poll_at_ms > now)
        {
            return Ok(IssuePageResult::NotDue);
        }
        let baseline = current
            .as_ref()
            .and_then(|row| row.watermark.clone())
            .map(serde_json::from_value::<RefBaseline>)
            .transpose()
            .map_err(|_| invalid_ref())?;
        let validators = current.as_ref().and_then(|row| {
            (baseline.as_ref().is_some_and(|value| {
                value.permission_revision == target.account.permission_revision
            }) && (row.etag.is_some() || row.last_modified.is_some()))
            .then(|| Validators {
                etag: row.etag.clone(),
                last_modified: row.last_modified.clone(),
            })
        });
        let uri = ref_uri(&target, &branch).map_err(repositories::api_fault)?;
        let endpoint = Endpoint::new(uri.as_str(), None).map_err(repositories::api_fault)?;
        let page = self
            .client
            .token_for_account(&*self.store, &target.account)
            .and_then(|token| {
                self.client.observe_authenticated_page(
                    &endpoint,
                    None,
                    validators.as_ref(),
                    now,
                    MINIMUM_MS,
                    token,
                )
            });
        let head = match &page {
            Ok(page) if page.status == 304 && validators.is_some() => Ok(None),
            Ok(page) if page.status == 200 => decode_head(page, &branch).map(Some),
            Ok(page) => Err(Fault::Rejected(page.status)),
            Err(_) => Err(Fault::Unavailable),
        };
        let failure = head.as_ref().err().map(|fault| match fault {
            Fault::Rejected(401) | Fault::CredentialsRequired => "GitHub authorization required",
            Fault::Rejected(429) => "GitHub request rate limited",
            Fault::Rejected(403) => "GitHub ref access denied or rate limited",
            _ => "GitHub ref observation unavailable",
        });
        let pacing = page
            .as_ref()
            .map_or(now.saturating_add(MINIMUM_MS), |page| {
                page.control.poll_after_ms
            });
        let throttle = page.as_ref().is_ok_and(|page| {
            page.status == 429
                || page.status == 403
                    && (page.control.remaining == Some(0) || page.control.retry_after_supplied)
        });
        run_write(&*self.store, WriteOptions::observed(), move |tx| {
            issue_poll::ensure_target(tx, &target)?;
            let revision = current.as_ref().map(|row| row.revision);
            let mut row = current.unwrap_or(StoredGithubPollCursor {
                repository_id: target.repository.id.clone(),
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
            row.last_error = failure.map(str::to_owned);
            let mut changes = 0;
            match head {
                Ok(head) => {
                    row.failure_count = 0;
                    row.next_poll_at_ms = pacing;
                    if let Some(oid) = head {
                        if let Some(previous) = baseline
                            && previous.oid != oid
                        {
                            enqueue_change(
                                tx,
                                &target,
                                &branch,
                                &previous.oid,
                                &oid,
                                row.revision,
                            )?;
                            changes = 1;
                        }
                        row.watermark = Some(json!(RefBaseline {
                            oid,
                            permission_revision: target.account.permission_revision,
                        }));
                        row.bootstrap_complete = true;
                    }
                    if let Ok(page) = page {
                        if page.status == 200 || page.control.etag.is_some() {
                            row.etag = page.control.etag;
                        }
                        if page.status == 200 || page.control.last_modified.is_some() {
                            row.last_modified = page.control.last_modified;
                        }
                    }
                }
                Err(_) => {
                    row.failure_count = row.failure_count.saturating_add(1);
                    let backoff = MINIMUM_MS * (1_i64 << (row.failure_count - 1).min(6));
                    row.next_poll_at_ms = pacing.max(now.saturating_add(backoff));
                }
            }
            let due = row.next_poll_at_ms;
            tx.github().put_github_poll_cursor(&row, revision)?;
            Ok(if throttle {
                IssuePageResult::Throttled {
                    next_due_at_ms: due,
                }
            } else {
                IssuePageResult::Captured {
                    reports: changes,
                    next_due_at_ms: due,
                }
            })
        })
        .map_err(Into::into)
    }
}

pub(super) fn endpoint_key(branch: &str) -> String {
    format!("ref:{}", sha256_hex(branch.as_bytes()))
}

fn ref_uri(target: &PollTarget, branch: &str) -> Result<url::Url, Fault> {
    let repo = &target.repository;
    let mut uri = url::Url::parse(&format!(
        "https://api.github.com/repos/{}/{}/git/ref/heads",
        repo.owner, repo.name
    ))
    .map_err(|_| Fault::InvalidResponse)?;
    if branch.is_empty()
        || branch.split('/').any(|part| {
            part.is_empty()
                || matches!(part, "." | "..")
                || part.bytes().any(|b| matches!(b, b'\\' | 0..=31))
        })
    {
        return Err(Fault::InvalidResponse);
    }
    uri.path_segments_mut()
        .map_err(|_| Fault::InvalidResponse)?
        .extend(branch.split('/'));
    Ok(uri)
}

fn decode_head(page: &ApiPage, branch: &str) -> Result<String, Fault> {
    let value = page
        .value
        .as_ref()
        .filter(|_| page.next_page.is_none())
        .ok_or(Fault::InvalidResponse)?;
    if value["ref"] != format!("refs/heads/{branch}") || value["object"]["type"] != "commit" {
        return Err(Fault::InvalidResponse);
    }
    let oid = value["object"]["sha"]
        .as_str()
        .filter(|oid| {
            oid.len() == 40
                && oid
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .ok_or(Fault::InvalidResponse)?;
    Ok(oid.to_owned())
}

fn enqueue_change(
    tx: &mut dyn WriteTransaction,
    target: &PollTarget,
    branch: &str,
    before: &str,
    after: &str,
    revision: i64,
) -> Result<(), StorageError> {
    let repo = &target.repository;
    let mut branch_url =
        url::Url::parse(&format!("{}/tree", repo.url)).map_err(|_| invalid_ref())?;
    branch_url
        .path_segments_mut()
        .map_err(|_| invalid_ref())?
        .extend(branch.split('/'));
    let payload: Value = json!({
        "githubRepositoryId": repo.id,
        "repositoryUrl": repo.url,
        "ref": format!("refs/heads/{branch}"),
        "refUrl": branch_url.as_str(),
        "previousHead": before,
        "head": after,
        "headUrl": format!("{}/commit/{after}", repo.url),
    });
    let mut after = None;
    const PAGE: u16 = 128;
    loop {
        let registrations =
            tx.github()
                .github_bound_registrations(&repo.id, after.as_ref(), PAGE)?;
        for local in &registrations {
            let mut local_payload = payload.clone();
            local_payload["repositoryId"] = json!(local);
            // One queue entry per consumer and event. Each native registration has its own
            // event while the authenticated ref cursor remains shared by the remote.
            let event = EventId(tx.new_id(IdKind::Event));
            let actor = tx.store().host_actor.clone();
            tx.history().append(NewEvent {
                id: event.clone(),
                actor,
                project_id: None,
                root_id: None,
                operation_id: None,
                aggregate_kind: "github_ref".into(),
                aggregate_id: format!(
                    "{}:{}:{}",
                    repo.id.as_str(),
                    local.as_str(),
                    endpoint_key(branch)
                ),
                aggregate_revision: revision,
                kind: "GithubRefChanged".into(),
                payload: local_payload.clone(),
            })?;
            enqueue(tx, &event, local, NOTICE_CONSUMER, local_payload.clone())?;
            enqueue(tx, &event, local, INDEX_CONSUMER, local_payload)?;
        }
        if registrations.len() < usize::from(PAGE) {
            return Ok(());
        }
        after = registrations.last().cloned();
    }
}

pub(super) fn enqueue(
    tx: &mut dyn WriteTransaction,
    event: &EventId,
    registration: &Id,
    consumer: &str,
    payload: Value,
) -> Result<(), StorageError> {
    let id = tx.new_id(IdKind::QueueEntry);
    let now = tx.now_ms();
    tx.queue().enqueue(NewQueueEntry {
        id,
        consumer: consumer.into(),
        event_id: event.clone(),
        root_id: None,
        operation_id: None,
        partition: Some(registration.as_str().into()),
        priority: 0,
        ready_at_ms: now,
        payload,
    })?;
    Ok(())
}

fn invalid_ref() -> StorageError {
    StorageError::InvalidRecord("Invalid GitHub ref observation".into())
}
