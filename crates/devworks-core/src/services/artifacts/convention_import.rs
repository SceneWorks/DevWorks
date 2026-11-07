//! Private import source leases and prospective artifact rows. No caller can publish an
//! arbitrary artifact through this seam, and no file open/hash occurs in the publishing UoW.
use super::convention_source::ConventionSourceLease;
use super::{ArtifactService, StagedFile, complete_operation};
use crate::{
    context::InvocationContext,
    ports::{IdKind, storage::*},
    protocol::provider::ArtifactRef,
    services::{
        conventions::importing::parser::{Entry, ReadSeek},
        sessions::Failure,
    },
};
use serde_json::json;
use std::{
    io::{self, Read, Seek, SeekFrom},
    sync::Arc,
};

pub(crate) type ImportSourceLease = ConventionSourceLease;
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
            .read_range_controlled(self.position, output.len().min(64 * 1024), &mut || {
                (self.stop)()
            })
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
impl ConventionSourceLease {
    pub(crate) fn import_reader(
        self,
        stop: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Box<dyn ReadSeek> {
        Box::new(SourceReader {
            lease: self,
            position: 0,
            stop,
        })
    }
}
fn matches_reference(version: &StoredVersion, reference: &ArtifactRef) -> bool {
    version.id == reference.version_id
        && version.artifact_id == reference.artifact_id
        && version.blob_digest == reference.sha256.0
        && version.byte_count == reference.byte_length.get()
        && version.media_type == reference.media_type
}
impl ArtifactService {
    pub(crate) fn check_import_quota(
        &self,
        tx: &mut dyn WriteTransaction,
        draft: &StoredConventionImport,
    ) -> Result<(), StorageError> {
        let pending = tx
            .conventions()
            .convention_import_storage_usage(&draft.operation_id)?;
        let maximum = super::setting_i64(
            tx.configuration(),
            None,
            crate::settings::catalog::ARTIFACT_MAX_FILE_KEY,
        )
        .map_err(|_| StorageError::ScopeDenied)?;
        let quota = super::setting_i64(
            tx.configuration(),
            draft.scope.project(),
            crate::settings::catalog::ARTIFACT_QUOTA_KEY,
        )
        .map_err(|_| StorageError::ScopeDenied)?;
        let scope = draft
            .scope
            .project()
            .cloned()
            .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
        let total = super::usage(tx.artifacts(), &scope)?
            .checked_add(pending.additional_scope_bytes)
            .ok_or_else(|| StorageError::InvalidRecord("import quota overflow".into()))?;
        if (maximum > 0 && pending.max_entry_bytes > maximum) || (quota > 0 && total > quota) {
            return Err(StorageError::InvalidRecord(
                "import exceeds current artifact quota".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn import_source(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        reference: &ArtifactRef,
    ) -> Result<ImportSourceLease, Failure> {
        self.resolve_convention_upload(read, ctx, reference)
    }
    pub(crate) fn validate_import_source(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        reference: &ArtifactRef,
        expected: &StoredVersion,
    ) -> Result<(), Failure> {
        if !expected.live() || !matches_reference(expected, reference) {
            return Err(StorageError::ScopeDenied.into());
        }
        self.revalidate_convention_upload(tx, ctx, expected)
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
