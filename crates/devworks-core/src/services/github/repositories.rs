//! Lists authenticated GitHub repositories for the workspace user. Scoped callers only see
//! qualified repositories registered in their authorized project.
use super::account::{GitHubService, workspace_human};
use super::credentials;
use super::identity::RepositoryIdentity;
use crate::authority::reach::{Reach, reach};
use crate::context::InvocationContext;
use crate::operations::github::RepositoriesInput;
use crate::ports::Preparation;
use crate::ports::storage::{
    ConfigScope, GithubScope, ReadSnapshot, StorageError, StoredGithubAccount,
};
use crate::protocol::data::{Coverage, CoverageState};
use crate::protocol::records::{LifeState, RepositoryRecord};
use crate::protocol::support::{BoundedList, Page, Url};
use crate::protocol::{DomainErrorCode, Id, ProjectId};
use crate::services::cursor::{self, CursorBinding, Paged};
use crate::services::git::github::Fault;
use crate::services::kernel::{Failure, Refusal, label, rev};
use crate::settings::model::current_layer;
use std::collections::HashMap;

pub(super) struct PreparedPage {
    revision: i64,
    settings_revision: i64,
    result: Result<Page<RepositoryRecord>, Failure>,
}

impl PreparedPage {
    pub(super) fn answer(
        &self,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<(Page<RepositoryRecord>, i64), Failure> {
        let current = snapshot.github().github_account()?;
        if current.as_ref().map_or(1, |row| row.revision) != self.revision
            || current_layer(snapshot.configuration(), &ConfigScope::Workspace)?.revision
                != self.settings_revision
        {
            return Err(refusal(DomainErrorCode::Conflict).into());
        }
        self.result.clone().map(|page| (page, self.revision))
    }
}

pub(super) fn prepare(
    service: &GitHubService,
    ctx: &InvocationContext,
    input: &RepositoriesInput,
    env: &Preparation<'_>,
) -> Result<PreparedPage, StorageError> {
    debug_assert!(workspace_human(ctx));
    let (account, settings_revision, qualified) = env.read(|s| {
        let account = s.github().github_account()?;
        let configured = match &account {
            Some(account) => {
                credentials::matches_revision(s.configuration(), account.credential_revision)?
            }
            None => false,
        };
        Ok((
            account,
            current_layer(s.configuration(), &ConfigScope::Workspace)?.revision,
            configured,
        ))
    })?;
    let revision = account.as_ref().map_or(1, |row| row.revision);
    let result = (|| {
        let row = qualified_account(account.as_ref(), input, qualified)?;
        let limit = input.page.limit.get().min(100) as u16;
        let limit_key = limit.to_string();
        let owner = input
            .organization
            .as_ref()
            .map_or("", |owner| owner.0.as_str());
        let bind = CursorBinding::new(
            ctx,
            "github.repositories",
            &[row.id.as_str(), owner, &limit_key],
            row.permission_revision,
        );
        let mut next = bind.resume(&input.page)?;
        let mut repositories = Vec::new();
        let mut next_page = None;
        let max_scans = if owner.is_empty() { 1 } else { 5 };
        for _ in 0..max_scans {
            let page = service
                .client
                .observe_account_repositories(
                    &*service.store,
                    next.as_deref(),
                    None,
                    limit,
                    service.clock.now_ms().get(),
                )
                .map_err(api_fault)?;
            let mut batch = RepositoryIdentity::decode_page(&page).map_err(api_fault)?;
            if batch.len() > usize::from(limit) {
                return Err(api_fault(Fault::InvalidResponse));
            }
            batch.retain(|repo| {
                owner.is_empty()
                    || repo.owner_kind == "Organization"
                        && repo.repository.owner.eq_ignore_ascii_case(owner)
            });
            repositories.extend(batch);
            next_page = page.next_page;
            if !repositories.is_empty() || next_page.is_none() {
                break;
            }
            next = next_page.clone();
        }
        let next_cursor = next_page.map(|page| bind.cursor(&page));
        if next_cursor
            .as_ref()
            .is_some_and(|cursor| cursor.0.len() > 4096)
        {
            return Err(api_fault(Fault::InvalidResponse));
        }
        Ok(Paged {
            items: repositories
                .into_iter()
                .map(|repo| discovered_record(repo, row.permission_revision))
                .collect(),
            next_cursor,
        }
        .into_page())
    })();
    Ok(PreparedPage {
        revision,
        settings_revision,
        result,
    })
}

pub(super) fn registered_page(
    ctx: &InvocationContext,
    snapshot: &dyn ReadSnapshot,
    input: &RepositoriesInput,
) -> Result<(Page<RepositoryRecord>, i64), Failure> {
    let project = authorized_project(ctx, snapshot)?;
    let account = snapshot.github().github_account()?;
    let configured = match &account {
        Some(account) => {
            credentials::matches_revision(snapshot.configuration(), account.credential_revision)?
        }
        None => false,
    };
    let account = qualified_account(account.as_ref(), input, configured)?;
    let scope = GithubScope {
        repositories: snapshot.projects().members(&project)?,
        project_id: Some(project.clone()),
    };
    let mut registrations = HashMap::new();
    for id in &scope.repositories {
        if let Some(binding) = snapshot.github().github_repository_binding(id)?
            && binding.state == "Active"
        {
            registrations
                .entry(binding.github_repository_id)
                .or_insert(id);
        }
    }
    let owner = input.organization.as_ref().map(|owner| owner.0.as_str());
    let membership_revision = snapshot
        .sessions()
        .project_scope_revision(&project)?
        .unwrap_or(1);
    let permission_key = account.permission_revision.to_string();
    let bind = CursorBinding::new(
        ctx,
        "github.repositories",
        &[
            account.id.as_str(),
            project.0.as_str(),
            owner.unwrap_or(""),
            &permission_key,
        ],
        membership_revision,
    );
    let rows = cursor::page(
        &bind,
        &input.page,
        |after, size| {
            let after = after
                .map(|value| Id::new(value.to_owned()).map_err(|_| StorageError::CursorExpired))
                .transpose()?;
            cursor::scan(
                after,
                size,
                |after, batch| {
                    snapshot
                        .github()
                        .github_repositories(&scope, after.as_ref(), batch)
                },
                |repo| Some(repo.id.clone()),
                |repo| {
                    Ok(repo.permission_revision == account.permission_revision
                        && owner.is_none_or(|owner| {
                            repo.owner_kind == "Organization"
                                && repo.owner.eq_ignore_ascii_case(owner)
                        }))
                },
            )
        },
        |repo| repo.id.as_str().to_owned(),
    )?;
    let records = rows.try_into_page(|repo| {
        let registered = registrations
            .get(&repo.id)
            .ok_or(StorageError::ScopeDenied)?;
        let registered = snapshot
            .projects()
            .repository(registered)?
            .ok_or(StorageError::ScopeDenied)?;
        let mut record =
            crate::services::projects::repository_record(snapshot.projects(), ctx, &registered)?;
        record.project_ids = BoundedList(vec![project.clone()]);
        Ok::<_, StorageError>(record)
    })?;
    Ok((
        records,
        membership_revision.max(account.permission_revision),
    ))
}

fn authorized_project(
    ctx: &InvocationContext,
    snapshot: &dyn ReadSnapshot,
) -> Result<ProjectId, Failure> {
    match (reach(snapshot.authority(), ctx)?, ctx.project_id()) {
        (Some(Reach::Project(project)), Some(requested)) if &project == requested => Ok(project),
        (Some(Reach::Project(project)), None) => Ok(project),
        (Some(Reach::Workspace), Some(project)) => Ok(project.clone()),
        _ => Err(refusal(DomainErrorCode::PermissionDenied).into()),
    }
}

fn qualified_account<'a>(
    row: Option<&'a StoredGithubAccount>,
    input: &RepositoriesInput,
    credential_matches: bool,
) -> Result<&'a StoredGithubAccount, Failure> {
    let row = row
        .filter(|row| row.id == input.account_id)
        .ok_or_else(|| refusal(DomainErrorCode::NotFound))?;
    if row.state != "Connected" || !credential_matches {
        return Err(refusal(DomainErrorCode::PermissionDenied).into());
    }
    Ok(row)
}

