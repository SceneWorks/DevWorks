//! Associate a native registration with an exact, authenticated GitHub identity.
use super::identity::RepositoryIdentity;
use super::{GitHubService, credentials};
use crate::fingerprint::sha256_hex;
use crate::ports::IdKind;
use crate::ports::storage::{
    ReadOptions, StorageError, StoredGithubAccount, StoredGithubObservation,
    StoredGithubRepository, StoredGithubRepositoryBinding, StoredRepository, WriteOptions,
    WriteTransaction, read_snapshot, run_write,
};
use crate::protocol::{DomainErrorCode, Id};
use crate::services::git::github::Repository;
use crate::services::kernel::{Failure, Refusal, not_found};
use serde_json::{Value, json};

impl GitHubService {
    /// Qualifies one host-selected native registration. Publication rechecks the repository,
    /// workspace credential and current account after the external read returns.
    pub fn qualify_registration(
        &self,
        repository_id: &Id,
    ) -> Result<StoredGithubRepositoryBinding, Failure> {
        let (registration, account) =
            read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
                let repo = snapshot.projects().repository(repository_id)?;
                let account = match snapshot.github().github_account()? {
                    Some(account)
                        if credentials::account_is_current(snapshot.configuration(), &account)? =>
                    {
                        Some(account)
                    }
                    _ => None,
                };
                Ok((repo, account))
            })?;
        let registration = registration
            .filter(|repo| repo.lifecycle == "Active")
            .ok_or_else(|| not_found("repository", repository_id))?;
        let account = account.ok_or_else(|| {
            Refusal::new(DomainErrorCode::PermissionDenied, "github_account", None)
        })?;
        let requested = registration
            .remote_url
            .as_deref()
            .and_then(Repository::from_remote)
            .ok_or_else(|| {
                Refusal::new(
                    DomainErrorCode::UnsupportedTarget,
                    "github_repository",
                    Some(repository_id.clone()),
                )
            })?;
        let observed = self
            .client
            .observe_repository(&*self.store, &requested, self.clock.now_ms().get())
            .map_err(super::repositories::api_fault)?;
        run_write(&*self.store, WriteOptions::observed(), move |tx| {
            bind(tx, &registration, &account, observed)
        })
        .map_err(Into::into)
    }
}

fn bind(
    tx: &mut dyn WriteTransaction,
    registration: &StoredRepository,
    account: &StoredGithubAccount,
    observed: RepositoryIdentity,
) -> Result<StoredGithubRepositoryBinding, StorageError> {
    let registered_remote = registration
        .remote_url
        .as_ref()
        .ok_or(StorageError::ScopeDenied)?;
    let current = tx.projects().repository(&registration.id)?;
    if current.as_ref().is_none_or(|current| {
        current.revision != registration.revision
            || current.lifecycle != "Active"
            || current.remote_url.as_ref() != Some(registered_remote)
    }) || tx.github().github_account()?.as_ref() != Some(account)
        || !credentials::account_is_current(tx.configuration(), account)?
    {
        return Err(StorageError::ScopeDenied);
    }
    let previous = tx
        .github()
        .github_repository_by_remote_id(&observed.remote_id)?;
    let before = tx.github().github_repository_binding(&registration.id)?;
    let canonical_id = previous.as_ref().map_or_else(
        || super::repositories::github_repository_id(&observed.remote_id),
        |repo| repo.id.clone(),
    );
    if before.as_ref().is_some_and(|binding| {
        &binding.registered_remote == registered_remote
            && binding.github_repository_id != canonical_id
    }) {
        return Err(StorageError::InvalidRecord(
            "GitHub repository identity changed for the registered remote".into(),
        ));
    }
    let now = tx.now_ms();
    let remote = StoredGithubRepository {
        id: canonical_id.clone(),
        account_id: account.id.clone(),
        revision: previous.as_ref().map_or(1, |repo| repo.revision + 1),
        remote_repository_id: observed.remote_id.clone(),
        owner: observed.repository.owner,
        name: observed.repository.name,
        owner_kind: observed.owner_kind,
        url: observed.web_url,
        default_branch: Some(observed.default_branch),
        permissions: Value::Object(observed.permissions),
        permission_revision: account.permission_revision,
        observed_at_ms: now,
    };
    tx.github()
        .put_github_repository(&remote, previous.map(|repo| repo.revision))?;
    // Retain only identity and permission evidence. Do not store the raw API document,
    // which can also contain transient clone credentials and unrelated fields.
    let body = json!({
        "requestedRemote": registered_remote,
        "repository": {
            "id": observed.numeric_id.parse::<u64>().map_err(|_| StorageError::InvalidRecord(
                "GitHub repository has an invalid numeric identity".into()
            ))?,
            "node_id": observed.remote_id,
            "owner": { "login": remote.owner, "type": remote.owner_kind },
            "name": remote.name,
            "default_branch": remote.default_branch,
            "permissions": remote.permissions,
            "archived": observed.archived,
            "disabled": observed.disabled
        },
        "permissionRevision": account.permission_revision
    });
    let observation = StoredGithubObservation {
        id: tx.new_id(IdKind::Event),
        repository_id: canonical_id.clone(),
        kind: "Repository".into(),
        subject_id: remote.remote_repository_id,
        remote_revision: sha256_hex(body.to_string().as_bytes()),
        url: remote.url,
        head: None,
        payload: body,
        source_updated_at_ms: None,
        observed_at_ms: now,
    };
    let observation = tx.github().append_github_observation(&observation)?;
    let binding = StoredGithubRepositoryBinding {
        repository_id: registration.id.clone(),
        github_repository_id: canonical_id,
        revision: before.as_ref().map_or(1, |binding| binding.revision + 1),
        repository_revision: registration.revision,
        registered_remote: registered_remote.clone(),
        observation_id: observation.id,
        state: "Active".into(),
    };
    tx.github()
        .put_github_repository_binding(&binding, before.map(|binding| binding.revision))?;
    Ok(binding)
}
