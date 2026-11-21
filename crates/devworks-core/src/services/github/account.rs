//! Reconciles the native workspace's GitHub API credential and stores only account facts.
use super::identity::AccountIdentity;
use super::{credentials, issues, publish, reads, repositories, sync};
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{
    OperationInput,
    github::{Input, RepositoriesInput},
    issues::Input as IssuesInput,
};
use crate::ports::storage::{
    ApplicationStore, ConfigScope, ReadSnapshot, StorageError, StoredGithubAccount,
    WriteTransaction,
};
use crate::ports::{
    Clock, CommandHandler, Decided, HandlerFailure, IdKind, IdSource, Preparation, Prepared,
    QueryAnswer, QueryHandler,
};
use crate::protocol::records::{
    AccountAction, GithubAccount, GithubAccountState, ItemState, RepositoryRecord,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, Page};
use crate::protocol::{CorrelationId, DomainErrorCode, DomainFault, Id};
use crate::services::git::github::{Fault, GitHubClient};
use crate::services::kernel::{Failure, Refusal, decided, label, query_fault, rev};
use crate::settings::model::current_layer;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub struct GitHubService {
    pub(super) store: Arc<dyn ApplicationStore>,
    pub(super) client: Arc<GitHubClient>,
    ids: Arc<dyn IdSource>,
    pub(super) clock: Arc<dyn Clock>,
    pub(super) stopping: AtomicBool,
}

struct CheckedAccount {
    settings_revision: i64,
    account_revision: Option<i64>,
    outcome: Result<AccountIdentity, Fault>,
    scopes: Option<Vec<String>>,
}

impl GitHubService {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        client: Arc<GitHubClient>,
        ids: Arc<dyn IdSource>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            client,
            ids,
            clock,
            stopping: AtomicBool::new(false),
        }
    }

    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }
}

pub(super) fn workspace_human(ctx: &InvocationContext) -> bool {
    ctx.principal_kind() == PrincipalKind::Human && ctx.project_id().is_none()
}

fn refusal(code: DomainErrorCode) -> Refusal {
    Refusal::new(code, "github_account", None)
}

fn account_id() -> Id {
    Id::new("github-com").expect("static GitHub account identifier")
}

fn auth_flow(row: &StoredGithubAccount) -> Option<Id> {
    (row.state == "Pending").then(|| {
        Id::new(format!("github-auth-{}", row.revision)).expect("bounded settings revision")
    })
}

fn account_state(state: &str) -> ItemState {
    match state {
        "Connected" => ItemState::Enabled,
        "Pending" => ItemState::Partial,
        "Disconnected" => ItemState::Disabled,
        "ReauthorizationRequired" => ItemState::Failed,
        _ => ItemState::Unavailable,
    }
}

fn projected_account(
    configuration: &dyn crate::ports::storage::ConfigurationRead,
    row: Option<StoredGithubAccount>,
) -> Result<Option<StoredGithubAccount>, StorageError> {
    let mut row = row;
    if let Some(account) = row.as_mut()
        && account.state == "Connected"
        && !credentials::matches_revision(configuration, account.credential_revision)?
    {
        account.state = "ReauthorizationRequired".into();
        account.granted_scopes.clear();
        account.granted_scopes_reported = false;
        account.last_error = Some("The workspace GitHub credential changed".into());
    }
    Ok(row)
}

fn account_result(row: Option<&StoredGithubAccount>) -> OperationResult {
    let state = row.map_or(ItemState::Disabled, |r| account_state(&r.state));
    OperationResult::GithubAccount {
        value: GithubAccountState {
            account: row.and_then(|r| {
                Some(GithubAccount {
                    id: r.id.clone(),
                    login: BoundedText(r.login.clone()?),
                    state: account_state(&r.state),
                    permission_revision: rev(r.permission_revision),
                })
            }),
            auth_flow_id: row.and_then(auth_flow),
            api_host: Some(label("github.com", "GitHub")),
            authorization_state: Some(label(
                row.map_or("Disconnected", |row| row.state.as_str()),
                "Unknown",
            )),
            permission_revision: row.map(|row| rev(row.permission_revision)),
            granted_scopes: row.filter(|row| row.granted_scopes_reported).map(|row| {
                BoundedList(
                    row.granted_scopes
                        .iter()
                        .take(128)
                        .map(|scope| label(scope, "Unknown"))
                        .collect(),
                )
            }),
            failure: row.and_then(|row| row.last_error.clone()).map(BoundedText),
            state,
        },
    }
}

