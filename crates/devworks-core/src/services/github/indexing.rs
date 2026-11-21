//! Converts a committed configured-ref observation into an ordinary, exact-source indexing run.
use super::{credentials, references, schedule};
use crate::ports::storage::{
    ACTIVE, EventRecord, RUN_INDEX, ReadSnapshot, StorageError, StoredGithubIndexTarget,
    WORKSPACE_SCOPE_KEY, WriteTransaction,
};
use crate::protocol::Id;
use crate::services::indexing::{
    IndexEnv, RUN_AGGREGATE, discover, effective_config_tx, history, new_run, section_config,
};
use serde_json::{Value, json};

pub enum IndexDelivery {
    Settled,
    Busy,
}

pub fn deliver(
    tx: &mut dyn WriteTransaction,
    index: &IndexEnv,
    event: &EventRecord,
) -> Result<IndexDelivery, StorageError> {
    if event.kind != "GithubRefChanged" {
        return Err(invalid());
    }
    let local = body_id(&event.payload, "repositoryId")?;
    let canonical = body_id(&event.payload, "githubRepositoryId")?;
    let reference = event.payload["ref"].as_str().ok_or_else(invalid)?;
    let branch = reference.strip_prefix("refs/heads/").ok_or_else(invalid)?;
    let head = event.payload["head"].as_str().ok_or_else(invalid)?;
    if !valid_oid(head) {
        return Err(invalid());
    }
    let Some(local_repo) = tx
        .projects()
        .repository(&local)?
        .filter(|repo| repo.lifecycle == ACTIVE)
    else {
        return Ok(IndexDelivery::Settled);
    };
    if !schedule::active_policy(tx, &local)?.changes {
        return Ok(IndexDelivery::Settled);
    }
    let Some(binding) = tx
        .github()
        .github_repository_binding(&local)?
        .filter(|binding| binding.state == "Active" && binding.github_repository_id == canonical)
    else {
        return Ok(IndexDelivery::Settled);
    };
    let Some(account) = tx.github().github_account()? else {
        return Err(StorageError::ScopeDenied);
    };
    if !credentials::account_is_current(tx.configuration(), &account)?
        || !tx
            .github()
            .github_discovered_repository(&canonical)?
            .is_some_and(|repo| {
                repo.account_id == account.id
                    && repo.permission_revision == account.permission_revision
                    && repo.default_branch.as_deref() == Some(branch)
            })
    {
        return Err(StorageError::ScopeDenied);
    }
    // Coalesce queued notifications from an endpoint that already observed a newer head.
    if !tx
        .github()
        .github_poll_cursor(&canonical, &references::endpoint_key(branch))?
        .and_then(|cursor| cursor.watermark)
        .is_some_and(|state| state["oid"].as_str() == Some(head))
    {
        return Ok(IndexDelivery::Settled);
    }
    if let Some(published) = tx.indexing().published(&local)?
        && tx
            .projects()
            .index_view(&published.view_id)?
            .is_some_and(|view| view.source_revision == head)
    {
        return Ok(IndexDelivery::Settled);
    }
    if tx.indexing().active_run(&local, None)?.is_some() {
        return Ok(IndexDelivery::Busy);
    }
    let config = effective_config_tx(tx, &local)?;
    if config.repository_excluded(&local_repo.name, &discover::group_path(tx, &local_repo)?) {
        return Ok(IndexDelivery::Settled);
    }
    let revision = section_config(tx.indexing(), WORKSPACE_SCOPE_KEY)?.revision;
    let owner = tx.store().host_actor.clone();
    let run = new_run(
        tx,
        RUN_INDEX,
        &local,
        None,
        None,
        &config,
        revision,
        &owner,
        index.settings.max_attempts,
    );
    tx.indexing().insert_run(&run)?;
    tx.github()
        .put_github_index_target(&StoredGithubIndexTarget {
            run_id: run.id.clone(),
            repository_id: local.clone(),
            github_repository_id: canonical,
            repository_revision: local_repo.revision,
            registered_remote: binding.registered_remote,
            reference: reference.into(),
            head: head.into(),
            source_event: event.id.clone(),
        })?;
    history(
        tx,
        &owner,
        None,
        (RUN_AGGREGATE, &run.id, run.revision),
        "IndexRunQueued",
        json!({
            "kind":RUN_INDEX, "repositoryId":local, "configRevision":revision,
            "configDigest":config.digest, "principalKind":"Host", "sourceEventId":event.id,
            "sourceRef":reference, "sourceHead":head,
        }),
    )?;
    Ok(IndexDelivery::Settled)
}

pub fn target_is_current(
    snapshot: &dyn ReadSnapshot,
    target: &StoredGithubIndexTarget,
) -> Result<bool, StorageError> {
    let Some(qualified) = super::issue_poll::target(snapshot, &target.repository_id)? else {
        return Ok(false);
    };
    Ok(qualified.repository.id == target.github_repository_id
        && qualified.binding.registered_remote == target.registered_remote
        && snapshot
            .projects()
            .repository(&target.repository_id)?
            .is_some_and(|row| {
                row.revision == target.repository_revision && row.lifecycle == ACTIVE
            }))
}

pub fn valid_oid(oid: &str) -> bool {
    oid.len() == 40
        && oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn body_id(payload: &Value, field: &str) -> Result<Id, StorageError> {
    let value = payload[field].as_str().ok_or_else(invalid)?;
    Id::new(value.to_owned()).map_err(|_| invalid())
}

fn invalid() -> StorageError {
    StorageError::InvalidRecord("Invalid GitHub index reference observation".into())
}
