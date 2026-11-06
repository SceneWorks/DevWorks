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

/// Trusted host source; expected identity is independently verified while consuming the reader.
pub(crate) struct HostStream<'a> {
    pub reader: &'a mut dyn std::io::Read,
    pub digest: &'a str,
    pub byte_count: u64,
    pub source: Value,
}
struct ControlledReader<'a> {
    reader: &'a mut dyn std::io::Read,
    stopped: &'a mut dyn FnMut() -> bool,
}
impl std::io::Read for ControlledReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if (self.stopped)() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "host source interrupted",
            ));
        }
        self.reader.read(bytes)
    }
}

/// A sealed Help-owned source captured from the current store, never from caller JSON.
pub(crate) struct HelpFile {
    version: StoredVersion,
    _hold: super::lifecycle::ReaderGuard,
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
        self.stage_host_reader(
            store,
            HostStream {
                reader: &mut std::io::Cursor::new(bytes),
                digest: &sha256_hex(bytes),
                byte_count: bytes.len() as u64,
                source,
            },
            &mut || false,
        )
    }

    /// Streams a bounded host source through the existing staging journal. Both source and
    /// reused blob identity are checked under control before a real generation is returned.
    pub(crate) fn stage_host_reader(
        &self,
        store: &dyn ApplicationStore,
        input: HostStream<'_>,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<Result<StagedFile, String>, StorageError> {
        use sha2::{Digest as _, Sha256};
        use std::io::Read as _;
        let HostStream {
            reader,
            digest,
            byte_count,
            source,
        } = input;
        if stopped() {
            return Err(StorageError::Cancelled);
        }
        let n = i64::try_from(byte_count)
            .map_err(|_| StorageError::InvalidRecord("host source byte count overflow".into()))?;
        let env = Preparation::new(store, None, None);
        let candidate = env
            .read(|s| s.artifacts().blob(digest))?
            .filter(|b| matches!(b.state, BlobState::Present | BlobState::UnlinkPending));
        let staging = self.journal_staging(
            &env,
            digest,
            n,
            json!({"digest":digest,"byteCount":byte_count,"source":source}),
        )?;
        let published = (|| {
            if let Some(blob) = candidate {
                match self
                    .files
                    .open_controlled(digest, &blob.generation, byte_count, stopped)
                {
                    Ok(verified) => {
                        drop(verified);
                        let mut source = ControlledReader { reader, stopped };
                        let mut hash = Sha256::new();
                        let mut count = 0u64;
                        let mut buffer = [0; 64 * 1024];
                        loop {
                            let length = source
                                .read(&mut buffer)
                                .map_err(|e| FileFault::Source(e.to_string()))?;
                            if length == 0 {
                                break;
                            }
                            count = count
                                .checked_add(length as u64)
                                .ok_or(FileFault::Mismatch)?;
                            if count > byte_count {
                                return Err(FileFault::Mismatch);
                            }
                            hash.update(&buffer[..length]);
                        }
                        let actual = hash
                            .finalize()
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<String>();
                        if count != byte_count || actual != digest {
                            return Err(FileFault::Mismatch);
                        }
                        return Ok(crate::ports::files::PublishedBlob {
                            generation: blob.generation,
                            relative_path: blob.relative_path,
                            reused: true,
                        });
                    }
                    Err(FileFault::NotFound) => {}
                    Err(FileFault::Corrupt(_)) => {
                        self.files.quarantine(digest, &blob.generation)?;
                    }
                    Err(error) => return Err(error),
                }
            }
            let mut source = ControlledReader { reader, stopped };
            self.files.publish(
                &format!("{}.part", staging.operation_id.0.as_str()),
                &mut source,
                digest,
                byte_count,
                None,
                staging.operation_id.0.as_str(),
            )
        })();
        let published = match published {
            Ok(value) => value,
            Err(error) => {
                self.abandon_or_defer(
                    &env,
                    &staging,
                    json!({"abandoned":"the host stream was not published"}),
                );
                if stopped() {
                    return Err(StorageError::Cancelled);
                }
                return Ok(Err(format!("host stream publication failed: {error:?}")));
            }
        };
        Ok(Ok(StagedFile {
            operation_id: staging.operation_id,
            digest: digest.into(),
            byte_count: n,
            generation: published.generation,
            reused: published.reused,
            relative_path: published.relative_path,
            origin: SourceOrigin::default(),
            source,
            fence: staging.fence,
        }))
    }

    pub(crate) fn capture_help_file(
        &self,
        read: &dyn crate::ports::storage::ReadSnapshot,
        info: &crate::ports::storage::StoreInfo,
        reference: &crate::protocol::provider::ArtifactRef,
    ) -> Result<HelpFile, StorageError> {
        let hold = self.readers.acquire(reference.sha256.as_str());
        if read.stamp().store_id != info.store_id
            || read.stamp().restore_epoch != info.restore_epoch
        {
            return Err(StorageError::ScopeDenied);
        }
        let a = read
            .artifacts()
            .artifact(&reference.artifact_id)?
            .ok_or(StorageError::ViewUnavailable)?;
        let v = read
            .artifacts()
            .version(&reference.version_id)?
            .ok_or(StorageError::ViewUnavailable)?;
        if a.state != ArtifactState::Live
            || a.scope != ArtifactScope::Workspace
            || a.owner.as_ref() != Some(&info.host_actor)
            || a.provenance["producedBy"].as_str() != Some(info.host_actor.0.as_str())
            || a.provenance["principalKind"] != "Host"
            || a.provenance["derived"]["kind"] != "HelpTopic"
            || !v.live()
            || v.artifact_id != a.id
            || v.blob_digest != reference.sha256.as_str()
            || v.byte_count != reference.byte_length.get()
            || v.media_type != reference.media_type
            || v.source["producedBy"].as_str() != Some(info.host_actor.0.as_str())
            || v.source["principalKind"] != "Host"
            || v.source["source"]["kind"] != "HelpTopic"
        {
            return Err(StorageError::ViewUnavailable);
        }
        let content = read
            .help()
            .help_content(&v.blob_digest)?
            .ok_or(StorageError::ViewUnavailable)?;
        let commitment = content
            .commitment()
            .map_err(|_| StorageError::ViewUnavailable)?;
        if v.source["source"]["helpIndexDigest"].as_str() != Some(commitment.as_str()) {
            return Err(StorageError::ViewUnavailable);
        }
        let blob = read
            .artifacts()
            .blob(&v.blob_digest)?
            .ok_or(StorageError::ViewUnavailable)?;
        if !matches!(blob.state, BlobState::Present | BlobState::UnlinkPending)
            || blob.generation != v.blob_generation
            || blob.byte_count != v.byte_count
        {
            return Err(StorageError::ViewUnavailable);
        }
        Ok(HelpFile {
            version: v,
            _hold: hold,
        })
    }
    pub(crate) fn open_help_file(
        &self,
        source: &HelpFile,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<Box<dyn std::io::Read + Send>, StorageError> {
        let v = &source.version;
        let result = self.files.open_controlled(
            &v.blob_digest,
            &v.blob_generation,
            v.byte_count as u64,
            stopped,
        );
        if stopped() {
            return Err(StorageError::Cancelled);
        }
        result.map_err(|_| StorageError::ViewUnavailable)
    }
    pub(crate) fn read_help_range(
        &self,
        source: &HelpFile,
        offset: u64,
        max: usize,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<Vec<u8>, StorageError> {
        if max > 2 * 1024 * 1024 {
            return Err(StorageError::InvalidRecord(
                "Help range buffer exceeds bound".into(),
            ));
        }
        let v = &source.version;
        let result = self.files.read_range_controlled(
            &v.blob_digest,
            &v.blob_generation,
            v.byte_count as u64,
            offset,
            max,
            stopped,
        );
        if stopped() {
            return Err(StorageError::Cancelled);
        }
        result.map_err(|_| StorageError::ViewUnavailable)
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
