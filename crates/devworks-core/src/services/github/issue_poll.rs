//! Inbound issue snapshots and the durable per-repository issue cursor.
//! Capturing an issue never changes local work state or starts a turn.
use super::api::{ApiPage, Endpoint, Validators};
use super::repositories::api_fault;
use super::{GitHubService, credentials, schedule};
use crate::fingerprint::sha256_hex;
use crate::ports::IdKind;
use crate::ports::storage::{
    GithubIssueKey, GithubScope, NewEvent, ReadOptions, ReadSnapshot, StorageError,
    StoredGithubAccount, StoredGithubIssueIntake, StoredGithubObservation, StoredGithubPollCursor,
    StoredGithubRepository, StoredGithubRepositoryBinding, WriteOptions, WriteTransaction,
    read_snapshot, run_write,
};
use crate::protocol::{DomainErrorCode, EventId, Id};
use crate::services::git::github::Fault;
use crate::services::kernel::{Failure, Refusal, not_found};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const ISSUES: &str = "issues";
const POLL_INTERVAL_MS: i64 = 60_000;

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IssueWatermark {
    permission_revision: Option<i64>,
    mode: Option<IssueMode>,
    through_ms: Option<i64>,
    started_ms: Option<i64>,
    validators_request: Option<String>,
    changed: bool,
}

#[derive(Clone)]
pub(super) struct PollTarget {
    pub(super) account: StoredGithubAccount,
    pub(super) binding: StoredGithubRepositoryBinding,
    pub(super) repository: StoredGithubRepository,
    pub(super) numeric_id: u64,
    pub(super) cursor: Option<StoredGithubPollCursor>,
}

#[derive(Clone)]
pub(super) struct IssueObservation {
    pub(super) number: i64,
    updated_at_ms: i64,
    pub(super) url: String,
    pub(super) payload: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum IssueMode {
    Import { linked_updates: bool },
    LinkedOnly,
}

impl IssueMode {
    fn imports(self) -> bool {
        matches!(self, Self::Import { .. })
    }

    fn accepts(self, linked: bool) -> bool {
        match self {
            Self::Import { linked_updates } => !linked || linked_updates,
            Self::LinkedOnly => linked,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IssuePageResult {
    NotDue,
    Captured { reports: usize, next_due_at_ms: i64 },
    Throttled { next_due_at_ms: i64 },
}

impl GitHubService {
    /// Captures at most one due page across the app's existing bounded GitHub transport.
    pub fn poll_issue_page(&self, registration: &Id) -> Result<IssuePageResult, Failure> {
        let target = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            target(snapshot, registration)
        })?
        .ok_or_else(|| not_found("github_repository", registration))?;
        self.poll_target_page(target)
    }

    pub(super) fn poll_configured_issues(
        &self,
        registration: &Id,
        mode: IssueMode,
    ) -> Result<IssuePageResult, Failure> {
        let target = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            target(snapshot, registration)
        })?
        .ok_or_else(|| not_found("github_repository", registration))?;
        self.poll_page(target, mode, true)
    }

    pub(super) fn poll_target_page(&self, target: PollTarget) -> Result<IssuePageResult, Failure> {
        self.poll_page(
            target,
            IssueMode::Import {
                linked_updates: true,
            },
            false,
        )
    }

