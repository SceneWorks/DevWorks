//! Read an Actions conclusion from the repository bound to the current GitHub account.
use super::{
    GitHubService,
    api::{ApiPage, Endpoint, Validators},
    issue_poll::{self, PollTarget},
    repositories,
};
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
        if let Some(until) = self.poll_deferred_until(self.clock.now_ms().get()) {
            return Ok((RunObservation::Deferred, until));
        }
        let target = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            issue_poll::target(snapshot, &binding.registration)
        })?
        .filter(|target| matches_binding(binding, target))
        .ok_or_else(|| not_found("github_repository", &binding.registration))?;
        let (observation, due) = match binding.subject {
            GithubWatchSubject::WorkflowRun(run) => {
                self.observe_watch_run(&target, run, validators.map(|stored| &stored.value))
            }
            GithubWatchSubject::PullRequest(number) => {
                self.observe_watch_checks(&target, number, validators)
            }
        }?;
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
        if let Some(until) = page.control.backoff_until_ms {
            self.defer_polls(until);
        }
        let observation =
            decode_run(page, target, run, validators.is_some()).map_err(repositories::api_fault)?;
        Ok((observation, due))
    }

    fn observe_watch_checks(
        &self,
        target: &PollTarget,
        number: u64,
        validators: Option<&WatchValidators>,
    ) -> Result<(RunObservation, i64), Failure> {
        let now = self.clock.now_ms().get();
        let fallback = now.saturating_add(POLL_INTERVAL_MS);
        let Some(head) = self.observe_pull_head(target, &number.to_string())? else {
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
        loop {
            let page = self
                .client
                .observe_authenticated_page(
                    &checks,
                    next.as_deref(),
                    if next.is_none() { validators } else { None },
                    now,
                    POLL_INTERVAL_MS,
                    token.clone(),
                )
                .map_err(repositories::api_fault)?;
            let due = page.control.poll_after_ms;
            if let Some(until) = page.control.backoff_until_ms {
                self.defer_polls(until);
            }
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
        let combined = self
            .client
            .observe_authenticated_page(
                &endpoint("status?per_page=1")?,
                None,
                None,
                now,
                POLL_INTERVAL_MS,
                token,
            )
            .map_err(repositories::api_fault)?;
        if let Some(until) = combined.control.backoff_until_ms {
            self.defer_polls(until);
        }
        let Some(value) = check_page(&combined).map_err(repositories::api_fault)? else {
            return Ok((RunObservation::Deferred, combined.control.poll_after_ms));
        };
        if value["sha"].as_str() != Some(&head) {
            return Err(repositories::api_fault(Fault::InvalidResponse));
        }
        match (value["total_count"].as_u64(), value["state"].as_str()) {
            (Some(0), _) => {}
            (Some(_), Some("success")) => statuses.passed = true,
            (Some(_), Some("failure" | "error")) => statuses.failed = true,
            (Some(_), Some("pending")) => statuses.pending = true,
            _ => return Err(repositories::api_fault(Fault::InvalidResponse)),
        }
        if statuses.pending
            || seen.is_empty() && !statuses.passed && !statuses.failed
            || self
                .observe_pull_head(target, &number.to_string())?
                .as_deref()
                != Some(&head)
        {
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
}

fn check_page(page: &ApiPage) -> Result<Option<&Value>, Fault> {
    match page.status {
        200 => page.value.as_ref().map(Some).ok_or(Fault::InvalidResponse),
        403 | 429 if page.control.backoff_until_ms.is_some() => Ok(None),
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
        match (check["status"].as_str(), check["conclusion"].as_str()) {
            (Some("completed"), Some("success")) => self.passed = true,
            (Some("completed"), Some("neutral" | "skipped")) => {}
            (
                Some("completed"),
                Some(
                    "failure" | "cancelled" | "timed_out" | "stale" | "action_required"
                    | "startup_failure",
                ),
            ) => self.failed = true,
            (Some("queued" | "in_progress" | "requested" | "waiting" | "pending"), None) => {
                self.pending = true;
            }
            _ => return Err(Fault::InvalidResponse),
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
        403 | 429 if page.control.backoff_until_ms.is_some() => {
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

fn workflow_conclusion(value: &Value, url: String) -> Result<Option<WatchConclusion>, Fault> {
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
