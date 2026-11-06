//! Private import source leases and prospective artifact rows. No caller can publish an
//! arbitrary artifact through this seam, and no file open/hash occurs in the publishing UoW.
use super::lifecycle::ReaderGuard;
use super::{ArtifactService, StagedFile, Viewer, complete_operation, visible};
use crate::{
    context::InvocationContext,
    ports::{
        IdKind, Preparation,
        files::{ArtifactFiles, SourceOrigin},
        storage::*,
    },
    protocol::{DomainErrorCode, provider::ArtifactRef},
    services::{
        conventions::importing::parser::{Entry, ReadSeek},
        sessions::{Failure, Refusal},
    },
};
use serde_json::json;
use std::{
    io::{self, Read, Seek, SeekFrom},
    sync::Arc,
};

pub(crate) struct ImportSourceLease {
    pub version: StoredVersion,
    files: Arc<dyn ArtifactFiles>,
    _hold: ReaderGuard,
}
struct SourceReader {
    lease: ImportSourceLease,
    position: u64,
    stop: Arc<dyn Fn() -> bool + Send + Sync>,
}
impl Read for SourceReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if (self.stop)() {
            return Err(io::Error::other("import stopped"));
        }
        if output.is_empty() || self.position >= self.lease.version.byte_count as u64 {
            return Ok(0);
        }
        let bytes = self
            .lease
            .files
            .read_range(
                &self.lease.version.blob_digest,
                &self.lease.version.blob_generation,
                self.lease.version.byte_count as u64,
                self.position,
                output.len().min(64 * 1024),
            )
            .map_err(|_| io::Error::other("immutable import source unavailable"))?;
        if bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "immutable import source shortened",
            ));
        }
        output[..bytes.len()].copy_from_slice(&bytes);
        self.position += bytes.len() as u64;
        Ok(bytes.len())
    }
}
impl Seek for SourceReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
            SeekFrom::End(delta) => i128::from(self.lease.version.byte_count) + i128::from(delta),
        };
        if position < 0 || position > i128::from(self.lease.version.byte_count) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "import source position outside immutable version",
            ));
        }
        self.position = position as u64;
        Ok(self.position)
    }
}
impl ImportSourceLease {
    pub fn reader(self, stop: Arc<dyn Fn() -> bool + Send + Sync>) -> Box<dyn ReadSeek> {
        Box::new(SourceReader {
            lease: self,
            position: 0,
            stop,
        })
    }
}
fn missing(reference: &ArtifactRef) -> Failure {
    Refusal::new(
        DomainErrorCode::NotFound,
        "convention_import_source",
        Some(reference.version_id.clone()),
    )
    .into()
}
fn matches_reference(version: &StoredVersion, reference: &ArtifactRef) -> bool {
    version.id == reference.version_id
        && version.artifact_id == reference.artifact_id
        && version.blob_digest == reference.sha256.0
        && version.byte_count == reference.byte_length.get()
        && version.media_type == reference.media_type
}
impl ArtifactService {
    pub(crate) fn import_source(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        reference: &ArtifactRef,
    ) -> Result<ImportSourceLease, Failure> {
        let viewer = Viewer::resolve(read.authority(), ctx)?;
        let artifact = read
            .artifacts()
            .artifact(&reference.artifact_id)?
            .filter(|artifact| artifact.state == ArtifactState::Live)
            .ok_or_else(|| missing(reference))?;
        if !visible(read.artifacts(), &viewer, &artifact)? {
            return Err(missing(reference));
        }
        let version = read
            .artifacts()
            .version(&reference.version_id)?
            .filter(|version| {
                version.live()
                    && version.artifact_id == artifact.id
                    && matches_reference(version, reference)
            })
            .ok_or_else(|| missing(reference))?;
        let hold = self.readers.acquire(&version.blob_digest);
        if !read
            .artifacts()
            .blob(&version.blob_digest)?
            .is_some_and(|blob| {
                blob.state == BlobState::Present
                    && blob.generation == version.blob_generation
                    && blob.byte_count == version.byte_count
            })
        {
            return Err(missing(reference));
        }
        Ok(ImportSourceLease {
            version,
            files: self.files.clone(),
            _hold: hold,
        })
    }
    pub(crate) fn validate_import_source(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        reference: &ArtifactRef,
        expected: &StoredVersion,
    ) -> Result<(), Failure> {
        let viewer = Viewer::resolve(tx.authority(), ctx)?;
        let artifact = tx
            .artifacts()
            .artifact(&reference.artifact_id)?
            .filter(|artifact| artifact.state == ArtifactState::Live)
            .ok_or_else(|| missing(reference))?;
        if !visible(tx.artifacts(), &viewer, &artifact)? {
            return Err(missing(reference));
        }
        if tx.artifacts().version(&reference.version_id)?.as_ref() != Some(expected)
            || !expected.live()
            || !matches_reference(expected, reference)
            || expected.artifact_id != artifact.id
        {
            return Err(missing(reference));
        }
        Ok(())
    }
    pub(crate) fn stage_import_reader(
        &self,
        store: &dyn ApplicationStore,
        cancellation: &Cancellation,
        reader: &mut dyn Read,
        expected_digest: &str,
        expected_bytes: u64,
        source: serde_json::Value,
    ) -> Result<StagedFile, StorageError> {
        let count = i64::try_from(expected_bytes)
            .map_err(|_| StorageError::InvalidRecord("import entry exceeds store range".into()))?;
        crate::protocol::provider::Digest::try_from(expected_digest.to_owned())
            .map_err(|_| StorageError::InvalidRecord("invalid import entry digest".into()))?;
        if cancellation.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        let env = Preparation::new(store, Some(cancellation), None);
        let digest = expected_digest.to_owned();
        let hash = digest.clone();
        let reuse = env.read(move |read| {
            Ok(read
                .artifacts()
                .blob(&hash)?
                .filter(|blob| matches!(blob.state, BlobState::Present | BlobState::UnlinkPending))
                .map(|blob| blob.generation))
        })?;
        let payload = json!({"digest":digest,"byteCount":count,"source":source});
        let (staging, published) =
            self.journal_and_publish(&env, reader, &digest, count, reuse.as_deref(), payload)?;
        let published = match published {
            Ok(published) => published,
            Err(_) => {
                self.abandon_or_defer(
                    &env,
                    &staging,
                    json!({"abandoned":"import entry did not stage"}),
                );
                return Err(if cancellation.is_cancelled() {
                    StorageError::Cancelled
                } else {
                    StorageError::InvalidRecord(
                        "import entry digest/length or managed staging failed".into(),
                    )
                });
            }
        };
        Ok(StagedFile {
            operation_id: staging.operation_id,
            digest,
            byte_count: count,
            generation: published.generation,
            reused: published.reused,
            relative_path: published.relative_path,
            origin: SourceOrigin::default(),
            source,
            fence: staging.fence,
        })
    }
    /// The enclosing worker must propagate any failure so the complete capture UoW rolls back.
    pub(crate) fn bind_import_capture(
        &self,
        tx: &mut dyn WriteTransaction,
        draft: &StoredConventionImport,
        entry: &Entry,
        staged: &StagedFile,
    ) -> Result<StoredConventionImportArtifact, StorageError> {
        let stage = tx
            .operations()
            .operation(&staged.operation_id)?
            .ok_or_else(|| StorageError::InvalidRecord("missing import staging journal".into()))?;
        if stage.state != crate::protocol::OperationState::Dispatching
            || stage
                .latest_attempt()
                .map(|attempt| attempt.fence())
                .as_ref()
                != Some(&staged.fence)
        {
            return Err(StorageError::Busy);
        }
        // Competing canonical generations are retried through a fresh verified staging read.
        // Never hash/open another generation while holding the SQL write owner.
        match tx.artifacts().blob(&staged.digest)? {
            None if !staged.reused => {}
            Some(blob)
                if blob.generation == staged.generation
                    && blob.byte_count == staged.byte_count
                    && matches!(blob.state, BlobState::Present | BlobState::UnlinkPending) => {}
            Some(blob)
                if !staged.reused
                    && matches!(blob.state, BlobState::Unlinked | BlobState::Retired) => {}
            _ => return Err(StorageError::Busy),
        }
        let blob = tx.artifacts().put_blob(
            &staged.digest,
            staged.byte_count,
            &staged.generation,
            &staged.relative_path,
        )?;
        let artifact_id = tx.new_id(IdKind::Artifact);
        let version_id = tx.new_id(IdKind::ArtifactVersion);
        let now = tx.now_ms();
        let host = tx.store().host_actor.clone();
        let scope = draft
            .scope
            .project()
            .cloned()
            .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
        let media = entry
            .media_type
            .as_deref()
            .filter(|value| {
                !value.is_empty() && value.chars().count() <= 128 && !value.contains('\0')
            })
            .unwrap_or(if entry.kind == "Resource" {
                "application/octet-stream"
            } else {
                "text/markdown; charset=utf-8"
            });
        let artifact = StoredArtifact {
            id: artifact_id.clone(),
            origin_project_id: draft.scope.project().cloned(),
            origin_work_id: None,
            title: format!("Imported convention {}", entry.kind),
            revision: 1,
            scope,
            owner: Some(draft.author_id.clone()),
            retention: ArtifactRetention::Explicit,
            scope_revision: 1,
            reference_revision: 1,
            current_version_id: Some(version_id.clone()),
            state: ArtifactState::Live,
            created_at_ms: now,
            provenance: json!({"producedBy":host,"principalKind":"Host","derived":{"kind":"ConventionImport","operationId":draft.operation_id,"actorId":draft.author_id}}),
        };
        let version = StoredVersion {
            id: version_id,
            artifact_id,
            version_no: 1,
            blob_digest: staged.digest.clone(),
            blob_generation: blob.generation,
            byte_count: staged.byte_count,
            media_type: media.into(),
            created_at_ms: now,
            deleted_at_ms: None,
            source: json!({"source":staged.source,"operationId":staged.operation_id,"producedBy":host,"principalKind":"Host"}),
        };
        let row = StoredConventionImportArtifact {
            operation_id: draft.operation_id.clone(),
            ordinal: entry.ordinal,
            kind: entry.kind.into(),
            resource_id: (entry.kind == "Resource").then(|| tx.new_id(IdKind::ConventionResource)),
            resource_key: entry.key.clone(),
            resource_name: entry.name.clone(),
            resource_lifecycle: entry.lifecycle.clone(),
            resource_media_type: entry.media_type.clone(),
            artifact,
            version,
            staging_operation_id: staged.operation_id.clone(),
            staging_fence: staged.fence.clone(),
            managed_path: staged.relative_path.clone(),
        };
        tx.conventions().stage_convention_import_artifact(&row)?;
        complete_operation(
            tx,
            &staged.operation_id,
            json!({"privateConventionImport":draft.operation_id,"ordinal":entry.ordinal}),
        )?;
        Ok(row)
    }
}
