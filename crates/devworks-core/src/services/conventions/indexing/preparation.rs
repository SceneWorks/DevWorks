//! Immutable embedding inputs and their actual caller-owned preparation operation. Artifact
//! staging is a phase; the preparation stays Dispatching until the native owner settles.
use super::{IndexService, embedding::EmbeddingOrigin};
use crate::{
    context::InvocationContext,
    fingerprint::sha256_hex,
    ports::{AuthorityFence, storage::*},
    protocol::{Id, OperationId, OperationState, provider::ArtifactRef},
    services::{
        approvals,
        artifacts::{host::HostArtifactPublication, version_ref},
    },
};
use serde_json::{Value, json};

const CAPABILITY: &str = "conventions.embedding_prepare";
#[derive(Clone, Debug)]
pub struct PreparedInputs {
    pub request_id: Id,
    pub operation_id: OperationId,
    pub context: InvocationContext,
    pub input_refs: Vec<ArtifactRef>,
}
/// A batch is captured from an actual admitted query or index operation, never caller wire.
pub struct InputPreparation<'a> {
    pub context: &'a InvocationContext,
    pub origin: &'a EmbeddingOrigin,
    pub embedding_space: &'a str,
    pub configuration_source_digest: &'a str,
    pub texts: &'a [&'a str],
    pub fences: &'a [AuthorityFence],
    pub cancellation: &'a Cancellation,
    pub deadline_ms: Option<i64>,
}
fn invalid(message: &str) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
fn digest(value: &Value) -> Result<String, StorageError> {
    crate::fingerprint::canonical_digest(value).map_err(|_| invalid("embedding identity"))
}
fn query_key(
    ctx: &InvocationContext,
    query_id: &Id,
    fingerprint: &str,
) -> Result<String, StorageError> {
    digest(
        &json!({"storeId":ctx.store_id(),"restoreEpoch":ctx.restore_epoch(),"actor":ctx.principal(),"queryId":query_id,"fingerprint":fingerprint}),
    )
}
fn stable_context(ctx: &InvocationContext) -> Result<Value, StorageError> {
    let mut source = approvals::save_source_context(ctx);
    source["invocationContext"]
        .as_object_mut()
        .ok_or_else(|| invalid("embedding context"))?
        .remove("invocation");
    Ok(source)
}
fn identity(prefix: &str, value: &str) -> Id {
    Id::new(format!("{prefix}{}", &sha256_hex(value.as_bytes())[..40])).expect("host digest ID")
}
fn check(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    fences: &[AuthorityFence],
) -> Result<(), StorageError> {
    if &tx.store().store_id != ctx.store_id()
        || tx.store().restore_epoch != ctx.restore_epoch().get()
        || tx.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
        || !crate::authority::fences_hold(tx, fences)?
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
}
fn captured(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    row: &StoredConventionEmbeddingInputs,
    operation: &OperationId,
    args_digest: &str,
) -> Result<PreparedInputs, StorageError> {
    let op = tx
        .operations()
        .operation(operation)?
        .ok_or_else(|| invalid("missing embedding preparation"))?;
    if op.capability != CAPABILITY
        || op.args_digest != args_digest
        || op.payload["requestId"] != json!(row.request_id)
        || op.payload["inputDigest"] != row.input_digest
        || op.payload["contextDigest"] != row.context_digest
        || op.payload["embeddingSpace"] != row.embedding_space
        || row.actor_id != *ctx.principal()
        || row.project_id.as_ref() != ctx.project_id()
    {
        return Err(StorageError::ScopeDenied);
    }
    let original = approvals::restore_source_context(
        tx.store(),
        ctx.principal(),
        ctx.principal_kind(),
        &op.payload["sourceContext"],
    )?;
    Ok(PreparedInputs {
        request_id: row.request_id.clone(),
        operation_id: operation.clone(),
        context: original,
        input_refs: row
            .input_refs
            .iter()
            .cloned()
            .map(|v| serde_json::from_value(v).map_err(|_| invalid("captured embedding ref")))
            .collect::<Result<_, _>>()?,
    })
}
impl IndexService {
    /// Read only the authenticated query's existing operational owner, independently of the
    /// current optional dense route. No slot, artifact or native request is created here.
    fn query_preparation(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        query_id: &Id,
        fingerprint: &str,
    ) -> Result<Option<OperationRecord>, StorageError> {
        if &read.stamp().store_id != ctx.store_id()
            || read.stamp().restore_epoch != ctx.restore_epoch().get()
            || read.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
        {
            return Err(StorageError::ScopeDenied);
        }
        let operation_id = OperationId(identity("cprep_", &query_key(ctx, query_id, fingerprint)?));
        let Some(op) = read.operations().operation(&operation_id)? else {
            return Ok(None);
        };
        if op.capability != CAPABILITY
            || op.payload["origin"]["kind"] != "Query"
            || op.payload["origin"]["queryId"] != json!(query_id)
            || op.payload["origin"]["fingerprint"] != fingerprint
        {
            return Err(StorageError::ScopeDenied);
        }
        let mut original = op.payload["sourceContext"].clone();
        original["invocationContext"]
            .as_object_mut()
            .ok_or_else(|| invalid("captured embedding context"))?
            .remove("invocation");
        if original != stable_context(ctx)? {
            return Err(StorageError::CursorExpired);
        }
        Ok(Some(op))
    }
    /// Even a now-lexical/empty response must not discard a prior owned request or escape
    /// its immutable configuration/context binding. This lookup never starts preparation.
    pub fn validate_query_custody(
        &self,
        ctx: &InvocationContext,
        origin: &EmbeddingOrigin,
        configuration_source_digest: &str,
    ) -> Result<(), StorageError> {
        let EmbeddingOrigin::Query {
            query_id,
            fingerprint,
            scope_digest,
        } = origin
        else {
            return Ok(());
        };
        let context_digest = digest(
            &json!({"context":stable_context(ctx)?,"configuration":configuration_source_digest}),
        )?;
        read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
            let Some(op) = self.query_preparation(read, ctx, query_id, fingerprint)? else {
                return Ok(());
            };
            if op.payload["origin"]["scopeDigest"] != scope_digest.as_str()
                || op.payload["contextDigest"] != context_digest
                || op.payload["requestId"]
                    != json!(identity("cemb_", &query_key(ctx, query_id, fingerprint)?))
            {
                return Err(StorageError::CursorExpired);
            }
            Ok(())
        })
    }
    /// Exact winner refs are reused across retries; concurrent losing files never become
    /// artifact rows. No source/provider bytes are read inside the publishing transaction.
    pub fn prepare_embedding_inputs(
        &self,
        input: InputPreparation<'_>,
    ) -> Result<PreparedInputs, StorageError> {
        self.embedding_inputs(input, false)
    }
    /// Observation is read-only: an absent original capture expires the cursor without
    /// staging files, creating a preparation operation or changing any cache fact.
    pub fn existing_embedding_inputs(
        &self,
        input: InputPreparation<'_>,
    ) -> Result<PreparedInputs, StorageError> {
        self.embedding_inputs(input, true)
    }
    fn embedding_inputs(
        &self,
        input: InputPreparation<'_>,
        existing_only: bool,
    ) -> Result<PreparedInputs, StorageError> {
        let _local_preparation = self.closed.enter()?;
        let InputPreparation {
            context,
            origin,
            embedding_space,
            configuration_source_digest,
            texts,
            fences,
            cancellation,
            deadline_ms,
        } = input;
        let bounded = || {
            if cancellation.is_cancelled()
                || deadline_ms.is_some_and(|d| self.clock.now_ms().get() >= d)
            {
                Err(StorageError::Cancelled)
            } else {
                Ok(())
            }
        };
        bounded()?;
        if texts.is_empty() || texts.len() > 32 {
            return Err(invalid("embedding batch must contain 1..32 actual inputs"));
        }
        let texts_bytes = texts.iter().map(|text| text.len() as u64).sum::<u64>();
        let source = approvals::save_source_context(context);
        let context_digest = digest(
            &json!({"context":stable_context(context)?,"configuration":configuration_source_digest}),
        )?;
        let input_digest = digest(&json!(
            texts
                .iter()
                .map(|t| json!({"sha256":sha256_hex(t.as_bytes()),"byteCount":t.len()}))
                .collect::<Vec<_>>()
        ))?;
        let query_identity = match origin {
            EmbeddingOrigin::Query {
                query_id,
                fingerprint,
                ..
            } => Some(query_key(context, query_id, fingerprint)?),
            EmbeddingOrigin::Index { .. } => None,
        };
        let origin = match origin {
            EmbeddingOrigin::Query {
                query_id,
                fingerprint,
                scope_digest,
            } => {
                json!({"kind":"Query","queryId":query_id,"fingerprint":fingerprint,"scopeDigest":scope_digest})
            }
            EmbeddingOrigin::Index {
                operation_id,
                generation_id,
                source_digest,
                first_chunk,
            } => {
                json!({"kind":"Index","operationId":operation_id,"generationId":generation_id,"sourceDigest":source_digest,"firstChunk":first_chunk})
            }
        };
        let args_digest = digest(
            &json!({"origin":origin,"context":context_digest,"space":embedding_space,"inputs":input_digest}),
        )?;
        // A repeated query frame cannot escape an admitted/unknown request by observing a
        // new index/configuration witness. Its immutable args below still bind those facts.
        let stable_identity = query_identity.as_deref().unwrap_or(&args_digest);
        let request_id = identity("cemb_", stable_identity);
        let operation_id = OperationId(identity("cprep_", stable_identity));
        let is_query = query_identity.is_some();
        let (
            ctx,
            fs,
            op_id,
            request,
            space,
            input_hash,
            context_hash,
            args,
            source_value,
            cancelled,
        ) = (
            context.clone(),
            fences.to_vec(),
            operation_id.clone(),
            request_id.clone(),
            embedding_space.to_owned(),
            input_digest.clone(),
            context_digest.clone(),
            args_digest.clone(),
            source,
            cancellation.clone(),
        );
        let prior = self.write(move |tx| {
            if cancelled.is_cancelled() || deadline_ms.is_some_and(|d| tx.now_ms() >= d) {
                return Err(StorageError::Cancelled);
            }
            check(tx,&ctx,&fs)?;
            if let Some(prior) = tx.operations().operation(&op_id)?
                && (prior.capability != CAPABILITY || prior.args_digest != args)
            {
                return Err(if is_query { StorageError::CursorExpired } else { StorageError::ScopeDenied });
            }
            if let Some(row)=tx.conventions().convention_embedding_inputs(&request)? {
                return captured(tx,&ctx,&row,&op_id,&args).map(Some);
            }
            if existing_only { return Err(StorageError::CursorExpired); }
            let op = match tx.operations().operation(&op_id)? {
                Some(op) => { if op.capability != CAPABILITY || op.args_digest != args { return Err(StorageError::ScopeDenied); } op },
                None => tx.operations().record_intent(NewOperation {id:op_id.clone(),root_id:ctx.root_id().cloned(),assignment_id:ctx.assignment_id().cloned(),capability:CAPABILITY.into(),schema_version:1,args_digest:args.clone(),
                    payload:json!({"requestId":request,"sourceContext":source_value,"embeddingSpace":space,"contextDigest":context_hash,"inputDigest":input_hash,"origin":origin,"inputReservationEstimate":{"basis":"Utf8ByteCount","value":texts_bytes,"guaranteedUpperBound":false}})})?,
            };
            let op = if matches!(op.state,OperationState::IntentRecorded|OperationState::RetryPending) {
                tx.operations().begin_attempt(&op.id,op.revision,AttemptBinding{policy_revision:Some(ctx.policy_revision().get()),..Default::default()})?;
                tx.operations().operation(&op.id)?.ok_or_else(||invalid("embedding operation disappeared"))?
            } else {op};
            if op.state==OperationState::Preflighted {
                tx.operations().transition(&op.id,op.revision,op.latest_attempt().map(|a|a.fence()).as_ref(),OperationState::Dispatching,None)?;
            } else if !matches!(op.state,OperationState::Dispatching|OperationState::OutcomeUnknown) {
                return Err(invalid("embedding preparation cannot restart terminal operation"));
            }
            Ok(None)
        })?;
        if let Some(prior) = prior {
            return Ok(prior);
        }
        let mut staged = Vec::with_capacity(texts.len());
        let stage_result = self.publication(|| {
            for (index,text) in texts.iter().enumerate() {
                bounded()?;
                staged.push(self.artifacts.stage_host_bytes(self.store.as_ref(),text.as_bytes(),json!({"kind":"ConventionEmbeddingInput","requestId":request_id,"inputIndex":index}))?
                    .map_err(StorageError::InvalidRecord)?);
            }
            Ok(())
        });
        if let Err(error) = stage_result {
            for file in &staged {
                self.artifacts
                    .settle_host_bytes(self.store.as_ref(), file, false);
            }
            return Err(error);
        }
        let (
            ctx,
            fs,
            op_id,
            request,
            space,
            input_hash,
            context_hash,
            args,
            files,
            artifacts,
            cancellation,
        ) = (
            context.clone(),
            fences.to_vec(),
            operation_id,
            request_id,
            embedding_space.to_owned(),
            input_digest,
            context_digest,
            args_digest,
            staged.clone(),
            self.artifacts.clone(),
            cancellation.clone(),
        );
        let now = self.clock.now_ms().get();
        let result = self.write(move |tx| {
            if cancellation.is_cancelled() || deadline_ms.is_some_and(|d|tx.now_ms()>=d) {return Err(StorageError::Cancelled);}
            check(tx,&ctx,&fs)?;
            if let Some(row)=tx.conventions().convention_embedding_inputs(&request)? {
                return captured(tx,&ctx,&row,&op_id,&args).map(|p|(p,false));
            }
            let mut refs=Vec::with_capacity(files.len());
            for (index,file) in files.iter().enumerate() {
                let version=artifacts.commit_host_bytes_in_scope(tx,file,HostArtifactPublication {
                    title:"Convention embedding input",media_type:"text/plain; charset=utf-8",
                    scope:ctx.project_id().cloned().map_or(ArtifactScope::Workspace,ArtifactScope::Project),owner:ctx.principal().clone(),
                    provenance:json!({"kind":"ConventionEmbeddingInput","requestId":request,"contextDigest":context_hash,"embeddingSpace":space,"inputDigest":input_hash,"inputIndex":index}),
                })?;
                refs.push(serde_json::to_value(version_ref(&version)).map_err(|_|invalid("embedding input ref"))?);
            }
            let row=tx.conventions().insert_convention_embedding_inputs(&StoredConventionEmbeddingInputs {request_id:request,actor_id:ctx.principal().clone(),project_id:ctx.project_id().cloned(),context_digest:context_hash,embedding_space:space,input_digest:input_hash,input_refs:refs,created_at_ms:now})?;
            // This observation completes input publication, not the actual inference owner.
            let op=tx.operations().operation(&op_id)?.ok_or_else(||invalid("embedding operation disappeared"))?;
            tx.operations().observe(&op_id,op.latest_attempt().ok_or_else(||invalid("embedding attempt missing"))?.attempt_no,json!({"inputsPublished":true,"requestId":row.request_id}))?;
            captured(tx,&ctx,&row,&op_id,&args).map(|p|(p,true))
        });
        let committed = result.as_ref().is_ok_and(|(_, committed)| *committed);
        for file in &staged {
            self.artifacts
                .settle_host_bytes(self.store.as_ref(), file, committed);
        }
        result.map(|(prepared, _)| prepared)
    }
}

