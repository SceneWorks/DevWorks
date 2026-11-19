//! The turn engine: the handles an agent turn and its durable tool dispatch need, and nothing
//! more. The runtime constructs one and hands out references; `tool_turn` and `agent_turns`
//! take it instead of the whole runtime, so a turn cannot reach services it does not own.
use crate::indexing::IndexSupervisor;
use crate::package_host::PackageHost;
use crate::providers::driver::NativeProviders;
use crate::runs::RunSupervisor;
use crate::runtime::{
    Admitter, NativeTurnProgress, PendingCall, ReceiveStart, ReceiveWait, ToolDispatch,
    ToolOperationUpdate, drop_stamped_guidance, join_all, skill_adoption_payload, with_role,
};
use devworks_core::context::InvocationContext;
use devworks_core::gateway::{CommandOutcome, QueryOutcome};
use devworks_core::locks::LockExt;
use devworks_core::ports::storage::{ApplicationStore, Cancellation, WriteOptions};
use devworks_core::ports::storage::{StorageError, Unavailable};
use devworks_core::protocol::Id;
use devworks_core::services::approvals::ApprovalService;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::mcp::McpClientService;
use std::sync::Arc;
use tokio::sync::watch;

/// The store, the artifact service, the native providers and the command path one turn uses.
/// Cloning shares the same handles.
#[derive(Clone)]
pub struct TurnEngine {
    pub(crate) store: Arc<dyn ApplicationStore>,
    pub(crate) artifacts: Arc<ArtifactService>,
    pub(crate) providers: Option<Arc<NativeProviders>>,
    pub(crate) mcp: Option<Arc<McpClientService>>,
    pub(crate) packages: Option<Arc<PackageHost>>,
    pub(crate) approvals: Arc<ApprovalService>,
    pub(crate) runs: Arc<RunSupervisor>,
    pub(crate) instructions: Option<Arc<devworks_core::services::instructions::InstructionService>>,
    pub(crate) indexing: Option<Arc<IndexSupervisor>>,
    pub(crate) admitter: Arc<Admitter>,
}

impl TurnEngine {
    pub fn store(&self) -> &Arc<dyn ApplicationStore> {
        &self.store
    }
    pub fn artifacts(&self) -> &Arc<ArtifactService> {
        &self.artifacts
    }
    pub fn providers(&self) -> Option<&Arc<NativeProviders>> {
        self.providers.as_ref()
    }
    pub fn mcp_service(&self) -> Option<Arc<McpClientService>> {
        self.mcp.clone()
    }
    pub fn packages(&self) -> Option<&Arc<PackageHost>> {
        self.packages.as_ref()
    }
    pub fn approvals(&self) -> &Arc<ApprovalService> {
        &self.approvals
    }
    /// Run supervision (activation claims, stops, recovery) over the composed control service.
    pub fn runs(&self) -> &Arc<RunSupervisor> {
        &self.runs
    }
    /// Changes each time an admitted command may have changed domain state.
    pub fn state_changes(&self) -> watch::Receiver<u64> {
        self.admitter.changes.subscribe()
    }
    /// Reads through the gateway, as the runtime's query path does.
    pub fn query_with(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
        options: &devworks_core::ports::storage::ReadOptions,
    ) -> QueryOutcome {
        self.admitter.query_with(ctx, operation, raw, options)
    }
    /// Admits a command through the runtime's fenced command path.
    pub fn command(&self, ctx: &InvocationContext, operation: &str, raw: &[u8]) -> CommandOutcome {
        self.admitter.command_with(
            ctx,
            operation,
            raw,
            devworks_core::ports::storage::WriteOptions::command(),
        )
    }

