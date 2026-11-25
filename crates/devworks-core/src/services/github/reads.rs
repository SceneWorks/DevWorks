//! Scoped views of durable GitHub facts. A remote account permission never grants project scope.
use super::{issue_poll, references};
use crate::authority::reach::{Reach, reach};
use crate::context::{InvocationContext, PrincipalKind};
use crate::ports::storage::{
    GithubIntakeFilter, GithubIssueKey, GithubObservationFilter, GithubScope, ReadSnapshot,
    StorageError, StoredGithubObservation, StoredGithubRepository,
};
use crate::protocol::records::{
    GithubIssueIntakePage, GithubIssueIntakeView, GithubObservation, GithubPollEndpoint,
    GithubPollStatusResult, GithubPollStatusView, PageRequest,
};
use crate::protocol::support::{BoundedList, BoundedText, Limit, Page, PositiveCounter, Url};
use crate::protocol::{Counter, Id, OperationState, ProjectId};
use crate::services::cursor::{self, CursorBinding};
use crate::services::kernel::{Failure, label, not_found};

#[derive(Clone, Copy)]
pub(super) enum ObservationKind {
    Checks,
    Reviews,
}

impl ObservationKind {
    fn operation(self) -> &'static str {
        match self {
            Self::Checks => "github.checks",
            Self::Reviews => "github.reviews",
        }
    }

    fn kinds(self) -> Vec<String> {
        match self {
            Self::Checks => ["CheckRun", "CheckSuite", "WorkflowRun", "CommitStatus"]
                .map(str::to_owned)
                .to_vec(),
            Self::Reviews => vec!["Review".into(), "ReviewComment".into()],
        }
    }
}

pub(super) fn project(
    ctx: &InvocationContext,
    snapshot: &dyn ReadSnapshot,
    repository: &Id,
) -> Result<Option<ProjectId>, Failure> {
    let bound = reach(snapshot.authority(), ctx)?;
    let project = match (bound, ctx.project_id()) {
        (Some(Reach::Project(project)), Some(current)) if &project == current => Some(project),
        (Some(Reach::Project(project)), None) => Some(project),
        (Some(Reach::Workspace), Some(project)) => Some(project.clone()),
        (Some(Reach::Workspace), None)
            if matches!(
                ctx.principal_kind(),
                PrincipalKind::Human | PrincipalKind::Host
            ) =>
        {
            None
        }
        _ => return Err(not_found("github_repository", repository).into()),
    };
    if let Some(project) = &project
        && !snapshot.projects().members(project)?.contains(repository)
    {
        return Err(not_found("github_repository", repository).into());
    }
    Ok(project)
}

fn qualified(
    ctx: &InvocationContext,
    snapshot: &dyn ReadSnapshot,
    repository: &Id,
) -> Result<(GithubScope, StoredGithubRepository), Failure> {
    let unavailable = || not_found("github_repository", repository);
    let project_id = project(ctx, snapshot, repository)?;
    let target = issue_poll::target(snapshot, repository)?.ok_or_else(unavailable)?;
    let remote = target.repository;
    Ok((
        GithubScope {
            repositories: vec![repository.clone()],
            project_id,
        },
        remote,
    ))
}

pub(super) struct PullObservationQuery<'a> {
    pub kind: ObservationKind,
    pub repository: &'a Id,
    pub number: &'a str,
    pub head: &'a str,
    pub page: &'a PageRequest,
}

pub(super) fn pull_observations(
    ctx: &InvocationContext,
    snapshot: &dyn ReadSnapshot,
    input: PullObservationQuery<'_>,
) -> Result<(Page<GithubObservation>, i64), Failure> {
    let (scope, remote) = qualified(ctx, snapshot, input.repository)?;
    let authorization = remote.permission_revision.to_string();
    let binding = CursorBinding::new(
        ctx,
        input.kind.operation(),
        &[
            input.repository.as_str(),
            remote.id.as_str(),
            &authorization,
            input.number,
            input.head,
        ],
        remote.revision,
    );
    let filter = GithubObservationFilter {
        scope,
        kinds: input.kind.kinds(),
        subject_id: Some(input.number.to_owned()),
        head: Some(input.head.to_owned()),
    };
    let page = cursor::page(
        &binding,
        input.page,
        |after, limit| {
            let after = after
                .map(|id| Id::new(id.to_owned()).map_err(|_| StorageError::CursorExpired))
                .transpose()?;
            snapshot
                .github()
                .github_observations(&filter, after.as_ref(), limit)
        },
        |row| row.id.as_str().to_owned(),
    )?;
    Ok((
        page.try_into_page(|row| observation(input.repository, row))?,
        remote.revision,
    ))
}

