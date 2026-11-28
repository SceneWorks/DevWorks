//! One query text embedded through the qualified route under the admitted query's own
//! custody: the selection witness is checked and the space recorded in one short write, the
//! exact input is captured, and the routed producer is asked once. Document and Knowledge
//! searches share it; neither writes another path to the producer.
use super::{
    IndexService,
    configuration::resolved_configuration,
    embedding::{EmbeddingFailure, EmbeddingOrigin, EmbeddingRequest},
    preparation::InputPreparation,
};
use crate::{
    context::InvocationContext,
    gateway::AdmittedQuery,
    ports::storage::{Cancellation, StorageError, StoredConventionEmbeddingSpace, WriteOptions},
    services::{kernel::Failure, router::embedding_spaces},
};

impl IndexService {
    /// The vector of `text` in `selected`, or `None` when the route is unavailable or its
    /// outcome unknown (the caller decides whether lexical results stand in). A selection that
    /// changed since `configuration_digest` was read expires the request.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn embed_query_text(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        origin: EmbeddingOrigin,
        selected: &StoredConventionEmbeddingSpace,
        configuration_digest: &str,
        text: &str,
        cancellation: Cancellation,
        deadline_ms: Option<i64>,
    ) -> Result<Option<Vec<f32>>, Failure> {
        let (context, authorization, expected) = (
            ctx.clone(),
            query.authorization().clone(),
            configuration_digest.to_owned(),
        );
        let (registered, index) = (selected.clone(), self.native_available());
        self.write_options(
            WriteOptions {
                cancellation: Some(cancellation.clone()),
                deadline_ms,
                ..WriteOptions::host_state()
            },
            move |tx| {
                if !crate::services::fences::authorization_holds_in(tx, &context, &authorization)? {
                    return Err(StorageError::ScopeDenied);
                }
                let resolved = embedding_spaces::resolve_selected_in(tx, &context)?;
                let actual = resolved_configuration(&resolved, index);
                if actual.source_digest != expected || actual.space.as_ref() != Some(&registered) {
                    return Err(StorageError::CursorExpired);
                }
                tx.conventions().put_convention_embedding_space(&registered)
            },
        )?;
        let inputs = self.prepare_embedding_inputs(InputPreparation {
            context: ctx,
            origin: &origin,
            embedding_space: &selected.id,
            configuration_source_digest: configuration_digest,
            texts: &[text],
            fences: query.authorization().fences(),
            cancellation: &cancellation,
            deadline_ms,
        })?;
        let request_id = inputs.request_id.clone();
        let outcome = self.embed_batch(
            &inputs.context,
            EmbeddingRequest {
                request_id: request_id.clone(),
                origin,
                space: selected.clone(),
                configuration_source_digest: configuration_digest.to_owned(),
                preparation_operation: inputs.operation_id,
                input_refs: inputs.input_refs,
                fences: query.authorization().fences().to_vec(),
                cancellation,
                deadline_ms,
            },
        );
        match outcome {
            Ok(embedding)
                if embedding.request_id == request_id
                    && embedding.space == *selected
                    && embedding.vectors.len() == 1 =>
            {
                Ok(embedding.vectors.into_iter().next())
            }
            Err(EmbeddingFailure::Cancelled) => Err(StorageError::Cancelled.into()),
            Err(EmbeddingFailure::Storage(error)) => Err(error.into()),
            Err(
                EmbeddingFailure::Unavailable(_)
                | EmbeddingFailure::InvalidVectors { .. }
                | EmbeddingFailure::Unknown { .. },
            ) => Ok(None),
            _ => Err(StorageError::InvalidRecord("query embedding custody".into()).into()),
        }
    }
}
