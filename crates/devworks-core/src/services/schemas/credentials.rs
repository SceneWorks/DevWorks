//! Human password intake has its own store/purpose namespace. Protocol bodies carry only
//! host-minted handles; startup reconciliation reads references, never password values.
use super::SchemaService;
use crate::{
    context::{InvocationContext, PrincipalKind},
    ports::{
        IdKind,
        secrets::{Secret, SecretError, SecretKey, SecretStore},
        storage::*,
    },
    protocol::{ActorId, Id},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
const STAGED_TTL_MS: i64 = 30 * 60 * 1000;
struct Staged {
    actor: ActorId,
    created_at_ms: i64,
    reserved: bool,
}
#[derive(Default)]
struct Intake {
    started: bool,
    staged: BTreeMap<String, Staged>,
}
pub(super) struct Credentials {
    store: Arc<dyn ApplicationStore>,
    secrets: Arc<dyn SecretStore>,
    intake: Mutex<Intake>,
}
impl Credentials {
    pub(super) fn password(&self, handle: &Id) -> Result<Option<Secret>, SecretError> {
        self.secrets.get(&self.key(handle))
    }
    fn key(&self, handle: &Id) -> SecretKey {
        SecretKey::new(
            format!("{}:schema-sources", self.store.info().store_id.0.as_str()),
            handle.clone(),
        )
    }
    pub(super) fn valid(&self, context: &InvocationContext, handle: &Id, now: i64) -> bool {
        let mut intake = self.intake.lock().unwrap_or_else(|p| p.into_inner());
        intake.started = true;
        if context.store_id() != &self.store.info().store_id {
            return false;
        }
        let Some(staged) = intake.staged.get_mut(handle.as_str()) else {
            return false;
        };
        if &staged.actor != context.principal()
            || now.saturating_sub(staged.created_at_ms) >= STAGED_TTL_MS
        {
            return false;
        }
        // Once a deciding transaction can publish this handle, cancellation cannot delete it
        // ahead of an unknown or queued commit. Startup reconciliation resolves unused custody.
        staged.reserved = true;
        true
    }
}
impl SchemaService {
    pub fn with_credentials(
        mut self,
        store: Arc<dyn ApplicationStore>,
        secrets: Arc<dyn SecretStore>,
    ) -> Self {
        self.credentials = Some(Credentials {
            store,
            secrets,
            intake: Mutex::default(),
        });
        self
    }
    /// Called by authenticated native human ingress. Password whitespace is meaningful.
    /// No secret is accepted from a tool body or another subsystem's opaque handle.
    pub fn stage_password(
        &self,
        context: &InvocationContext,
        password: Secret,
    ) -> Result<Id, SecretError> {
        let owner = self
            .credentials
            .as_ref()
            .ok_or_else(|| SecretError::Unavailable("no schema credential facility".into()))?;
        if context.principal_kind() != PrincipalKind::Human
            || context.store_id() != &owner.store.info().store_id
        {
            return Err(SecretError::Denied);
        }
        if password.is_empty() || password.len() > 65_536 {
            return Err(SecretError::TooLarge);
        }
        let mut intake = owner.intake.lock().unwrap_or_else(|p| p.into_inner());
        intake.started = true;
        // A committed handle is owned by its durable source, not an everlasting intake slot.
        // Keep uncertain reserved handles until startup reconciliation can prove them unused.
        let committed = read_snapshot(owner.store.as_ref(), &ReadOptions::default(), |read| {
            intake
                .staged
                .keys()
                .filter_map(|key| Id::new(key.clone()).ok())
                .map(|id| {
                    read.schemas()
                        .schema_credential_in_use(&id)
                        .map(|used| (id, used))
                })
                .collect::<Result<Vec<_>, StorageError>>()
        })
        .map_err(|_| SecretError::Failed("schema credential reference check unavailable".into()))?;
        for (id, used) in committed {
            if used {
                intake.staged.remove(id.as_str());
            }
        }
        if intake.staged.len() >= 128 {
            return Err(SecretError::Failed(
                "schema credential intake is full".into(),
            ));
        }
        let handle = self.ids.next_id(IdKind::Credential);
        owner.secrets.put(&owner.key(&handle), &password)?;
        intake.staged.insert(
            handle.as_str().to_owned(),
            Staged {
                actor: context.principal().clone(),
                created_at_ms: self.clock.now_ms().get(),
                reserved: false,
            },
        );
        Ok(handle)
    }
    /// UI cancellation discards only this human's unsubmitted handle. A submitted source's
    /// credential cannot be erased through this intake seam.
    pub fn discard_password(
        &self,
        context: &InvocationContext,
        handle: &Id,
    ) -> Result<(), SecretError> {
        let owner = self
            .credentials
            .as_ref()
            .ok_or_else(|| SecretError::Unavailable("no schema credential facility".into()))?;
        let mut intake = owner.intake.lock().unwrap_or_else(|p| p.into_inner());
        if context.principal_kind() != PrincipalKind::Human
            || context.store_id() != &owner.store.info().store_id
            || !intake
                .staged
                .get(handle.as_str())
                .is_some_and(|s| &s.actor == context.principal() && !s.reserved)
        {
            return Err(SecretError::Denied);
        }
        let used = read_snapshot(owner.store.as_ref(), &ReadOptions::default(), |r| {
            r.schemas().schema_credential_in_use(handle)
        })
        .map_err(|_| SecretError::Failed("schema reference check unavailable".into()))?;
        if used {
            return Err(SecretError::Denied);
        }
        owner.secrets.delete(&owner.key(handle))?;
        intake.staged.remove(handle.as_str());
        Ok(())
    }
    /// Run once before exposing this newly composed owner. Intake and reference validation
    /// permanently close this startup-only cleanup seam, avoiding a delete/publication race.
    pub fn reconcile_credential_orphans(&self) -> Result<usize, SecretError> {
        let owner = self
            .credentials
            .as_ref()
            .ok_or_else(|| SecretError::Unavailable("no schema credential facility".into()))?;
        let mut intake = owner.intake.lock().unwrap_or_else(|p| p.into_inner());
        if intake.started {
            return Err(SecretError::Failed(
                "schema startup reconciliation already closed".into(),
            ));
        }
        intake.started = true;
        let namespace = owner
            .key(&Id::new("namespace-probe").expect("owned ID"))
            .namespace;
        let handles = owner.secrets.list(&namespace)?;
        let mut removed = 0;
        for handle in handles {
            let used = read_snapshot(owner.store.as_ref(), &ReadOptions::default(), |r| {
                r.schemas().schema_credential_in_use(&handle)
            })
            .map_err(|_| SecretError::Failed("schema reference check unavailable".into()))?;
            if !used && owner.secrets.delete(&owner.key(&handle))? {
                removed += 1;
            }
        }
        Ok(removed)
    }
}
