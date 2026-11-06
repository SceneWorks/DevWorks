//! A private, purpose-bound convention reader. A stored, currently authorized convention
//! association supplies the reference; callers never supply an arbitrary reference or gain a
//! fabricated Host context. Resolving metadata is separate from opening/hashing file bytes.
use super::ArtifactService;
use super::lifecycle::{ExactReadRefusal, ReaderGuard};
use crate::context::InvocationContext;
use crate::ports::files::ArtifactFiles;
use crate::ports::storage::{
    ArtifactState, BlobState, ReadSnapshot, StoredConvention, StoredConventionResource,
    StoredVersion,
};
use crate::protocol::data::DataScope;
use crate::protocol::provider::ArtifactRef;
use crate::services::conventions::access;
use crate::services::sessions::{Failure, Refusal, not_found};
use std::io::Read;
use std::sync::Arc;

pub(crate) enum ConventionSourceSelection<'a> {
    Markdown,
    Raw,
    Resource(&'a StoredConventionResource),
}

pub(crate) struct ConventionSourceLease {
    pub version: StoredVersion,
    files: Arc<dyn ArtifactFiles>,
    hold: ReaderGuard,
}

struct Reader {
    bytes: Box<dyn Read + Send>,
    _hold: ReaderGuard,
}
impl Read for Reader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.bytes.read(buffer)
    }
}
impl ConventionSourceLease {
    pub fn read_range_controlled(
        &self,
        offset: u64,
        max: usize,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<Vec<u8>, ExactReadRefusal> {
        let result = self.files.read_range_controlled(
            &self.version.blob_digest,
            &self.version.blob_generation,
            self.version
                .byte_count
                .try_into()
                .map_err(|_| ExactReadRefusal::Unreadable)?,
            offset,
            max,
            stopped,
        );
        if stopped() {
            return Err(ExactReadRefusal::Storage(
                crate::ports::storage::StorageError::Cancelled,
            ));
        }
        result.map_err(|_| ExactReadRefusal::Unreadable)
    }
    pub fn open_controlled(
        self,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<Box<dyn Read + Send>, ExactReadRefusal> {
        let result = self.files.open_controlled(
            &self.version.blob_digest,
            &self.version.blob_generation,
            self.version
                .byte_count
                .try_into()
                .map_err(|_| ExactReadRefusal::Unreadable)?,
            stopped,
        );
        if stopped() {
            return Err(ExactReadRefusal::Storage(
                crate::ports::storage::StorageError::Cancelled,
            ));
        }
        let bytes = result.map_err(|_| ExactReadRefusal::Unreadable)?;
        Ok(Box::new(Reader {
            bytes,
            _hold: self.hold,
        }))
    }
}

impl ArtifactService {
    pub(crate) fn resolve_convention_source(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        requested: Option<&DataScope>,
        owner: &StoredConvention,
        selection: ConventionSourceSelection<'_>,
    ) -> Result<ConventionSourceLease, Failure> {
        let scoped = access::scope(read, ctx, requested, false)?;
        if read
            .conventions()
            .convention(&owner.id, Some(owner.revision), &scoped.readable)?
            .as_ref()
            != Some(owner)
        {
            return Err(not_found("convention", &owner.id).into());
        }
        if owner.lifecycle == "Deleted" {
            return Err(not_found("convention", &owner.id).into());
        }
        let reference = match selection {
            ConventionSourceSelection::Markdown => owner.content_ref.as_ref(),
            ConventionSourceSelection::Raw => owner.raw_content_ref.as_ref(),
            ConventionSourceSelection::Resource(resource) => {
                if resource.convention_id != owner.id
                    || resource.lifecycle == "Deleted"
                    || read
                        .conventions()
                        .convention_resource(
                            &owner.id,
                            &resource.id,
                            Some(owner.revision),
                            &scoped.readable,
                        )?
                        .as_ref()
                        != Some(resource)
                {
                    return Err(not_found("convention_resource", &resource.id).into());
                }
                Some(&resource.content_ref)
            }
        }
        .ok_or_else(|| not_found("convention_content", &owner.id))?;
        let reference: ArtifactRef =
            serde_json::from_value(reference.clone()).map_err(|error| {
                crate::ports::storage::StorageError::InvalidRecord(error.to_string())
            })?;
        let artifact = read
            .artifacts()
            .artifact(&reference.artifact_id)?
            .filter(|artifact| artifact.state == ArtifactState::Live)
            .ok_or_else(|| not_found("convention_content", &owner.id))?;
        let version = read
            .artifacts()
            .version(&reference.version_id)?
            .filter(|version| {
                version.artifact_id == artifact.id
                    && version.deleted_at_ms.is_none()
                    && version.blob_digest == String::from(reference.sha256.clone())
                    && version.byte_count == reference.byte_length.get()
                    && version.media_type == reference.media_type
            })
            .ok_or_else(|| not_found("convention_content", &owner.id))?;
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
            return Err(Refusal::new(
                crate::protocol::DomainErrorCode::SourceChanged,
                "convention_content",
                Some(owner.id.clone()),
            )
            .into());
        }
        Ok(ConventionSourceLease {
            version,
            files: self.files.clone(),
            hold,
        })
    }
}

impl ArtifactService {
    /// An edit/import attachment may name a caller-owned upload. This normal artifact
    /// visibility check does not inherit the Conventions association of any other source.
    pub(crate) fn resolve_convention_upload(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        reference: &ArtifactRef,
    ) -> Result<ConventionSourceLease, Failure> {
        let viewer = super::Viewer::resolve(read.authority(), ctx)?;
        let artifact = read
            .artifacts()
            .artifact(&reference.artifact_id)?
            .filter(|a| a.state == ArtifactState::Live)
            .ok_or_else(|| not_found("convention_upload", &reference.artifact_id))?;
        if !super::visible(read.artifacts(), &viewer, &artifact)? {
            return Err(not_found("convention_upload", &reference.artifact_id).into());
        }
        let version = read
            .artifacts()
            .version(&reference.version_id)?
            .filter(|v| {
                v.live()
                    && v.artifact_id == artifact.id
                    && v.blob_digest == String::from(reference.sha256.clone())
                    && v.byte_count == reference.byte_length.get()
                    && v.media_type == reference.media_type
            })
            .ok_or_else(|| not_found("convention_upload", &reference.artifact_id))?;
        let hold = self.readers.acquire(&version.blob_digest);
        if !read
            .artifacts()
            .blob(&version.blob_digest)?
            .is_some_and(|b| {
                b.state == BlobState::Present
                    && b.generation == version.blob_generation
                    && b.byte_count == version.byte_count
            })
        {
            return Err(not_found("convention_upload", &reference.artifact_id).into());
        }
        Ok(ConventionSourceLease {
            version,
            files: self.files.clone(),
            hold,
        })
    }
}

impl ArtifactService {
    /// Retains an already committed managed generation under a Conventions-owned immutable
    /// reference. No file bytes or external helpers are written here. The domain owner first
    /// authorizes the source association/upload and its destination in this same unit of work.
    pub(crate) fn retain_convention_version(
        &self,
        tx: &mut dyn crate::ports::storage::WriteTransaction,
        source: &StoredVersion,
        scope: &crate::ports::storage::KnowledgeScope,
        convention: &crate::protocol::Id,
        resource: bool,
    ) -> Result<ArtifactRef, crate::ports::storage::StorageError> {
        use crate::ports::{
            IdKind,
            storage::{ArtifactRetention, ArtifactScope, StoredArtifact},
        };
        use crate::protocol::provider::Digest;
        use crate::services::sessions::counter;
        use serde_json::json;
        let changed = || {
            crate::ports::storage::StorageError::InvalidRecord(
                "convention source changed before retention".into(),
            )
        };
        if tx.artifacts().version(&source.id)?.as_ref() != Some(source)
            || !source.live()
            || !tx
                .artifacts()
                .artifact(&source.artifact_id)?
                .is_some_and(|a| a.state == ArtifactState::Live)
            || !tx.artifacts().blob(&source.blob_digest)?.is_some_and(|b| {
                b.state == BlobState::Present
                    && b.generation == source.blob_generation
                    && b.byte_count == source.byte_count
            })
        {
            return Err(changed());
        }
        let host = tx.store().host_actor.clone();
        let artifact_id = tx.new_id(IdKind::Artifact);
        let version_id = tx.new_id(IdKind::ArtifactVersion);
        let now = tx.now_ms();
        let artifact = StoredArtifact {
            id: artifact_id.clone(),
            origin_project_id: scope.project().cloned(),
            origin_work_id: None,
            title: if resource {
                "Convention attachment"
            } else {
                "Convention source"
            }
            .into(),
            revision: 1,
            scope: match scope {
                crate::ports::storage::KnowledgeScope::Workspace => ArtifactScope::Workspace,
                crate::ports::storage::KnowledgeScope::Project(p) => {
                    ArtifactScope::Project(p.clone())
                }
            },
            owner: Some(host.clone()),
            retention: ArtifactRetention::Explicit,
            scope_revision: 1,
            reference_revision: 1,
            current_version_id: Some(version_id.clone()),
            state: ArtifactState::Live,
            created_at_ms: now,
            provenance: json!({"producedBy":host,"principalKind":"Host","derived":{"kind":if resource{"ConventionResource"}else{"ConventionContent"},"conventionId":convention,"sourceVersionId":source.id,"sourceArtifactId":source.artifact_id}}),
        };
        let version = StoredVersion {
            id: version_id.clone(),
            artifact_id: artifact_id.clone(),
            version_no: 1,
            blob_digest: source.blob_digest.clone(),
            byte_count: source.byte_count,
            blob_generation: source.blob_generation.clone(),
            media_type: source.media_type.clone(),
            created_at_ms: now,
            deleted_at_ms: None,
            source: artifact.provenance.clone(),
        };
        // Artifact insertion precedes its version, as in the shared host publication path.
        let mut pending = artifact.clone();
        pending.current_version_id = None;
        tx.artifacts().insert_artifact(&pending)?;
        tx.artifacts().insert_version(&version)?;
        tx.artifacts().update_artifact(&artifact)?;
        Ok(ArtifactRef {
            artifact_id,
            version_id,
            sha256: Digest(source.blob_digest.clone()),
            byte_length: counter(source.byte_count),
            media_type: source.media_type.clone(),
        })
    }
}

impl ArtifactService {
    pub(crate) fn revalidate_convention_upload(
        &self,
        tx: &mut dyn crate::ports::storage::WriteTransaction,
        ctx: &InvocationContext,
        source: &StoredVersion,
    ) -> Result<(), Failure> {
        let viewer = super::Viewer::resolve(tx.authority(), ctx)?;
        let artifact = tx
            .artifacts()
            .artifact(&source.artifact_id)?
            .filter(|a| a.state == ArtifactState::Live)
            .ok_or_else(|| not_found("convention_upload", &source.artifact_id))?;
        if !super::visible(tx.artifacts(), &viewer, &artifact)?
            || tx.artifacts().version(&source.id)?.as_ref() != Some(source)
        {
            return Err(not_found("convention_upload", &source.artifact_id).into());
        }
        Ok(())
    }
}
