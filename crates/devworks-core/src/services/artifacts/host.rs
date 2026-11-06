//! Host-produced artifacts (DW-023 repository README and structural summary texts): staged and
//! published with the same journaled staging, generation binding and recovery as every save,
//! then recorded inside the caller's own unit of work (the index publication), so a text is
//! never visible without its view and a view never names a text that was not durably written.
//!
//! The sequence is [`ArtifactService::stage_host_bytes`] (outside any transaction),
//! [`ArtifactService::commit_host_bytes`] (inside the publishing transaction) and
//! [`ArtifactService::settle_host_bytes`] (after its outcome is known): a staging the
//! transaction did not commit is resolved at once, and recovery resolves one a crash left.
use super::{ArtifactService, StagedFile, put_staged_blob};
use crate::fingerprint::sha256_hex;
use crate::ports::IdKind;
use crate::ports::Preparation;
use crate::ports::files::{FileFault, SourceOrigin};
use crate::ports::storage::{
    ApplicationStore, ArtifactRetention, ArtifactScope, ArtifactState, BlobState, NewEvent,
    StorageError, StoredArtifact, StoredVersion, WriteTransaction,
};
use crate::protocol::EventId;
use serde_json::{Value, json};

/// History aggregate of host-produced artifacts.
pub const HOST_ARTIFACT_AGGREGATE: &str = "host_artifact";

/// Original scope and attribution of trusted host-produced bytes.
pub struct HostArtifactPublication<'a> {
    pub title: &'a str,
    pub media_type: &'a str,
    pub provenance: Value,
    pub scope: ArtifactScope,
    pub owner: crate::protocol::ActorId,
}

impl ArtifactService {
    /// Stages `bytes` as a new blob generation (or verifies and reuses the existing one) under
    /// a journaled staging operation. `Ok(Err(cause))` is a publication failure already
    /// resolved on the journal.
    pub fn stage_host_bytes(
        &self,
        store: &dyn ApplicationStore,
        bytes: &[u8],
        source: Value,
    ) -> Result<Result<StagedFile, String>, StorageError> {
        let env = Preparation::new(store, None, None);
        let digest = sha256_hex(bytes);
        let d = digest.clone();
        let reuse = env
            .read(|s| s.artifacts().blob(&d))?
            .filter(|b| matches!(b.state, BlobState::Present | BlobState::UnlinkPending))
            .map(|b| b.generation);
        let payload = json!({
            "digest": digest,
            "byteCount": bytes.len(),
            "source": source,
        });
        let (staging, published) = self.journal_and_publish(
            &env,
            &mut std::io::Cursor::new(bytes),
            &digest,
            bytes.len() as i64,
            reuse.as_deref(),
            payload,
        )?;
        let published = match published {
            Ok(p) => p,
            Err(fault) => {
                self.abandon_or_defer(
                    &env,
                    &staging,
                    json!({"abandoned": "the host file was not published"}),
                );
                if let (FileFault::Corrupt(_), Some(g)) = (&fault, reuse.as_deref()) {
                    let _ = self.files.quarantine(&digest, g);
                }
                return Ok(Err(format!("host file publication failed: {fault:?}")));
            }
        };
        Ok(Ok(StagedFile {
            operation_id: staging.operation_id.clone(),
            digest,
            byte_count: bytes.len() as i64,
            generation: published.generation,
            reused: published.reused,
            relative_path: published.relative_path,
            origin: SourceOrigin::default(),
            source,
            fence: staging.fence,
        }))
    }

    /// Records the staged bytes as a workspace artifact owned by the host (blob, artifact,
    /// immutable version, staging completion and history) inside `tx`.
    pub fn commit_host_bytes(
        &self,
        tx: &mut dyn WriteTransaction,
        staged: &StagedFile,
        title: &str,
        media_type: &str,
        provenance: Value,
    ) -> Result<StoredVersion, StorageError> {
        let owner = tx.store().host_actor.clone();
        self.commit_host_bytes_in_scope(
            tx,
            staged,
            HostArtifactPublication {
                title,
                media_type,
                provenance,
                scope: ArtifactScope::Workspace,
                owner,
            },
        )
    }

