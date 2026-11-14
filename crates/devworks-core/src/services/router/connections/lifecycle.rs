//! OAuth refresh custody. Only short native publication units hold this fence; HTTP and
//! keychain calls run outside it. A closed application never publishes a late credential.
use super::*;
use crate::ports::storage::{AttemptBinding, AttemptFence, NewOperation, OperationRecord};
use crate::protocol::{OperationId, OperationState};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
#[derive(Default)]
pub(super) struct Lifecycle {
    closed: Arc<AtomicBool>,
    // Only close callers acquire this mutex; the database writer never does.
    closing: Mutex<()>,
    owners: Arc<Mutex<BTreeMap<String, RefreshOwner>>>,
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
        self.lifecycle.closed.load(Ordering::SeqCst)
    }
    pub(super) fn publication<T, F>(&self, work: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
    {
        let closed = self.lifecycle.closed.clone();
        // The queue is the publication barrier. Never wait for its writer while holding a
        // product lock: configure handlers run on that writer and also inspect lifecycle.
        run_write(self.store.as_ref(), WriteOptions::default(), move |tx| {
            if closed.load(Ordering::SeqCst) {
                return Err(StorageError::Cancelled);
            }
            work(tx)
        })
    }
    pub(super) fn register_stage(
        &self,
        handle: &Id,
        principal: &ActorId,
        api_key: bool,
    ) -> Result<(), SecretError> {
        let mut live = self.live();
        if self.is_closed() {
            return Err(SecretError::Failed(
                "provider connection owner closed".into(),
            ));
        }
        live.staged.insert(
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
        let closed = self.lifecycle.closed.clone();
        let owners = self.lifecycle.owners.clone();
        let handle = self.ids.next_id(IdKind::Credential);
        let candidate = handle.clone();
        let before = seen.clone();
        run_write(self.store.as_ref(), WriteOptions::default(), move |tx| {
            if closed.load(Ordering::SeqCst) {
                return Err(StorageError::Cancelled);
            }
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
            let owner = RefreshOwner {
                operation: id,
                handle: candidate,
                fence: attempt.fence(),
                connection: current.id,
                credential_revision: current.credential_revision,
                account_revision: current.account_revision,
                profile: current.profile,
                endpoint_digest: crate::fingerprint::sha256_hex(current.endpoint.as_bytes()),
            };
            // Register before this writer callback retires. A close queued behind it must
            // see every accepted refresh, even if the caller has not received its result.
            owners
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(owner.operation.0.as_str().into(), owner.clone());
            Ok(owner)
        })
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
                    let saved = tx
                        .router()
                        .put_connection(&changed, Some(current.revision))?;
                    // Recovery can run again for the same unrepaired credential: announce only
                    // the transition into reauthentication.
                    if current.lifecycle != "ReauthenticationRequired" {
                        crate::services::notifications::credentials_rejected(tx, &saved)?;
                    }
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
        let _closing = self
            .lifecycle
            .closing
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if self.lifecycle.closed.swap(true, Ordering::SeqCst) {
            return Ok(RefreshCloseReport::default());
        }
        // A writer barrier drains previously accepted publications. Later callbacks see
        // closed before touching durable state; no lifecycle lock is needed on the writer.
        run_write(self.store.as_ref(), WriteOptions::default(), |_| Ok(()))?;
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
            let unknown = run_write(self.store.as_ref(), WriteOptions::default(), move |tx| {
                let Some(op) = tx.operations().operation(&owned.operation)? else {
                    return Ok(false);
                };
                if !crate::ports::storage::possibly_dispatched(op.state) {
                    return Ok(false);
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
                    let transition = current.lifecycle != "ReauthenticationRequired";
                    current.lifecycle = "ReauthenticationRequired".into();
                    current.state_reason = Some(
                        "An interrupted token refresh has an unknown outcome; sign in again."
                            .into(),
                    );
                    current.updated_at_ms = tx.now_ms();
                    let saved = tx.router().put_connection(&current, Some(expected))?;
                    if transition {
                        crate::services::notifications::credentials_rejected(tx, &saved)?;
                    }
                }
                Ok(true)
            })?;
            if unknown {
                report.unknown_operations.push(owner.operation);
            }
        }
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
