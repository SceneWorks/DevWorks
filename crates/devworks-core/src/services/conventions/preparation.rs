//! Exact, caller-authorized source captures. Local metadata decisions do no file I/O.
use super::{ConventionService, access, hierarchy, model};
use crate::{
    context::InvocationContext,
    operations::conventions as ops,
    ports::{Preparation, storage::*},
    protocol::{provider::ArtifactRef, records::*},
    services::{
        artifacts::{ConventionSourceLease, ConventionSourceSelection},
        sessions::{Failure, Refusal},
    },
};
use std::sync::Arc;

pub(super) struct SourceCapture {
    pub reference: ArtifactRef,
    pub lease: Arc<ConventionSourceLease>,
    pub upload: bool,
}
pub(super) struct PreparedEdit {
    pub owner: StoredConvention,
    pub sources: Vec<SourceCapture>,
}
pub(super) struct PreparedMove {
    pub owner: StoredConvention,
    pub source_revision: i64,
    pub destination_revision: i64,
    pub witness: hierarchy::SubtreeWitness,
}
fn invalid(message: &'static str) -> Failure {
    Refusal::new(
        crate::protocol::DomainErrorCode::InvalidArgument,
        message,
        None,
    )
    .into()
}
fn same(value: Option<&serde_json::Value>, reference: &ArtifactRef) -> bool {
    value.is_some_and(|v| serde_json::to_value(reference).ok().as_ref() == Some(v))
}
impl ConventionService {
    fn capture(
        &self,
        ctx: &InvocationContext,
        env: &Preparation<'_>,
        owner: &StoredConvention,
        reference: &ArtifactRef,
        text: bool,
    ) -> Result<SourceCapture, Failure> {
        if text && reference.byte_length.get() as u64 > model::MAX_MARKDOWN_BYTES {
            return Err(invalid("convention_source_size"));
        }
        let (lease, upload) = env.read(|read| {
            Ok((|| {
                let selection = if same(owner.content_ref.as_ref(), reference) {
                    Some(ConventionSourceSelection::Markdown)
                } else if same(owner.raw_content_ref.as_ref(), reference) {
                    Some(ConventionSourceSelection::Raw)
                } else {
                    None
                };
                if let Some(selection) = selection {
                    Ok::<_, Failure>((
                        self.artifacts
                            .resolve_convention_source(read, ctx, None, owner, selection)?,
                        false,
                    ))
                } else {
                    let mut after = None;
                    loop {
                        if env.stopped(self.clock.now_ms().get()) {
                            return Err(StorageError::Cancelled.into());
                        }
                        let rows = read.conventions().convention_resources(
                            &ConventionResourceQuery {
                                scopes: vec![owner.scope.clone()],
                                convention_id: Some(owner.id.clone()),
                                convention_revision: Some(owner.revision),
                                lifecycles: vec!["Active".into(), "Retired".into()],
                            },
                            after.as_ref(),
                            32,
                        )?;
                        if rows.is_empty() {
                            break;
                        }
                        after = rows.last().map(|r| r.id.clone());
                        if let Some(resource) =
                            rows.iter().find(|r| same(Some(&r.content_ref), reference))
                        {
                            return Ok((
                                self.artifacts.resolve_convention_source(
                                    read,
                                    ctx,
                                    None,
                                    owner,
                                    ConventionSourceSelection::Resource(resource),
                                )?,
                                false,
                            ));
                        }
                    }
                    let scoped = access::scope(read, ctx, None, false)?;
                    let mut before = None;
                    loop {
                        if env.stopped(self.clock.now_ms().get()) {
                            return Err(StorageError::Cancelled.into());
                        }
                        let rows = read.conventions().convention_versions(
                            &owner.id,
                            &scoped.readable,
                            before,
                            32,
                        )?;
                        if rows.is_empty() {
                            break;
                        }
                        before = rows.last().map(|r| r.revision);
                        for prior in rows {
                            let selection = if same(prior.content_ref.as_ref(), reference) {
                                Some(ConventionSourceSelection::Markdown)
                            } else if same(prior.raw_content_ref.as_ref(), reference) {
                                Some(ConventionSourceSelection::Raw)
                            } else {
                                None
                            };
                            if let Some(selection) = selection {
                                return Ok((
                                    self.artifacts.resolve_convention_source(
                                        read, ctx, None, &prior, selection,
                                    )?,
                                    false,
                                ));
                            }
                        }
                    }
                    Ok((
                        self.artifacts
                            .resolve_convention_upload(read, ctx, reference)?,
                        true,
                    ))
                }
            })())
        })??;
        if text {
            let mut offset = 0_u64;
            let mut pending = Vec::new();
            while offset < lease.version.byte_count as u64 {
                if self.is_stopped() || env.stopped(self.clock.now_ms().get()) {
                    return Err(StorageError::Cancelled.into());
                }
                let bytes = lease
                    .read_range(offset, 65536)
                    .map_err(|_| invalid("convention_source"))?;
                if bytes.is_empty() {
                    return Err(invalid("convention_source"));
                }
                offset += bytes.len() as u64;
                pending.extend_from_slice(&bytes);
                match std::str::from_utf8(&pending) {
                    Ok(_) => pending.clear(),
                    Err(e) if e.error_len().is_none() => {
                        pending.drain(..e.valid_up_to());
                    }
                    Err(_) => return Err(invalid("convention_utf8")),
                }
            }
            if !pending.is_empty() {
                return Err(invalid("convention_utf8"));
            }
        } else {
            let _ = lease
                .read_range(0, 1)
                .map_err(|_| invalid("convention_source"))?;
        }
        Ok(SourceCapture {
            reference: reference.clone(),
            lease: Arc::new(lease),
            upload,
        })
    }
    pub(super) fn prepare_edit(
        &self,
        ctx: &InvocationContext,
        env: &Preparation<'_>,
        input: &ops::EditInput,
    ) -> Result<PreparedEdit, Failure> {
        let owner = env.read(|read| {
            Ok(access::owner(
                read,
                ctx,
                &input.convention_id,
                None,
                input.scope.as_ref(),
            ))
        })??;
        let mut sources = vec![self.capture(ctx, env, &owner, &input.content, true)?];
        if let Some(ConventionRawChange::Set { content }) = input.raw_content.as_ref() {
            sources.push(self.capture(ctx, env, &owner, content, true)?);
        }
        if let Some(changes) = input.resources.as_ref() {
            for change in &changes.0 {
                match change {
                    ConventionResourceChange::Attach { content, .. } => {
                        sources.push(self.capture(ctx, env, &owner, content, false)?)
                    }
                    ConventionResourceChange::Update {
                        content: Some(content),
                        ..
                    } => sources.push(self.capture(ctx, env, &owner, content, false)?),
                    _ => {}
                }
            }
        }
        Ok(PreparedEdit { owner, sources })
    }
    pub(super) fn prepare_move(
        &self,
        ctx: &InvocationContext,
        env: &Preparation<'_>,
        input: &ops::MoveInput,
    ) -> Result<PreparedMove, Failure> {
        env.read(|read| {
            Ok((|| {
                let owner =
                    access::owner(read, ctx, &input.convention_id, None, input.scope.as_ref())?;
                let destination = access::scope(read, ctx, Some(&input.destination), true)?;
                let source = access::scope(read, ctx, Some(&model::scope(&owner.scope)), true)?;
                let witness = hierarchy::witness(read.conventions(), &owner)?;
                let mut prospective = owner.clone();
                prospective.scope = destination.exact.clone();
                let parent = input.parent.as_ref().and_then(|p| match p {
                    ConventionParentChange::Root {} => None,
                    ConventionParentChange::Child { parent_id } => Some(parent_id),
                });
                let depth = hierarchy::parent_depth(read.conventions(), &prospective, parent)?;
                if depth + witness.height > model::MAX_DEPTH {
                    return Err(invalid("convention_depth"));
                }
                Ok(PreparedMove {
                    owner,
                    source_revision: read
                        .conventions()
                        .convention_scope_revision(&source.exact)?,
                    destination_revision: read
                        .conventions()
                        .convention_scope_revision(&destination.exact)?,
                    witness,
                })
            })())
        })?
    }
}
