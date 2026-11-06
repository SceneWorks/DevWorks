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
    /// Call outside every SQL snapshot/transaction. The managed file adapter verifies the
    /// complete immutable generation on the same handle it returns.
    pub fn open(self) -> Result<Box<dyn Read + Send>, ExactReadRefusal> {
        let bytes = self
            .files
            .open(
                &self.version.blob_digest,
                &self.version.blob_generation,
                self.version
                    .byte_count
                    .try_into()
                    .map_err(|_| ExactReadRefusal::Unreadable)?,
            )
            .map_err(|_| ExactReadRefusal::Unreadable)?;
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
