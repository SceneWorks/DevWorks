//! Host-authored coordinator screen context. The artifact and transcript entry commit together;
//! old notices remain immutable so provider history has its original prefix.
use crate::context::{InvocationContext, PrincipalKind};
use crate::ports::IdKind;
use crate::ports::storage::{
    ApplicationStore, ArtifactScope, StorageError, WriteOptions, run_write,
};
use crate::protocol::Id;
use crate::protocol::provider::TranscriptBody;
use crate::services::artifacts::host::HostArtifactPublication;
use crate::services::artifacts::{ArtifactService, version_ref};
use crate::services::sessions::transcript::{BatchOutcome, SessionBatch, SessionWriter};
use crate::services::sessions::turn;
use serde_json::json;
use std::sync::Arc;

/// Append one bounded context notice to the selected conversation. The caller is a trusted host
/// ingress, not a generic agent command. A stale or closed session is refused atomically.
pub fn append_screen_context(
    store: &dyn ApplicationStore,
    artifacts: &Arc<ArtifactService>,
    host: &InvocationContext,
    session_id: &Id,
    screen: &str,
    context: &str,
) -> Result<(), StorageError> {
    if host.principal_kind() != PrincipalKind::Host
        || context.is_empty()
        || context.len() > 8_000
        || screen.is_empty()
        || screen.len() > 120
    {
        return Err(StorageError::InvalidRecord(
            "invalid coordinator screen context".into(),
        ));
    }
    let staged = artifacts
        .stage_host_bytes(
            store,
            context.as_bytes(),
            json!({
                "kind":"CoordinatorScreenContext","screen":screen,"sessionId":session_id.as_str(),
            }),
        )?
        .map_err(|_| StorageError::InvalidRecord("context staging failed".into()))?;
    let pending = staged.clone();
    let service = artifacts.clone();
    let session_id = session_id.clone();
    let host = host.clone().with_session(session_id.clone());
    let screen = screen.to_owned();
    let committed = run_write(store, WriteOptions::default(), move |tx| {
        let session =
            tx.sessions()
                .session(&session_id)?
                .ok_or_else(|| StorageError::NotFound {
                    kind: "session",
                    id: session_id.as_str().into(),
                })?;
        if session.closed() {
            return Err(StorageError::InvalidRecord("session closed".into()));
        }
        let scope = session
            .project_id
            .clone()
            .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
        let version=service.commit_host_bytes_in_scope(tx,&pending,HostArtifactPublication{
            title:"Coordinator screen context",media_type:"text/plain; charset=utf-8",
            provenance:json!({"kind":"CoordinatorScreenContext","screen":screen,"sessionId":session_id.as_str()}),
            scope,owner:host.principal().clone(),
        })?;
        let batch = turn::batch_of(
            &session_id,
            tx.new_id(IdKind::TranscriptBatch),
            session.context_epoch,
            session.next_sequence,
            vec![TranscriptBody::HostNotice {
                source: tx.new_id(IdKind::Event),
                artifact: version_ref(&version),
            }],
            || tx.new_id(IdKind::TranscriptEntry),
        );
        match SessionWriter::append_in(tx, &host, &SessionBatch::entries(batch))? {
            BatchOutcome::Accepted(_) | BatchOutcome::Replayed(_) => Ok(()),
            BatchOutcome::Refused(refusal) => Err(StorageError::InvalidRecord(format!(
                "context notice refused: {refusal:?}"
            ))),
        }
    });
    artifacts.settle_host_bytes(store, &staged, committed.is_ok());
    committed
}
