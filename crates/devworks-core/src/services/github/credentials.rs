//! The workspace secret reference shared by native GitHub API operations.
use crate::authority::redact::SecretRef;
use crate::ports::storage::{ConfigScope, ConfigurationRead, StorageError};
use crate::settings::catalog::descriptor;
use crate::settings::model::{Layer, chain, current_layer, layer_at};

pub(crate) const KEY: &str = "github.credential";

pub(crate) fn current(
    read: &dyn ConfigurationRead,
    verified_revision: Option<i64>,
) -> Result<Option<SecretRef>, StorageError> {
    let layer = current_layer(read, &ConfigScope::Workspace)?;
    let revision = layer.revision;
    let current = reference(layer)?;
    let Some(verified) = verified_revision.filter(|verified| *verified != revision) else {
        return Ok(current);
    };
    let Some(captured) = layer_at(read, &ConfigScope::Workspace, verified)? else {
        return Ok(None);
    };
    let verified = reference(captured)?;
    Ok(current.filter(|credential| verified.as_ref() == Some(credential)))
}

pub(crate) fn matches_revision(
    read: &dyn ConfigurationRead,
    revision: i64,
) -> Result<bool, StorageError> {
    Ok(current(read, Some(revision))?.is_some())
}

pub(crate) fn account_is_current(
    read: &dyn ConfigurationRead,
    account: &crate::ports::storage::StoredGithubAccount,
) -> Result<bool, StorageError> {
    Ok(account.state == "Connected" && matches_revision(read, account.credential_revision)?)
}

fn reference(layer: Layer) -> Result<Option<SecretRef>, StorageError> {
    let descriptor = descriptor(KEY).expect("catalog GitHub credential");
    let entries = chain(descriptor, &[layer])
        .map_err(|_| StorageError::InvalidRecord("GitHub credential configuration".into()))?;
    Ok(entries
        .into_iter()
        .next()
        .and_then(|entry| SecretRef::from_setting(&entry.value.value.0)))
}