    /// Drive a host-qualified native generation through durable tool dispatch and publish its
    /// next immutable context, including an ordinary completed assistant turn. Replaying the
    /// same request recovers saved results; it never sends inference or a gateway command twice.
    pub async fn produce_and_dispatch_native_turn(
        &self,
        invocation: crate::providers::driver::NativeInvocation,
        cancellation: Option<devworks_core::ports::storage::Cancellation>,
    ) -> Result<NativeTurnProgress, StorageError> {
        if matches!(
            invocation.request.purpose,
            devworks_core::protocol::provider::InferencePurpose::Compaction
        ) {
            // Compaction output is an artifact, not a conversation turn with a transcript
            // start position or a publishable assistant continuation.
            return Err(StorageError::ScopeDenied);
        }
        let ctx = invocation.context.clone();
        let (session_id, previous) = match &invocation.request.input {
            devworks_core::protocol::provider::InferenceInput::Generation {
                session_id,
                context,
                ..
            } if invocation.logical.session_id.as_ref() == Some(session_id)
                && ctx.session_id() == Some(session_id) =>
            {
                (session_id.clone(), context.clone())
            }
            _ => return Err(StorageError::ScopeDenied),
        };
        let providers = self.providers.as_ref().ok_or_else(|| {
            StorageError::unavailable(
                Unavailable::UnsupportedRuntime,
                "native provider unavailable",
            )
        })?;
        let tool_cancellation = cancellation.clone();
        let provider = providers
            .produce_cancellable(invocation, cancellation)
            .await?;
        let mut tool_dispatches = Vec::new();
        let mut continuation = None;
        if matches!(
            provider.result.outcome,
            Some(
                devworks_core::services::usage::accounting::AttemptEnd::ToolCallsReady
                    | devworks_core::services::usage::accounting::AttemptEnd::Completed
            )
        ) {
            let has_tools = provider.result.outcome
                == Some(devworks_core::services::usage::accounting::AttemptEnd::ToolCallsReady);
            let attempt_id = provider.result.attempt_id.as_ref().ok_or_else(|| {
                StorageError::InvalidRecord("tool turn lacks settled attempt identity".into())
            })?;
            let binding = provider.result.binding.as_ref().ok_or_else(|| {
                StorageError::InvalidRecord("tool turn lacks native binding".into())
            })?;
            let after_sequence = provider.result.turn_start_sequence.ok_or_else(|| {
                StorageError::InvalidRecord("tool turn lacks durable start position".into())
            })?;
            if has_tools {
                let pass = self.dispatch_pending_pass(
                    &ctx,
                    &session_id,
                    tool_cancellation.as_ref(),
                    true,
                )?;
                tool_dispatches = self.settle_pending(pass).await;
            }
            let unanswered =
                !crate::tool_turn::pending_calls(self.store.as_ref(), &ctx, &session_id)?
                    .is_empty();
            if !unanswered {
                let material = crate::tool_turn::continuation_context(
                    self.store.as_ref(),
                    &self.artifacts,
                    &ctx,
                    crate::tool_turn::TurnSource {
                        session_id: &session_id,
                        attempt_id,
                        after_sequence,
                        has_tools,
                    },
                    binding,
                    &previous,
                )?;
                continuation = Some(crate::tool_turn::publish_continuation(
                    self.store.as_ref(),
                    &self.artifacts,
                    &ctx,
                    &session_id,
                    attempt_id,
                    binding,
                    material,
                )?);
            }
        }
        Ok(NativeTurnProgress {
            provider,
            tool_dispatches,
            continuation,
        })
    }

    /// Resume one durable provider call through the shared gateway, recording its single
    /// initial response in the owning session. A caller can retry the same call after restart.
    pub fn dispatch_tool_call(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        call: &devworks_core::protocol::provider::ToolCallKey,
    ) -> Result<devworks_core::services::sessions::transcript::BatchOutcome, StorageError> {
        match self.dispatch_tool_call_step(ctx, session_id, call, None, false)? {
            crate::tool_turn::DispatchStep::Answered(outcome) => Ok(outcome),
            crate::tool_turn::DispatchStep::Parked(_) => Err(StorageError::InvalidRecord(
                "a blocking dispatch cannot park".into(),
            )),
        }
    }

