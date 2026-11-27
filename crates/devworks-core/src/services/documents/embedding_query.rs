//! Document query embeddings use the same qualified route and durable producer custody as
//! corpus indexing. Preparation finishes before the gateway opens its ranking snapshot.
use super::{retrieval, version_ref};
use crate::{
    context::InvocationContext,
    gateway::AdmittedQuery,
    ports::{
        Preparation,
        storage::{ReadOptions, ReadSnapshot, StorageError, WriteOptions},
    },
    protocol::{
        DomainErrorCode,
        data::{EmbeddingSpace, RetrievalInput, RetrievalMode, RetrievalRequest},
        provider::ArtifactRef,
    },
    services::{
        artifacts::{ArtifactService, Viewer, lifecycle::ExactReadRefusal, visible},
        conventions::indexing::{
            IndexService,
            configuration::resolved_configuration,
            embedding::{EmbeddingFailure, EmbeddingOrigin, EmbeddingRequest},
            preparation::InputPreparation,
        },
        documents::indexing::same_source,
        kernel::{Failure, Refusal},
        router::embedding_spaces,
    },
};

pub(super) struct PreparedSearch {
    pub deadline_ms: i64,
    pub dense: Option<QueryEmbedding>,
}

pub(super) struct QueryEmbedding {
    pub space: EmbeddingSpace,
    pub vector: Vec<f32>,
    configuration_digest: String,
    source: Option<ArtifactRef>,
}

impl QueryEmbedding {
    /// A prepared vector is meaningful only under the same qualified selection. An uploaded
    /// query vector also remains an exact, visible artifact before and after ranking.
    pub fn check(&self, read: &dyn ReadSnapshot, ctx: &InvocationContext) -> Result<(), Failure> {
        let resolved = embedding_spaces::resolve_selected_read(read, ctx)?;
        if resolved_configuration(&resolved, true).source_digest != self.configuration_digest
            || resolved.binding().map(|binding| binding.space()) != Some(&self.space)
        {
            return Err(StorageError::CursorExpired.into());
        }
        if let Some(source) = &self.source {
            let artifacts = read.artifacts();
            let viewer = Viewer::resolve(read.authority(), ctx)?;
            let Some(artifact) = artifacts.artifact(&source.artifact_id)? else {
                return Err(unavailable().into());
            };
            if !matches!(artifact.state, crate::ports::storage::ArtifactState::Live)
                || !visible(artifacts, &viewer, &artifact)?
                || !artifacts
                    .version(&source.version_id)?
                    .is_some_and(|version| {
                        version.live() && same_source(&version_ref(&version), source)
                    })
            {
                return Err(unavailable().into());
            }
        }
        Ok(())
    }
}