    fn poll_page(
        &self,
        target: PollTarget,
        mode: IssueMode,
        scheduled: bool,
    ) -> Result<IssuePageResult, Failure> {
        let now = self.clock.now_ms().get();
        if target
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.next_poll_at_ms > now)
        {
            return Ok(IssuePageResult::NotDue);
        }
        let mut watermark = decode_watermark(target.cursor.as_ref())?;
        let authorized = watermark.permission_revision == Some(target.account.permission_revision)
            && watermark.mode == Some(mode);
        if !authorized {
            if mode.imports() && watermark.mode.is_some_and(|old| !old.imports()) {
                watermark.through_ms = None;
            }
            watermark.mode = Some(mode);
            watermark.permission_revision = Some(target.account.permission_revision);
            watermark.started_ms = None;
            watermark.validators_request = None;
            watermark.changed = false;
        }
        let first = issue_uri(&target, &watermark)?;
        let endpoint = Endpoint::new(
            &first,
            Some(&format!("/repositories/{}/issues", target.numeric_id)),
        )
        .map_err(api_fault)?;
        let next = target
            .cursor
            .as_ref()
            .filter(|_| authorized)
            .and_then(|cursor| cursor.next_page.as_deref());
        let validators = target.cursor.as_ref().and_then(|cursor| {
            (next.is_none()
                && watermark.validators_request.as_deref() == Some(first.as_str())
                && (cursor.etag.is_some() || cursor.last_modified.is_some()))
            .then(|| Validators {
                etag: cursor.etag.clone(),
                last_modified: cursor.last_modified.clone(),
            })
        });
        let result = self
            .client
            .token_for_account(&*self.store, &target.account)
            .and_then(|token| {
                self.client.observe_authenticated_page(
                    &endpoint,
                    next,
                    validators.as_ref(),
                    now,
                    POLL_INTERVAL_MS,
                    token,
                )
            });
        let page = match result {
            Ok(page) => page,
            Err(error) => return self.record_poll_failure(target, now, None, error),
        };
        if !matches!(page.status, 200 | 304) {
            let status = page.status;
            return self.record_poll_failure(target, now, Some(page), Fault::Rejected(status));
        }
        if page.status == 304 && validators.is_none() {
            return self.record_poll_failure(target, now, Some(page), Fault::InvalidResponse);
        }
        let reports = if page.status == 304 {
            Vec::new()
        } else {
            match decode_page(&target.repository, &page) {
                Ok(reports) => reports,
                Err(error) => return self.record_poll_failure(target, now, Some(page), error),
            }
        };
        if watermark.started_ms.is_none() {
            watermark.started_ms = Some(
                page.control
                    .server_date_ms
                    .map_or(now, |date| now.min(date)),
            );
        }
        let continuing = next.is_some();
        run_write(&*self.store, WriteOptions::observed(), move |tx| {
            let revision = target.cursor.as_ref().map(|cursor| cursor.revision);
            ensure_current(tx, &target)?;
            if scheduled
                && schedule::active_policy(tx, &target.binding.repository_id)?.issue_mode()
                    != Some(mode)
            {
                return Err(StorageError::ScopeDenied);
            }
            let mut cursor = cursor_row(&target);
            let mut count = 0;
            for report in reports {
                let key = GithubIssueKey {
                    repository_id: target.repository.id.clone(),
                    number: report.number,
                };
                let linked = !tx
                    .github()
                    .github_work_links(&key, None, None, 1)?
                    .is_empty();
                if mode.accepts(linked) {
                    count += usize::from(capture(tx, &target, report)?);
                }
            }
            watermark.changed |= count > 0;
            if !continuing {
                watermark.validators_request = Some(first);
                if page.status == 200 || page.control.etag.is_some() {
                    cursor.etag = page.control.etag.clone();
                }
                if page.status == 200 || page.control.last_modified.is_some() {
                    cursor.last_modified = page.control.last_modified.clone();
                }
            }
            cursor.next_page = page.next_page;
            if cursor.next_page.is_none() {
                if !cursor.bootstrap_complete || watermark.changed {
                    watermark.through_ms = watermark.started_ms;
                }
                watermark.started_ms = None;
                watermark.changed = false;
                cursor.bootstrap_complete = true;
            }
            cursor.last_polled_at_ms = Some(now);
            cursor.next_poll_at_ms = page.control.poll_after_ms;
            cursor.failure_count = 0;
            cursor.last_error = None;
            cursor.watermark = Some(
                serde_json::to_value(watermark)
                    .map_err(|error| StorageError::InvalidRecord(error.to_string()))?,
            );
            tx.github().put_github_poll_cursor(&cursor, revision)?;
            Ok(IssuePageResult::Captured {
                reports: count,
                next_due_at_ms: cursor.next_poll_at_ms,
            })
        })
        .map_err(Into::into)
    }

    fn record_poll_failure(
        &self,
        target: PollTarget,
        now: i64,
        page: Option<ApiPage>,
        error: Fault,
    ) -> Result<IssuePageResult, Failure> {
        let throttle = page.as_ref().is_some_and(ApiPage::throttled);
        let safe_error = match error {
            Fault::Rejected(401) | Fault::CredentialsRequired => "GitHub authorization required",
            Fault::Rejected(403 | 429) if throttle => "GitHub request rate limited",
            Fault::Rejected(403) => "GitHub repository access denied",
            _ => "GitHub issue observation unavailable",
        };
        let due = run_write(&*self.store, WriteOptions::observed(), move |tx| {
            ensure_current(tx, &target)?;
            let mut cursor = cursor_row(&target);
            let revision = target.cursor.as_ref().map(|cursor| cursor.revision);
            cursor.failure_count = cursor.failure_count.saturating_add(1);
            cursor.last_error = Some(safe_error.into());
            let delay = POLL_INTERVAL_MS * (1_i64 << (cursor.failure_count - 1).min(6));
            cursor.next_poll_at_ms = now
                .saturating_add(delay)
                .max(page.as_ref().map_or(now, |page| page.control.poll_after_ms));
            cursor.last_polled_at_ms = Some(now);
            tx.github().put_github_poll_cursor(&cursor, revision)?;
            Ok(cursor.next_poll_at_ms)
        })?;
        if throttle {
            Ok(IssuePageResult::Throttled {
                next_due_at_ms: due,
            })
        } else {
            Err(api_fault(error))
        }
    }
}

