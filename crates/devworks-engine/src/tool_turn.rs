//! Host-owned framing of provider tool arguments. A provider chooses arguments only; the
//! selected operation, mode, request identity and contract envelope come from the host.
use crate::{providers::schemas::RegisteredSchemas, runtime::Runtime};
use devworks_core::{
    admission::{self, Selection},
    context::{InvocationContext, PrincipalKind},
    fingerprint::sha256_hex,
    ports::{IdKind, storage::*},
    protocol::{
        CONTRACT, CommandReply, Id,
        provider::{CompleteToolCall, ToolCallKey, ToolOutcome, TranscriptBody},
    },
    registry::{self, Envelope, Mode, OperationDescriptor, SchemaRevision},
    services::{
        artifacts::{ArtifactService, host::HostArtifactPublication, version_ref},
        sessions::{
            transcript::{BatchOutcome, SessionBatch, SessionWriter},
            turn,
        },
    },
};
use serde_json::{Value, json};
use std::sync::Arc;

/// Build the exact frame sent to the application gateway for a recorded native call. The
/// stable key makes a retried command find its original receipt after a crash or lost reply.
pub(crate) fn frame(
    session: &Id,
    call: &ToolCallKey,
    descriptor: &'static OperationDescriptor,
    mode: Mode,
    revision: SchemaRevision,
    arguments: Value,
) -> Result<Vec<u8>, &'static str> {
    call.validate()?;
    if !descriptor.supports(revision) || descriptor.binding(mode).is_none() {
        return Err("unsupported selected tool binding");
    }
    let Value::Object(mut arguments) = arguments else {
        return Err("tool arguments must be an object");
    };
    let identity = format!(
        "tool-{}",
        sha256_hex(
            format!(
                "{}\0{}\0{}\0{}",
                session.as_str(),
                call.attempt_id.0.as_str(),
                call.native_call_id,
                call.ordinal.as_str()
            )
            .as_bytes()
        )
    );
    let key = mode.key_member();
    let value = match descriptor.envelope {
        Envelope::Domain => {
            // The discriminator is host selected. Accepting a provider-supplied one would let a
            // schema shared by multiple operations silently select another authority binding.
            if arguments.remove("kind").is_some() {
                return Err("provider supplied a domain selector");
            }
            arguments.insert("kind".into(), Value::String(descriptor.name.into()));
            let mut envelope = serde_json::Map::new();
            envelope.insert("protocol".into(), Value::String(CONTRACT.into()));
            envelope.insert("mode".into(), Value::String(mode.as_str().into()));
            envelope.insert(key.into(), Value::String(identity));
            envelope.insert("body".into(), Value::Object(arguments));
            envelope.insert("schemaRevision".into(), json!(revision.0));
            Value::Object(envelope)
        }
        Envelope::Specialized { .. } => {
            // Older specialized schemas describe the whole wire envelope. Treat any model
            // values in its identity members as data and replace them with the host binding.
            arguments.remove("protocol");
            arguments.remove(key);
            arguments.remove("schemaRevision");
            arguments.insert("protocol".into(), Value::String(CONTRACT.into()));
            arguments.insert(key.into(), Value::String(identity));
            arguments.insert("schemaRevision".into(), json!(revision.0));
            Value::Object(arguments)
        }
    };
    let bytes = serde_json::to_vec(&value).map_err(|_| "invalid tool frame")?;
    admission::decode(
        Selection {
            operation: descriptor.name,
            mode,
        },
        &bytes,
    )
    .map_err(|_| "tool arguments failed selected-operation admission")?;
    Ok(bytes)
}

