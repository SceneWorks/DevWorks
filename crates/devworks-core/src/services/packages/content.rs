//! Materializes a package's declarative content for the calling principal. Adoption stays with
//! the existing skill/role/prompt owners and their normal approval and visibility rules.
use super::{PackageService, conflict, not_found, policy};
use crate::context::InvocationContext;
use crate::operations::addons::ReadContentInput;
use crate::ports::Preparation;
use crate::ports::packages::Inspection;
use crate::ports::storage::{
    ArtifactScope, PackageRegistryRead, StorageError, StoredPackageVersion, WriteTransaction,
};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::PackageContent;
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedText, Label};
use crate::services::artifacts::host::HostArtifactPublication;
use crate::services::artifacts::{StagedFile, version_ref};
use crate::services::kernel::Failure;
use serde_json::json;

const MAX_CONTENT: u64 = 1024 * 1024;

pub(super) struct ContentInput {
    kind: Label,
    media_type: String,
    staged: StagedFile,
}

fn current(
    packages: &dyn PackageRegistryRead,
    ctx: &InvocationContext,
    i: &ReadContentInput,
) -> Result<Option<StoredPackageVersion>, StorageError> {
    let package = i.package_id.as_str();
    let digest = i.manifest_digest.as_str();
    let Some(record) = packages.package(package)? else {
        return Ok(None);
    };
    if !record.installed
        || !record.enabled
        || record.active_digest.as_deref() != Some(digest)
        || ctx
            .project_id()
            .is_some_and(|project| policy::project_disabled(&record, project.0.as_str()))
    {
        return Ok(None);
    }
    packages.package_version(package, digest)
}

impl PackageService {
    pub(super) fn prepare_content(
        &self,
        ctx: &InvocationContext,
        i: &ReadContentInput,
        env: &Preparation<'_>,
    ) -> Result<Option<ContentInput>, StorageError> {
        let Some(version) = env.read(|s| current(s.packages(), ctx, i))? else {
            return Ok(None);
        };
        let Some(mut inspection) = policy::inspection(&version) else {
            return Ok(None);
        };
        if super::recovery::compact(&inspection) {
            let Ok(recovered) =
                super::recovery::recover(&*self.store, &self.artifacts, self, ctx, &version)
            else {
                return Ok(None);
            };
            inspection = recovered;
        }
        let Some((kind, reference)) = content(&inspection, &i.contribution_id) else {
            return Ok(None);
        };
        let length = reference.byte_length.get() as u64;
        if length == 0
            || length > MAX_CONTENT
            || !matches!(
                reference.media_type.as_str(),
                "text/plain" | "text/markdown" | "application/json"
            )
        {
            return Ok(None);
        }
        let bytes = match self
            .artifacts
            .read_exact(&*self.store, ctx, &reference, MAX_CONTENT)
        {
            Ok((bytes, _)) => bytes,
            Err(crate::services::artifacts::lifecycle::ExactReadRefusal::Storage(error)) => {
                return Err(error);
            }
            Err(_) => {
                // A portable bundle may ship the exact bytes for a reference created on a
                // different installation. The manifest must also list the signed file bytes.
                let Some(file) = inspection.files.iter().find(|file| {
                    !file.executable
                        && file.sha256 == reference.sha256.as_str()
                        && file.byte_length == length
                }) else {
                    return Ok(None);
                };
                let Ok(bytes) = self.files.read_verified(
                    i.package_id.as_str(),
                    i.manifest_digest.as_str(),
                    file,
                    MAX_CONTENT,
                ) else {
                    return Ok(None);
                };
                bytes
            }
        };
        if std::str::from_utf8(&bytes).is_err() || bytes.contains(&0) {
            return Ok(None);
        }
        let provenance = json!({"kind":"PackageContent","packageId":i.package_id,
            "contributionId":i.contribution_id,"manifestDigest":i.manifest_digest});
        let Ok(staged) = self
            .artifacts
            .stage_host_bytes(&*self.store, &bytes, provenance)?
        else {
            return Ok(None);
        };
        Ok(Some(ContentInput {
            kind,
            media_type: reference.media_type,
            staged,
        }))
    }

    pub(super) fn read_content(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ReadContentInput,
        prepared: Option<&super::Prep>,
    ) -> Result<DomainDecision, Failure> {
        let Some(super::Prep::Content(Some(prepared))) = prepared else {
            return Err(not_found("package_content", i.contribution_id.0.as_str()).into());
        };
        if current(tx.packages(), ctx, i)?.is_none() {
            return Err(conflict("package_version", i.package_id.as_str()).into());
        }
        let scope = ctx
            .project_id()
            .cloned()
            .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
        let version = self.artifacts.commit_host_bytes_in_scope(
            tx,
            &prepared.staged,
            HostArtifactPublication {
                title: i.contribution_id.0.as_str(),
                media_type: &prepared.media_type,
                provenance: json!({"kind":"PackageContent","contentKind":prepared.kind,
                "packageId":i.package_id,"manifestDigest":i.manifest_digest,
                "contributionId":i.contribution_id}),
                scope,
                owner: ctx.principal().clone(),
            },
        )?;
        let artifact = version_ref(&version);
        Ok(DomainDecision::Applied {
            result: OperationResult::AddonsReadContent {
                value: PackageContent {
                    package_id: i.package_id.clone(),
                    contribution_id: i.contribution_id.clone(),
                    content_kind: prepared.kind.clone(),
                    manifest_digest: i.manifest_digest.clone(),
                    artifact,
                },
            },
        })
    }

    pub(super) fn settle_content(&self, prepared: &ContentInput, committed: bool) {
        self.artifacts
            .settle_host_bytes(&*self.store, &prepared.staged, committed);
    }
}

fn content(inspection: &Inspection, id: &Label) -> Option<(Label, ArtifactRef)> {
    let declaration = inspection
        .contributions
        .iter()
        .find(|entry| entry.kind == "Content" && entry.id == id.0)
        .and_then(|entry| entry.verified_descriptor())?;
    let kind = declaration["contentKind"].as_str()?;
    if !matches!(kind, "Skill" | "Role" | "Prompt") {
        return None;
    }
    let artifact = serde_json::from_value(declaration["artifact"].clone()).ok()?;
    Some((BoundedText(kind.to_owned()), artifact))
}
