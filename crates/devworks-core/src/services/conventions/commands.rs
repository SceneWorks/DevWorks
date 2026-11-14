//! Revisioned metadata changes and exact source retention in the command's deciding unit.
use super::{
    ConventionService, access, hierarchy, model,
    preparation::{PreparedEdit, PreparedMove},
};
use crate::{
    context::InvocationContext,
    gateway::AdmittedCommand,
    operations::conventions as ops,
    ports::{IdKind, storage::*},
    protocol::{
        DomainErrorCode, EventId, Id, Revision, RevisionDimension,
        provider::ArtifactRef,
        records::*,
        results::{DomainDecision, OperationResult},
    },
    services::sessions::{Failure, Refusal, rev},
};
use serde_json::{Value, json};
fn invalid(kind: &'static str) -> Failure {
    Refusal::new(DomainErrorCode::InvalidArgument, kind, None).into()
}
fn value<T: serde::Serialize>(value: &T) -> Result<Value, Failure> {
    serde_json::to_value(value).map_err(|e| StorageError::InvalidRecord(e.to_string()).into())
}
fn owner(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    id: &Id,
    expected: &Revision,
    scope: Option<&crate::protocol::data::DataScope>,
) -> Result<StoredConvention, Failure> {
    let scoped = access::scope_in(tx, ctx, scope, false)?;
    let row = tx
        .conventions()
        .convention(id, None, &scoped.readable)?
        .ok_or_else(|| crate::services::sessions::not_found("convention", id))?;
    access::writable(tx, ctx, &row.scope)?;
    if row.revision != expected.get() {
        return Err(StorageError::conflict(
            RevisionDimension::Source,
            "convention",
            id.clone(),
            expected.clone(),
            rev(row.revision),
        )
        .into());
    }
    if row.lifecycle == "Deleted" {
        return Err(invalid("deleted_convention"));
    }
    Ok(row)
}
fn event(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    row: &StoredConvention,
    kind: &str,
    payload: Value,
) -> Result<(), StorageError> {
    let id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id,
        actor: ctx.principal().clone(),
        project_id: row.scope.project().cloned(),
        root_id: ctx.root_id().cloned(),
        operation_id: None,
        aggregate_kind: "convention".into(),
        aggregate_id: row.id.as_str().to_owned(),
        aggregate_revision: row.revision,
        kind: kind.into(),
        payload,
    })?;
    Ok(())
}
fn resource(
    tx: &mut dyn WriteTransaction,
    row: &StoredConvention,
    id: &Id,
) -> Result<StoredConventionResource, Failure> {
    tx.conventions()
        .convention_resource(
            &row.id,
            id,
            Some(row.revision),
            std::slice::from_ref(&row.scope),
        )?
        .ok_or_else(|| crate::services::sessions::not_found("convention_resource", id).into())
}
impl ConventionService {
    fn retain(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        owner: &StoredConvention,
        reference: &ArtifactRef,
        prepared: &PreparedEdit,
        binary: bool,
    ) -> Result<Value, Failure> {
        let reference_value = value(reference)?;
        if owner.content_ref.as_ref() == Some(&reference_value)
            || owner.raw_content_ref.as_ref() == Some(&reference_value)
        {
            return Ok(reference_value);
        }
        let capture = prepared
            .sources
            .iter()
            .find(|s| {
                s.reference.artifact_id == reference.artifact_id
                    && s.reference.version_id == reference.version_id
                    && s.reference.sha256.as_str() == reference.sha256.as_str()
                    && s.reference.byte_length == reference.byte_length
                    && s.reference.media_type == reference.media_type
            })
            .ok_or_else(|| invalid("convention_source_capture"))?;
        if capture.upload {
            self.artifacts
                .revalidate_convention_upload(tx, ctx, &capture.lease.version)?;
        }
        value(&self.artifacts.retain_convention_version(
            tx,
            &capture.lease.version,
            &owner.scope,
            &owner.id,
            binary,
        )?)
    }
    fn write(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        old: &StoredConvention,
        mut row: StoredConvention,
        kind: &str,
    ) -> Result<StoredConvention, Failure> {
        row.revision = old.revision + 1;
        row.author_id = ctx.principal().clone();
        row.updated_at_ms = tx.now_ms();
        tx.conventions().revise_convention(&row, old.revision)?;
        tx.conventions().bump_convention_scope(&row.scope)?;
        event(
            tx,
            ctx,
            &row,
            kind,
            json!({"version":1,"previousRevision":old.revision,"previousScope":crate::protocol::data::DataScope::from(&old.scope),"previousParentId":old.parent_id}),
        )?;
        Ok(row)
    }
    fn record_result(
        &self,
        tx: &mut dyn WriteTransaction,
        row: &StoredConvention,
    ) -> Result<ConventionRecord, Failure> {
        Ok(model::record(row, self.index.status_in(tx, row)?)?)
    }
    pub(super) fn edit(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
        input: &ops::EditInput,
    ) -> Result<DomainDecision, Failure> {
        let old = owner(
            tx,
            ctx,
            &input.convention_id,
            &input.expected_revision,
            input.scope.as_ref(),
        )?;
        let prepared = command
            .prepared::<PreparedEdit>()
            .ok_or_else(|| invalid("convention_preparation"))?;
        if prepared.owner != old {
            return Err(invalid("convention_source_changed"));
        }
        let mut row = old.clone();
        row.content_ref = Some(self.retain(tx, ctx, &old, &input.content, prepared, false)?);
        if let Some(title) = input.title.as_ref() {
            row.title = title.0.clone();
        }
        if let Some(slug) = input.slug.as_ref() {
            row.slug = slug.0.clone();
        }
        if let Some(parent) = input.parent.as_ref() {
            row.parent_id = match parent {
                ConventionParentChange::Root {} => None,
                ConventionParentChange::Child { parent_id } => Some(parent_id.clone()),
            };
        }
        if let Some(sort) = input.sort_order.as_ref() {
            row.sort_order = sort.0.parse().map_err(|_| invalid("convention_order"))?;
        }
        if let Some(raw) = input.raw_content.as_ref() {
            row.raw_content_ref = match raw {
                ConventionRawChange::Remove {} => None,
                ConventionRawChange::Set { content } => {
                    Some(self.retain(tx, ctx, &old, content, prepared, false)?)
                }
            };
        }
        let depth = hierarchy::parent_depth(tx.conventions(), &row, row.parent_id.as_ref())?;
        if depth + hierarchy::witness(tx.conventions(), &old)?.height > model::MAX_DEPTH {
            return Err(invalid("convention_depth"));
        }
        let changed_body = row.content_ref != old.content_ref
            || row.title != old.title
            || row.parent_id != old.parent_id;
        // Prepare all resource source retention before committing the owner snapshot. The
        // resource revisions themselves follow that snapshot in the same unit of work.
        let mut changes = Vec::new();
        if let Some(resources) = input.resources.as_ref() {
            for change in &resources.0 {
                match change {
                    ConventionResourceChange::Attach {
                        name,
                        content,
                        media_type,
                        ..
                    } => changes.push((
                        None,
                        name.0.clone(),
                        self.retain(tx, ctx, &old, content, prepared, true)?,
                        "Active".to_owned(),
                        media_type
                            .as_ref()
                            .map(|s| s.0.clone())
                            .unwrap_or_else(|| content.media_type.clone()),
                    )),
                    ConventionResourceChange::Update {
                        resource_id,
                        expected_revision,
                        name,
                        content,
                        lifecycle,
                        media_type,
                        ..
                    } => {
                        let previous = resource(tx, &old, resource_id)?;
                        if previous.revision != expected_revision.get() {
                            return Err(StorageError::conflict(
                                RevisionDimension::Source,
                                "convention_resource",
                                resource_id.clone(),
                                expected_revision.clone(),
                                rev(previous.revision),
                            )
                            .into());
                        }
                        if previous.lifecycle == "Deleted" {
                            return Err(invalid("deleted_convention_resource"));
                        }
                        let reference = content
                            .as_ref()
                            .map(|c| self.retain(tx, ctx, &old, c, prepared, true))
                            .transpose()?
                            .unwrap_or_else(|| previous.content_ref.clone());
                        let state = lifecycle
                            .as_ref()
                            .map(|s| {
                                serde_json::to_value(s)
                                    .expect("closed lifecycle")
                                    .as_str()
                                    .expect("enum")
                                    .to_owned()
                            })
                            .unwrap_or_else(|| previous.lifecycle.clone());
                        changes.push((
                            Some(previous.clone()),
                            name.as_ref()
                                .map(|s| s.0.clone())
                                .unwrap_or_else(|| previous.name.clone()),
                            reference,
                            state,
                            media_type
                                .as_ref()
                                .map(|s| s.0.clone())
                                .unwrap_or(previous.media_type),
                        ));
                    }
                }
            }
        }
        row = self.write(tx, ctx, &old, row, "ConventionEdited")?;
        for (previous, name, content_ref, lifecycle, media_type) in changes {
            let deleted = (lifecycle == "Deleted").then(|| tx.now_ms());
            let resource = StoredConventionResource {
                id: previous
                    .as_ref()
                    .map(|p| p.id.clone())
                    .unwrap_or_else(|| tx.new_id(IdKind::ConventionResource)),
                convention_id: row.id.clone(),
                revision: previous.as_ref().map_or(1, |p| p.revision + 1),
                convention_revision: row.revision,
                name,
                media_type,
                content_ref,
                author_id: ctx.principal().clone(),
                created_at_ms: previous.as_ref().map_or(tx.now_ms(), |p| p.created_at_ms),
                deleted_at_ms: deleted,
                lifecycle,
            };
            if let Some(previous) = previous {
                tx.conventions().revise_convention_resource(
                    &resource,
                    previous.revision,
                    row.revision,
                )?;
            } else {
                tx.conventions()
                    .add_convention_resource(&resource, row.revision)?;
            }
        }
        if changed_body && row.lifecycle == "Active" {
            self.index.record_changed(tx, ctx, &row)?;
        }
        Ok(DomainDecision::Applied {
            result: OperationResult::ConventionsEdit {
                value: ConventionRecordResult::Current(self.record_result(tx, &row)?),
            },
        })
    }
    pub(super) fn applicability(
        &self,
        ctx: &InvocationContext,
        tx: &mut dyn WriteTransaction,
        input: &ops::ApplicabilityInput,
    ) -> Result<DomainDecision, Failure> {
        let old = owner(
            tx,
            ctx,
            &input.convention_id,
            &input.expected_revision,
            input.scope.as_ref(),
        )?;
        let mut row = old.clone();
        row.applicability = input.applicability.0.iter().map(|s| s.0.clone()).collect();
        if let Some(state) = input.lifecycle.as_ref() {
            row.lifecycle = serde_json::to_value(state)
                .expect("closed state")
                .as_str()
                .expect("enum")
                .to_owned();
        }
        let row = self.write(tx, ctx, &old, row, "ConventionApplicabilityChanged")?;
        if old.lifecycle != "Active" && row.lifecycle == "Active" {
            self.index.record_changed(tx, ctx, &row)?;
        }
        Ok(DomainDecision::Applied {
            result: OperationResult::ConventionsApplicability {
                value: ConventionRecordResult::Current(self.record_result(tx, &row)?),
            },
        })
    }
    pub(super) fn retire(
        &self,
        ctx: &InvocationContext,
        tx: &mut dyn WriteTransaction,
        input: &ops::RetireInput,
    ) -> Result<DomainDecision, Failure> {
        if input.resource_id.is_some() {
            return Err(Refusal::new(
                DomainErrorCode::UnsupportedCapability,
                "convention_resource_index",
                None,
            )
            .into());
        }
        let id = input
            .convention_id
            .as_ref()
            .ok_or_else(|| invalid("convention_target"))?;
        let old = owner(tx, ctx, id, &input.expected_revision, input.scope.as_ref())?;
        let mut row = old.clone();
        row.lifecycle = "Retired".into();
        let row = self.write(tx, ctx, &old, row, "ConventionRetired")?;
        let convention = self.record_result(tx, &row)?;
        Ok(DomainDecision::Applied {
            result: OperationResult::ConventionsRetire {
                value: ConventionMutationResult::Current(ConventionMutation {
                    id: row.id,
                    revision: rev(row.revision),
                    changed: true,
                    convention: Some(convention),
                    resource: None,
                    operation: None,
                }),
            },
        })
    }
    pub(super) fn delete(
        &self,
        ctx: &InvocationContext,
        tx: &mut dyn WriteTransaction,
        input: &ops::DeleteInput,
    ) -> Result<DomainDecision, Failure> {
        self.delete_target(
            ctx,
            tx,
            DeleteTarget {
                id: &input.convention_id,
                expected: &input.expected_revision,
                scope: input.scope.as_ref(),
                resource: input.resource_id.as_ref(),
                resource_operation: false,
            },
        )
    }
    pub(super) fn delete_resource(
        &self,
        ctx: &InvocationContext,
        tx: &mut dyn WriteTransaction,
        input: &ops::DeleteResourceInput,
    ) -> Result<DomainDecision, Failure> {
        let resource = input
            .resource_id
            .as_ref()
            .ok_or_else(|| invalid("convention_resource"))?;
        self.delete_target(
            ctx,
            tx,
            DeleteTarget {
                id: &input.convention_id,
                expected: &input.expected_revision,
                scope: input.scope.as_ref(),
                resource: Some(resource),
                resource_operation: true,
            },
        )
    }
}

