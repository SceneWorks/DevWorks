//! Pure package policy: compatibility, prerequisite closure, effective availability, the
//! automatic-update eligibility of a trusted publisher's signed update (EXT-07) and the waits a
//! candidate version must clear before it may activate (EXT-02).
//!
//! Nothing here reads a store or a clock; the service supplies current records and decides in
//! its unit of work.
use crate::ports::packages::{HostPlatform, Inspection, RuntimeProbe};
use crate::ports::storage::{StoredPackage, StoredPackageVersion};
use crate::protocol::records::{PackageAvailability, PackageWaitReason};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// A version's verified inspection as stored.
pub fn inspection(v: &StoredPackageVersion) -> Option<Inspection> {
    serde_json::from_value(v.inspection.clone()).ok()
}

/// Why a version cannot run on this host at all (target, protocol, host version), if it cannot.
pub fn static_incompatibility(i: &Inspection, host: &HostPlatform) -> Option<String> {
    if i.entrypoint(&host.target_triple).is_none() {
        return Some(format!(
            "No prebuilt helper for this platform ({}); the package supports {}",
            host.target_triple,
            i.targets
                .iter()
                .map(|t| t.target_triple.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let (min, max) = (i.protocol_min, i.protocol_max);
    let major = host.protocol_major;
    if min.0 != major || max.0 != major || min.1 > host.protocol_minor {
        return Some(format!(
            "The package needs protocol {}.{}–{}.{}; this host speaks {}.0–{}.{}",
            min.0, min.1, max.0, max.1, major, major, host.protocol_minor
        ));
    }
    match (
        semver::Version::parse(&i.minimum_host_version),
        semver::Version::parse(&host.host_version),
    ) {
        (Ok(need), Ok(have)) if have >= need => None,
        (Ok(need), Ok(_)) => Some(format!("The package needs DevWorks {need} or newer")),
        _ => Some("The package's minimum host version cannot be compared".into()),
    }
}

/// One prerequisite and whether it is satisfied now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prerequisite {
    /// `package` or `runtime`.
    pub kind: &'static str,
    pub id: String,
    pub requirement: String,
    pub required: bool,
    pub satisfied: bool,
    pub reason: Option<String>,
    /// `Missing`, `Incompatible` or `Ready`.
    pub state: &'static str,
    /// The detected version (the dependency's active version or the probed runtime's).
    pub detected: Option<String>,
}

/// Everything availability needs about the other packages: their records and active versions.
pub struct Registry<'a> {
    pub packages: &'a BTreeMap<String, (StoredPackage, Option<StoredPackageVersion>)>,
    pub runtimes: &'a dyn RuntimeProbe,
    pub host: &'a HostPlatform,
}

fn version_matches(requirement: &str, version: &str) -> bool {
    match (
        semver::VersionReq::parse(requirement),
        semver::Version::parse(version),
    ) {
        (Ok(req), Ok(v)) => req.matches(&v),
        _ => false,
    }
}

impl Registry<'_> {
    /// The prerequisites of `i` with their current state. A package dependency is satisfied
    /// only by an installed, enabled package whose active version has exactly the pinned
    /// manifest digest, matches the version requirement and is itself available.
    pub fn prerequisites(&self, i: &Inspection) -> Vec<Prerequisite> {
        self.prerequisites_at(i, 0)
    }

    fn prerequisites_at(&self, i: &Inspection, depth: usize) -> Vec<Prerequisite> {
        let mut out = Vec::new();
        for d in &i.dependencies {
            let reason = match self.packages.get(&d.package_id) {
                None => Some(format!("Package {} is not installed", d.package_id)),
                Some((p, _)) if !p.installed => {
                    Some(format!("Package {} is not installed", d.package_id))
                }
                Some((p, _)) if !p.enabled => Some(format!("Package {} is disabled", d.package_id)),
                Some((_, None)) => Some(format!("Package {} has no active version", d.package_id)),
                Some((p, Some(v))) => {
                    if v.manifest_digest != d.manifest_digest {
                        Some(format!(
                            "Package {} is a different build than the one required",
                            d.package_id
                        ))
                    } else if !version_matches(&d.version_requirement, &v.version) {
                        Some(format!(
                            "Package {} {} does not satisfy {}",
                            d.package_id, v.version, d.version_requirement
                        ))
                    } else if depth >= 8 {
                        Some(format!(
                            "Package {} is part of a dependency chain that is too deep or cyclic",
                            d.package_id
                        ))
                    } else {
                        match self.availability_depth(p, Some(v), depth + 1) {
                            (PackageAvailability::Ready, _) => None,
                            (_, why) => {
                                Some(format!("Package {} is unavailable: {why}", d.package_id))
                            }
                        }
                    }
                }
            };
            let detected = match self.packages.get(&d.package_id) {
                Some((p, Some(v))) if p.installed => Some(v.version.clone()),
                _ => None,
            };
            let state = match (&reason, &detected) {
                (None, _) => "Ready",
                (Some(_), Some(_)) => "Incompatible",
                (Some(_), None) => "Missing",
            };
            out.push(Prerequisite {
                kind: "package",
                id: d.package_id.clone(),
                requirement: d.version_requirement.clone(),
                required: d.required,
                satisfied: reason.is_none(),
                reason,
                state,
                detected,
            });
        }
        for r in &i.runtimes {
            let detected = self.runtimes.runtime(&r.runtime_id);
            let reason = match detected.clone() {
                None => Some(match self.runtimes.missing_reason(&r.runtime_id) {
                    Some(why) => format!("Runtime {} is not available ({why})", r.runtime_id),
                    None => format!("Runtime {} is not available", r.runtime_id),
                }),
                Some(v) if !version_matches(&r.version_requirement, &v) => Some(format!(
                    "Runtime {} {v} does not satisfy {}",
                    r.runtime_id, r.version_requirement
                )),
                Some(_) => None,
            };
            let state = match (&reason, &detected) {
                (None, _) => "Ready",
                (Some(_), Some(_)) => "Incompatible",
                (Some(_), None) => "Missing",
            };
            out.push(Prerequisite {
                kind: "runtime",
                id: r.runtime_id.clone(),
                requirement: r.version_requirement.clone(),
                required: r.required,
                satisfied: reason.is_none(),
                reason,
                state,
                detected,
            });
        }
        out
    }

    /// Effective availability, separate from configured enablement (ADDON-02).
    pub fn availability(
        &self,
        p: &StoredPackage,
        active: Option<&StoredPackageVersion>,
    ) -> (PackageAvailability, String) {
        self.availability_depth(p, active, 0)
    }

    fn availability_depth(
        &self,
        p: &StoredPackage,
        active: Option<&StoredPackageVersion>,
        depth: usize,
    ) -> (PackageAvailability, String) {
        let Some(v) = active.filter(|_| p.installed) else {
            return (
                PackageAvailability::NotInstalled,
                "The package is not installed".into(),
            );
        };
        if !p.enabled {
            return (
                PackageAvailability::Disabled,
                "The package is disabled".into(),
            );
        }
        let Some(i) = inspection(v) else {
            return (
                PackageAvailability::Failed,
                "The installed version's record cannot be read".into(),
            );
        };
        if let Some(why) = static_incompatibility(&i, self.host) {
            return (PackageAvailability::Incompatible, why);
        }
        let unmet: Vec<String> = self
            .prerequisites_at(&i, depth)
            .into_iter()
            .filter(|q| q.required && !q.satisfied)
            .filter_map(|q| q.reason)
            .collect();
        if !unmet.is_empty() {
            return (PackageAvailability::MissingDependency, unmet.join("; "));
        }
        if let Some(f) = p
            .failure
            .as_ref()
            .filter(|f| f["digest"].as_str() == Some(v.manifest_digest.as_str()))
        {
            return (
                PackageAvailability::Failed,
                f["message"]
                    .as_str()
                    .unwrap_or("The helper failed")
                    .to_owned(),
            );
        }
        (PackageAvailability::Ready, "Ready".into())
    }
}

/// The publisher trust receipt of a package (EXT-07), as stored on the package record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trust {
    pub publisher_id: String,
    pub signer_key: String,
    pub package_id: String,
    /// Only `stable` (same major, no prerelease) is offered.
    pub stream: String,
    /// The approved normalized access envelope.
    pub approved_access: BTreeSet<String>,
}
impl Trust {
    pub fn from_record(v: &Value) -> Option<Self> {
        Some(Self {
            publisher_id: v["publisherId"].as_str()?.to_owned(),
            signer_key: v["signerKey"].as_str()?.to_owned(),
            package_id: v["packageId"].as_str()?.to_owned(),
            stream: v["stream"].as_str()?.to_owned(),
            approved_access: v["approvedAccess"]
                .as_array()?
                .iter()
                .map(|a| a.as_str().map(str::to_owned))
                .collect::<Option<_>>()?,
        })
    }
}