    fn dispatch_tool_call_step(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        call: &devworks_core::protocol::provider::ToolCallKey,
        cancellation: Option<&Cancellation>,
        defer_receive: bool,
    ) -> Result<crate::tool_turn::DispatchStep, StorageError> {
        let providers = self.providers.as_ref().ok_or_else(|| {
            StorageError::unavailable(
                Unavailable::UnsupportedRuntime,
                "native provider unavailable",
            )
        })?;
        crate::tool_turn::dispatch_step(
            self,
            self.store.as_ref(),
            &self.artifacts,
            providers.schemas().as_ref(),
            ctx,
            session_id,
            call,
            cancellation,
            defer_receive,
        )
    }

    /// The first, non-blocking look at an `agents.receive` (see [`Admitter::begin_receive`]).
    pub(crate) fn begin_receive(
        &self,
        ctx: &InvocationContext,
        raw: &[u8],
        options: WriteOptions,
    ) -> ReceiveStart {
        self.admitter.begin_receive(ctx, raw, options)
    }

    /// Awaits a receive's wait and decides it (see [`Admitter::finish_receive`]).
    pub(crate) async fn finish_receive(&self, wait: Box<ReceiveWait>) -> CommandOutcome {
        self.admitter.clone().finish_receive(wait).await
    }

    /// Record a final cancellation only if this durable call has no dispatch-start marker.
    /// A call already started requires same-key recovery or effect reconciliation.
    pub fn cancel_tool_call_before_dispatch(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        call: &devworks_core::protocol::provider::ToolCallKey,
    ) -> Result<devworks_core::services::sessions::transcript::BatchOutcome, StorageError> {
        crate::tool_turn::cancel_before_dispatch(self.store.as_ref(), ctx, session_id, call)
    }

    /// Resume the durable unanswered calls of a session. Each call is handled independently:
    /// an approval, status uncertainty or failure leaves the other calls eligible to run.
    pub fn dispatch_pending_tool_calls(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
    ) -> Result<Vec<ToolDispatch>, StorageError> {
        self.dispatch_pending_tool_calls_cancellable(ctx, session_id, None)
    }

    /// A stopped turn cancels calls that have not started. Started calls retain their durable
    /// marker and must reconcile under the same identity when work resumes.
    pub fn dispatch_pending_tool_calls_cancellable(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        cancellation: Option<&Cancellation>,
    ) -> Result<Vec<ToolDispatch>, StorageError> {
        Ok(self
            .dispatch_pending_pass(ctx, session_id, cancellation, false)?
            .into_iter()
            .filter_map(|pass| match pass {
                PendingCall::Done(dispatch) => Some(dispatch),
                PendingCall::Parked(_) => None,
            })
            .collect())
    }

    /// The synchronous part of dispatching a session's pending calls on a bounded worker set.
    /// With `defer_receive`, an `agents.receive` that has nothing to read yet comes back parked,
    /// holding no thread; [`Self::settle_pending`] awaits it.
    pub(crate) fn dispatch_pending_pass(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        cancellation: Option<&Cancellation>,
        defer_receive: bool,
    ) -> Result<Vec<PendingCall>, StorageError> {
        let calls = crate::tool_turn::pending_calls(self.store.as_ref(), ctx, session_id)?;
        if calls.is_empty() {
            return Ok(Vec::new());
        }
        // A bounded worker set allows independent gateway calls to finish out of order. Each
        // writer reads the current sequence in its own serialized transaction, so a slow sibling
        // cannot reserve a transcript position or cancel completed results.
        let next = std::sync::atomic::AtomicUsize::new(0);
        let results = std::sync::Mutex::new(
            std::iter::repeat_with(|| None)
                .take(calls.len())
                .collect::<Vec<Option<PendingCall>>>(),
        );
        std::thread::scope(|scope| {
            for _ in 0..calls.len().min(8) {
                let calls = &calls;
                let next = &next;
                let results = &results;
                scope.spawn(move || {
                    loop {
                        let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(call) = calls.get(index) else { break };
                        let done = |outcome| {
                            PendingCall::Done(ToolDispatch {
                                call: call.clone(),
                                outcome,
                            })
                        };
                        let result = if cancellation.is_some_and(Cancellation::is_cancelled) {
                            done(self.cancel_tool_call_before_dispatch(ctx, session_id, call))
                        } else {
                            match self.dispatch_tool_call_step(
                                ctx,
                                session_id,
                                call,
                                cancellation,
                                defer_receive,
                            ) {
                                Ok(crate::tool_turn::DispatchStep::Answered(outcome)) => {
                                    done(Ok(outcome))
                                }
                                Ok(crate::tool_turn::DispatchStep::Parked(parked)) => {
                                    PendingCall::Parked(parked)
                                }
                                Err(StorageError::Cancelled) => done(
                                    self.cancel_tool_call_before_dispatch(ctx, session_id, call),
                                ),
                                Err(error) => done(Err(error)),
                            }
                        };
                        results.locked()[index] = Some(result);
                    }
                });
            }
        });
        Ok(results
            .into_inner()
            .unwrap_or_else(|p| p.into_inner())
            .into_iter()
            .map(|result| result.expect("joined tool worker"))
            .collect())
    }

