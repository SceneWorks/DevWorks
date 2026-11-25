//! GitHub watch sources share one poll per remote target. Backup reads bypass the validator
//! cache while still honoring the account's current pacing.
use super::{WatchService, finish_in, outcome};
use crate::locks::LockExt;
use crate::ports::storage::{
    GithubWatchSubject, ReadOptions, StorageError, StoredGithubWatch, StoredWatcher, WatcherPhase,
    WriteOptions, WriteTransaction, read_snapshot, run_write,
};
use crate::protocol::records::WatchGithubTarget;
use crate::protocol::{Id, ProjectId};
use crate::services::github::{
    GitHubService, credentials,
    watch::{
        POLL_INTERVAL_MS, RunObservation, WatchConclusion, WatchValidators, bind_watch,
        watch_permission,
    },
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub(super) const REVISION: u32 = 9;

pub(super) struct GithubWatch {
    service: Arc<GitHubService>,
    cache: Mutex<TargetCache>,
    generation: AtomicU64,
}

#[derive(Clone, Copy)]
pub(super) struct PollPass {
    generation: u64,
    direct: bool,
}

type TargetCache = HashMap<(String, Id, GithubWatchSubject, i64), Poll>;

#[derive(Default)]
struct Poll {
    due: i64,
    validators: Option<WatchValidators>,
    conclusion: Option<WatchConclusion>,
    unavailable: bool,
    observed_at_ms: Option<i64>,
    generation: u64,
}

impl GithubWatch {
    pub(super) fn new(service: Arc<GitHubService>) -> Self {
        Self {
            service,
            cache: Mutex::default(),
            generation: AtomicU64::new(1),
        }
    }

    pub(super) fn pass(&self, direct: bool) -> PollPass {
        PollPass {
            generation: self.generation.fetch_add(1, Ordering::Relaxed),
            direct,
        }
    }

    fn poll(
        &self,
        binding: &StoredGithubWatch,
        permission: i64,
        now: i64,
        pass: PollPass,
    ) -> (PollResult, Option<i64>) {
        let mut cache = self.cache.locked();
        cache.retain(|_, poll| poll.due >= now.saturating_sub(2 * POLL_INTERVAL_MS));
        let key = (
            binding.remote_user.clone(),
            binding.remote_repository.clone(),
            binding.subject,
            permission,
        );
        let poll = cache.entry(key).or_default();
        if poll.generation == pass.generation {
            return (poll.result(), poll.observed_at_ms);
        }
        if !pass.direct && poll.due > now {
            return (PollResult::Running, poll.observed_at_ms);
        }
        let validators = if pass.direct {
            None
        } else {
            poll.validators.as_ref()
        };
        match self.service.read_watch(binding, validators) {
            Ok((RunObservation::Changed(conclusion, validators), due)) => {
                poll.due = due;
                poll.conclusion = conclusion;
                poll.validators = validators;
                poll.unavailable = false;
            }
            Ok((RunObservation::Unchanged, due)) => {
                poll.due = due;
                poll.unavailable = false;
            }
            Ok((RunObservation::Deferred, due)) => {
                poll.due = due;
                return (PollResult::Running, None);
            }
            Err(_) => {
                poll.due = now.saturating_add(POLL_INTERVAL_MS);
                poll.unavailable = true;
                poll.conclusion = None;
                poll.validators = None;
            }
        }
        poll.generation = pass.generation;
        poll.observed_at_ms = Some(now);
        (poll.result(), poll.observed_at_ms)
    }
}

impl Poll {
    fn result(&self) -> PollResult {
        if self.unavailable {
            PollResult::Unavailable
        } else if let Some(result) = &self.conclusion {
            PollResult::Finished(result.clone())
        } else {
            PollResult::Running
        }
    }
}

enum PollResult {
    Running,
    Unavailable,
    Finished(WatchConclusion),
}

pub(super) fn subject(input: &WatchGithubTarget) -> GithubWatchSubject {
    match &input.workflow_run_id {
        Some(run) => GithubWatchSubject::WorkflowRun(run.get() as u64),
        None => GithubWatchSubject::PullRequest(
            input
                .pull_request_number
                .as_ref()
                .expect("admitted GitHub target")
                .get() as u64,
        ),
    }
}

pub(super) fn record(binding: &StoredGithubWatch) -> WatchGithubTarget {
    let (run, pull) = match binding.subject {
        GithubWatchSubject::WorkflowRun(run) => (Some(super::counter(run as i64)), None),
        GithubWatchSubject::PullRequest(pull) => (None, Some(super::counter(pull as i64))),
    };
    WatchGithubTarget {
        repository_id: binding.registration.clone(),
        workflow_run_id: run,
        pull_request_number: pull,
    }
}

fn current_in(
    tx: &mut dyn WriteTransaction,
    source: &StoredGithubWatch,
    project: Option<&ProjectId>,
    permission: Option<i64>,
) -> Result<bool, StorageError> {
    let Some(project) = project else {
        return Ok(false);
    };
    let Some(account) = tx.github().github_account()? else {
        return Ok(false);
    };
    if account.remote_user_id.as_ref() != Some(&source.remote_user)
        || permission.is_some_and(|revision| revision != account.permission_revision)
        || !credentials::account_is_current(tx.configuration(), &account)?
        || !tx
            .projects()
            .repository_projects(&source.registration)?
            .contains(project)
    {
        return Ok(false);
    }
    let Some(binding) = tx
        .github()
        .github_repository_binding(&source.registration)?
    else {
        return Ok(false);
    };
    let Some(repository) = tx.projects().repository(&source.registration)? else {
        return Ok(false);
    };
    Ok(binding.state == "Active"
        && binding.github_repository_id == source.remote_repository
        && repository.lifecycle == "Active"
        && repository.remote_url.as_ref() == Some(&binding.registered_remote)
        && tx
            .github()
            .github_discovered_repository(&source.remote_repository)?
            .is_some_and(|remote| {
                remote.account_id == account.id
                    && remote.permission_revision == account.permission_revision
            }))
}

impl WatchService {
    pub(super) fn prepare_github(
        &self,
        input: &WatchGithubTarget,
    ) -> Result<Option<StoredGithubWatch>, StorageError> {
        if self.github.is_none() {
            return Ok(None);
        }
        read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            bind_watch(snapshot, &input.repository_id, subject(input))
        })
    }

    pub(super) fn github_bound(
        &self,
        tx: &mut dyn WriteTransaction,
        source: &StoredGithubWatch,
        project: Option<&ProjectId>,
    ) -> Result<bool, StorageError> {
        current_in(tx, source, project, None)
    }

    pub(super) fn step_github(
        &self,
        watcher: &StoredWatcher,
        source: &StoredGithubWatch,
        pass: Option<PollPass>,
    ) -> Result<(bool, Option<&'static str>), StorageError> {
        let now = self.clock.now_ms().get();
        let expired = watcher
            .expires_at_ms
            .is_some_and(|deadline| now >= deadline);
        let observation = if expired {
            None
        } else {
            let (Some(github), Some(pass)) = (&self.github, pass) else {
                return Ok((
                    false,
                    Some("the GitHub watcher has no available account service"),
                ));
            };
            let current = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
                let registered = snapshot
                    .projects()
                    .repository_projects(&source.registration)?;
                if !watcher
                    .project_id
                    .as_ref()
                    .is_some_and(|project| registered.contains(project))
                {
                    return Ok(None);
                }
                watch_permission(snapshot, source)
            })?;
            let Some(permission) = current else {
                return Ok((false, Some("its GitHub repository binding is unavailable")));
            };
            let (observed, polled_at) = github.poll(source, permission, now, pass);
            if let Some(polled_at) = polled_at {
                self.last_polled
                    .locked()
                    .insert(watcher.id.clone(), polled_at);
            }
            match observed {
                PollResult::Running => return Ok((false, None)),
                PollResult::Unavailable => {
                    return Ok((false, Some("its GitHub status cannot be established")));
                }
                PollResult::Finished(observed) => Some((observed, permission)),
            }
        };
        let id = watcher.id.clone();
        let source = source.clone();
        let result = run_write(&*self.store, WriteOptions::host_state(), move |tx| {
            super::end_with_owner_in(tx, &id)?;
            let Some(watcher) = tx.watchers().watcher(&id)? else {
                return Ok(false);
            };
            if watcher.phase != WatcherPhase::Active || watcher.github.as_ref() != Some(&source) {
                return Ok(false);
            }
            let now = tx.now_ms();
            let (kind, detail, duration) = if watcher.expires_at_ms.is_some_and(|end| now >= end) {
                (
                    "TimedOut",
                    "the watcher's maximum lifetime elapsed".to_owned(),
                    Some(now.saturating_sub(watcher.created_at_ms)),
                )
            } else {
                let Some((observed, permission)) = observation else {
                    return Ok(false);
                };
                if !current_in(tx, &source, watcher.project_id.as_ref(), Some(permission))? {
                    return Ok(false);
                }
                (
                    observed.kind,
                    format!(
                        "GitHub conclusion: {}\n{}",
                        observed.conclusion, observed.url
                    ),
                    observed.duration_ms,
                )
            };
            finish_in(
                tx,
                watcher,
                outcome(kind, None, None, Some(&detail)),
                duration,
                None,
                false,
                None,
            )?;
            Ok(true)
        })?;
        Ok((result, None))
    }
}