impl QueryHandler for GitHubService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, DomainFault> {
        let external_fault =
            |failure| query_fault(failure, "github_observation", self.correlation());
        let group = match query.input() {
            OperationInput::Github(Input::Repositories(input)) if workspace_human(ctx) => {
                return repositories::prepare(self, ctx, input, env)
                    .map(|prepared| Some(Prepared::new(prepared)))
                    .map_err(|error| external_fault(error.into()));
            }
            OperationInput::Github(Input::Checks(input)) if input.page.cursor.is_none() => (
                reads::ObservationKind::Checks,
                &input.repository_id,
                &input.pull_request_number.0,
                &input.head.hex,
            ),
            OperationInput::Github(Input::Reviews(input)) if input.page.cursor.is_none() => (
                reads::ObservationKind::Reviews,
                &input.repository_id,
                &input.pull_request_number.0,
                &input.head.hex,
            ),
            _ => return Ok(None),
        };
        let (kind, repository, number, head) = group;
        env.read(|snapshot| Ok(reads::project(ctx, snapshot, repository)))
            .map_err(|error| external_fault(error.into()))?
            .map_err(external_fault)?;
        if env.stopped(self.clock.now_ms().get()) {
            return Err(external_fault(StorageError::Cancelled.into()));
        }
        self.poll_pull_evidence(repository, number, head, kind)
            .map_err(external_fault)?;
        Ok(None)
    }

    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let fault = |failure: Failure| query_fault(failure, "github_account", self.correlation());
        let (result, revision) = match query.input() {
            OperationInput::Github(Input::AccountQuery(_)) => {
                if !workspace_human(ctx) {
                    return Err(fault(refusal(DomainErrorCode::PermissionDenied).into()));
                }
                let row = snapshot
                    .github()
                    .github_account()
                    .and_then(|row| projected_account(snapshot.configuration(), row))
                    .map_err(|e| fault(e.into()))?;
                (
                    account_result(row.as_ref()),
                    row.as_ref().map_or(1, |row| row.revision),
                )
            }
            OperationInput::Github(Input::Repositories(input)) => {
                let (value, revision) =
                    repository_page(ctx, query, snapshot, input).map_err(fault)?;
                (OperationResult::GithubRepositories { value }, revision)
            }
            OperationInput::Github(Input::PollStatus(input)) => {
                let (value, revision) = reads::poll_status(
                    ctx,
                    snapshot,
                    &input.repository_id,
                    input.page.as_ref(),
                    query.schema_revision().0,
                )
                .map_err(fault)?;
                (OperationResult::GithubPollStatus { value }, revision)
            }
            OperationInput::Github(Input::Checks(input)) => {
                let (value, revision) = reads::pull_observations(
                    ctx,
                    snapshot,
                    reads::PullObservationQuery {
                        kind: reads::ObservationKind::Checks,
                        repository: &input.repository_id,
                        number: &input.pull_request_number.0,
                        head: &input.head.hex,
                        page: &input.page,
                    },
                )
                .map_err(fault)?;
                (OperationResult::GithubChecks { value }, revision)
            }
            OperationInput::Github(Input::Reviews(input)) => {
                let (value, revision) = reads::pull_observations(
                    ctx,
                    snapshot,
                    reads::PullObservationQuery {
                        kind: reads::ObservationKind::Reviews,
                        repository: &input.repository_id,
                        number: &input.pull_request_number.0,
                        head: &input.head.hex,
                        page: &input.page,
                    },
                )
                .map_err(fault)?;
                (OperationResult::GithubReviews { value }, revision)
            }
            _ => {
                return Err(fault(
                    refusal(DomainErrorCode::UnsupportedCapability).into(),
                ));
            }
        };
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(revision)),
            result,
        })
    }
}

fn repository_page(
    ctx: &InvocationContext,
    query: &AdmittedQuery,
    snapshot: &dyn ReadSnapshot,
    input: &RepositoriesInput,
) -> Result<(Page<RepositoryRecord>, i64), Failure> {
    if !workspace_human(ctx) {
        return repositories::registered_page(ctx, snapshot, input);
    }
    let prepared = query
        .prepared::<repositories::PreparedPage>()
        .ok_or_else(|| Refusal::new(DomainErrorCode::Conflict, "github_repositories", None))?;
    prepared.answer(snapshot)
}

