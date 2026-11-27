//! `sessions.append_screen_context`: the host tells a conversation which screen the user is on.
//! The notice text is staged as host bytes before the unit of work ([`SessionService::prepare`]),
//! then the artifact and its transcript entry commit together in the command's own unit of work,
//! with its receipt and history. Old notices remain immutable so provider history keeps its
//! original prefix. Only the host may append one; any other caller is refused.
use super::{SESSION_CLOSED, SessionService, append_refusal, record_from};
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::AdmittedCommand;
use crate::operations::sessions as ops;
use crate::ports::storage::{ArtifactScope, StorageError, WriteTransaction};
use crate::ports::{IdKind, Preparation, Prepared};
use crate::protocol::provider::TranscriptBody;
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::{DomainErrorCode, OperationState};
use crate::services::artifacts::StagedFile;
use crate::services::artifacts::host::HostArtifactPublication;
use crate::services::artifacts::version_ref;
use crate::services::kernel::{Failure, Refusal, not_found};
use crate::services::sessions::transcript::{BatchOutcome, SessionBatch, SessionWriter};
use crate::services::sessions::turn;
use serde_json::json;

/// Largest notice text in bytes.
pub const MAX_NOTICE_BYTES: usize = 8_000;

/// The notice text staged for one command.
pub(super) struct StagedNotice(StagedFile);

fn provenance(input: &ops::AppendScreenContextInput) -> serde_json::Value {
    json!({
        "kind": "CoordinatorScreenContext",
        "screen": input.screen_id.as_str(),
        "sessionId": input.session_id.as_str(),
    })
}

fn refusal(input: &ops::AppendScreenContextInput, ctx: &InvocationContext) -> Option<Refusal> {
    if ctx.principal_kind() != PrincipalKind::Host {
        return Some(Refusal::new(
            DomainErrorCode::PermissionDenied,
            "session_writer",
            None,
        ));
    }
    (input.context.0.len() > MAX_NOTICE_BYTES).then(|| {
        Refusal::new(
            DomainErrorCode::InvalidArgument,
            "screen_context",
            Some(input.session_id.clone()),
        )
    })
}

impl SessionService {
    /// Stages the notice text outside any transaction. A caller that is refused stages nothing.
    pub(super) fn prepare_screen_context(
        &self,
        ctx: &InvocationContext,
        input: &ops::AppendScreenContextInput,
    ) -> Result<Option<Prepared>, StorageError> {
        let Some(artifacts) = &self.artifacts else {
            return Ok(None);
        };
        if refusal(input, ctx).is_some() {
            return Ok(None);
        }
        let staged = artifacts
            .stage_host_bytes(&*self.audit, input.context.0.as_bytes(), provenance(input))?
            .map_err(|_| StorageError::InvalidRecord("context staging failed".into()))?;
        Ok(Some(Prepared::new(StagedNotice(staged))))
    }

    /// Publishes the staged text and appends its host notice at the session's next sequence.
    /// A stale or closed session is refused, and the refusal undoes the publication.
    pub(super) fn append_screen_context(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        input: &ops::AppendScreenContextInput,
    ) -> Result<DomainDecision, Failure> {
        if let Some(refused) = refusal(input, ctx) {
            return Err(refused.into());
        }
        let (Some(artifacts), Some(StagedNotice(staged))) =
            (&self.artifacts, command.prepared::<StagedNotice>())
        else {
            return Err(Refusal::new(
                DomainErrorCode::UnsupportedCapability,
                "screen_context",
                None,
            )
            .into());
        };
        let session_id = &input.session_id;
        let session = tx
            .sessions()
            .session(session_id)?
            .ok_or_else(|| not_found("session", session_id))?;
        if session.closed() {
            return Err(Refusal::new(DomainErrorCode::Conflict, SESSION_CLOSED, None).into());
        }
        let scope = session
            .project_id
            .clone()
            .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
        let version = artifacts.commit_host_bytes_in_scope(
            tx,
            staged,
            HostArtifactPublication {
                title: "Coordinator screen context",
                media_type: "text/plain; charset=utf-8",
                provenance: provenance(input),
                scope,
                owner: ctx.principal().clone(),
            },
        )?;
        let batch = turn::batch_of(
            session_id,
            tx.new_id(IdKind::TranscriptBatch),
            session.context_epoch,
            session.next_sequence,
            vec![TranscriptBody::HostNotice {
                source: tx.new_id(IdKind::Event),
                artifact: version_ref(&version),
            }],
            || tx.new_id(IdKind::TranscriptEntry),
        );
        let host = ctx.clone().with_session(session_id.clone());
        match SessionWriter::append_in(tx, &host, &SessionBatch::entries(batch))? {
            BatchOutcome::Accepted(_) | BatchOutcome::Replayed(_) => {}
            BatchOutcome::Refused(refused) => return Err(append_refusal(&refused).into()),
        }
        let session = tx
            .sessions()
            .session(session_id)?
            .ok_or_else(|| not_found("session", session_id))?;
        Ok(DomainDecision::Applied {
            result: OperationResult::SessionsAppendScreenContext {
                value: record_from(tx.sessions(), &session)?,
            },
        })
    }

    /// Resolves the staging once the outcome is known: a staging the unit of work did not
    /// publish (a refusal, replay or rollback) is abandoned.
    pub(super) fn settle_screen_context(&self, prepared: &Prepared, env: &Preparation<'_>) {
        if let (Some(artifacts), Some(StagedNotice(staged))) =
            (&self.artifacts, prepared.get::<StagedNotice>())
            && let Ok(staging) = env.read(|s| s.operations().operation(&staged.operation_id))
        {
            artifacts.settle_host_bytes(
                &*self.audit,
                staged,
                staging.is_some_and(|record| record.state == OperationState::ObservedComplete),
            );
        }
    }
}