pub(super) fn prepare(
    runtime: Option<&IndexService>,
    artifacts: &ArtifactService,
    ctx: &InvocationContext,
    query: &AdmittedQuery,
    request: &RetrievalRequest,
    env: &Preparation<'_>,
    deadline_ms: i64,
) -> Result<PreparedSearch, Failure> {
    retrieval::validate_search(request)?;
    let mut prepared = PreparedSearch {
        deadline_ms,
        dense: None,
    };
    if matches!(request.mode, RetrievalMode::Lexical) {
        return Ok(prepared);
    }
    let allow_fallback =
        request.lexical_fallback && matches!(request.input, RetrievalInput::Text { .. });
    let Some(runtime) = runtime.filter(|runtime| !runtime.is_stopped()) else {
        return if allow_fallback {
            Ok(prepared)
        } else {
            Err(unavailable().into())
        };
    };
    let env = Preparation::new(runtime.store.as_ref(), env.cancellation, Some(deadline_ms));
    let origin = EmbeddingOrigin::Query {
        query_id: query.query_id().0.clone(),
        fingerprint: query.fingerprint().as_str().into(),
        scope_digest: query.fingerprint().as_str().into(),
    };
    let (configuration, space, searchable) = env.read(|read| {
        Ok((|| {
            if !crate::services::fences::authorization_holds(read, query.authorization())? {
                return Err(StorageError::ScopeDenied.into());
            }
            let (scope, viewer) = retrieval::audience(read, ctx, request)?;
            let exact = retrieval::exact_indexes(read, &viewer, &scope, &request.versions)?;
            let resolved = embedding_spaces::resolve_selected_read(read, ctx)?;
            let configuration = resolved_configuration(
                &resolved,
                matches!(request.input, RetrievalInput::Vector { .. })
                    || runtime.native_available(),
            );
            runtime.validate_query_custody_in(read, ctx, &origin, &configuration.source_digest)?;
            let space = resolved.binding().map(|binding| binding.space().clone());
            let searchable = match (&configuration.space, &space) {
                (Some(_), Some(space)) if matches!(request.input, RetrievalInput::Text { .. }) => {
                    read.documents().has_document_vectors(
                        &scope,
                        (!viewer.sees_owned(None)).then_some(ctx.principal()),
                        exact.as_deref(),
                        &space.space_id,
                    )?
                }
                _ => false,
            };
            Ok::<_, Failure>((configuration, space, searchable))
        })())
    })??;
    let (Some(selected), Some(space)) = (configuration.space, space) else {
        return if allow_fallback {
            Ok(prepared)
        } else {
            Err(unavailable().into())
        };
    };
    let (vector, source) = match &request.input {
        RetrievalInput::Vector {
            space: supplied,
            float32_le,
        } => {
            if supplied != &space {
                return Err(Refusal::new(
                    DomainErrorCode::InvalidArgument,
                    "document_embedding_space",
                    None,
                )
                .into());
            }
            let (bytes, _) = artifacts
                .read_exact_with_options(
                    runtime.store.as_ref(),
                    ctx,
                    float32_le,
                    u64::from(space.dimensions) * 4,
                    &ReadOptions {
                        cancellation: env.cancellation.cloned(),
                        deadline_ms: Some(deadline_ms),
                    },
                )
                .map_err(|error| match error {
                    ExactReadRefusal::Storage(error) => Failure::Storage(error),
                    _ => unavailable().into(),
                })?;
            let (chunks, trailing) = bytes.as_chunks::<4>();
            if !trailing.is_empty() {
                return Err(Refusal::new(
                    DomainErrorCode::InvalidArgument,
                    "document_query_vector",
                    None,
                )
                .into());
            }
            let vector = chunks
                .iter()
                .copied()
                .map(f32::from_le_bytes)
                .collect::<Vec<_>>();
            (vector, Some(float32_le.clone()))
        }
        RetrievalInput::Text { text } if searchable => {
            let cancel = env.cancellation.cloned().unwrap_or_default();
            let (context, authorization, expected) = (
                ctx.clone(),
                query.authorization().clone(),
                configuration.source_digest.clone(),
            );
            let (registered, index) = (selected.clone(), runtime.native_available());
            runtime.write_options(
                WriteOptions {
                    cancellation: Some(cancel.clone()),
                    deadline_ms: Some(deadline_ms),
                    ..WriteOptions::host_state()
                },
                move |tx| {
                    if !crate::services::fences::authorization_holds_in(
                        tx,
                        &context,
                        &authorization,
                    )? {
                        return Err(StorageError::ScopeDenied);
                    }
                    let resolved = embedding_spaces::resolve_selected_in(tx, &context)?;
                    let actual = resolved_configuration(&resolved, index);
                    if actual.source_digest != expected
                        || actual.space.as_ref() != Some(&registered)
                    {
                        return Err(StorageError::CursorExpired);
                    }
                    tx.conventions().put_convention_embedding_space(&registered)
                },
            )?;
            let inputs = runtime.prepare_embedding_inputs(InputPreparation {
                context: ctx,
                origin: &origin,
                embedding_space: &selected.id,
                configuration_source_digest: &configuration.source_digest,
                texts: &[text.as_str()],
                fences: query.authorization().fences(),
                cancellation: &cancel,
                deadline_ms: Some(deadline_ms),
            })?;
            let request_id = inputs.request_id.clone();
            let outcome = runtime.embed_batch(
                &inputs.context,
                EmbeddingRequest {
                    request_id: request_id.clone(),
                    origin,
                    space: selected.clone(),
                    configuration_source_digest: configuration.source_digest.clone(),
                    preparation_operation: inputs.operation_id,
                    input_refs: inputs.input_refs,
                    fences: query.authorization().fences().to_vec(),
                    cancellation: cancel,
                    deadline_ms: Some(deadline_ms),
                },
            );
            match outcome {
                Ok(embedding)
                    if embedding.request_id == request_id
                        && embedding.space == selected
                        && embedding.vectors.len() == 1 =>
                {
                    (
                        embedding.vectors.into_iter().next().expect("one vector"),
                        None,
                    )
                }
                Err(EmbeddingFailure::Cancelled) => return Err(StorageError::Cancelled.into()),
                Err(EmbeddingFailure::Storage(error)) => return Err(error.into()),
                Err(EmbeddingFailure::Unavailable(_) | EmbeddingFailure::Unknown { .. })
                    if allow_fallback =>
                {
                    return Ok(prepared);
                }
                Err(EmbeddingFailure::Unavailable(_) | EmbeddingFailure::Unknown { .. }) => {
                    return Err(unavailable().into());
                }
                _ => {
                    return Err(StorageError::InvalidRecord(
                        "document query embedding custody".into(),
                    )
                    .into());
                }
            }
        }
        RetrievalInput::Text { .. } => {
            return if allow_fallback {
                Ok(prepared)
            } else {
                Err(unavailable().into())
            };
        }
    };
    if vector.len() != space.dimensions as usize
        || vector.iter().any(|value| !value.is_finite())
        || vector.iter().all(|value| *value == 0.0)
    {
        return Err(Refusal::new(
            DomainErrorCode::InvalidArgument,
            "document_query_vector",
            None,
        )
        .into());
    }
    prepared.dense = Some(QueryEmbedding {
        space,
        vector,
        configuration_digest: configuration.source_digest,
        source,
    });
    Ok(prepared)
}

fn unavailable() -> Refusal {
    Refusal::new(DomainErrorCode::ViewUnavailable, "document_embedding", None)
}
