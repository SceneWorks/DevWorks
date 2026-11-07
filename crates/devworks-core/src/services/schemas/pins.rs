//! Metadata retention is not authority. Leases keep one exact source view and current access
//! is independently resolved by every read. Expired identities are never revived or reused.
use super::{SchemaService, access};
use crate::{
    context::InvocationContext,
    gateway::AdmittedQuery,
    ports::{IdKind, Preparation, Prepared, storage::*},
    protocol::{
        data::{DataScope, ResolvedViewSet, SchemaView, ViewSelection},
        provider::Digest,
    },
    services::{
        graph::lease,
        sessions::{Failure, rev},
    },
};
use serde_json::json;
use std::collections::BTreeMap;
#[derive(Default)]
pub(super) struct PreparedViews(pub BTreeMap<String, StoredSchemaReadPin>);
fn unavailable() -> Failure {
    StorageError::ViewUnavailable.into()
}
fn checked(
    read: &dyn SchemaRead,
    ctx: &InvocationContext,
    scope: &DataScope,
    source: &StoredSchemaSource,
    pin: &StoredSchemaReadPin,
    now: i64,
) -> Result<StoredSchemaView, Failure> {
    if pin.owner_namespace != lease::owner(ctx)
        || pin.store_id != *ctx.store_id()
        || pin.restore_epoch != ctx.restore_epoch().get()
        || pin.scope_digest != lease::scope_digest(scope)
        || pin.expires_at_ms <= now
        || pin.connection_id != source.connection.id
        || pin.configuration_revision != source.configuration_revision
    {
        return Err(unavailable());
    }
    read.schema_view(&pin.view_id)?
        .filter(|v| {
            v.connection_id == source.connection.id
                && v.configuration_revision == source.configuration_revision
                && v.digest == pin.view_digest
                && v.sealed
        })
        .ok_or_else(unavailable)
}
pub(super) fn selected(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    scope: &DataScope,
    source: &StoredSchemaSource,
    selection: &ViewSelection,
    now: i64,
) -> Result<Option<StoredSchemaView>, Failure> {
    let ViewSelection::Pinned { pin_id } = selection else {
        return Ok(None);
    };
    let pin = read.schemas().schema_pin(pin_id)?.ok_or_else(unavailable)?;
    checked(read.schemas(), ctx, scope, source, &pin, now).map(Some)
}
pub(super) fn projection(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    scope: &DataScope,
    source: &StoredSchemaSource,
    prepared: Option<&PreparedViews>,
    now: i64,
) -> Result<Option<ResolvedViewSet>, Failure> {
    let Some(pin) = prepared.and_then(|p| p.0.get(source.connection.id.as_str())) else {
        return Ok(None);
    };
    // Listing remains a fresh current-source page if a publication changes during preparation.
    // It never attaches the earlier capture to a different current view.
    if source.connection.current_view_id.as_ref() != Some(&pin.view_id)
        || source.configuration_revision != pin.configuration_revision
    {
        return Ok(None);
    }
    let current = read
        .schemas()
        .schema_pin(&pin.id)?
        .ok_or_else(unavailable)?;
    let view = checked(read.schemas(), ctx, scope, source, &current, now)?;
    let resolved = ResolvedViewSet {
        schema_views: vec![SchemaView {
            connection_id: source.connection.id.clone(),
            snapshot_id: view.id.clone(),
            generation_id: view.id,
            schema_digest: Digest(view.digest),
            access_revision: rev(source.configuration_revision),
            coverage: view.coverage,
        }],
        pin_id: current.id,
        stamp: read.stamp().read_stamp(
            ctx.policy_revision().clone(),
            rev(source.configuration_revision),
        ),
        expires_at_ms: crate::protocol::Counter::new(current.expires_at_ms)
            .map_err(|_| unavailable())?,
        views: vec![],
    };
    resolved.validate().map_err(|_| unavailable())?;
    Ok(Some(resolved))
}
impl SchemaService {
    pub(super) fn prepare_connection_views(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
        scope: &DataScope,
        sources: Vec<StoredSchemaSource>,
        visibility: SchemaVisibility,
    ) -> Result<Option<Prepared>, Failure> {
        let Some(runtime) = &self.runtime else {
            return Ok(None);
        };
        if query.schema_revision().0 < 5 {
            return Ok(None);
        }
        let _phase = runtime.closed.enter()?;
        let (closed, ids, clock, ctx, auth, scope) = (
            runtime.closed.clone(),
            self.ids.clone(),
            self.clock.clone(),
            ctx.clone(),
            query.authorization().clone(),
            scope.clone(),
        );
        let cancellation = env.cancellation.cloned();
        let deadline = env.deadline_ms;
        let prepared = run_write(
            runtime.store.as_ref(),
            WriteOptions {
                cancellation: cancellation.clone(),
                deadline_ms: deadline,
                ..Default::default()
            },
            move |tx| {
                // Storage validates new leases against this transaction's captured
                // time. A wall-clock tick after admission must not date them ahead
                // of their writer or extend their fixed retention interval.
                let created_at_ms = tx.now_ms();
                let now = clock.now_ms().get();
                if closed.is_closed()
                    || cancellation
                        .as_ref()
                        .is_some_and(Cancellation::is_cancelled)
                    || deadline.is_some_and(|d| now > d)
                {
                    return Err(StorageError::Cancelled);
                }
                if !crate::authority::authorization_holds_in(tx, &ctx, &auth)? {
                    return Err(StorageError::ScopeDenied);
                }
                let reach = crate::services::knowledge::reach(tx.authority(), &ctx)
                    .map_err(|_| StorageError::ScopeDenied)?;
                crate::services::knowledge::resolve_scope_with(&reach, &ctx, &scope, false, |p| {
                    tx.artifacts().project_exists(p)
                })
                .map_err(|_| StorageError::ScopeDenied)?;
                tx.schemas().expire_schema_pins(now, 500)?;
                let mut result = PreparedViews::default();
                for captured in sources {
                    let Some(source) = tx.schemas().schema_source(&captured.connection.id)? else {
                        continue;
                    };
                    if !access::sees(&source, &visibility)
                        || source.configuration_revision != captured.configuration_revision
                        || source.connection.current_view_id != captured.connection.current_view_id
                    {
                        continue;
                    }
                    let Some(view_id) = &source.connection.current_view_id else {
                        continue;
                    };
                    let Some(view) = tx.schemas().schema_view(view_id)? else {
                        continue;
                    };
                    if view.connection_id != source.connection.id
                        || view.configuration_revision != source.configuration_revision
                    {
                        continue;
                    }
                    let owner = lease::owner(&ctx);
                    let scope_digest = lease::scope_digest(&scope);
                    let intent=crate::fingerprint::sha256_hex(json!({"owner":owner,"store":ctx.store_id(),"restore":ctx.restore_epoch(),"scope":scope_digest,"source":source.connection.id,"configuration":source.configuration_revision,"view":view.id,"digest":view.digest}).to_string().as_bytes());
                    let pin = if let Some(pin) = tx.schemas().schema_active_pin(&intent, now)? {
                        pin
                    } else {
                        let pin = StoredSchemaReadPin {
                            id: ids.next_id(IdKind::Lease),
                            owner_namespace: owner,
                            store_id: ctx.store_id().clone(),
                            restore_epoch: ctx.restore_epoch().get(),
                            scope_digest,
                            intent_digest: intent,
                            connection_id: source.connection.id.clone(),
                            view_id: view.id,
                            view_digest: view.digest,
                            configuration_revision: source.configuration_revision,
                            created_at_ms,
                            expires_at_ms: created_at_ms.saturating_add(GRAPH_PIN_TTL_MS),
                        };
                        tx.schemas().create_schema_pin(&pin)?;
                        pin
                    };
                    result.0.insert(source.connection.id.as_str().into(), pin);
                }
                Ok(result)
            },
        )?;
        Ok(Some(Prepared::new(prepared)))
    }
}

impl SchemaService {
    /// Fixed-size local retention pass; no target connection or authority grant is involved.
    pub fn cleanup_retained_views(&self) -> Result<usize, StorageError> {
        let Some(runtime) = &self.runtime else {
            return Ok(0);
        };
        let _phase = runtime.closed.enter()?;
        let (closed, clock) = (runtime.closed.clone(), self.clock.clone());
        run_write(runtime.store.as_ref(), WriteOptions::default(), move |tx| {
            if closed.is_closed() {
                return Err(StorageError::Cancelled);
            }
            let pins = tx.schemas().expire_schema_pins(clock.now_ms().get(), 500)?;
            let rows = tx.schemas().prune_schema_views(500)?;
            Ok(pins + rows)
        })
    }
}