    /// The asynchronous part: awaits every parked receive together, without a thread each, and
    /// returns every call's dispatch in the order the calls were listed.
    pub(crate) async fn settle_pending(&self, passes: Vec<PendingCall>) -> Vec<ToolDispatch> {
        let mut waiting: Vec<
            std::pin::Pin<Box<dyn std::future::Future<Output = ToolDispatch> + Send + '_>>,
        > = Vec::new();
        let mut slots = Vec::new();
        let mut dispatches = Vec::new();
        for (index, pass) in passes.into_iter().enumerate() {
            match pass {
                PendingCall::Done(dispatch) => dispatches.push(Some(dispatch)),
                PendingCall::Parked(parked) => {
                    dispatches.push(None);
                    slots.push(index);
                    waiting.push(Box::pin(async move {
                        let call = parked.key.clone();
                        let outcome = parked.finish(self).await;
                        ToolDispatch { call, outcome }
                    }));
                }
            }
        }
        let mut finished = join_all(waiting).await.into_iter();
        for index in slots {
            dispatches[index] = finished.next();
        }
        dispatches.into_iter().flatten().collect()
    }

    /// Read a closed tool turn in native ordinal order after every emitted call has its one
    /// durable initial response. The caller can then encode the next provider request without
    /// depending on the order in which independent workers finished.
    pub fn resolved_tool_turn(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        attempt_id: &devworks_core::protocol::AttemptId,
        after_sequence: i64,
    ) -> Result<Vec<crate::ResolvedToolCall>, StorageError> {
        crate::tool_turn::resolved_turn(
            self.store.as_ref(),
            ctx,
            session_id,
            attempt_id,
            after_sequence,
        )
    }

    /// Record a newer journal state for an operation linked to this session's accepted tool
    /// response. The first response remains unchanged; replaying the same revision adds nothing.
    pub fn record_tool_operation_update(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        operation_id: &devworks_core::protocol::OperationId,
    ) -> Result<Option<devworks_core::protocol::provider::ArtifactRef>, StorageError> {
        crate::tool_turn::record_operation_update(
            self.store.as_ref(),
            &self.artifacts,
            ctx,
            session_id,
            operation_id,
        )
    }

    /// Reconcile every operation linked to an initial tool response without making a failure
    /// on one operation suppress updates for unrelated calls.
    pub fn reconcile_tool_operations(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
    ) -> Result<Vec<ToolOperationUpdate>, StorageError> {
        let linked = crate::tool_turn::linked_operations(self.store.as_ref(), ctx, session_id)?;
        let mut operations: Vec<_> = linked
            .into_iter()
            .filter_map(|call| call.operation_id)
            .collect();
        operations
            .sort_by(|a: &devworks_core::protocol::OperationId, b| a.0.as_str().cmp(b.0.as_str()));
        operations.dedup();
        Ok(operations
            .into_iter()
            .map(|operation_id| ToolOperationUpdate {
                outcome: self.record_tool_operation_update(ctx, session_id, &operation_id),
                operation_id,
            })
            .collect())
    }

