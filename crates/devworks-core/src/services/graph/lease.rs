//! Internal retention leases. A lease identifies a view set and never grants repository access.
use super::scope;
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::gateway::AdmittedQuery;
use crate::ports::storage::*;
use crate::ports::{Clock, IdKind, IdSource};
use crate::protocol::Id;
use crate::protocol::data::{DataScope, ViewSelection};
use crate::services::indexing::views::ViewLayers;
use crate::services::sessions::Failure;
use serde_json::json;
use std::sync::Arc;

pub(super) struct Selected {
    pub pin: StoredGraphReadPin,
    pub layers: Vec<ViewLayers>,
}

pub(crate) fn owner(ctx: &InvocationContext) -> String {
    let worktree=ctx.worktree().map(|tree|json!({"id":tree.id(),"workId":tree.work_id(),"repositoryId":tree.repository_id(),"ownershipRevision":tree.ownership_revision()}));
    sha256_hex(json!({"actor":ctx.principal(),"kind":format!("{:?}",ctx.principal_kind()),"project":ctx.project_id(),"root":ctx.root_id(),"assignment":ctx.assignment_id(),"session":ctx.session_id(),"ingress":ctx.ingress_namespace(),"worktree":worktree}).to_string().as_bytes())
}

pub(crate) fn scope_digest(scope: &DataScope) -> String {
    sha256_hex(
        serde_json::to_string(scope)
            .expect("typed scope")
            .as_bytes(),
    )
}

pub(super) fn check_pin(
    pin: &StoredGraphReadPin,
    ctx: &InvocationContext,
    requested_scope: &DataScope,
    now: i64,
) -> Result<(), Failure> {
    if pin.owner_namespace != owner(ctx)
        || pin.scope_digest != scope_digest(requested_scope)
        || pin.store_id != *ctx.store_id()
        || pin.restore_epoch != ctx.restore_epoch().get()
        || pin.expires_at_ms <= now
    {
        return Err(scope::unavailable(Some(pin.id.clone())));
    }
    Ok(())
}