/// Execute one durable, unanswered provider call through the same gateway used by UI and agent
/// tools. The call's recorded schema chooses the operation; provider arguments never choose it.
/// Command retries reuse the host key, so a lost reply cannot repeat the effect.
pub(crate) fn dispatch_one(
    runtime: &Runtime,
    store: &dyn ApplicationStore,
    artifacts: &Arc<ArtifactService>,
    schemas: &RegisteredSchemas,
    ctx: &InvocationContext,
    session_id: &Id,
    key: &ToolCallKey,
) -> Result<BatchOutcome, StorageError> {
    let call = read_snapshot(store, &ReadOptions::default(), |read| {
        let session = read
            .sessions()
            .session(session_id)?
            .ok_or(StorageError::ScopeDenied)?;
        if ctx.session_id() != Some(session_id)
            || session.closed()
            || read.stamp().store_id != *ctx.store_id()
            || read.stamp().restore_epoch != ctx.restore_epoch().get()
            || !matches!(
                ctx.principal_kind(),
                PrincipalKind::Agent | PrincipalKind::Host
            )
            || read.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
        {
            return Err(StorageError::ScopeDenied);
        }
        if ctx.principal_kind() == PrincipalKind::Agent {
            let binding = read
                .authority()
                .principal(ctx.principal())?
                .ok_or(StorageError::ScopeDenied)?;
            if !binding.active
                || session.agent_id != *ctx.principal()
                || binding
                    .project_id
                    .as_ref()
                    .is_some_and(|p| session.project_id.as_ref() != Some(p))
            {
                return Err(StorageError::ScopeDenied);
            }
        }
        let row = read
            .sessions()
            .tool_call(session_id, &key.attempt_id.0, &key.native_call_id)?
            .ok_or(StorageError::ScopeDenied)?;
        if row.ordinal != key.ordinal.get() || row.response_sequence.is_some() {
            return Err(StorageError::ScopeDenied);
        }
        let entry = read
            .sessions()
            .entries(session_id, row.call_sequence - 1, 1)?
            .into_iter()
            .next()
            .filter(|e| e.sequence == row.call_sequence)
            .ok_or(StorageError::ScopeDenied)?;
        let TranscriptBody::ToolCallRecorded { tool_call } =
            serde_json::from_value(entry.body).map_err(|_| StorageError::ScopeDenied)?
        else {
            return Err(StorageError::ScopeDenied);
        };
        if tool_call.call.attempt_id != key.attempt_id
            || tool_call.call.native_call_id != key.native_call_id
            || tool_call.call.ordinal != key.ordinal
            || tool_call.capability_id.as_str() != row.capability_id
        {
            return Err(StorageError::ScopeDenied);
        }
        Ok(tool_call)
    })?;

    let result = match selected_builtin(schemas, &call) {
        Ok((descriptor, mode, revision)) => {
            // Missing or unreadable input material is a recovery hold. It must not become a
            // permanent Failed response just because the local artifact read is unavailable.
            let bytes = artifacts
                .read_exact(store, ctx, &call.arguments, 8 * 1024 * 1024)
                .map_err(|_| StorageError::ScopeDenied)?
                .0;
            match devworks_core::framing::parse(&bytes) {
                Ok(args) => frame(session_id, key, descriptor, mode, revision, args)
                    .map(|raw| (descriptor, mode, raw)),
                Err(_) => Err("invalid tool arguments"),
            }
        }
        Err(_) => Err("tool binding unavailable"),
    };
    let (outcome, bytes) = match result {
        Ok((descriptor, Mode::Query, raw)) => match runtime.query(ctx, descriptor.name, &raw) {
            Ok(reply) => (
                ToolOutcome::Completed { artifacts: vec![] },
                Some(serde_json::to_vec(&reply).map_err(invalid)?),
            ),
            Err(_) => (
                ToolOutcome::Failed {
                    code: static_id("tool_dispatch_failed"),
                },
                None,
            ),
        },
        Ok((descriptor, Mode::Command, raw)) => match runtime.command(ctx, descriptor.name, &raw) {
            Ok(CommandReply::CommandStatusUnknown { .. }) => {
                // Same host key is retried on recovery until the gateway returns its receipt.
                return Err(StorageError::unavailable(
                    Unavailable::UnsupportedRuntime,
                    "tool command status unknown",
                ));
            }
            Ok(reply) => (
                ToolOutcome::Completed { artifacts: vec![] },
                Some(serde_json::to_vec(&reply).map_err(invalid)?),
            ),
            Err(_) => (
                ToolOutcome::Failed {
                    code: static_id("tool_dispatch_failed"),
                },
                None,
            ),
        },
        Err(_) => (
            ToolOutcome::Failed {
                code: static_id("tool_invalid_arguments"),
            },
            None,
        ),
    };
    let staged =
        if let Some(bytes) = &bytes {
            Some(artifacts.stage_host_bytes(store, bytes,
            json!({"kind":"ToolResult","sessionId":session_id,"attemptId":key.attempt_id,
                "nativeCallId":key.native_call_id}))?
            .map_err(invalid)?)
        } else {
            None
        };
    let staged_for_write = staged.clone();
    let artifacts_for_write = artifacts.clone();
    let ctx_for_write = ctx.clone();
    let session_for_write = session_id.clone();
    let key_for_write = key.clone();
    let committed = run_write(store, WriteOptions::default(), move |tx| {
        let mut outcome = outcome.clone();
        if let Some(staged) = &staged_for_write {
            let session = tx
                .sessions()
                .session(&session_for_write)?
                .ok_or(StorageError::ScopeDenied)?;
            let scope = session
                .project_id
                .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
            let version = artifacts_for_write.commit_host_bytes_in_scope(tx, staged, HostArtifactPublication {
                title: "Tool result", media_type: "application/json",
                provenance: json!({"kind":"ToolResult","sessionId":session_for_write,"call":key_for_write}),
                scope, owner: ctx_for_write.principal().clone(),
            })?;
            outcome = ToolOutcome::Completed {
                artifacts: vec![version_ref(&version)],
            };
        }
        let session = tx
            .sessions()
            .session(&session_for_write)?
            .ok_or(StorageError::ScopeDenied)?;
        let batch = turn::batch_of(
            &session_for_write,
            tx.new_id(IdKind::TranscriptBatch),
            session.context_epoch,
            session.next_sequence,
            vec![TranscriptBody::ToolResponseRecorded {
                call: key_for_write.clone(),
                outcome,
            }],
            || tx.new_id(IdKind::TranscriptEntry),
        );
        match SessionWriter::append_in(tx, &ctx_for_write, &SessionBatch::entries(batch))? {
            accepted @ (BatchOutcome::Accepted(_) | BatchOutcome::Replayed(_)) => Ok(accepted),
            BatchOutcome::Refused(_) => Err(StorageError::ScopeDenied),
        }
    });
    if let Some(staged) = &staged {
        artifacts.settle_host_bytes(store, staged, committed.is_ok());
    }
    committed
}