    /// On a session wake, record the current states of linked tool operations and append their
    /// new facts to the previous native continuation. A redirect or epoch change requires the
    /// caller to rebuild context before requesting more inference.
    pub fn refresh_native_tool_context(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        previous: &devworks_core::protocol::provider::ArtifactRef,
    ) -> Result<devworks_core::protocol::provider::ArtifactRef, StorageError> {
        for update in self.reconcile_tool_operations(ctx, session_id)? {
            update.outcome?;
        }
        crate::tool_turn::append_operation_updates(
            self.store.as_ref(),
            &self.artifacts,
            ctx,
            session_id,
            previous,
        )
    }

    /// Recover the exact context last published for this session, including frozen native
    /// aliases. The returned cursor tells the caller whether later transcript facts need
    /// reconciliation before another provider request.
    pub fn recover_native_context_head(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
    ) -> Result<
        Option<(
            devworks_core::protocol::provider::ArtifactRef,
            devworks_core::protocol::provider::NativeBinding,
            i64,
        )>,
        StorageError,
    > {
        crate::tool_turn::recover_context_head(
            self.store.as_ref(),
            &self.artifacts,
            ctx,
            session_id,
        )
    }

    /// Fix the first provider prompt and binding for a fresh session before its first send.
    /// A restart recovers this head verbatim; a matching retry does not create another version.
    pub fn publish_initial_native_context(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        binding: &devworks_core::protocol::provider::NativeBinding,
        context: &devworks_core::providers::context::ProviderContext,
    ) -> Result<devworks_core::protocol::provider::ArtifactRef, StorageError> {
        crate::tool_turn::publish_initial_context(
            self.store.as_ref(),
            &self.artifacts,
            ctx,
            session_id,
            binding,
            context,
        )
    }

    /// Build the next provider context from the immutable request context and settled native
    /// exchange. The caller publishes the returned material before a new request.
    pub fn tool_continuation_context(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        attempt_id: &devworks_core::protocol::AttemptId,
        after_sequence: i64,
        binding: &devworks_core::protocol::provider::NativeBinding,
        previous: &devworks_core::protocol::provider::ArtifactRef,
    ) -> Result<devworks_core::providers::context::ProviderContext, StorageError> {
        crate::tool_turn::continuation_context(
            self.store.as_ref(),
            &self.artifacts,
            ctx,
            crate::tool_turn::TurnSource {
                session_id,
                attempt_id,
                after_sequence,
                has_tools: true,
            },
            binding,
            previous,
        )
        .map(|material| material.context)
    }

    /// Record an explicit provider-change context epoch after all emitted calls have their
    /// initial responses. Known tool exchanges survive; opaque provider blocks are recorded as
    /// omitted sources and cannot leak into the next provider's request.
    pub fn change_provider_epoch(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        previous_context: &devworks_core::protocol::provider::ArtifactRef,
        previous_binding: &devworks_core::protocol::provider::NativeBinding,
        next_binding: &devworks_core::protocol::provider::NativeBinding,
    ) -> Result<devworks_core::protocol::provider::ArtifactRef, StorageError> {
        crate::context_epoch::provider_change(
            self.store.as_ref(),
            &self.artifacts,
            ctx,
            session_id,
            previous_context,
            previous_binding,
            next_binding,
        )
    }

