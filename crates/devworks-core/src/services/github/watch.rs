//! Read an Actions conclusion from the repository bound to the current GitHub account.
use super::{
    GitHubService,
    api::{ApiPage, Endpoint, Validators},
    issue_poll::{self, PollTarget},
    pull_observations::Evidence,
    repositories,
    schedule::PollAccount,
};
use crate::ports::secrets::Secret;
use crate::ports::storage::{
    GithubWatchSubject, ReadOptions, ReadSnapshot, StorageError, StoredGithubWatch, read_snapshot,
};
use crate::protocol::Id;
use crate::services::git::github::Fault;
use crate::services::kernel::{Failure, not_found};
use serde_json::Value;

pub(crate) const POLL_INTERVAL_MS: i64 = 30_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WatchConclusion {
    pub kind: &'static str,
    pub conclusion: &'static str,
    pub url: String,
    pub duration_ms: Option<i64>,
}

pub(crate) enum RunObservation {
    Changed(Option<WatchConclusion>, Option<WatchValidators>),
    Unchanged,
    Deferred,
}

pub(crate) struct WatchValidators {
    head: Option<String>,
    value: Validators,
}

impl WatchValidators {
    fn from_page(page: &ApiPage, head: Option<String>) -> Option<Self> {
        let control = &page.control;
        (control.etag.is_some() || control.last_modified.is_some()).then(|| Self {
            head,
            value: Validators {
                etag: control.etag.clone(),
                last_modified: control.last_modified.clone(),
            },
        })
    }
}

/// Identifies an observed repository and account independently of their mutable remote names.
/// The registration remains the scope authority every time the watcher reads the API.
pub(crate) fn bind_watch(
    snapshot: &dyn ReadSnapshot,
    registration: &Id,
    subject: GithubWatchSubject,
) -> Result<Option<StoredGithubWatch>, StorageError> {
    Ok(
        issue_poll::target(snapshot, registration)?.and_then(|target| {
            Some(StoredGithubWatch {
                registration: registration.clone(),
                remote_user: target.account.remote_user_id?,
                remote_repository: target.repository.id,
                subject,
            })
        }),
    )
}

/// Revalidate a stored source and return the permission generation used to share its reads.
pub(crate) fn watch_permission(
    snapshot: &dyn ReadSnapshot,
    binding: &StoredGithubWatch,
) -> Result<Option<i64>, StorageError> {
    Ok(issue_poll::target(snapshot, &binding.registration)?
        .filter(|target| matches_binding(binding, target))
        .map(|target| target.account.permission_revision))
}

fn matches_binding(binding: &StoredGithubWatch, target: &PollTarget) -> bool {
    target.account.remote_user_id.as_ref() == Some(&binding.remote_user)
        && target.repository.id == binding.remote_repository
}

