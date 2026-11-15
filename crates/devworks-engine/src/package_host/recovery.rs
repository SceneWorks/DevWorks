//! Recovers the full verified inspection of a version stored with an older compact inspection
//! (no contribution descriptors, no schema assets) from the exact bundle its record retains.
//!
//! Nothing is taken from the stored record but the bundle's reference and identity: the bytes
//! must hash to the recorded bundle digest, pass the inspector again and name the same manifest
//! and package. Anything else is refused, so a caller that needs a descriptor stays fail-closed.
use devworks_core::context::InvocationContext;
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::packages::{Inspection, MAX_BUNDLE_BYTES};
use devworks_core::ports::storage::{ApplicationStore, StoredPackageVersion};
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::packages::PackageService;

/// Why a compact inspection could not be recovered.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// The record retains no bundle reference (it must be reinstalled).
    NoSource,
    /// The retained bundle cannot be read.
    Unreadable,
    /// The retained bundle is not the recorded one or no longer verifies.
    Mismatch,
}

/// Whether `inspection` is an older compact one (some descriptor or every schema asset absent).
pub(crate) fn compact(inspection: &Inspection) -> bool {
    inspection.schema_assets.is_empty()
        || inspection
            .contributions
            .iter()
            .any(|c| c.descriptor.is_none())
}

/// Re-inspects the bundle `version` retains, read as `ctx` (module documentation).
pub(crate) fn recover(
    store: &dyn ApplicationStore,
    artifacts: &ArtifactService,
    service: &PackageService,
    ctx: &InvocationContext,
    version: &StoredPackageVersion,
) -> Result<Inspection, Refused> {
    // The record's own reference: a caller never substitutes a bundle.
    let source = version
        .provenance
        .get("source")
        .cloned()
        .ok_or(Refused::NoSource)?;
    let reference = serde_json::from_value(source).map_err(|_| Refused::Mismatch)?;
    let (bytes, _) = artifacts
        .read_exact(store, ctx, &reference, MAX_BUNDLE_BYTES)
        .map_err(|_| Refused::Unreadable)?;
    if sha256_hex(&bytes) != version.bundle_digest {
        return Err(Refused::Mismatch);
    }
    let inspection = service.inspect(&bytes).map_err(|_| Refused::Mismatch)?;
    if inspection.manifest_digest != version.manifest_digest
        || inspection.bundle_digest != version.bundle_digest
        || inspection.package_id != version.package_id
    {
        return Err(Refused::Mismatch);
    }
    Ok(inspection)
}