impl CommandHandler for GitHubService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        if let OperationInput::Github(Input::IssueSync(input)) = command.input() {
            return match sync::prepare(ctx, input, env) {
                Err(Failure::Storage(error)) => Err(error),
                outcome => Ok(Some(Prepared::new(outcome))),
            };
        }
        if let OperationInput::Issues(IssuesInput::Publish(input)) = command.input() {
            if command.schema_revision().0 < 3 {
                return Ok(None);
            }
            return match publish::prepare(ctx, input, env) {
                Err(Failure::Storage(error)) => Err(error),
                outcome => Ok(Some(Prepared::new(outcome))),
            };
        }
        if let OperationInput::Issues(IssuesInput::Link(input)) = command.input() {
            if command.schema_revision().0 < 3 {
                return Ok(None);
            }
            return match self.prepare_link(ctx, input, env) {
                Err(Failure::Storage(error)) => Err(error),
                outcome => Ok(Some(Prepared::new(outcome))),
            };
        }
        let OperationInput::Github(Input::AccountCommand(input)) = command.input() else {
            return Ok(None);
        };
        if !workspace_human(ctx)
            || input
                .connection_id
                .as_ref()
                .is_some_and(|id| id != &account_id())
            || !matches!(
                input.action,
                AccountAction::CompleteSignIn | AccountAction::Refresh
            )
        {
            return Ok(None);
        }
        let (settings_revision, account) = env.read(|snapshot| {
            Ok((
                current_layer(snapshot.configuration(), &ConfigScope::Workspace)?.revision,
                snapshot.github().github_account()?,
            ))
        })?;
        if matches!(input.action, AccountAction::CompleteSignIn)
            && !input
                .auth_flow_id
                .as_ref()
                .is_some_and(|id| account.as_ref().and_then(auth_flow).as_ref() == Some(id))
        {
            return Ok(None);
        }
        let result = self
            .client
            .observe_account(&*self.store, self.clock.now_ms().get());
        let (outcome, scopes) = match result {
            Ok(page) => (AccountIdentity::decode(&page), page.control.granted_scopes),
            Err(fault) => (Err(fault), None),
        };
        Ok(Some(Prepared::new(CheckedAccount {
            settings_revision,
            account_revision: account.as_ref().map(|r| r.revision),
            outcome,
            scopes,
        })))
    }

    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        if let OperationInput::Github(Input::IssueSync(input)) = command.input() {
            let outcome = match command.prepared::<Result<sync::CheckedSync, Failure>>() {
                Some(Ok(checked)) => sync::admit(ctx, command, tx, input, checked),
                Some(Err(Failure::Refused(refusal))) => Err(refusal.clone().into()),
                _ => Err(refusal(DomainErrorCode::UnsupportedCapability).into()),
            };
            return match outcome {
                Ok(decision) => Ok(decision),
                Err(error) => decided(tx, Err(error)),
            };
        }
        if let OperationInput::Issues(IssuesInput::Publish(input)) = command.input() {
            if command.schema_revision().0 < 3 {
                return decided(tx, Err(refusal(DomainErrorCode::SchemaMismatch).into()));
            }
            let outcome = match command.prepared::<Result<publish::PreparedPublish, Failure>>() {
                Some(Ok(prepared)) => publish::admit(ctx, command, tx, input, prepared),
                Some(Err(Failure::Refused(refusal))) => Err(refusal.clone().into()),
                _ => Err(refusal(DomainErrorCode::UnsupportedCapability).into()),
            };
            return match outcome {
                Ok(decision) => Ok(decision),
                Err(error) => decided(tx, Err(error)),
            };
        }
        if let OperationInput::Issues(IssuesInput::Link(input)) = command.input() {
            if command.schema_revision().0 < 3 {
                return decided(tx, Err(refusal(DomainErrorCode::SchemaMismatch).into()));
            }
            let outcome = match command.prepared::<Result<issues::CheckedLink, Failure>>() {
                Some(Ok(checked)) => issues::link(input, checked, tx),
                Some(Err(Failure::Refused(refusal))) => Err(refusal.clone().into()),
                _ => Err(refusal(DomainErrorCode::UnsupportedCapability).into()),
            };
            return decided(tx, outcome);
        }
        let OperationInput::Github(Input::AccountCommand(input)) = command.input() else {
            return decided(
                tx,
                Err(refusal(DomainErrorCode::UnsupportedCapability).into()),
            );
        };
        if !workspace_human(ctx) {
            return decided(tx, Err(refusal(DomainErrorCode::PermissionDenied).into()));
        }
        let current = tx.github().github_account()?;
        if input
            .connection_id
            .as_ref()
            .is_some_and(|id| id != &account_id())
        {
            return decided(tx, Err(refusal(DomainErrorCode::NotFound).into()));
        }
        if matches!(input.action, AccountAction::Inspect) {
            let projected = projected_account(tx.configuration(), current)?;
            return decided(
                tx,
                Ok(DomainDecision::NoChange {
                    result: account_result(projected.as_ref()),
                }),
            );
        }
        let mut row = current.clone().unwrap_or_else(|| StoredGithubAccount {
            id: account_id(),
            revision: 0,
            state: "Disconnected".into(),
            remote_user_id: None,
            login: None,
            credential_revision: 1,
            permission_revision: 1,
            granted_scopes: Vec::new(),
            granted_scopes_reported: false,
            observed_at_ms: 0,
            last_error: None,
        });
        match input.action {
            AccountAction::BeginSignIn => {
                let effective =
                    projected_account(tx.configuration(), Some(row.clone()))?.expect("Some row");
                if effective.state != "Disconnected" && effective.state != "ReauthorizationRequired"
                {
                    return decided(tx, Err(refusal(DomainErrorCode::Conflict).into()));
                }
                row.state = "Pending".into();
                row.last_error = None;
            }
            AccountAction::Disconnect => {
                if row.state == "Disconnected" {
                    return decided(
                        tx,
                        Ok(DomainDecision::NoChange {
                            result: account_result(current.as_ref()),
                        }),
                    );
                }
                row.state = "Disconnected".into();
                row.remote_user_id = None;
                row.login = None;
                row.granted_scopes.clear();
                row.granted_scopes_reported = false;
                row.last_error = None;
                row.permission_revision += 1;
            }
            AccountAction::CompleteSignIn | AccountAction::Refresh => {
                let Some(checked) = command.prepared::<CheckedAccount>() else {
                    return decided(tx, Err(refusal(DomainErrorCode::Conflict).into()));
                };
                let settings_revision =
                    current_layer(tx.configuration(), &ConfigScope::Workspace)?.revision;
                if settings_revision != checked.settings_revision
                    || checked.account_revision != current.as_ref().map(|r| r.revision)
                {
                    return decided(tx, Err(refusal(DomainErrorCode::Conflict).into()));
                }
                let same_credential =
                    credentials::matches_revision(tx.configuration(), row.credential_revision)?;
                update_checked(&mut row, checked, same_credential);
            }
            AccountAction::Inspect => unreachable!("Inspect returned above"),
        }
        row.revision += 1;
        row.observed_at_ms = tx.now_ms();
        tx.github()
            .put_github_account(&row, current.map(|r| r.revision))?;
        decided(
            tx,
            Ok(DomainDecision::Applied {
                result: account_result(Some(&row)),
            }),
        )
    }
}