/// Why a trusted publisher's update cannot activate automatically, as the wait it needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ineligible {
    /// A different signing key or publisher, an unsigned candidate, or no applicable trust.
    Trust(&'static str),
    /// The candidate requests access beyond the approved envelope.
    Access(Vec<String>),
    /// A reviewed transition is needed: prerelease or major change, a changed public contract,
    /// a new required contribution, a new prerequisite or a changed state schema.
    Transition(&'static str),
}
impl Ineligible {
    pub fn wait(&self) -> PackageWaitReason {
        match self {
            Self::Trust(_) => PackageWaitReason::TrustApproval,
            Self::Access(_) => PackageWaitReason::AccessApproval,
            Self::Transition(_) => PackageWaitReason::ExactVersionApproval,
        }
    }
    pub fn reason(&self) -> String {
        match self {
            Self::Trust(r) | Self::Transition(r) => (*r).to_owned(),
            Self::Access(a) => {
                format!("requests access beyond the trusted scope: {}", a.join(", "))
            }
        }
    }
}

/// Whether `next` may replace `current` automatically under `trust` (EXT-07). Compares
/// normalized capability envelopes, prerequisites and public contracts, never labels.
pub fn auto_update(
    trust: &Trust,
    current: &Inspection,
    next: &Inspection,
) -> Result<(), Ineligible> {
    if trust.package_id != next.package_id || trust.publisher_id != next.publisher_id {
        return Err(Ineligible::Trust(
            "the publisher is not the trusted publisher",
        ));
    }
    match &next.signer_key {
        None => return Err(Ineligible::Trust("the update is not signed")),
        Some(k) if *k != trust.signer_key => {
            return Err(Ineligible::Trust("the update is signed by a different key"));
        }
        Some(_) => {}
    }
    let (Ok(have), Ok(want)) = (
        semver::Version::parse(&current.version),
        semver::Version::parse(&next.version),
    ) else {
        return Err(Ineligible::Transition("the versions cannot be compared"));
    };
    if trust.stream != "stable" || !want.pre.is_empty() {
        return Err(Ineligible::Transition(
            "prerelease updates need an explicit selection",
        ));
    }
    if want <= have {
        return Err(Ineligible::Transition(
            "only newer versions update automatically",
        ));
    }
    if want.major != have.major {
        return Err(Ineligible::Transition(
            "a major version change needs review",
        ));
    }
    let expanded: Vec<String> = next
        .access
        .iter()
        .filter(|a| !trust.approved_access.contains(*a))
        .cloned()
        .collect();
    if !expanded.is_empty() {
        return Err(Ineligible::Access(expanded));
    }
    let deps = |i: &Inspection| -> BTreeSet<(String, String)> {
        i.dependencies
            .iter()
            .map(|d| (d.package_id.clone(), d.manifest_digest.clone()))
            .collect()
    };
    if !deps(next).is_subset(&deps(current)) {
        return Err(Ineligible::Transition(
            "the update adds or changes a dependency",
        ));
    }
    let runtimes = |i: &Inspection| -> BTreeSet<String> {
        i.runtimes.iter().map(|r| r.runtime_id.clone()).collect()
    };
    if !runtimes(next).is_subset(&runtimes(current)) {
        return Err(Ineligible::Transition("the update needs a new runtime"));
    }
    if next.state_schema != current.state_schema {
        return Err(Ineligible::Transition(
            "the update changes the state schema",
        ));
    }
    let contracts: BTreeMap<&str, &str> = next
        .contributions
        .iter()
        .map(|c| (c.id.as_str(), c.contract_digest.as_str()))
        .collect();
    for c in &current.contributions {
        if contracts.get(c.id.as_str()) != Some(&c.contract_digest.as_str()) {
            return Err(Ineligible::Transition(
                "the update changes or removes a public contribution contract",
            ));
        }
    }
    let known: BTreeSet<&str> = current
        .contributions
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    if next
        .contributions
        .iter()
        .any(|c| c.required && !known.contains(c.id.as_str()))
    {
        return Err(Ineligible::Transition(
            "the update adds a required contribution",
        ));
    }
    Ok(())
}

/// Whether the candidate's prerequisites or platform support regress against the running
/// version: an update must not replace a working version with one that cannot run here.
pub fn update_regresses(
    reg: &Registry<'_>,
    current: &Inspection,
    next: &Inspection,
) -> Option<String> {
    if static_incompatibility(current, reg.host).is_none()
        && let Some(why) = static_incompatibility(next, reg.host)
    {
        return Some(why);
    }
    let met = |i: &Inspection| -> BTreeSet<(&'static str, String)> {
        reg.prerequisites(i)
            .into_iter()
            .filter(|q| q.required && !q.satisfied)
            .map(|q| (q.kind, q.id))
            .collect()
    };
    let before = met(current);
    let after = reg.prerequisites(next);
    let new_unmet: Vec<String> = after
        .into_iter()
        .filter(|q| q.required && !q.satisfied && !before.contains(&(q.kind, q.id.clone())))
        .filter_map(|q| q.reason)
        .collect();
    (!new_unmet.is_empty()).then(|| new_unmet.join("; "))
}

/// The wire name of a wait.
pub fn wait_name(w: &PackageWaitReason) -> &'static str {
    match w {
        PackageWaitReason::TrustApproval => "TrustApproval",
        PackageWaitReason::ExactVersionApproval => "ExactVersionApproval",
        PackageWaitReason::AccessApproval => "AccessApproval",
        PackageWaitReason::Compatibility => "Compatibility",
        PackageWaitReason::Quiescence => "Quiescence",
    }
}
pub fn parse_wait(s: &str) -> Option<PackageWaitReason> {
    Some(match s {
        "TrustApproval" => PackageWaitReason::TrustApproval,
        "ExactVersionApproval" => PackageWaitReason::ExactVersionApproval,
        "AccessApproval" => PackageWaitReason::AccessApproval,
        "Compatibility" => PackageWaitReason::Compatibility,
        "Quiescence" => PackageWaitReason::Quiescence,
        _ => return None,
    })
}