pub(super) fn target(
    snapshot: &dyn ReadSnapshot,
    local: &Id,
) -> Result<Option<PollTarget>, StorageError> {
    let Some(account) = snapshot.github().github_account()? else {
        return Ok(None);
    };
    if !credentials::account_is_current(snapshot.configuration(), &account)? {
        return Ok(None);
    }
    let Some(binding) = snapshot
        .github()
        .github_repository_binding(local)?
        .filter(|binding| binding.state == "Active")
    else {
        return Ok(None);
    };
    let Some(repository) = snapshot
        .github()
        .github_discovered_repository(&binding.github_repository_id)?
        .filter(|repo| {
            repo.account_id == account.id && repo.permission_revision == account.permission_revision
        })
    else {
        return Ok(None);
    };
    let scope = scope(local);
    let Some(identity) = snapshot
        .github()
        .github_observation(&binding.observation_id, &scope)?
    else {
        return Ok(None);
    };
    let registration = snapshot.projects().repository(local)?;
    if !registration.as_ref().is_some_and(|row| {
        row.lifecycle == "Active" && row.remote_url.as_ref() == Some(&binding.registered_remote)
    }) {
        return Ok(None);
    }
    let numeric_id = identity.payload["repository"]["id"]
        .as_u64()
        .filter(|id| *id > 0)
        .ok_or_else(|| {
            StorageError::InvalidRecord(
                "GitHub repository observation lacks numeric identity".into(),
            )
        })?;
    let cursor = snapshot
        .github()
        .github_poll_cursor(&repository.id, ISSUES)?;
    Ok(Some(PollTarget {
        account,
        binding,
        repository,
        numeric_id,
        cursor,
    }))
}

fn ensure_current(
    tx: &mut dyn WriteTransaction,
    expected: &PollTarget,
) -> Result<(), StorageError> {
    ensure_target(tx, expected)?;
    if tx
        .github()
        .github_poll_cursor(&expected.repository.id, ISSUES)?
        != expected.cursor
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
}

pub(super) fn ensure_target(
    tx: &mut dyn WriteTransaction,
    expected: &PollTarget,
) -> Result<(), StorageError> {
    let registration = tx.projects().repository(&expected.binding.repository_id)?;
    if !credentials::account_is_current(tx.configuration(), &expected.account)?
        || tx.github().github_account()?.as_ref() != Some(&expected.account)
        || tx
            .github()
            .github_repository_binding(&expected.binding.repository_id)?
            .as_ref()
            != Some(&expected.binding)
        || tx
            .github()
            .github_discovered_repository(&expected.repository.id)?
            .as_ref()
            != Some(&expected.repository)
        || !registration.as_ref().is_some_and(|row| {
            row.lifecycle == "Active"
                && row.remote_url.as_ref() == Some(&expected.binding.registered_remote)
        })
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
}