    /// Adopt changed repository guidance at an explicit safe context boundary. Unchanged
    /// source stamps reuse the existing snapshot; a changed bundle creates a new immutable
    /// context and records any opaque native state that cannot cross the epoch.
    pub fn adopt_instruction_epoch(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        previous_context: &devworks_core::protocol::provider::ArtifactRef,
        previous_binding: &devworks_core::protocol::provider::NativeBinding,
        targets: &[(Id, Vec<String>)],
    ) -> Result<devworks_core::protocol::provider::ArtifactRef, StorageError> {
        if ctx.session_id() != Some(session_id) {
            return Err(StorageError::ScopeDenied);
        }
        let previous_epoch = previous_binding.context_epoch.get();
        devworks_core::ports::storage::read_snapshot(
            self.store.as_ref(),
            &devworks_core::ports::storage::ReadOptions::default(),
            |read| {
                let session = read
                    .sessions()
                    .session(session_id)?
                    .ok_or(StorageError::ScopeDenied)?;
                if session.closed() || session.context_epoch != previous_epoch {
                    return Err(StorageError::ScopeDenied);
                }
                Ok(())
            },
        )?;
        let source = crate::context_epoch::current_source(
            self.store.as_ref(),
            &self.artifacts,
            ctx,
            session_id,
            previous_context,
            previous_binding,
        )?;
        let mut base = source.context;
        let old = std::mem::take(&mut base.instruction_stamps);
        let old_count: usize = old.iter().map(|stamp| stamp.sources.len()).sum();
        drop_stamped_guidance(&mut base.instructions, old_count)?;
        let old_skills = base.skill_stamp.take();
        if old_skills.is_some() {
            base.instructions.pop();
        }
        // An assignment's context starts with its role's instructions, captured when the work
        // began; a role changed since is adopted here, like changed guidance and skills.
        let mut role_changed = false;
        if let Some(role) = self.current_role_prefix(ctx)?
            && let Some(rebound) = with_role(&base.instructions, role)
        {
            base.instructions = rebound;
            role_changed = true;
        }
        let rebound = self.bind_provider_instructions(ctx, base, targets)?;
        if !role_changed && rebound.instruction_stamps == old && rebound.skill_stamp == old_skills {
            return Ok(source.artifact);
        }
        // Recorded in the transaction that publishes the epoch: a refused or failed transition
        // records no adoption, and a retry records it once.
        let adoption = (rebound.skill_stamp != old_skills)
            .then(|| skill_adoption_payload(old_skills.as_ref(), rebound.skill_stamp.as_ref()));
        let next_binding = devworks_core::protocol::provider::NativeBinding {
            context_epoch: devworks_core::protocol::Counter::new(
                previous_epoch
                    .checked_add(1)
                    .ok_or_else(|| StorageError::InvalidRecord("context epoch overflow".into()))?,
            )
            .map_err(|e| StorageError::InvalidRecord(e.into()))?,
            ..previous_binding.clone()
        };
        let (converted, omitted) = rebound
            .converted_for_new_epoch(previous_binding, &next_binding)
            .map_err(|e| StorageError::InvalidRecord(format!("instruction adoption: {e:?}")))?;
        crate::context_epoch::publish(
            self.store.as_ref(),
            &self.artifacts,
            ctx,
            session_id,
            crate::context_epoch::EpochChange {
                previous_epoch,
                binding: next_binding,
                observed_sequence: source.observed_sequence,
                source: source.artifact,
                reason: devworks_core::protocol::provider::ContextChangeReason::InstructionAdoption,
                context: converted,
                omitted,
                adoption,
            },
        )
    }

    /// The current role instructions of the agent assignment `ctx` runs, as the role prefix of
    /// its provider context (empty when the role has no instructions); `None` outside an agent
    /// assignment.
    fn current_role_prefix(
        &self,
        ctx: &InvocationContext,
    ) -> Result<Option<Vec<devworks_core::providers::context::TextBlock>>, StorageError> {
        let Some(assignment) = ctx.assignment_id() else {
            return Ok(None);
        };
        devworks_core::ports::storage::read_snapshot(
            self.store.as_ref(),
            &devworks_core::ports::storage::ReadOptions::default(),
            |read| {
                let Some(source) = read.runs().agent_assignment_context(assignment)? else {
                    return Ok(None);
                };
                let role =
                    devworks_core::settings::catalog::AgentRole::parse(source.role_id.as_str())
                        .ok_or_else(|| StorageError::InvalidRecord("assignment role".into()))?;
                let text = devworks_core::settings::service::role_instructions(
                    read.configuration(),
                    role,
                    Some(&source.project_id),
                )
                .map_err(|failure| match failure {
                    devworks_core::settings::service::Failure::Storage(e) => e,
                    devworks_core::settings::service::Failure::Refused(_) => {
                        StorageError::InvalidRecord("role instructions".into())
                    }
                })?
                .text;
                Ok(Some(if text.is_empty() {
                    Vec::new()
                } else {
                    vec![devworks_core::providers::context::TextBlock { text, cache: None }]
                }))
            },
        )
    }

