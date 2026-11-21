//! The foreground runtime cooperatively scans native registrations. Network I/O happens after
//! releasing each snapshot, and a process restart simply scans the same durable endpoint cursors.
use super::issue_poll::{self, IssuePageResult};
use super::{GitHubService, credentials};
use crate::ports::storage::{
    ConfigScope, ConfigurationRead, ProjectRead, ReadOptions, RepositoryFilter, StorageError,
    WriteTransaction, read_snapshot,
};
use crate::protocol::Id;
use crate::services::git::github::Repository;
use crate::settings::catalog::descriptor;
use crate::settings::model::{chain, current_chain};
use std::sync::atomic::Ordering;

const PAGE: u16 = 32;
const IMPORT: &str = "github.importOpenIssues";
const SYNC: &str = "github.syncLinkedIssueFields";
const CHANGES: &str = "github.pollForChanges";

struct Registration {
    local: Id,
    qualified: bool,
    policy: Policy,
}

pub struct PollPass {
    pub after: Option<Id>,
    pub defer_until_ms: Option<i64>,
}

impl GitHubService {
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    pub fn is_stopped(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Read at most one registration page and capture at most one endpoint page per registration.
    /// A throttle ends the pass and the runtime waits before proceeding to other registrations.
    pub fn poll_pass(&self, after: Option<Id>) -> Result<PollPass, StorageError> {
        let (page, next) = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            let Some(account) = snapshot.github().github_account()? else {
                return Ok((Vec::new(), None));
            };
            if !credentials::account_is_current(snapshot.configuration(), &account)? {
                return Ok((Vec::new(), None));
            }
            let cursor_page = snapshot.projects().repositories(
                &RepositoryFilter::default(),
                after.as_ref(),
                PAGE,
            )?;
            let next = if cursor_page.len() == usize::from(PAGE) {
                cursor_page.last().map(|row| row.id.clone())
            } else {
                None
            };
            let mut work = Vec::new();
            for row in cursor_page {
                if row.lifecycle != "Active"
                    || row
                        .remote_url
                        .as_deref()
                        .and_then(Repository::from_remote)
                        .is_none()
                {
                    continue;
                }
                let enabled = policy(snapshot.projects(), snapshot.configuration(), &row.id)?;
                if enabled.import || enabled.sync || enabled.changes {
                    work.push(Registration {
                        qualified: issue_poll::target(snapshot, &row.id)?.is_some(),
                        local: row.id,
                        policy: enabled,
                    });
                }
            }
            Ok((work, next))
        })?;
        for row in page {
            if self.is_stopped() {
                break;
            }
            if !row.qualified && self.qualify_registration(&row.local).is_err() {
                continue;
            }
            if let Some(mode) = row.policy.issue_mode()
                && let Ok(IssuePageResult::Throttled { next_due_at_ms }) =
                    self.poll_configured_issues(&row.local, mode)
            {
                return Ok(PollPass {
                    after,
                    defer_until_ms: Some(next_due_at_ms),
                });
            }
            if row.policy.changes
                && let Ok(IssuePageResult::Throttled { next_due_at_ms }) =
                    self.poll_default_ref(&row.local)
            {
                return Ok(PollPass {
                    after,
                    defer_until_ms: Some(next_due_at_ms),
                });
            }
        }
        Ok(PollPass {
            after: next,
            defer_until_ms: None,
        })
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct Policy {
    import: bool,
    sync: bool,
    pub(super) changes: bool,
}

impl Policy {
    pub(super) fn issue_mode(self) -> Option<issue_poll::IssueMode> {
        match self {
            Self {
                import: true, sync, ..
            } => Some(issue_poll::IssueMode::Import {
                linked_updates: sync,
            }),
            Self { sync: true, .. } => Some(issue_poll::IssueMode::LinkedOnly),
            _ => None,
        }
    }
}

pub(super) fn active_policy(
    tx: &mut dyn WriteTransaction,
    repository: &Id,
) -> Result<Policy, StorageError> {
    let scopes = scopes(tx.projects(), repository)?;
    resolve_policy(tx.configuration(), &scopes)
}

fn policy(
    projects: &dyn ProjectRead,
    settings: &dyn ConfigurationRead,
    repository: &Id,
) -> Result<Policy, StorageError> {
    resolve_policy(settings, &scopes(projects, repository)?)
}

fn scopes(projects: &dyn ProjectRead, repository: &Id) -> Result<Vec<ConfigScope>, StorageError> {
    let mut scopes = Vec::new();
    for project in projects.repository_projects(repository)? {
        if projects
            .project(&project)?
            .is_some_and(|row| row.lifecycle == "Active")
        {
            scopes.push(ConfigScope::Repository {
                project,
                repository: repository.clone(),
            });
        }
    }
    if scopes.is_empty() {
        scopes.push(ConfigScope::Workspace);
    }
    Ok(scopes)
}

fn resolve_policy(
    settings: &dyn ConfigurationRead,
    scopes: &[ConfigScope],
) -> Result<Policy, StorageError> {
    let mut enabled = Policy::default();
    for scope in scopes {
        let current = configured(settings, scope)?;
        enabled.import |= current.import;
        enabled.sync |= current.sync;
        enabled.changes |= current.changes;
    }
    Ok(enabled)
}

fn configured(read: &dyn ConfigurationRead, scope: &ConfigScope) -> Result<Policy, StorageError> {
    let layers = current_chain(read, scope)?;
    let read_flag = |key| {
        chain(descriptor(key).expect("GitHub catalog setting"), &layers)
            .ok()
            .and_then(|entries| entries.first()?.value.value.0.as_bool())
            .ok_or_else(|| StorageError::InvalidRecord("Invalid GitHub polling preference".into()))
    };
    Ok(Policy {
        import: read_flag(IMPORT)?,
        sync: read_flag(SYNC)?,
        changes: read_flag(CHANGES)?,
    })
}
