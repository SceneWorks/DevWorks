use super::{PackageHost, policy, project_disabled, recovery};
use devworks_core::authority::reach::{Reach, reach};
use devworks_core::context::InvocationContext;
use devworks_core::ports::packages::Inspection;
use devworks_core::ports::storage::{
    ArtifactScope, AuthorityRead, ReadOptions, StorageError, StoreInfo, StoredPackageVersion,
    WriteOptions, read_snapshot, run_write,
};
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::services::artifacts::{host::HostArtifactPublication, version_ref};
use devworks_extension_protocol::package::InvocationScope;
use devworks_extension_protocol::scalars as wire;
use serde_json::Value;

pub(super) fn scope_of(principal: &InvocationContext) -> Result<InvocationScope, String> {
    let id = |value: &str| wire::Id::try_from(value.to_owned()).map_err(str::to_owned);
    Ok(
        match (
            principal.project_id(),
            principal.root_id(),
            principal.assignment_id(),
        ) {
            (None, None, None) => InvocationScope::Workspace,
            (Some(project), None, None) => InvocationScope::Project {
                project_id: wire::ProjectId(id(project.0.as_str())?),
            },
            (Some(project), Some(root), None) => InvocationScope::Root {
                project_id: wire::ProjectId(id(project.0.as_str())?),
                root_id: wire::RootId(id(root.0.as_str())?),
            },
            (Some(project), Some(root), Some(assignment)) => InvocationScope::Assignment {
                project_id: wire::ProjectId(id(project.0.as_str())?),
                root_id: wire::RootId(id(root.0.as_str())?),
                assignment_id: wire::AssignmentId(id(assignment.0.as_str())?),
            },
            _ => return Err("inconsistent package invocation scope".into()),
        },
    )
}

fn current_scope(
    store: &StoreInfo,
    authority: &dyn AuthorityRead,
    principal: &InvocationContext,
) -> Result<bool, StorageError> {
    if store.restore_epoch != principal.restore_epoch().get()
        || &store.store_id != principal.store_id()
    {
        return Ok(false);
    }
    Ok(match reach(authority, principal)? {
        Some(Reach::Workspace) => true,
        Some(Reach::Project(project)) => principal.project_id() == Some(&project),
        None => false,
    })
}

impl PackageHost {
    pub(super) fn verified_inspection(
        &self,
        version: &StoredPackageVersion,
    ) -> Result<Inspection, String> {
        let inspection = policy::inspection(version).ok_or("the package inspection is invalid")?;
        if !recovery::compact(&inspection) {
            return Ok(inspection);
        }
        let ports = self
            .broker
            .get()
            .ok_or("package artifacts are not available")?;
        recovery::recover(
            &*self.store,
            &ports.artifacts,
            &self.service,
            &self.host_ctx(),
            version,
        )
        .map_err(|_| "the approved package manifest cannot be verified".into())
    }

    /// Reads the document identified by the exact schema asset in a verified manifest.
    pub(super) fn contribution_schema(
        &self,
        inspection: &Inspection,
        reference: &wire::SchemaRef,
    ) -> Result<Value, String> {
        let reference = serde_json::to_value(reference).map_err(|error| error.to_string())?;
        let file = inspection
            .schema_file(&reference)
            .ok_or("the package schema is absent")?;
        let bytes = self
            .service
            .files()
            .read_verified(
                &inspection.package_id,
                &inspection.manifest_digest,
                file,
                1024 * 1024,
            )
            .map_err(|error| error.to_string())?;
        devworks_core::framing::parse(&bytes).map_err(|error| error.to_string())
    }

