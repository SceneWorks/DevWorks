//! Select and retain analysis inputs through the same authority as exact graph reads.
use super::super::{lease, model, scope};
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::ports::IdKind;
use crate::ports::storage::{
    GraphScope, GraphUnavailableRepository, GraphViewBinding, GraphViewWitness, IndexRead,
    ReadSnapshot, StoredAnalysisRun, StoredGraphReadPin, WriteTransaction,
};
use crate::protocol::data::{DataScope, ViewSelection};
use crate::protocol::records::AnalysisViewBinding;
use crate::protocol::{DomainErrorCode, Id};
use crate::services::indexing::{self, views::ViewLayers};
use crate::services::kernel::{Failure, Refusal};
use serde_json::json;

const REPORT_ALGORITHM_REVISION: u32 = 1;
const PIN_TTL_MS: i64 = 300_000;
pub(super) const MAX_COMMAND_REPOSITORIES: usize = 32;

pub(super) struct Selection {
    pub scope: DataScope,
    pub layers: Vec<ViewLayers>,
    pub views: Vec<AnalysisViewBinding>,
    pub unavailable: Vec<GraphUnavailableRepository>,
    pub binding_key: String,
    pub lineage_key: String,
    pub repository_lineages: Vec<String>,
    pub configuration_revision: i64,
    witnesses: Vec<GraphViewWitness>,
    published: bool,
}

pub(super) fn unavailable() -> Failure {
    Refusal::new(DomainErrorCode::ViewUnavailable, "analysis_view", None).into()
}

pub(super) fn configuration_revision(
    read: &dyn IndexRead,
    scope: &DataScope,
) -> Result<i64, Failure> {
    let project = match scope {
        DataScope::Project { project_id } => Some(project_id),
        DataScope::Workspace => None,
    };
    Ok(indexing::section_config(read, &indexing::section_key(project))?.revision)
}

impl Selection {
    pub(super) fn resolve(
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        data_scope: &DataScope,
        requested: &ViewSelection,
        now: i64,
    ) -> Result<Self, Failure> {
        if matches!(data_scope, DataScope::Workspace) && ctx.project_id().is_some() {
            return Err(
                Refusal::new(DomainErrorCode::PermissionDenied, "workspace_scope", None).into(),
            );
        }
        scope::owned(read, ctx)?;
        if let ViewSelection::Pinned { pin_id } = requested {
            let pin = read.graph().graph_pin(pin_id)?.ok_or_else(unavailable)?;
            lease::check_pin(&pin, ctx, data_scope, now).map_err(hide_view_identity)?;
            let mut layers = Vec::with_capacity(pin.scope.views.len());
            for binding in &pin.scope.views {
                layers.push(
                    scope::retained(read, ctx, data_scope, &pin, binding, now)
                        .map_err(hide_view_identity)?,
                );
            }
            for missing in &pin.unavailable_repositories {
                scope::authorize(read, ctx, data_scope, &missing.repository_id)
                    .map_err(hide_view_identity)?;
            }
            return Self::capture(
                read,
                data_scope,
                layers,
                pin.view_witnesses,
                pin.unavailable_repositories,
                false,
            );
        }
        let resolved = if let ViewSelection::Published { repository_ids } = requested {
            let mut layers = Vec::new();
            let mut unavailable = Vec::new();
            for repository in repository_ids {
                scope::authorize(read, ctx, data_scope, repository).map_err(hide_view_identity)?;
                if let Some(pointer) = read.indexing().published(repository)? {
                    layers.push(
                        scope::exact(read, ctx, data_scope, &pointer.view_id, now)
                            .map_err(hide_view_identity)?,
                    );
                } else {
                    unavailable.push((repository.clone(), "RepositoryNotIndexed"));
                }
            }
            scope::Resolved {
                layers,
                unavailable,
            }
        } else {
            scope::resolve(read, ctx, data_scope, Some(requested), now, None)
                .map_err(hide_view_identity)?
        };
        let witnesses = resolved
            .layers
            .iter()
            .map(|layer| scope::witness(read, &layer.view).map_err(hide_view_identity))
            .collect::<Result<Vec<_>, _>>()?;
        let unavailable = resolved
            .unavailable
            .into_iter()
            .map(|(repository_id, reason)| GraphUnavailableRepository {
                repository_id,
                reason: reason.to_owned(),
            })
            .collect();
        Self::capture(
            read,
            data_scope,
            resolved.layers,
            witnesses,
            unavailable,
            matches!(requested, ViewSelection::Published { .. }),
        )
    }