impl GitHubService {
    pub(crate) fn read_watch(
        &self,
        binding: &StoredGithubWatch,
        validators: Option<&WatchValidators>,
    ) -> Result<(RunObservation, i64), Failure> {
        let target = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            issue_poll::target(snapshot, &binding.registration)
        })?
        .filter(|target| matches_binding(binding, target))
        .ok_or_else(|| not_found("github_repository", &binding.registration))?;
        if self.is_stopped() {
            return Err(StorageError::Cancelled.into());
        }
        let account = PollAccount::from(&target.account);
        if let Some(until) = self.poll_deferred_until(&account, self.clock.now_ms().get()) {
            return Ok((RunObservation::Deferred, until));
        }
        let (observation, due) = match binding.subject {
            GithubWatchSubject::WorkflowRun(run) => self.observe_watch_run(
                &target,
                &account,
                run,
                validators.map(|stored| &stored.value),
            ),
            GithubWatchSubject::PullRequest(number) => {
                self.observe_watch_checks(&target, &account, number, validators)
            }
        }?;
        if self.is_stopped() {
            return Err(StorageError::Cancelled.into());
        }
        let current = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            issue_poll::target(snapshot, &binding.registration)
        })?;
        if !current.is_some_and(|current| {
            current.account == target.account
                && current.binding == target.binding
                && current.repository == target.repository
        }) {
            return Err(StorageError::ScopeDenied.into());
        }
        Ok((observation, due))
    }

    fn observe_watch_run(
        &self,
        target: &PollTarget,
        account: &PollAccount,
        run: u64,
        validators: Option<&Validators>,
    ) -> Result<(RunObservation, i64), Failure> {
        let endpoint = Endpoint::new(
            &format!(
                "https://api.github.com/repositories/{}/actions/runs/{run}",
                target.numeric_id
            ),
            None,
        )
        .map_err(repositories::api_fault)?;
        let now = self.clock.now_ms().get();
        let token = self
            .client
            .token_for_account(&*self.store, &target.account)
            .map_err(repositories::api_fault)?;
        let page = self
            .client
            .observe_authenticated_page(&endpoint, None, validators, now, POLL_INTERVAL_MS, token)
            .map_err(repositories::api_fault)?;
        let due = page.control.poll_after_ms;
        self.note_poll_pacing(account, &page);
        let observation =
            decode_run(page, target, run, validators.is_some()).map_err(repositories::api_fault)?;
        Ok((observation, due))
    }

    fn observe_watch_checks(
        &self,
        target: &PollTarget,
        account: &PollAccount,
        number: u64,
        validators: Option<&WatchValidators>,
    ) -> Result<(RunObservation, i64), Failure> {
        let now = self.clock.now_ms().get();
        let fallback = now.saturating_add(POLL_INTERVAL_MS);
        let number_text = number.to_string();
        let head_page = self.observe_pull_head_page(target, &number_text)?;
        self.note_poll_pacing(account, &head_page);
        if head_page.throttled() {
            return Ok((RunObservation::Deferred, head_page.control.poll_after_ms));
        }
        let Some(head) = Self::decode_pull_head(&head_page, target, &number_text)? else {
            return Err(repositories::api_fault(Fault::InvalidResponse));
        };
        if head.len() != 40 || !head.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(repositories::api_fault(Fault::InvalidResponse));
        }
        let token = self
            .client
            .token_for_account(&*self.store, &target.account)
            .map_err(repositories::api_fault)?;
        let endpoint = |suffix| {
            Endpoint::new(
                &format!(
                    "https://api.github.com/repositories/{}/commits/{head}/{suffix}",
                    target.numeric_id,
                ),
                None,
            )
            .map_err(repositories::api_fault)
        };
        let checks = endpoint("check-runs?filter=latest&per_page=100")?;
        let validators = validators
            .filter(|stored| stored.head.as_deref() == Some(head.as_str()))
            .map(|stored| &stored.value);
        let mut next = None;
        let mut expected = None;
        let mut statuses = Checks::default();
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..10 {
            let Some(page) = self.observe_watch_page(
                account,
                &checks,
                next.as_deref(),
                validators.filter(|_| next.is_none()),
                &token,
            )?
            else {
                return Ok(self.deferred_watch(account));
            };
            let due = page.control.poll_after_ms;
            if page.status == 304 && next.is_none() && validators.is_some() {
                return Ok((RunObservation::Unchanged, due));
            }
            let value = match check_page(&page).map_err(repositories::api_fault)? {
                Some(value) => value,
                None => return Ok((RunObservation::Deferred, due)),
            };
            let total = value["total_count"]
                .as_u64()
                .filter(|count| *count <= 1_000)
                .ok_or_else(|| repositories::api_fault(Fault::InvalidResponse))?;
            if expected
                .replace(total)
                .is_some_and(|before| before != total)
            {
                return Err(repositories::api_fault(Fault::InvalidResponse));
            }
            let runs = value["check_runs"]
                .as_array()
                .ok_or_else(|| repositories::api_fault(Fault::InvalidResponse))?;
            for check in runs {
                let id = check["id"]
                    .as_u64()
                    .filter(|id| seen.insert(*id))
                    .ok_or_else(|| repositories::api_fault(Fault::InvalidResponse))?;
                if id == 0 || check["head_sha"].as_str() != Some(&head) {
                    return Err(repositories::api_fault(Fault::InvalidResponse));
                }
                statuses.record(check).map_err(repositories::api_fault)?;
            }
            if statuses.pending {
                // A pending result on the first page is sufficient to wait. Once that page
                // changes, read the complete set again before trying to settle the watcher.
                let validators = if next.is_none() {
                    WatchValidators::from_page(&page, Some(head))
                } else {
                    None
                };
                return Ok((RunObservation::Changed(None, validators), due));
            }
            let no_checks = runs.is_empty();
            next = page.next_page;
            if next.is_none() {
                if seen.len() as u64 != total {
                    return Err(repositories::api_fault(Fault::InvalidResponse));
                }
                break;
            }
            if no_checks || seen.len() as u64 >= total {
                return Err(repositories::api_fault(Fault::InvalidResponse));
            }
        }
        if next.is_some() {
            return Err(repositories::api_fault(Fault::InvalidResponse));
        }
        let Some(combined) =
            self.observe_watch_page(account, &endpoint("status?per_page=1")?, None, None, &token)?
        else {
            return Ok(self.deferred_watch(account));
        };
        let Some(value) = check_page(&combined).map_err(repositories::api_fault)? else {
            return Ok((RunObservation::Deferred, combined.control.poll_after_ms));
        };
        if value["sha"].as_str() != Some(&head) {
            return Err(repositories::api_fault(Fault::InvalidResponse));
        }
        let commit_statuses = value["total_count"]
            .as_u64()
            .ok_or_else(|| repositories::api_fault(Fault::InvalidResponse))?;
        match (commit_statuses, value["state"].as_str()) {
            (0, _) => {}
            (_, Some("success")) => statuses.passed = true,
            (_, Some("failure" | "error")) => statuses.failed = true,
            (_, Some("pending")) => statuses.pending = true,
            _ => return Err(repositories::api_fault(Fault::InvalidResponse)),
        }
        if statuses.pending {
            return Ok((RunObservation::Changed(None, None), fallback));
        }
        let mut has_evidence = !seen.is_empty() || commit_statuses != 0;
        for evidence in [Evidence::CheckSuite, Evidence::WorkflowRun] {
            let (mut path, member) = evidence.endpoint(&number_text, &head);
            if !path.ends_with('?') {
                path.push('&');
            }
            let review = Endpoint::new(
                &format!(
                    "https://api.github.com/repositories/{}{path}per_page=100",
                    target.numeric_id
                ),
                None,
            )
            .map_err(repositories::api_fault)?;
            let mut next = None;
            let mut total = None;
            let mut ids = std::collections::BTreeSet::new();
            for _ in 0..10 {
                let Some(page) =
                    self.observe_watch_page(account, &review, next.as_deref(), None, &token)?
                else {
                    return Ok(self.deferred_watch(account));
                };
                let Some(value) = check_page(&page).map_err(repositories::api_fault)? else {
                    return Ok((RunObservation::Deferred, page.control.poll_after_ms));
                };
                let count = value["total_count"]
                    .as_u64()
                    .filter(|count| *count <= 1_000)
                    .ok_or_else(|| repositories::api_fault(Fault::InvalidResponse))?;
                if total
                    .replace(count)
                    .is_some_and(|previous| previous != count)
                {
                    return Err(repositories::api_fault(Fault::InvalidResponse));
                }
                let rows = value[member]
                    .as_array()
                    .ok_or_else(|| repositories::api_fault(Fault::InvalidResponse))?;
                for row in rows {
                    if row["head_sha"].as_str() != Some(&head)
                        || !row["id"]
                            .as_u64()
                            .is_some_and(|id| id != 0 && ids.insert(id))
                    {
                        return Err(repositories::api_fault(Fault::InvalidResponse));
                    }
                    statuses.record(row).map_err(repositories::api_fault)?;
                    if statuses.pending {
                        return Ok((
                            RunObservation::Changed(None, None),
                            page.control.poll_after_ms,
                        ));
                    }
                }
                next = page.next_page;
                if next.is_none() {
                    break;
                }
            }
            if next.is_some() || Some(ids.len() as u64) != total {
                return Err(repositories::api_fault(Fault::InvalidResponse));
            }
            has_evidence |= !ids.is_empty();
        }
        if !has_evidence {
            return Ok((RunObservation::Changed(None, None), fallback));
        }
        if self.is_stopped() {
            return Err(StorageError::Cancelled.into());
        }
        if self
            .poll_deferred_until(account, self.clock.now_ms().get())
            .is_some()
        {
            return Ok(self.deferred_watch(account));
        }
        let head_page = self.observe_pull_head_page(target, &number_text)?;
        self.note_poll_pacing(account, &head_page);
        if head_page.throttled() {
            return Ok((RunObservation::Deferred, head_page.control.poll_after_ms));
        }
        if Self::decode_pull_head(&head_page, target, &number_text)?.as_deref() != Some(&head) {
            return Ok((RunObservation::Changed(None, None), fallback));
        }
        let (kind, conclusion) = if statuses.failed {
            ("Failed", "failure")
        } else if statuses.passed {
            ("Succeeded", "success")
        } else {
            ("Uncertain", "neutral or skipped")
        };
        Ok((
            RunObservation::Changed(
                Some(WatchConclusion {
                    kind,
                    conclusion,
                    url: format!("{}/pull/{number}/checks", target.repository.url),
                    duration_ms: None,
                }),
                None,
            ),
            fallback,
        ))
    }

    fn observe_watch_page(
        &self,
        account: &PollAccount,
        endpoint: &Endpoint,
        next: Option<&str>,
        validators: Option<&Validators>,
        token: &Secret,
    ) -> Result<Option<ApiPage>, Failure> {
        if self.is_stopped() {
            return Err(StorageError::Cancelled.into());
        }
        let now = self.clock.now_ms().get();
        if self.poll_deferred_until(account, now).is_some() {
            return Ok(None);
        }
        let page = self
            .client
            .observe_authenticated_page(
                endpoint,
                next,
                validators,
                now,
                POLL_INTERVAL_MS,
                token.clone(),
            )
            .map_err(repositories::api_fault)?;
        self.note_poll_pacing(account, &page);
        Ok(Some(page))
    }

    pub(super) fn note_poll_pacing(&self, account: &PollAccount, page: &ApiPage) {
        if let Some(until) = page.shared_backoff() {
            self.defer_polls(account.clone(), until);
        }
    }

    fn deferred_watch(&self, account: &PollAccount) -> (RunObservation, i64) {
        let now = self.clock.now_ms().get();
        (
            RunObservation::Deferred,
            self.poll_deferred_until(account, now).unwrap_or(now),
        )
    }
}