pub(super) fn poll_status(
    ctx: &InvocationContext,
    snapshot: &dyn ReadSnapshot,
    repository: &Id,
    request: Option<&PageRequest>,
    schema_revision: u32,
) -> Result<(GithubPollStatusResult, i64), Failure> {
    let (scope, remote) = qualified(ctx, snapshot, repository)?;
    if schema_revision < 3 {
        let (page, revision) = legacy_poll_status(snapshot, repository, scope, remote)?;
        return Ok((GithubPollStatusResult::Legacy(page), revision));
    }
    let scope_revision = match scope.project_id.as_ref() {
        Some(project) => {
            snapshot
                .projects()
                .project(project)?
                .filter(|row| row.lifecycle == "Active")
                .ok_or_else(|| not_found("github_repository", repository))?
                .scope_revision
        }
        None => 0,
    };
    let binding = snapshot
        .github()
        .github_repository_binding(repository)?
        .ok_or_else(|| not_found("github_repository", repository))?;
    let cursor_binding = CursorBinding::new(
        ctx,
        "github.poll_status",
        &[
            repository.as_str(),
            remote.id.as_str(),
            remote.account_id.as_str(),
            &remote.permission_revision.to_string(),
            &binding.revision.to_string(),
            scope.project_id.as_ref().map_or("", |id| id.0.as_str()),
        ],
        scope_revision,
    );
    let request = PageRequest {
        limit: Limit::new(request.map_or(50, |page| page.limit.get().min(100)))
            .expect("GitHub status page limit"),
        cursor: request.and_then(|page| page.cursor.clone()),
    };
    let filter = GithubIntakeFilter {
        scope,
        states: Vec::new(),
    };
    let page = cursor::page(
        &cursor_binding,
        &request,
        |after, limit| {
            let key = after
                .map(|after| {
                    let number = after
                        .parse::<i64>()
                        .ok()
                        .filter(|number| *number > 0)
                        .ok_or(StorageError::CursorExpired)?;
                    Ok::<_, StorageError>(GithubIssueKey {
                        repository_id: remote.id.clone(),
                        number,
                    })
                })
                .transpose()?;
            snapshot
                .github()
                .github_issues(&filter, key.as_ref(), limit)
        },
        |row| row.key.number.to_string(),
    )?;
    let intake = page.try_into_page(|issue| {
        let observation = snapshot
            .github()
            .github_observation(&issue.observation_id, &filter.scope)?
            .ok_or_else(|| {
                StorageError::InvalidRecord("Missing GitHub issue observation".into())
            })?;
        let (comment_state, close_state) = resolution_state(snapshot, &filter.scope, &issue.key)?;
        Ok::<_, StorageError>(GithubIssueIntakeView {
            remote_issue_number: PositiveCounter(issue.key.number.to_string()),
            state: label(&issue.state, "Unknown"),
            url: Url(observation.url),
            source_revision: BoundedText(issue.remote_revision),
            work_id: snapshot
                .github()
                .github_work_links(&issue.key, filter.scope.project_id.as_ref(), None, 1)?
                .into_iter()
                .next()
                .map(|link| link.work_id)
                .or(issue.work_id),
            comment_state,
            close_state,
        })
    })?;
    let mut endpoint_keys = vec![
        ("issues", "issues".to_owned()),
        ("pull requests", super::pulls::ENDPOINT.to_owned()),
    ];
    if let Some(branch) = &remote.default_branch {
        endpoint_keys.push(("default ref", references::endpoint_key(branch)));
    }
    let mut revision = remote.revision;
    let mut endpoints = Vec::new();
    for (name, key) in endpoint_keys {
        if let Some(poll) = snapshot.github().github_poll_cursor(&remote.id, &key)? {
            revision = revision.max(poll.revision);
            endpoints.push(GithubPollEndpoint {
                endpoint: label(name, "GitHub"),
                state: label(
                    if poll.last_error.is_some() {
                        "Backoff"
                    } else if poll.bootstrap_complete {
                        "Watching"
                    } else {
                        "Initializing"
                    },
                    "Initializing",
                ),
                next_poll_at_ms: Counter(poll.next_poll_at_ms.to_string()),
                last_polled_at_ms: poll.last_polled_at_ms.map(|time| Counter(time.to_string())),
                failure: poll.last_error.map(BoundedText),
            });
        }
    }
    let clock = snapshot.github().github_intake_clock()?;
    Ok((
        GithubPollStatusResult::Current(GithubPollStatusView {
            repository_id: repository.clone(),
            endpoints: BoundedList(endpoints),
            intake: GithubIssueIntakePage {
                items: intake.items,
                next_cursor: intake.next_cursor,
                has_more: intake.has_more,
            },
            interval_seconds: PositiveCounter(clock.interval_seconds.to_string()),
            next_admission_at_ms: Counter(clock.next_admission_at_ms.to_string()),
            last_admitted_at_ms: clock
                .last_admitted_at_ms
                .map(|time| Counter(time.to_string())),
        }),
        revision.max(clock.revision),
    ))
}