pub(super) struct Host<'a> {
    pub store: &'a Arc<dyn ApplicationStore>,
    pub clock: &'a dyn Clock,
    pub ids: &'a dyn IdSource,
}
pub(super) fn select(
    host: Host<'_>,
    ctx: &InvocationContext,
    query: &AdmittedQuery,
    data_scope: &DataScope,
    requested: Option<&ViewSelection>,
    options: &ReadOptions,
    current_worktree: Option<&Id>,
) -> Result<Selected, Failure> {
    let Host { store, clock, ids } = host;
    let now = clock.now_ms().get();
    let found = read_snapshot(&**store, options, |read| {
        Ok(match requested {
            Some(ViewSelection::Pinned { pin_id }) => {
                let pin = read.graph().graph_pin(pin_id)?.ok_or_else(|| scope::unavailable(Some(pin_id.clone())));
                match pin {
                    Ok(pin) => {
                        (|| {
                            check_pin(&pin, ctx, data_scope, now)?;
                            for missing in &pin.unavailable_repositories { scope::authorize(read, ctx, data_scope, &missing.repository_id)?; }
                            let mut layers = Vec::with_capacity(pin.scope.views.len());
                            for binding in &pin.scope.views {
                                let view = scope::retained(read, ctx, data_scope, &pin, binding, now)?;
                                if view.view.repository_id != binding.repository_id || view.view.view_digest != binding.view_digest {
                                    return Err(scope::unavailable(Some(pin.id.clone())));
                                }
                                layers.push(view);
                            }
                            Ok(Selected { pin, layers })
                        })()
                    }
                    Err(e) => Err(e),
                }
            }
            _ => scope::resolve(read, ctx, data_scope, requested, now, current_worktree).and_then(|resolved| {
                let layers = resolved.layers;
                let unavailable_repositories = resolved.unavailable.into_iter().map(|(repository_id,reason)|GraphUnavailableRepository { repository_id,reason:reason.to_owned() }).collect::<Vec<_>>();
                let witnesses = layers.iter().map(|v| scope::witness(read, &v.view)).collect::<Result<Vec<_>, _>>()?;
                let expires = witnesses.iter().filter_map(|w| w.expires_at_ms).fold(now.saturating_add(GRAPH_PIN_TTL_MS), i64::min);
                let graph_scope = GraphScope { views: layers.iter().map(|v| GraphViewBinding { repository_id: v.view.repository_id.clone(), view_id: v.view.view_id.clone(), view_digest: v.view.view_digest.clone() }).collect() };
                let namespace = owner(ctx);
                let scope_hash = scope_digest(data_scope);
                let intent_digest = sha256_hex(json!({"owner":namespace,"store":ctx.store_id(),"restore":ctx.restore_epoch(),"allowedScope":scope_hash,"views":graph_scope,"witnesses":witnesses,"unavailableRepositories":unavailable_repositories}).to_string().as_bytes());
                Ok(Selected { pin: StoredGraphReadPin { id:ids.next_id(IdKind::Lease),owner_namespace:namespace,store_id:ctx.store_id().clone(),restore_epoch:ctx.restore_epoch().get(),scope_digest:scope_hash,intent_digest,scope:graph_scope,revision:1,created_at_ms:now,expires_at_ms:expires,view_witnesses:witnesses,unavailable_repositories }, layers })
            }),
        })
    })??;
    let authority = query.authorization().clone();
    let context = ctx.clone();
    let data_scope = data_scope.clone();
    let current = !matches!(
        requested,
        Some(ViewSelection::Exact { .. } | ViewSelection::Pinned { .. })
    );
    let renew = matches!(requested, Some(ViewSelection::Pinned { .. }));
    let selected = run_write(
        &**store,
        WriteOptions {
            deadline_ms: options.deadline_ms,
            cancellation: options.cancellation.clone(),
            ..Default::default()
        },
        move |tx| {
            if !crate::authority::authorization_holds_in(tx, &context, &authority)? {
                return Ok(Err(crate::services::sessions::Refusal::new(
                    crate::protocol::DomainErrorCode::PermissionDenied,
                    "graph_scope",
                    None,
                )
                .into()));
            }
            if let Err(error) = scope::validate_in(
                tx,
                &context,
                &data_scope,
                &found.layers,
                current,
                renew.then_some(found.pin.view_witnesses.as_slice()),
            ) {
                return Ok(Err(error));
            }
            for missing in &found.pin.unavailable_repositories {
                if let Err(error) = crate::services::projects::visible_repository(
                    tx.projects(),
                    &context,
                    &missing.repository_id,
                ) {
                    return Ok(Err(error));
                }
                if let DataScope::Project { project_id } = &data_scope
                    && !tx
                        .projects()
                        .members(project_id)?
                        .contains(&missing.repository_id)
                {
                    return Ok(Err(crate::services::sessions::Refusal::new(
                        crate::protocol::DomainErrorCode::PermissionDenied,
                        "graph_scope",
                        None,
                    )
                    .into()));
                }
            }
            let mut selected = found;
            if renew {
                // Concurrent detail/source reads may renew the same immutable lease. Refresh
                // only its lifecycle CAS; owner, scope, views and witnesses cannot change.
                let Some(current) = tx.graph().graph_pin(&selected.pin.id)? else {
                    return Ok(Err(scope::unavailable(Some(selected.pin.id.clone()))));
                };
                if current.intent_digest != selected.pin.intent_digest {
                    return Ok(Err(scope::unavailable(Some(selected.pin.id.clone()))));
                }
                if let Err(error) = check_pin(&current, &context, &data_scope, tx.now_ms()) {
                    return Ok(Err(error));
                }
                let expiry = selected
                    .pin
                    .view_witnesses
                    .iter()
                    .filter_map(|w| w.expires_at_ms)
                    .fold(tx.now_ms().saturating_add(GRAPH_PIN_TTL_MS), i64::min);
                selected.pin = if expiry > current.expires_at_ms {
                    tx.graph().renew_graph_pin(
                        &selected.pin.id,
                        &selected.pin.owner_namespace,
                        current.revision,
                        expiry,
                    )?
                } else {
                    // A repeated page in the same clock tick (or at the overlay's hard
                    // expiry ceiling) reuses the still-live lease without a false renewal.
                    current
                };
            } else {
                selected.pin.created_at_ms = tx.now_ms();
                selected.pin.expires_at_ms = selected
                    .pin
                    .view_witnesses
                    .iter()
                    .filter_map(|w| w.expires_at_ms)
                    .fold(tx.now_ms().saturating_add(GRAPH_PIN_TTL_MS), i64::min);
                tx.graph().acquire_graph_pin(&selected.pin)?;
            }
            Ok(Ok(selected))
        },
    )??;
    Ok(selected)
}