/// Settle the operational preparation from its one actual native journal. This is derived
/// lifecycle metadata, not another inference result or an authorization to start a source.
/// No source files or provider bytes are read inside this transaction.
pub fn settle_native_preparation(
    tx: &mut dyn WriteTransaction,
    operation_id: &OperationId,
) -> Result<bool, StorageError> {
    let Some(mut preparation) = tx.operations().operation(operation_id)? else {
        return Ok(false);
    };
    if preparation.capability != CAPABILITY {
        return Err(StorageError::ScopeDenied);
    }
    if is_terminal(preparation.state) {
        return Ok(false);
    }
    let request: Id = serde_json::from_value(preparation.payload["requestId"].clone())
        .map_err(|_| invalid("preparation native request identity"))?;
    let Some(route) = tx.router().route_request(&request)? else {
        return Ok(false);
    };
    let native = tx
        .operations()
        .operation(&route.operation_id)?
        .ok_or(StorageError::ScopeDenied)?;
    if route.purpose != "Embedding"
        || native.payload["actorId"]
            != preparation.payload["sourceContext"]["invocationContext"]["principal"]
    {
        return Err(StorageError::ScopeDenied);
    }
    let Some(attempt) = native.latest_attempt() else {
        return Ok(false);
    };
    let Some(usage) = tx.usage().inference_attempt(&attempt.id.0)? else {
        return Ok(false);
    };
    if usage.operation_id != native.id
        || usage.request_id != request
        || usage.purpose != "Embedding"
    {
        return Err(StorageError::ScopeDenied);
    }
    let Some(balance) = tx.usage().attempt_balance(&usage.attempt_id)? else {
        return Ok(false);
    };
    let target = if balance.unknown_tail || !balance.local_quiescent || !is_terminal(native.state) {
        OperationState::OutcomeUnknown
    } else if native.state == OperationState::ObservedComplete && usage.state == "Settled" {
        OperationState::ObservedComplete
    } else {
        OperationState::Failed
    };
    let Some(fence) = preparation.latest_attempt().map(|a| a.fence()) else {
        return Ok(false);
    };
    if preparation.state == target {
        return Ok(false);
    }
    // NeedsInput intentionally cannot jump straight to successful completion.
    if preparation.state == OperationState::NeedsInput && target == OperationState::ObservedComplete
    {
        preparation = tx.operations().transition(
            &preparation.id,
            preparation.revision,
            Some(&fence),
            OperationState::OutcomeUnknown,
            None,
        )?;
    }
    if !transition_allowed(preparation.state, target) {
        return Ok(false);
    }
    tx.operations().transition(&preparation.id, preparation.revision, Some(&fence), target,
        Some(json!({"kind":"NativeEmbeddingPreparationSettled","requestId":request,"nativeOperationId":native.id,"attemptId":usage.attempt_id,"nativeState":native.state,"localQuiescent":balance.local_quiescent,"unknownTail":balance.unknown_tail})))?;
    Ok(true)
}