pub(super) fn status_scope_revision(
    github: &dyn crate::ports::storage::GithubRead,
    remote: &crate::ports::storage::StoredGithubRepository,
) -> Result<i64, StorageError> {
    let mut keys = vec!["issues".to_owned(), super::pulls::ENDPOINT.to_owned()];
    if let Some(branch) = &remote.default_branch {
        keys.push(references::endpoint_key(branch));
    }
    let mut rev = remote.revision;
    for key in keys {
        if let Some(poll) = github.github_poll_cursor(&remote.id, &key)? {
            rev = rev.max(poll.revision);
        }
    }
    Ok(rev.max(github.github_intake_clock()?.revision))
}

fn resolution_state(
    snapshot: &dyn ReadSnapshot,
    scope: &GithubScope,
    issue: &GithubIssueKey,
) -> Result<(Option<OperationState>, Option<OperationState>), StorageError> {
    let mut after = None;
    let mut latest: Option<(i64, OperationState, Option<OperationState>)> = None;
    const PAGE: u16 = 32;
    loop {
        let page = snapshot
            .github()
            .github_resolutions(issue, after.as_ref(), PAGE)?;
        for row in &page {
            if let Some(project) = &scope.project_id
                && snapshot
                    .github()
                    .github_resolution_witnesses(&row.id, Some(project), None, 1)?
                    .is_empty()
            {
                continue;
            }
            if let Some(comment) = snapshot.operations().operation(&row.comment_operation_id)?
                && latest
                    .as_ref()
                    .is_none_or(|(time, ..)| comment.created_at_ms >= *time)
            {
                let close = snapshot.operations().operation(&row.close_operation_id)?;
                latest = Some((
                    comment.created_at_ms,
                    comment.state,
                    close.map(|close| close.state),
                ));
            }
        }
        if page.len() < usize::from(PAGE) {
            return Ok(latest.map_or((None, None), |(_, comment, close)| (Some(comment), close)));
        }
        after = page.last().map(|row| row.id.clone());
    }
}

fn legacy_poll_status(
    snapshot: &dyn ReadSnapshot,
    repository: &Id,
    scope: GithubScope,
    remote: StoredGithubRepository,
) -> Result<(Page<GithubObservation>, i64), Failure> {
    let mut rows = Vec::new();
    let cursor = snapshot.github().github_poll_cursor(&remote.id, "issues")?;
    if let Some(cursor) = &cursor {
        rows.push(GithubObservation {
            repository_id: repository.clone(),
            subject_id: Id::new("issues").expect("static endpoint ID"),
            kind: label("Issue poll", "poll"),
            state: label(
                cursor
                    .last_error
                    .as_deref()
                    .unwrap_or(if cursor.bootstrap_complete {
                        "Watching"
                    } else {
                        "Importing"
                    }),
                "Watching",
            ),
            revision: BoundedText(cursor.revision.to_string()),
            url: Some(Url(remote.url.clone())),
            artifact: None,
        });
    }
    for issue in snapshot.github().github_issues(
        &GithubIntakeFilter {
            scope: scope.clone(),
            states: Vec::new(),
        },
        None,
        499,
    )? {
        let Some(row) = snapshot
            .github()
            .github_observation(&issue.observation_id, &scope)?
        else {
            continue;
        };
        let mut wire = observation(repository, row)?;
        wire.kind = label("Issue intake", "Issue");
        wire.state = label(&issue.state, "Unknown");
        rows.push(wire);
    }
    Ok((
        Page {
            items: BoundedList(rows),
            next_cursor: None,
            has_more: false,
        },
        cursor.map_or(remote.revision, |cursor| {
            cursor.revision.max(remote.revision)
        }),
    ))
}

fn observation(
    repository: &Id,
    row: StoredGithubObservation,
) -> Result<GithubObservation, StorageError> {
    let subject = row
        .payload
        .get("resourceId")
        .and_then(|id| id.as_str())
        .unwrap_or(&row.subject_id);
    let subject_id = Id::new(subject.to_owned()).map_err(|_| {
        StorageError::InvalidRecord("GitHub observation subject cannot be projected".into())
    })?;
    let state = row.payload["state"].as_str().unwrap_or("Observed");
    Ok(GithubObservation {
        repository_id: repository.clone(),
        subject_id,
        kind: label(&row.kind, "GitHub"),
        state: label(state, "Observed"),
        revision: BoundedText(row.remote_revision),
        url: Some(Url(row.url)),
        artifact: None,
    })
}