    /// Publishes bounded, host-prepared input only while the chosen version is active and the
    /// original invocation still belongs to this store and scope.
    pub(super) fn stage_contribution_input(
        &self,
        principal: &InvocationContext,
        inspection_identity: (&str, &str),
        bytes: &[u8],
        provenance: Value,
    ) -> Result<ArtifactRef, String> {
        let (package, digest) = inspection_identity;
        let ports = self
            .broker
            .get()
            .ok_or("package artifacts are not available")?;
        let staged = ports
            .artifacts
            .stage_host_bytes(&*self.store, bytes, provenance.clone())
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        let scope = principal
            .project_id()
            .cloned()
            .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
        let artifacts = ports.artifacts.clone();
        let pending = staged.clone();
        let digest = digest.to_owned();
        let package = package.to_owned();
        let principal = principal.clone();
        let committed = run_write(&*self.store, WriteOptions::observed(), move |tx| {
            let info = tx.store().clone();
            if !current_scope(&info, &*tx.authority(), &principal)?
                || tx.actors().actor_kind(principal.principal())?
                    != Some(principal.principal_kind())
                || !tx.packages().package(&package)?.is_some_and(|record| {
                    record.installed
                        && record.enabled
                        && record.active_digest.as_deref() == Some(digest.as_str())
                        && scope
                            .project()
                            .is_none_or(|project| !project_disabled(&record, project.0.as_str()))
                })
            {
                return Err(StorageError::ScopeDenied);
            }
            let version = artifacts.commit_host_bytes_in_scope(
                tx,
                &pending,
                HostArtifactPublication {
                    title: "Package contribution input",
                    media_type: "application/json",
                    provenance,
                    scope,
                    owner: principal.principal().clone(),
                },
            )?;
            Ok(version_ref(&version))
        });
        ports
            .artifacts
            .settle_host_bytes(&*self.store, &staged, committed.is_ok());
        committed.map_err(|error| error.to_string())
    }

    pub(super) fn contribution_candidates(
        &self,
        principal: &InvocationContext,
        kind: &str,
        access: Option<&str>,
        identity: Option<(&str, &str)>,
    ) -> Result<Vec<StoredPackageVersion>, String> {
        let store = self.store.info();
        read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            if !current_scope(&store, snapshot.authority(), principal)?
                || snapshot.actors().actor_kind(principal.principal())?
                    != Some(principal.principal_kind())
            {
                return Err(StorageError::ScopeDenied);
            }
            let packages = self.service.registry(snapshot.packages())?;
            let registry = policy::Registry {
                packages: &packages,
                runtimes: &**self.service.runtimes(),
                host: self.service.platform(),
            };
            Ok(packages
                .values()
                .filter_map(|(record, version)| {
                    let version = version.as_ref()?;
                    if identity.is_some_and(|(package, digest)| {
                        record.package_id != package || version.manifest_digest != digest
                    }) {
                        return None;
                    }
                    let enabled = principal
                        .project_id()
                        .is_none_or(|project| !project_disabled(record, project.0.as_str()));
                    let ready = matches!(
                        registry.availability(record, Some(version)).0,
                        devworks_core::protocol::records::PackageAvailability::Ready
                    );
                    if !enabled || !ready {
                        return None;
                    }
                    let inspection = policy::inspection(version)?;
                    if access.is_some_and(|access| {
                        !inspection.access.iter().any(|entry| entry == access)
                    }) {
                        return None;
                    }
                    inspection
                        .contributions
                        .iter()
                        .any(|entry| entry.kind == kind)
                        .then(|| version.clone())
                })
                .collect::<Vec<_>>())
        })
        .map_err(|error| error.to_string())
    }

    pub(super) fn active_contributions(
        &self,
        principal: &InvocationContext,
        kind: &str,
    ) -> Result<Vec<Inspection>, String> {
        let candidates = self.contribution_candidates(principal, kind, None, None)?;
        Ok(candidates
            .into_iter()
            .filter_map(|version| match self.verified_inspection(&version) {
                Ok(inspection) => Some(inspection),
                Err(error) => {
                    self.service.record_log(
                        &version.package_id,
                        devworks_core::protocol::records::Severity::Warning,
                        "PackageContributionUnavailable",
                        "The installed package contribution could not be verified",
                        Some(error),
                    );
                    None
                }
            })
            .collect())
    }

    pub(super) fn contribution_current(
        &self,
        principal: &InvocationContext,
        kind: &str,
        identity: (&str, &str),
    ) -> Result<bool, String> {
        Ok(!self
            .contribution_candidates(principal, kind, None, Some(identity))?
            .is_empty())
    }
}