    /// Resolve current path-scoped repository instructions from the checkout the invocation is
    /// authorized to use. A missing/changed source is observed before each new dispatch; the
    /// caller binds the returned revision and sources into its context epoch.
    pub fn effective_instructions(
        &self,
        ctx: &InvocationContext,
        repository_id: &Id,
        target_paths: &[String],
    ) -> Result<devworks_core::providers::instructions::InstructionBundle, StorageError> {
        crate::instruction_guard::InstructionGuard::new(
            self.store.clone(),
            self.indexing.as_ref().map(|ix| ix.env().source.clone()),
        )
        .resolve(ctx, repository_id, target_paths)
    }

    /// The provider dispatch gate for instruction material, as the provider driver applies it:
    /// repository guidance and the skill set bound to `context` must still be current.
    pub fn verify_instruction_context(
        &self,
        ctx: &InvocationContext,
        context: &devworks_core::providers::context::ProviderContext,
        targets: Option<&[(Id, Vec<String>)]>,
    ) -> Result<(), StorageError> {
        crate::instruction_guard::InstructionGuard::new(
            self.store.clone(),
            self.indexing.as_ref().map(|ix| ix.env().source.clone()),
        )
        .with_skills(self.instructions.clone())
        .verify(ctx, context, targets)
    }

    /// The project whose skills an invocation is offered: its own, else its session's.
    pub(crate) fn skill_project(
        &self,
        ctx: &InvocationContext,
    ) -> Result<Option<devworks_core::protocol::ProjectId>, StorageError> {
        if let Some(project) = ctx.project_id() {
            return Ok(Some(project.clone()));
        }
        let Some(session_id) = ctx.session_id() else {
            return Ok(None);
        };
        devworks_core::ports::storage::read_snapshot(
            self.store.as_ref(),
            &devworks_core::ports::storage::ReadOptions::default(),
            |read| {
                Ok(read
                    .sessions()
                    .session(session_id)?
                    .and_then(|s| s.project_id))
            },
        )
    }
    /// Compose the effective instruction prefix and its exact source stamps for one provider
    /// request. The trusted turn caller supplies its authorized repository and target paths;
    /// an empty path set means repository-root guidance only.
    pub fn bind_provider_instructions(
        &self,
        ctx: &InvocationContext,
        base: devworks_core::providers::context::ProviderContext,
        targets: &[(Id, Vec<String>)],
    ) -> Result<devworks_core::providers::context::ProviderContext, StorageError> {
        let selected = if targets.is_empty() {
            if let Some(bound) = ctx.worktree() {
                vec![(bound.repository_id().clone(), Vec::new())]
            } else if let Some(project) = ctx.project_id() {
                devworks_core::ports::storage::read_snapshot(
                    self.store.as_ref(),
                    &devworks_core::ports::storage::ReadOptions::default(),
                    |read| {
                        Ok(read
                            .projects()
                            .members(project)?
                            .into_iter()
                            .map(|id| (id, Vec::new()))
                            .collect())
                    },
                )?
            } else {
                Vec::new()
            }
        } else {
            targets.to_vec()
        };
        if selected.len() > 64 {
            return Err(StorageError::InvalidRecord(
                "too many instruction repositories".into(),
            ));
        }
        let mut bundles = Vec::with_capacity(selected.len());
        for (repository, paths) in &selected {
            let scope = if paths.is_empty() {
                vec![".".into()]
            } else {
                paths.clone()
            };
            bundles.push(self.effective_instructions(ctx, repository, &scope)?);
        }
        let base = match crate::instruction_guard::InstructionGuard::new(self.store.clone(), None)
            .with_skills(self.instructions.clone())
            .current_skills(self.skill_project(ctx)?.as_ref())?
        {
            Some(stamp) => devworks_core::providers::instructions::bind_skills(base, stamp)
                .map_err(|e| StorageError::InvalidRecord(format!("skill binding: {e:?}")))?,
            None => base,
        };
        devworks_core::providers::instructions::bind_context(base, &bundles)
            .map_err(|error| StorageError::InvalidRecord(format!("instruction binding: {error:?}")))
    }
}