    /// Publishes trusted host output in its original invocation's artifact scope.
    pub fn commit_host_bytes_in_scope(
        &self,
        tx: &mut dyn WriteTransaction,
        staged: &StagedFile,
        publication: HostArtifactPublication<'_>,
    ) -> Result<StoredVersion, StorageError> {
        let HostArtifactPublication {
            title,
            media_type,
            provenance,
            scope,
            owner,
        } = publication;
        let generation = put_staged_blob(tx, &*self.files, staged)?;
        let host = tx.store().host_actor.clone();
        let now = tx.now_ms();
        let artifact_id = tx.new_id(IdKind::Artifact);
        let version_id = tx.new_id(IdKind::ArtifactVersion);
        let mut artifact = StoredArtifact {
            id: artifact_id.clone(),
            origin_project_id: scope.project().cloned(),
            origin_work_id: None,
            title: title.chars().take(256).collect(),
            revision: 1,
            scope,
            owner: Some(owner),
            retention: ArtifactRetention::Explicit,
            scope_revision: 1,
            reference_revision: 1,
            current_version_id: None,
            state: ArtifactState::Live,
            created_at_ms: now,
            provenance: json!({
                "producedBy": host.0.as_str(),
                "principalKind": "Host",
                "derived": provenance,
            }),
        };
        tx.artifacts().insert_artifact(&artifact)?;
        let version = StoredVersion {
            id: version_id.clone(),
            artifact_id: artifact_id.clone(),
            version_no: 1,
            blob_digest: staged.digest.clone(),
            byte_count: staged.byte_count,
            blob_generation: generation,
            media_type: media_type.into(),
            created_at_ms: now,
            deleted_at_ms: None,
            source: json!({
                "source": staged.source,
                "operationId": staged.operation_id.0.as_str(),
                "producedBy": host.0.as_str(),
                "principalKind": "Host",
            }),
        };
        tx.artifacts().insert_version(&version)?;
        artifact.current_version_id = Some(version_id.clone());
        tx.artifacts().update_artifact(&artifact)?;
        super::complete_operation(
            tx,
            &staged.operation_id,
            json!({"artifactId": artifact_id.as_str(), "versionId": version_id.as_str()}),
        )?;
        let event = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: event,
            actor: host,
            project_id: None,
            root_id: None,
            operation_id: Some(staged.operation_id.clone()),
            aggregate_kind: HOST_ARTIFACT_AGGREGATE.into(),
            aggregate_id: artifact_id.as_str().to_owned(),
            aggregate_revision: 1,
            kind: "HostArtifactPublished".into(),
            payload: json!({
                "artifactId": artifact_id.as_str(), "versionId": version_id.as_str(),
                "digest": staged.digest, "byteCount": staged.byte_count,
                "mediaType": media_type, "principalKind": "Host",
            }),
        })?;
        Ok(version)
    }

    /// Resolves a staging after its publishing transaction: one that did not commit is
    /// abandoned, and a committed one that reused another live generation removes its own
    /// orphan generation.
    pub fn settle_host_bytes(
        &self,
        store: &dyn ApplicationStore,
        staged: &StagedFile,
        committed: bool,
    ) {
        let env = Preparation::new(store, None, None);
        if !committed {
            self.abandon_or_defer(
                &env,
                &staged.staging_ref(),
                json!({"abandoned": "the publishing transaction did not commit"}),
            );
            return;
        }
        if staged.reused {
            return;
        }
        let (digest, own) = (staged.digest.clone(), staged.operation_id.0.clone());
        let bound = env.read(|s| {
            Ok(s.artifacts()
                .blob(&digest)?
                .is_some_and(|b| b.generation == own.as_str()))
        });
        if matches!(bound, Ok(false)) {
            let _ = self.files.unlink(&staged.digest, own.as_str());
        }
    }
}

impl ArtifactService {
    /// Internal cleanup for a terminal, unpublished Help draft. Public mutations cannot mint
    /// trusted host provenance, and a reused published reference has a different generation.
    pub(crate) fn retire_help_draft(
        &self,
        tx: &mut dyn WriteTransaction,
        reference: &crate::protocol::provider::ArtifactRef,
        generation: &crate::protocol::Id,
        operation: &crate::protocol::OperationId,
    ) -> Result<(), StorageError> {
        let Some(mut artifact) = tx.artifacts().artifact(&reference.artifact_id)? else {
            return Ok(());
        };
        if !super::retained_help(&artifact, &tx.store().host_actor)
            || artifact.provenance["derived"]["generationId"] != serde_json::json!(generation)
            || artifact.provenance["derived"]["operationId"] != serde_json::json!(operation)
        {
            return Ok(());
        }
        let record = tx
            .operations()
            .operation(operation)?
            .ok_or_else(|| StorageError::InvalidRecord("Help cleanup operation missing".into()))?;
        if !crate::ports::storage::is_terminal(record.state) {
            return Err(StorageError::Busy);
        }
        let now = tx.now_ms();
        for version in tx.artifacts().versions(&artifact.id)? {
            if version.live() {
                tx.artifacts().tombstone_version(&version.id, now)?;
                super::release_blob(tx, &version.blob_digest)?;
            }
        }
        self.retire(tx, &mut artifact)?;
        artifact.revision += 1;
        tx.artifacts().update_artifact(&artifact)?;
        Ok(())
    }
}