struct DeleteTarget<'a> {
    id: &'a Id,
    expected: &'a Revision,
    scope: Option<&'a crate::protocol::data::DataScope>,
    resource: Option<&'a Id>,
    resource_operation: bool,
}
impl ConventionService {
    fn delete_target(
        &self,
        ctx: &InvocationContext,
        tx: &mut dyn WriteTransaction,
        target: DeleteTarget<'_>,
    ) -> Result<DomainDecision, Failure> {
        let old = owner(tx, ctx, target.id, target.expected, target.scope)?;
        let (row, deleted) = if let Some(id) = target.resource {
            let old_resource = resource(tx, &old, id)?;
            if old_resource.lifecycle == "Deleted" {
                return Err(invalid("deleted_convention_resource"));
            }
            let row = self.write(tx, ctx, &old, old.clone(), "ConventionAttachmentDeleted")?;
            let mut deleted = old_resource.clone();
            deleted.revision += 1;
            deleted.convention_revision = row.revision;
            deleted.lifecycle = "Deleted".into();
            deleted.deleted_at_ms = Some(tx.now_ms());
            deleted.author_id = ctx.principal().clone();
            tx.conventions().revise_convention_resource(
                &deleted,
                old_resource.revision,
                row.revision,
            )?;
            (row, Some(deleted))
        } else {
            (self.delete_tree(tx, ctx, &old)?, None)
        };
        let convention = self.record_result(tx, &row)?;
        let resource = deleted
            .as_ref()
            .map(|r| model::resource(r, &row))
            .transpose()?;
        let result = ConventionMutationResult::Current(ConventionMutation {
            id: target.id.clone(),
            revision: rev(row.revision),
            changed: true,
            convention: Some(convention),
            resource,
            operation: None,
        });
        Ok(DomainDecision::Applied {
            result: if target.resource_operation {
                OperationResult::ConventionsDeleteResource { value: result }
            } else {
                OperationResult::ConventionsDelete { value: result }
            },
        })
    }
    fn delete_tree(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        old: &StoredConvention,
    ) -> Result<StoredConvention, Failure> {
        let query = ConventionQuery {
            scopes: vec![old.scope.clone()],
            lifecycles: vec!["Active".into(), "Retired".into()],
            parent_id: Some(Some(old.id.clone())),
            ..Default::default()
        };
        let mut after = None;
        loop {
            let children = tx.conventions().conventions(&query, after.as_ref(), 32)?;
            if children.is_empty() {
                break;
            }
            after = children.last().map(StoredConvention::position);
            for child in children {
                self.delete_tree(tx, ctx, &child)?;
            }
        }
        let mut row = old.clone();
        row.lifecycle = "Deleted".into();
        row.content_ref = None;
        row.raw_content_ref = None;
        let row = self.write(tx, ctx, old, row, "ConventionDeleted")?;
        let mut after = None;
        let query = ConventionResourceQuery {
            scopes: vec![old.scope.clone()],
            convention_id: Some(old.id.clone()),
            convention_revision: Some(old.revision),
            lifecycles: vec!["Active".into(), "Retired".into()],
        };
        loop {
            let resources = tx
                .conventions()
                .convention_resources(&query, after.as_ref(), 32)?;
            if resources.is_empty() {
                break;
            }
            after = resources.last().map(|r| r.id.clone());
            for prior in resources {
                let mut deleted = prior.clone();
                deleted.revision += 1;
                deleted.convention_revision = row.revision;
                deleted.lifecycle = "Deleted".into();
                deleted.deleted_at_ms = Some(tx.now_ms());
                deleted.author_id = ctx.principal().clone();
                tx.conventions().revise_convention_resource(
                    &deleted,
                    prior.revision,
                    row.revision,
                )?;
            }
        }
        Ok(row)
    }
    pub(super) fn move_tree(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
        input: &ops::MoveInput,
    ) -> Result<DomainDecision, Failure> {
        let old = owner(
            tx,
            ctx,
            &input.convention_id,
            &input.expected_revision,
            input.scope.as_ref(),
        )?;
        let prepared = command
            .prepared::<PreparedMove>()
            .ok_or_else(|| invalid("convention_preparation"))?;
        let destination = access::scope_in(tx, ctx, Some(&input.destination), true)?.exact;
        if old != prepared.owner
            || tx.conventions().convention_scope_revision(&old.scope)? != prepared.source_revision
            || tx.conventions().convention_scope_revision(&destination)?
                != prepared.destination_revision
            || hierarchy::witness(tx.conventions(), &old)? != prepared.witness
        {
            return Err(Refusal::new(
                DomainErrorCode::SourceChanged,
                "convention_subtree",
                Some(old.id.clone()),
            )
            .into());
        }
        let mut row = old.clone();
        row.scope = destination;
        row.parent_id = input.parent.as_ref().and_then(|p| match p {
            ConventionParentChange::Root {} => None,
            ConventionParentChange::Child { parent_id } => Some(parent_id.clone()),
        });
        if let Some(sort) = input.sort_order.as_ref() {
            row.sort_order = sort.0.parse().map_err(|_| invalid("convention_order"))?;
        }
        let depth = hierarchy::parent_depth(tx.conventions(), &row, row.parent_id.as_ref())?;
        if depth + prepared.witness.height > model::MAX_DEPTH {
            return Err(invalid("convention_depth"));
        }
        let row = self.move_node(tx, ctx, &old, row)?;
        Ok(DomainDecision::Applied {
            result: OperationResult::ConventionsMove {
                value: ConventionRecordResult::Current(self.record_result(tx, &row)?),
            },
        })
    }
    fn relocate_reference(
        &self,
        tx: &mut dyn WriteTransaction,
        reference: &Value,
        row: &StoredConvention,
        binary: bool,
    ) -> Result<Value, Failure> {
        let reference: ArtifactRef = serde_json::from_value(reference.clone())
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
        let version = tx
            .artifacts()
            .version(&reference.version_id)?
            .filter(|v| {
                v.artifact_id == reference.artifact_id
                    && v.blob_digest == reference.sha256.as_str()
                    && v.byte_count == reference.byte_length.get()
                    && v.media_type == reference.media_type
            })
            .ok_or_else(|| invalid("convention_source"))?;
        value(
            &self
                .artifacts
                .retain_convention_version(tx, &version, &row.scope, &row.id, binary)?,
        )
    }
    fn move_node(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        old: &StoredConvention,
        mut row: StoredConvention,
    ) -> Result<StoredConvention, Failure> {
        let cross_scope = old.scope != row.scope;
        if cross_scope {
            row.content_ref = old
                .content_ref
                .as_ref()
                .map(|r| self.relocate_reference(tx, r, &row, false))
                .transpose()?;
            row.raw_content_ref = old
                .raw_content_ref
                .as_ref()
                .map(|r| self.relocate_reference(tx, r, &row, false))
                .transpose()?;
        }
        let row = self.write(tx, ctx, old, row, "ConventionMoved")?;
        if cross_scope {
            tx.conventions().bump_convention_scope(&old.scope)?;
        }
        let query = ConventionResourceQuery {
            scopes: vec![old.scope.clone(), row.scope.clone()],
            convention_id: Some(old.id.clone()),
            convention_revision: Some(old.revision),
            lifecycles: Vec::new(),
        };
        let mut after = None;
        loop {
            let resources = tx
                .conventions()
                .convention_resources(&query, after.as_ref(), 32)?;
            if resources.is_empty() {
                break;
            }
            after = resources.last().map(|r| r.id.clone());
            for prior in resources {
                let mut resource = prior.clone();
                resource.revision += 1;
                resource.convention_revision = row.revision;
                resource.author_id = ctx.principal().clone();
                if cross_scope {
                    resource.content_ref =
                        self.relocate_reference(tx, &prior.content_ref, &row, true)?;
                }
                tx.conventions().revise_convention_resource(
                    &resource,
                    prior.revision,
                    row.revision,
                )?;
            }
        }
        let query = ConventionQuery {
            scopes: vec![old.scope.clone()],
            lifecycles: vec!["Active".into(), "Retired".into()],
            parent_id: Some(Some(old.id.clone())),
            ..Default::default()
        };
        let mut after = None;
        loop {
            let children = tx.conventions().conventions(&query, after.as_ref(), 32)?;
            if children.is_empty() {
                break;
            }
            after = children.last().map(StoredConvention::position);
            for child in children {
                let mut next = child.clone();
                next.scope = row.scope.clone();
                self.move_node(tx, ctx, &child, next)?;
            }
        }
        if row.lifecycle == "Active" {
            self.index.record_changed(tx, ctx, &row)?;
        }
        Ok(row)
    }
}