    /// Automatic work includes the new view, with explicit coverage if the
    /// project's complete repository inventory exceeds one bounded run.
    pub(super) fn for_publication(
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        data_scope: &DataScope,
        repository: &Id,
        view: &Id,
    ) -> Result<Self, Failure> {
        let resolved = scope::resolve(read, ctx, data_scope, None, read.stamp().now_ms, None)?;
        let mut layers = resolved.layers;
        let Some(at) = layers.iter().position(|layer| {
            layer.view.repository_id == *repository && layer.view.view_id == *view
        }) else {
            return Err(unavailable());
        };
        let mut missing: Vec<_> = resolved
            .unavailable
            .into_iter()
            .map(|(repository_id, reason)| GraphUnavailableRepository {
                repository_id,
                reason: reason.into(),
            })
            .collect();
        if layers.len() + missing.len() > MAX_COMMAND_REPOSITORIES {
            // Keep an actual omitted member as the coverage witness.
            layers.swap(0, at);
            let marker = layers
                .get(MAX_COMMAND_REPOSITORIES - 1)
                .map(|layer| layer.view.repository_id.clone())
                .or_else(|| missing.last().map(|row| row.repository_id.clone()));
            layers.truncate(MAX_COMMAND_REPOSITORIES - 1);
            let max_missing = MAX_COMMAND_REPOSITORIES - layers.len();
            missing.truncate(max_missing.saturating_sub(1));
            if let Some(repository_id) = marker {
                if missing.iter().any(|row| row.repository_id == repository_id) {
                    missing.retain(|row| row.repository_id != repository_id);
                }
                missing.push(GraphUnavailableRepository {
                    repository_id,
                    reason: "ScopeEnumerationIncomplete".into(),
                });
            }
        }
        let witnesses = layers
            .iter()
            .map(|layer| scope::witness(read, &layer.view))
            .collect::<Result<_, _>>()?;
        Self::capture(read, data_scope, layers, witnesses, missing, true)
    }

    fn capture(
        read: &dyn ReadSnapshot,
        scope: &DataScope,
        mut layers: Vec<ViewLayers>,
        mut witnesses: Vec<GraphViewWitness>,
        mut unavailable: Vec<GraphUnavailableRepository>,
        published: bool,
    ) -> Result<Self, Failure> {
        layers.sort_by(|a, b| {
            a.view
                .repository_id
                .as_str()
                .cmp(b.view.repository_id.as_str())
        });
        witnesses.sort_by(|a, b| a.view_id.as_str().cmp(b.view_id.as_str()));
        unavailable.sort_by(|a, b| a.repository_id.as_str().cmp(b.repository_id.as_str()));
        let views = layers
            .iter()
            .map(|layer| {
                let view = &layer.view;
                let snapshot = model::snapshot(view).map_err(hide_view_identity)?;
                snapshot.validate().map_err(|_| self::unavailable())?;
                Ok(AnalysisViewBinding {
                    repository_id: view.repository_id.clone(),
                    view_id: view.view_id.clone(),
                    view_digest: model::text(&view.view_digest)?,
                    snapshot,
                })
            })
            .collect::<Result<Vec<_>, Failure>>()?;
        let configuration_revision = configuration_revision(read.indexing(), scope)?;
        let lineage = layers
            .iter()
            .zip(&views)
            .map(|(layer, binding)| {
                let coverage = &layer.view.coverage;
                let kind = if binding.snapshot.worktree_id.is_some() {
                    "worktree"
                } else if scope::overlay(&layer.view) {
                    "ref"
                } else {
                    "baseline"
                };
                let reference = coverage["headRef"]
                    .as_str()
                    .or_else(|| (kind != "worktree").then_some(binding.snapshot.head.hex.as_str()));
                json!({
                    "repository": binding.repository_id,
                    "kind": kind,
                    "ref": reference,
                    "worktree": binding.snapshot.worktree_id,
                })
            })
            .collect::<Vec<_>>();
        let binding_key = digest(&json!({
            "algorithm": REPORT_ALGORITHM_REVISION,
            "scope": scope,
            "views": views,
            "unavailable": unavailable,
            "configuration": configuration_revision,
        }));
        let lineage_key = digest(&json!({
            "algorithm": REPORT_ALGORITHM_REVISION,
            "scope": scope,
            "lineage": lineage,
            "unavailable": unavailable,
        }));
        Ok(Self {
            scope: scope.clone(),
            layers,
            views,
            unavailable,
            binding_key,
            lineage_key,
            repository_lineages: lineage.iter().map(digest).collect(),
            configuration_revision,
            witnesses,
            published,
        })
    }

