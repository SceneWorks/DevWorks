//! OAuth refresh custody. Only short native publication units hold this fence; HTTP and
//! keychain calls run outside it. A closed application never publishes a late credential.
use super::*;
use crate::ports::storage::{AttemptBinding, AttemptFence, NewOperation, OperationRecord};
use crate::protocol::{OperationId, OperationState};
use std::sync::RwLock;
#[derive(Default)]
pub(super) struct Lifecycle {
    closed: RwLock<bool>,
    owners: Mutex<BTreeMap<String, RefreshOwner>>,
}
#[derive(Clone)]
pub(super) struct RefreshOwner {
    pub operation: OperationId,
    pub handle: Id,
    pub fence: AttemptFence,
    connection: Id,
    credential_revision: i64,
    pub account_revision: i64,
    profile: String,
    endpoint_digest: String,
}
#[derive(Clone, Debug, Default)]
pub struct RefreshCloseReport {
    pub unknown_operations: Vec<OperationId>,
    /// Already-started OS writes cannot be atomically cancelled. These known handles remain
    /// unpublished; exact-handle cleanup and namespace reconciliation may remove their orphans.
    pub unpublished_handles: Vec<Id>,
}
impl ConnectionService {
    pub fn is_closed(&self) -> bool {
        *self
            .lifecycle
            .closed
            .read()
            .unwrap_or_else(|p| p.into_inner())
    }
    pub(super) fn publication<T, F>(&self, work: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
    {
        let closed = self
            .lifecycle
            .closed
            .read()
            .unwrap_or_else(|p| p.into_inner());
        if *closed {
            return Err(StorageError::Cancelled);
        }
        let result = run_write(self.store.as_ref(), WriteOptions::default(), work);
        drop(closed);
        result
    }
    pub(super) fn register_stage(
        &self,
        handle: &Id,
        principal: &ActorId,
        api_key: bool,
    ) -> Result<(), SecretError> {
        let closed = self
            .lifecycle
            .closed
            .read()
            .unwrap_or_else(|p| p.into_inner());
        if *closed {
            return Err(SecretError::Failed(
                "provider connection owner closed".into(),
            ));
        }
        self.live().staged.insert(
            handle.as_str().into(),
            Staged {
                principal: principal.clone(),
                api_key,
                created_at_ms: self.now(),
            },
        );
        Ok(())
    }
    pub(super) fn begin_refresh(
        &self,
        seen: &StoredConnection,
    ) -> Result<RefreshOwner, StorageError> {
        let closed = self
            .lifecycle
            .closed
            .read()
            .unwrap_or_else(|p| p.into_inner());
        if *closed {
            return Err(StorageError::Cancelled);
        }
        let handle = self.ids.next_id(IdKind::Credential);
        let candidate = handle.clone();
        let before = seen.clone();
        let owner = run_write(self.store.as_ref(), WriteOptions::default(), move |tx| {
            let mut current = tx
                .router()
                .connection(&before.id)?
                .ok_or(StorageError::ScopeDenied)?;
            if current.credential_revision != before.credential_revision
                || current.account_revision != before.account_revision
                || current.profile != before.profile
                || current.endpoint != before.endpoint
            {
                return Err(StorageError::Busy);
            }
            if let Some(prior) = &current.refresh_operation_id
                && let Some(op) = tx.operations().operation(prior)?
                && op.payload["credentialRevision"].as_i64() == Some(current.credential_revision)
                && (op.state == OperationState::Dispatching || interrupted(tx, &op))
            {
                return Err(StorageError::Busy);
            }
            let payload = json!({"connectionId":current.id,"credentialRevision":current.credential_revision,"accountRevision":current.account_revision,"profile":current.profile,"endpointDigest":crate::fingerprint::sha256_hex(current.endpoint.as_bytes()),"allocatedHandle":candidate});
            let id = OperationId(tx.new_id(IdKind::Operation));
            let op = tx.operations().record_intent(NewOperation {
                id: id.clone(),
                root_id: None,
                assignment_id: None,
                capability: "router.refresh_access".into(),
                schema_version: 1,
                args_digest: crate::fingerprint::sha256_hex(payload.to_string().as_bytes()),
                payload,
            })?;
            let attempt =
                tx.operations()
                    .begin_attempt(&id, op.revision, AttemptBinding::default())?;
            tx.operations().transition(
                &id,
                op.revision + 1,
                Some(&attempt.fence()),
                OperationState::Dispatching,
                Some(json!({"credentialPublication":"Pending","externalSecretWrite":"NotStarted"})),
            )?;
            let expected = current.revision;
            current.refresh_operation_id = Some(id.clone());
            current.updated_at_ms = tx.now_ms();
            tx.router().put_connection(&current, Some(expected))?;
            Ok(RefreshOwner {
                operation: id,
                handle: candidate,
                fence: attempt.fence(),
                connection: current.id,
                credential_revision: current.credential_revision,
                account_revision: current.account_revision,
                profile: current.profile,
                endpoint_digest: crate::fingerprint::sha256_hex(current.endpoint.as_bytes()),
            })
        })?;
        self.lifecycle
            .owners
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(owner.operation.0.as_str().into(), owner.clone());
        drop(closed);
        Ok(owner)
    }
    pub(super) fn finish_refresh(
        &self,
        owner: &RefreshOwner,
        to: OperationState,
        observation: Value,
    ) -> Result<(), StorageError> {
        let owned = owner.clone();
        let result = self.publication(move |tx| finish_in(tx, &owned, to, observation));
        if result.is_ok() && crate::ports::storage::is_terminal(to) {
            self.lifecycle
                .owners
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(owner.operation.0.as_str());
        }
        result
    }
    pub(super) fn refresh_published(&self, owner: &RefreshOwner) {
        self.lifecycle
            .owners
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(owner.operation.0.as_str());
    }
    pub(super) fn check_refresh_owner(
        &self,
        current: &StoredConnection,
    ) -> Result<(), AccessError> {
        if self.is_closed() {
            return Err(AccessError::Unavailable(
                "the provider connection owner is closed".into(),
            ));
        }
        let Some(id) = &current.refresh_operation_id else {
            return Ok(());
        };
        let op = self
            .read(|s| s.operations().operation(id))
            .map_err(|_| AccessError::Unavailable("refresh custody unavailable".into()))?;
        let Some(op) = op else {
            return Err(AccessError::Unavailable("refresh custody missing".into()));
        };
        if op.payload["credentialRevision"].as_i64() != Some(current.credential_revision) {
            return Ok(());
        }
        let old_boot = op
            .latest_attempt()
            .is_some_and(|a| Some(&a.boot_id) != self.store.info().boot_id.as_ref());
        let closed = op
            .attempts
            .iter()
            .flat_map(|a| &a.observations)
            .any(|v| v["hostClosed"] == true);
        if crate::ports::storage::possibly_dispatched(op.state) && (old_boot || closed) {
            let row = current.clone();
            let operation = op.id.clone();
            let _ = self.publication(move |tx| {
                let current = tx
                    .router()
                    .connection(&row.id)?
                    .ok_or(StorageError::ScopeDenied)?;
                if current.credential_revision == row.credential_revision {
                    let mut changed = current.clone();
                    changed.lifecycle = "ReauthenticationRequired".into();
                    changed.state_reason = Some(
                        "An interrupted token refresh has an unknown outcome; sign in again."
                            .into(),
                    );
                    changed.updated_at_ms = tx.now_ms();
                    tx.router()
                        .put_connection(&changed, Some(current.revision))?;
                }
                let op = tx
                    .operations()
                    .operation(&operation)?
                    .ok_or(StorageError::ScopeDenied)?;
                if op.state == OperationState::Dispatching {
                    let fence = op.latest_attempt().map(|a| a.fence());
                    tx.operations().transition(
                        &operation,
                        op.revision,
                        fence.as_ref(),
                        OperationState::OutcomeUnknown,
                        Some(json!({"recoveredRefresh":true,"hostClosed":true})),
                    )?;
                }
                Ok(())
            });
            return Err(AccessError::ReauthenticationRequired(
                "An interrupted token refresh has an unknown outcome; sign in again.".into(),
            ));
        }
        Ok(())
    }
    /// Drains accepted credential units, closes publication and marks every possibly spent
    /// refresh token unknown. It never waits for HTTP or an already-started OS secret write.
    pub fn close(&self) -> Result<RefreshCloseReport, StorageError> {
        let mut closed = self
            .lifecycle
            .closed
            .write()
            .unwrap_or_else(|p| p.into_inner());
        if *closed {
            return Ok(RefreshCloseReport::default());
        }
        *closed = true;
        let owners = self
            .lifecycle
            .owners
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let handles = self
            .live()
            .staged
            .keys()
            .filter_map(|id| Id::new(id.clone()).ok())
            .collect::<Vec<_>>();
        let mut report = RefreshCloseReport {
            unknown_operations: Vec::new(),
            unpublished_handles: handles,
        };
        for owner in owners {
            let owned = owner.clone();
            run_write(self.store.as_ref(), WriteOptions::default(), move |tx| {
                let Some(op) = tx.operations().operation(&owned.operation)? else {
                    return Ok(());
                };
                if !crate::ports::storage::possibly_dispatched(op.state) {
                    return Ok(());
                }
                finish_in(
                    tx,
                    &owned,
                    OperationState::OutcomeUnknown,
                    json!({"hostClosed":true,"credentialPublication":"NotPublished","externalSecretWrite":"MayBeInProgress","allocatedHandle":owned.handle}),
                )?;
                if let Some(mut current) = tx.router().connection(&owned.connection)?
                    && current.credential_revision == owned.credential_revision
                    && current.account_revision == owned.account_revision
                    && current.profile == owned.profile
                    && crate::fingerprint::sha256_hex(current.endpoint.as_bytes())
                        == owned.endpoint_digest
                {
                    let expected = current.revision;
                    current.lifecycle = "ReauthenticationRequired".into();
                    current.state_reason = Some(
                        "An interrupted token refresh has an unknown outcome; sign in again."
                            .into(),
                    );
                    current.updated_at_ms = tx.now_ms();
                    tx.router().put_connection(&current, Some(expected))?;
                }
                Ok(())
            })?;
            report.unknown_operations.push(owner.operation);
        }
        drop(closed);
        Ok(report)
    }
}
pub(super) fn finish_in(
    tx: &mut dyn WriteTransaction,
    owner: &RefreshOwner,
    to: OperationState,
    observation: Value,
) -> Result<(), StorageError> {
    let op = tx
        .operations()
        .operation(&owner.operation)?
        .ok_or(StorageError::ScopeDenied)?;
    if op.latest_attempt().is_none_or(|a| a.fence() != owner.fence) {
        return Err(StorageError::ScopeDenied);
    }
    if op.state == to {
        tx.operations()
            .observe(&owner.operation, owner.fence.attempt_no, observation)?;
        return Ok(());
    }
    if !crate::ports::storage::transition_allowed(op.state, to) {
        return Err(StorageError::ScopeDenied);
    }
    tx.operations().transition(
        &owner.operation,
        op.revision,
        Some(&owner.fence),
        to,
        Some(observation),
    )?;
    Ok(())
}
fn interrupted(tx: &dyn WriteTransaction, op: &OperationRecord) -> bool {
    crate::ports::storage::possibly_dispatched(op.state)
        && (op
            .latest_attempt()
            .is_some_and(|a| Some(&a.boot_id) != tx.store().boot_id.as_ref())
            || op
                .attempts
                .iter()
                .flat_map(|a| &a.observations)
                .any(|v| v["hostClosed"] == true))
}