pub(super) fn scope(local: &Id) -> GithubScope {
    GithubScope {
        repositories: vec![local.clone()],
        project_id: None,
    }
}

fn cursor_row(target: &PollTarget) -> StoredGithubPollCursor {
    let mut row = target
        .cursor
        .clone()
        .unwrap_or_else(|| StoredGithubPollCursor {
            repository_id: target.repository.id.clone(),
            endpoint: ISSUES.into(),
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
    row
}

fn decode_watermark(
    cursor: Option<&StoredGithubPollCursor>,
) -> Result<IssueWatermark, StorageError> {
    cursor
        .and_then(|cursor| cursor.watermark.clone())
        .map(serde_json::from_value)
        .transpose()
        .map(|state| state.unwrap_or_default())
        .map_err(|error| StorageError::InvalidRecord(format!("GitHub issue watermark: {error}")))
}

fn issue_uri(target: &PollTarget, watermark: &IssueWatermark) -> Result<String, Failure> {
    let mut url = url::Url::parse(&format!(
        "https://api.github.com/repos/{}/{}/issues",
        target.repository.owner, target.repository.name,
    ))
    .map_err(|_| {
        Refusal::new(
            DomainErrorCode::InvalidArgument,
            "github_repository",
            Some(target.repository.id.clone()),
        )
    })?;
    let mut query = url.query_pairs_mut();
    query
        .append_pair("per_page", "100")
        .append_pair("sort", "updated")
        .append_pair("direction", "asc");
    if let Some(through) = watermark.through_ms {
        // GitHub returns timestamps to the second. Keep an overlap and deduplicate by
        // the immutable observed issue content so an update at the waterline is seen.
        let from = jiff::Timestamp::from_millisecond(through.saturating_sub(1000).max(0)).map_err(
            |_| StorageError::InvalidRecord("GitHub issue time outside the supported range".into()),
        )?;
        query
            .append_pair("state", "all")
            .append_pair("since", &from.to_string());
    } else {
        query.append_pair("state", "open");
    }
    drop(query);
    Ok(url.into())
}

fn decode_page(
    repo: &StoredGithubRepository,
    page: &ApiPage,
) -> Result<Vec<IssueObservation>, Fault> {
    let rows = page
        .value
        .as_ref()
        .and_then(Value::as_array)
        .filter(|rows| rows.len() <= 100)
        .ok_or(Fault::InvalidResponse)?;
    rows.iter()
        .filter(|row| row.get("pull_request").is_none())
        .map(|row| decode_issue(repo, row))
        .collect()
}

pub(super) fn decode_issue(
    repo: &StoredGithubRepository,
    row: &Value,
) -> Result<IssueObservation, Fault> {
    let number = row["number"]
        .as_i64()
        .filter(|number| *number > 0)
        .ok_or(Fault::InvalidResponse)?;
    let url = format!(
        "https://github.com/{}/{}/issues/{number}",
        repo.owner, repo.name
    );
    if !row["html_url"]
        .as_str()
        .is_some_and(|value| value.eq_ignore_ascii_case(&url))
    {
        return Err(Fault::InvalidResponse);
    }
    let state = row["state"]
        .as_str()
        .filter(|state| matches!(*state, "open" | "closed"))
        .ok_or(Fault::InvalidResponse)?;
    let updated_at_ms = row["updated_at"]
        .as_str()
        .and_then(|text| text.parse::<jiff::Timestamp>().ok())
        .map(|timestamp| timestamp.as_millisecond())
        .filter(|ms| *ms >= 0)
        .ok_or(Fault::InvalidResponse)?;
    let issue_id = row["node_id"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 256 && id.bytes().all(|b| b.is_ascii_graphic()))
        .map(str::to_owned)
        .or_else(|| {
            row["id"]
                .as_u64()
                .filter(|id| *id > 0)
                .map(|id| id.to_string())
        })
        .ok_or(Fault::InvalidResponse)?;
    let title = row["title"].as_str().ok_or(Fault::InvalidResponse)?;
    let body = row["body"].as_str().unwrap_or("");
    if title.len() > 4096 || body.len() > 4 * 65_536 {
        return Err(Fault::InvalidResponse);
    }
    let labels = row["labels"]
        .as_array()
        .ok_or(Fault::InvalidResponse)?
        .iter()
        .filter_map(|label| label.as_str().or_else(|| label["name"].as_str()))
        .collect::<Vec<_>>();
    Ok(IssueObservation {
        number,
        updated_at_ms,
        url,
        payload: json!({
            "title": title, "body": body, "state": state, "number": number, "remoteIssueId": issue_id,
            "author": row["user"]["login"].as_str(), "authorId": row["user"]["id"].as_u64(), "labels": labels,
            "createdAt": row["created_at"].as_str(), "updatedAtMs": updated_at_ms,
            "stateReason": row["state_reason"].as_str(), "commentCount": row["comments"].as_u64()
        }),
    })
}

pub(super) fn capture(
    tx: &mut dyn WriteTransaction,
    target: &PollTarget,
    report: IssueObservation,
) -> Result<bool, StorageError> {
    let key = GithubIssueKey {
        repository_id: target.repository.id.clone(),
        number: report.number,
    };
    let scope = scope(&target.binding.repository_id);
    let current = tx.github().github_issue(&key, &scope)?;
    let observation = StoredGithubObservation {
        id: tx.new_id(IdKind::Event),
        repository_id: key.repository_id.clone(),
        kind: "Issue".into(),
        subject_id: report.number.to_string(),
        url: report.url,
        head: None,
        remote_revision: sha256_hex(report.payload.to_string().as_bytes()),
        payload: report.payload,
        source_updated_at_ms: Some(report.updated_at_ms),
        observed_at_ms: tx.now_ms(),
    };
    let previous = match &current {
        Some(row) => tx
            .github()
            .github_observation(&row.observation_id, &scope)?,
        None => None,
    };
    if previous.as_ref().is_some_and(|previous| {
        previous.payload["remoteIssueId"].as_str().is_some()
            && previous.payload["remoteIssueId"] != observation.payload["remoteIssueId"]
    }) {
        return Err(StorageError::InvalidRecord(
            "GitHub issue identity changed".into(),
        ));
    }
    let observation = tx.github().append_github_observation(&observation)?;
    if previous.as_ref().is_some_and(|previous| {
        previous.id == observation.id
            || previous.source_updated_at_ms > observation.source_updated_at_ms
    }) {
        return Ok(false);
    }
    let revision = current.as_ref().map(|row| row.revision);
    let mut row = current.unwrap_or(StoredGithubIssueIntake {
        key,
        revision: 0,
        state: "Queued".into(),
        observation_id: observation.id.clone(),
        remote_revision: observation.id.as_str().into(),
        project_id: None,
        work_id: None,
        operation_id: None,
        last_triaged_revision: None,
    });
    row.revision += 1;
    row.remote_revision = observation.id.as_str().into();
    row.observation_id = observation.id;
    tx.github().put_github_issue(&row, revision)?;
    if let Some(prev) = &previous
        && (!tx
            .github()
            .github_work_links(&row.key, None, None, 1)?
            .is_empty()
            || row.state == "AwaitingDecision")
    {
        let event = EventId(tx.new_id(IdKind::Event));
        let actor = tx.store().host_actor.clone();
        let payload = json!({
            "repositoryId": target.binding.repository_id,
            "githubRepositoryId": row.key.repository_id,
            "issueNumber": row.key.number,
            "observationId": row.observation_id,
            "previousObservationId": prev.id,
            "url": observation.url,
            "repositoryUrl": target.repository.url,
            "state": observation.payload["state"],
        });
        tx.history().append(NewEvent {
            id: event.clone(),
            actor,
            project_id: None,
            root_id: None,
            operation_id: None,
            aggregate_kind: "github_issue".into(),
            aggregate_id: format!("{}:{}", row.key.repository_id.as_str(), row.key.number),
            aggregate_revision: row.revision,
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
    Ok(true)
}