    pub(super) fn validate_in(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
    ) -> Result<(), Failure> {
        scope::validate_in(
            tx,
            ctx,
            &self.scope,
            &self.layers,
            self.published,
            Some(&self.witnesses),
        )
        .map_err(hide_view_identity)?;
        for missing in &self.unavailable {
            crate::services::projects::visible_repository(
                tx.projects(),
                ctx,
                &missing.repository_id,
            )
            .map_err(hide_view_identity)?;
            if let DataScope::Project { project_id } = &self.scope
                && !tx
                    .projects()
                    .members(project_id)?
                    .contains(&missing.repository_id)
            {
                return Err(unavailable());
            }
        }
        Ok(())
    }

    pub(super) fn graph_scope(&self) -> GraphScope {
        GraphScope {
            views: self
                .views
                .iter()
                .map(|view| GraphViewBinding {
                    repository_id: view.repository_id.clone(),
                    view_id: view.view_id.clone(),
                    view_digest: view.view_digest.as_str().to_owned(),
                })
                .collect(),
        }
    }

    pub(super) fn retain_in(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
    ) -> Result<Id, Failure> {
        let now = tx.now_ms();
        let expires_at_ms = self
            .witnesses
            .iter()
            .filter_map(|witness| witness.expires_at_ms)
            .fold(now.saturating_add(PIN_TTL_MS), i64::min);
        if expires_at_ms <= now {
            return Err(unavailable());
        }
        let id = tx.new_id(IdKind::Lease);
        let owner = lease::owner(ctx);
        let scope_digest = lease::scope_digest(&self.scope);
        let pin = StoredGraphReadPin {
            id: id.clone(),
            owner_namespace: owner.clone(),
            store_id: ctx.store_id().clone(),
            restore_epoch: ctx.restore_epoch().get(),
            scope_digest: scope_digest.clone(),
            intent_digest: digest(&json!({
                "purpose": "repository_analysis",
                "lease": id,
                "store": ctx.store_id(),
                "restore": ctx.restore_epoch().get(),
                "owner": owner,
                "scope": scope_digest,
                "binding": self.binding_key,
                "witnesses": self.witnesses,
            })),
            scope: self.graph_scope(),
            view_witnesses: self.witnesses.clone(),
            unavailable_repositories: self.unavailable.clone(),
            revision: 1,
            created_at_ms: now,
            expires_at_ms,
        };
        tx.graph().acquire_graph_pin(&pin)?;
        Ok(id)
    }

    /// Historical evidence may be read after a baseline is superseded. Missing
    /// memberships and expired overlays make the whole saved report unavailable.
    pub(super) fn check_saved(
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        scope: &DataScope,
        run: &StoredAnalysisRun,
        now: i64,
    ) -> Result<(), Failure> {
        if lease::scope_digest(scope) != lease::scope_digest(&run.intent.scope)
            || run.owner.restore_epoch != ctx.restore_epoch().get()
        {
            return Err(unavailable());
        }
        for view in &run.intent.views {
            scope::authorize(read, ctx, scope, &view.repository_id).map_err(hide_view_identity)?;
            if let Some(worktree) = view.snapshot.worktree_id.as_ref() {
                let bound = scope::owned(read, ctx)?.ok_or_else(unavailable)?;
                if &bound.id != worktree || bound.repository_id != view.repository_id {
                    return Err(unavailable());
                }
            }
        }
        for missing in &run.intent.unavailable {
            scope::authorize(read, ctx, scope, &missing.repository_id)
                .map_err(hide_view_identity)?;
        }
        if run
            .intent
            .witnesses
            .iter()
            .any(|witness| witness.expires_at_ms.is_some_and(|expires| expires <= now))
        {
            return Err(unavailable());
        }
        Ok(())
    }

    pub(super) fn witnesses(&self) -> &[GraphViewWitness] {
        &self.witnesses
    }
}

pub(super) fn digest(value: &serde_json::Value) -> String {
    sha256_hex(value.to_string().as_bytes())
}

/// One inaccessible member cannot reveal the presence or identity of other
/// constituents of a saved view set.
fn hide_view_identity(error: Failure) -> Failure {
    match error {
        Failure::Refused(r)
            if matches!(
                r.code,
                DomainErrorCode::NotFound
                    | DomainErrorCode::PermissionDenied
                    | DomainErrorCode::ViewUnavailable
            ) =>
        {
            unavailable()
        }
        other => other,
    }
}