fn selected_builtin(
    schemas: &RegisteredSchemas,
    call: &CompleteToolCall,
) -> Result<(&'static OperationDescriptor, Mode, SchemaRevision), StorageError> {
    let revision =
        SchemaRevision(u32::try_from(call.input_schema.revision.get()).map_err(invalid)?);
    for descriptor in registry::all() {
        if call.capability_id.as_str() != format!("devworks/{}", descriptor.name) {
            continue;
        }
        for binding in descriptor.modes {
            let Ok(registered) =
                schemas.builtin(descriptor.name, binding.mode, revision, "recorded")
            else {
                continue;
            };
            if registered.input_schema.schema_id == call.input_schema.schema_id
                && registered.input_schema.revision == call.input_schema.revision
                && registered.input_schema.sha256 == call.input_schema.sha256
            {
                return Ok((descriptor, binding.mode, revision));
            }
        }
    }
    Err(StorageError::ScopeDenied)
}
fn static_id(value: &str) -> Id {
    Id::new(value).expect("static tool error code")
}
fn invalid(error: impl ToString) -> StorageError {
    StorageError::InvalidRecord(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use devworks_core::protocol::{AttemptId, Counter};

    fn key(native: &str) -> ToolCallKey {
        ToolCallKey {
            attempt_id: AttemptId(Id::new("attempt").unwrap()),
            native_call_id: native.into(),
            ordinal: Counter::new(0).unwrap(),
        }
    }

    #[test]
    fn domain_tool_arguments_cannot_choose_operation_or_replay_identity() {
        let session = Id::new("session").unwrap();
        let descriptor = registry::find("sessions.get").unwrap();
        let one = frame(
            &session,
            &key("call-a"),
            descriptor,
            Mode::Query,
            SchemaRevision(1),
            json!({"sessionId":"session"}),
        )
        .unwrap();
        let replay = frame(
            &session,
            &key("call-a"),
            descriptor,
            Mode::Query,
            SchemaRevision(1),
            json!({"sessionId":"session"}),
        )
        .unwrap();
        let other = frame(
            &session,
            &key("call-b"),
            descriptor,
            Mode::Query,
            SchemaRevision(1),
            json!({"sessionId":"session"}),
        )
        .unwrap();
        assert_eq!(one, replay);
        assert_ne!(one, other);
        let value: Value = serde_json::from_slice(&one).unwrap();
        assert_eq!(value["body"]["kind"], "sessions.get");
        assert_eq!(value["mode"], "Query");
        assert!(
            frame(
                &session,
                &key("call-a"),
                descriptor,
                Mode::Query,
                SchemaRevision(1),
                json!({"kind":"sessions.end","sessionId":"session"})
            )
            .is_err()
        );
    }

    #[test]
    fn specialized_tool_identity_is_replaced_and_selector_still_checked() {
        let session = Id::new("session").unwrap();
        let descriptor = registry::find("runs.inspect").unwrap();
        let valid = json!({"protocol":"other","queryId":"provider-chosen",
            "query":{"kind":"runs.inspect","rootId":"root"}});
        let built = frame(
            &session,
            &key("native"),
            descriptor,
            Mode::Query,
            SchemaRevision(1),
            valid,
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&built).unwrap();
        assert_eq!(value["protocol"], CONTRACT);
        assert_ne!(value["queryId"], "provider-chosen");
        assert!(
            frame(
                &session,
                &key("native"),
                descriptor,
                Mode::Query,
                SchemaRevision(1),
                json!({"query":{"kind":"runs.eligibility","rootId":"root"}})
            )
            .is_err()
        );
    }
}