fn check_page(page: &ApiPage) -> Result<Option<&Value>, Fault> {
    match page.status {
        200 => page.value.as_ref().map(Some).ok_or(Fault::InvalidResponse),
        403 | 429 if page.throttled() => Ok(None),
        status => Err(Fault::Rejected(status)),
    }
}

#[derive(Default)]
struct Checks {
    pending: bool,
    passed: bool,
    failed: bool,
}

impl Checks {
    fn record(&mut self, check: &Value) -> Result<(), Fault> {
        match workflow_result(check)? {
            Some(("Succeeded", _)) => self.passed = true,
            Some(("Uncertain", _)) => {}
            Some(_) => self.failed = true,
            None if check["conclusion"].is_null() => self.pending = true,
            None => return Err(Fault::InvalidResponse),
        }
        Ok(())
    }
}
fn decode_run(
    page: ApiPage,
    target: &PollTarget,
    run: u64,
    conditional: bool,
) -> Result<RunObservation, Fault> {
    match page.status {
        200 => {}
        304 if conditional => return Ok(RunObservation::Unchanged),
        403 | 429 if page.throttled() => {
            return Ok(RunObservation::Deferred);
        }
        status => return Err(Fault::Rejected(status)),
    }
    let value = page.value.as_ref().ok_or(Fault::InvalidResponse)?;
    let url = format!(
        "https://github.com/{}/{}/actions/runs/{run}",
        target.repository.owner, target.repository.name
    );
    if value["id"].as_u64() != Some(run)
        || value["repository"]["id"].as_u64() != Some(target.numeric_id)
        || !value["html_url"]
            .as_str()
            .is_some_and(|actual| actual.eq_ignore_ascii_case(&url))
    {
        return Err(Fault::InvalidResponse);
    }
    let conclusion = workflow_conclusion(value, url)?;
    Ok(RunObservation::Changed(
        conclusion,
        WatchValidators::from_page(&page, None),
    ))
}