pub(super) fn github_repository_id(remote_id: &str) -> Id {
    Id::new(format!(
        "github-repo-{}",
        crate::fingerprint::sha256_hex(remote_id.as_bytes())
    ))
    .expect("bounded GitHub identity digest")
}

fn discovered_record(repo: RepositoryIdentity, permission_revision: i64) -> RepositoryRecord {
    RepositoryRecord {
        id: github_repository_id(&repo.remote_id),
        revision: rev(permission_revision),
        name: label(
            &format!("{}/{}", repo.repository.owner, repo.repository.name),
            "repository",
        ),
        remote_url: Some(Url(repo.web_url)),
        project_ids: BoundedList(Vec::new()),
        primary: false,
        lifecycle: if repo.archived || repo.disabled {
            LifeState::Archived
        } else {
            LifeState::Active
        },
        coverage: Coverage {
            state: CoverageState::Unavailable,
            gaps: Vec::new(),
            more_gaps: false,
        },
        indexed_view_ids: BoundedList(Vec::new()),
    }
}

pub(super) fn api_fault(fault: Fault) -> Failure {
    refusal(match fault {
        Fault::CredentialsRequired | Fault::Rejected(401 | 403) => {
            DomainErrorCode::PermissionDenied
        }
        Fault::Rejected(404) => DomainErrorCode::NotFound,
        _ => DomainErrorCode::ExternalFailure,
    })
    .into()
}

fn refusal(code: DomainErrorCode) -> Refusal {
    Refusal::new(code, "github_repositories", None)
}