fn update_checked(row: &mut StoredGithubAccount, checked: &CheckedAccount, same_credential: bool) {
    let previous_state = row.state.clone();
    let previous_revision = row.credential_revision;
    let previous_user = row.remote_user_id.clone();
    let previous_scopes = row.granted_scopes.clone();
    let previous_scopes_reported = row.granted_scopes_reported;
    if !same_credential {
        row.credential_revision = checked.settings_revision;
    }
    match &checked.outcome {
        Ok(identity)
            if row
                .remote_user_id
                .as_ref()
                .is_some_and(|id| id != &identity.remote_id) =>
        {
            row.state = "ReauthorizationRequired".into();
            row.granted_scopes.clear();
            row.granted_scopes_reported = false;
            row.last_error = Some(
                "Disconnect the current GitHub account before connecting another account".into(),
            );
        }
        Ok(identity) => {
            row.state = "Connected".into();
            row.remote_user_id = Some(identity.remote_id.clone());
            row.login = Some(identity.login.clone());
            row.granted_scopes = checked.scopes.clone().unwrap_or_default();
            row.granted_scopes_reported = checked.scopes.is_some();
            row.last_error = None;
        }
        Err(Fault::CredentialsRequired | Fault::Rejected(401)) => {
            row.state = "ReauthorizationRequired".into();
            row.granted_scopes.clear();
            row.granted_scopes_reported = false;
            row.last_error = Some("GitHub API authorization is required".into());
        }
        Err(_) => {
            row.state = "Unavailable".into();
            row.granted_scopes.clear();
            row.granted_scopes_reported = false;
            row.last_error = Some("GitHub API account observation failed".into());
        }
    }
    if row.state != previous_state
        || row.credential_revision != previous_revision
        || row.remote_user_id != previous_user
        || row.granted_scopes != previous_scopes
        || row.granted_scopes_reported != previous_scopes_reported
    {
        row.permission_revision += 1;
    }
}