fn workflow_result(value: &Value) -> Result<Option<(&'static str, &'static str)>, Fault> {
    match value["status"].as_str() {
        Some("queued" | "in_progress" | "requested" | "waiting" | "pending") => return Ok(None),
        Some("completed") => {}
        _ => return Err(Fault::InvalidResponse),
    }
    let (kind, conclusion) = match value["conclusion"].as_str() {
        Some("success") => ("Succeeded", "success"),
        Some("cancelled") => ("Cancelled", "cancelled"),
        Some("timed_out") => ("TimedOut", "timed_out"),
        Some("failure") => ("Failed", "failure"),
        Some("action_required") => ("Failed", "action_required"),
        Some("startup_failure") => ("Failed", "startup_failure"),
        Some("stale") => ("Failed", "stale"),
        Some("neutral") => ("Uncertain", "neutral"),
        Some("skipped") => ("Uncertain", "skipped"),
        _ => return Err(Fault::InvalidResponse),
    };
    Ok(Some((kind, conclusion)))
}

fn workflow_conclusion(value: &Value, url: String) -> Result<Option<WatchConclusion>, Fault> {
    let Some((kind, conclusion)) = workflow_result(value)? else {
        return Ok(None);
    };
    let timestamp = |name: &str| {
        value[name]
            .as_str()?
            .parse::<jiff::Timestamp>()
            .ok()
            .map(|time| time.as_millisecond())
    };
    let duration_ms = timestamp("run_started_at")
        .zip(timestamp("updated_at"))
        .and_then(|(start, end)| end.checked_sub(start).filter(|duration| *duration >= 0));
    Ok(Some(WatchConclusion {
        kind,
        conclusion,
        url,
        duration_ms,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_queued_run_is_not_a_conclusion() {
        let pending = json!({"status":"queued", "conclusion":null});
        assert!(
            workflow_conclusion(&pending, String::new())
                .unwrap()
                .is_none()
        );
        let missing = json!({"status":"completed", "conclusion":null});
        assert_eq!(
            workflow_conclusion(&missing, String::new()),
            Err(Fault::InvalidResponse)
        );
    }

    #[test]
    fn a_completed_run_preserves_the_remote_conclusion_and_duration() {
        let run = json!({
            "status":"completed", "conclusion":"failure",
            "run_started_at":"2026-11-24T10:00:00Z", "updated_at":"2026-11-24T10:01:30Z"
        });
        assert_eq!(
            workflow_conclusion(&run, "run link".into()).unwrap(),
            Some(WatchConclusion {
                kind: "Failed",
                conclusion: "failure",
                url: "run link".into(),
                duration_ms: Some(90_000),
            })
        );
    }
}
