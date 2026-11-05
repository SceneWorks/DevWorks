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

type AvailabilityCache = BTreeMap<(String, String, usize), (PackageAvailability, String)>;

impl Registry<'_> {
    /// The prerequisites of `i` with their current state. A package dependency is satisfied
    /// only by an installed, enabled package whose active version has exactly the pinned
    /// manifest digest, matches the version requirement and is itself available.
    pub fn prerequisites(&self, i: &Inspection) -> Vec<Prerequisite> {
        self.prerequisites_at(i, 0, &mut BTreeMap::new())
    }

    fn prerequisites_at(
        &self,
        i: &Inspection,
        depth: usize,
        cache: &mut AvailabilityCache,
    ) -> Vec<Prerequisite> {
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
                        match self.availability_depth(p, Some(v), depth + 1, cache) {
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
        self.availability_depth(p, active, 0, &mut BTreeMap::new())
    }

    fn availability_depth(
        &self,
        p: &StoredPackage,
        active: Option<&StoredPackageVersion>,
        depth: usize,
        cache: &mut AvailabilityCache,
    ) -> (PackageAvailability, String) {
        // The remaining depth budget affects availability, even when the same node was
        // already reached along a shorter path. Cache only within this immutable traversal.
        let key = (
            p.package_id.clone(),
            active
                .map(|v| v.manifest_digest.clone())
                .unwrap_or_default(),
            depth,
        );
        if let Some(result) = cache.get(&key) {
            return result.clone();
        }
        let (state, reason) = self.evaluate_availability(p, active, depth, cache);
        let result = (state, reason.chars().take(4096).collect());
        cache.insert(key, result.clone());
        result
    }

    fn evaluate_availability(
        &self,
        p: &StoredPackage,
        active: Option<&StoredPackageVersion>,
        depth: usize,
        cache: &mut AvailabilityCache,
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
            .prerequisites_at(&i, depth, cache)
            .into_iter()
            .filter(|q| q.required && !q.satisfied)
            .filter_map(|q| q.reason)
            .collect();
        if !unmet.is_empty() {
            return (
                PackageAvailability::MissingDependency,
                unmet
                    .iter()
                    .enumerate()
                    .flat_map(|(n, reason)| {
                        (if n == 0 { "" } else { "; " })
                            .chars()
                            .chain(reason.chars())
                    })
                    .take(4096)
                    .collect(),
            );
        }
        if let Some(f) = p
            .failure
            .as_ref()
            .filter(|f| failure_applies(f, p, &v.manifest_digest))
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

/// Whether a failure record marks `digest` failed as `p` stands now: it names that version and,
/// when it records them, the configuration revision and state generation it failed under (a
/// new configuration or generation is a new instance, with a new budget).
pub fn failure_applies(f: &Value, p: &StoredPackage, digest: &str) -> bool {
    f["digest"].as_str() == Some(digest)
        && f["configRevision"]
            .as_i64()
            .is_none_or(|c| c == p.config_revision)
        && f["stateGeneration"]
            .as_i64()
            .is_none_or(|g| g == p.state_generation)
}

/// Whether activating `next` as `package_id`'s active version would regress anything that runs
/// now: the candidate's platform support, its own prerequisite closure evaluated with its
/// proposed active pointer substituted in (so a dependency that pins the version being
/// replaced counts), and every other package that is ready now and would not be with the
/// candidate active (reverse pins and requirements). An activation must not replace a working
/// version with one that cannot run here, nor break another package. `None` without an active
/// version (a first install has nothing to regress).
pub fn activation_regresses(
    reg: &Registry<'_>,
    package_id: &str,
    next: &StoredPackageVersion,
) -> Option<String> {
    let (p, active) = reg.packages.get(package_id)?;
    let current = active.as_ref().and_then(inspection)?;
    let candidate = inspection(next)?;
    if static_incompatibility(&current, reg.host).is_none()
        && let Some(why) = static_incompatibility(&candidate, reg.host)
    {
        return Some(why);
    }
    let mut proposed = reg.packages.clone();
    proposed.insert(package_id.to_owned(), (p.clone(), Some(next.clone())));
    let after = Registry {
        packages: &proposed,
        runtimes: reg.runtimes,
        host: reg.host,
    };
    let unmet_before: BTreeSet<(&'static str, String)> = reg
        .prerequisites(&current)
        .into_iter()
        .filter(|q| q.required && !q.satisfied)
        .map(|q| (q.kind, q.id))
        .collect();
    let mut reasons: Vec<String> = after
        .prerequisites(&candidate)
        .into_iter()
        .filter(|q| q.required && !q.satisfied && !unmet_before.contains(&(q.kind, q.id.clone())))
        .filter_map(|q| q.reason)
        .collect();
    for (id, (q, qa)) in reg
        .packages
        .iter()
        .filter(|(id, _)| id.as_str() != package_id)
    {
        if matches!(
            reg.availability(q, qa.as_ref()).0,
            PackageAvailability::Ready
        ) {
            let (a, why) = after.availability(q, qa.as_ref());
            if !matches!(a, PackageAvailability::Ready) {
                reasons.push(format!("Package {id} would become unavailable: {why}"));
            }
        }
    }
    (!reasons.is_empty()).then(|| reasons.join("; "))
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

#[cfg(test)]
mod closure_tests {
    use super::*;
    use crate::fingerprint::sha256_hex;
    use crate::ports::storage::VersionPhase;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Probe {
        calls: AtomicUsize,
        budget: usize,
        available: bool,
    }
    impl RuntimeProbe for Probe {
        fn runtime(&self, _: &str) -> Option<String> {
            assert!(
                self.calls.fetch_add(1, Ordering::Relaxed) < self.budget,
                "shared dependency closure exceeded a linear evaluation budget"
            );
            self.available.then(|| "1.0.0".into())
        }
    }
    fn entry(id: &str, dependencies: &[String]) -> (StoredPackage, Option<StoredPackageVersion>) {
        let digest = sha256_hex(id.as_bytes());
        let facts = json!({"packageId": id, "version": "1.0.0", "publisherId": "acme",
            "manifestDigest": digest, "bundleDigest": digest, "signerKey": null,
            "protocolMin": [1,0], "protocolMax": [1,0], "minimumHostVersion": "0.1.0",
            "targets": [{"targetTriple": "test-target", "entrypoint": "helper"}],
            "files": [{"path": "helper", "sha256": "a".repeat(64), "byteLength": 1, "executable": true}],
            "dependencies": dependencies.iter().map(|d| json!({"packageId": d, "versionRequirement": "^1", "manifestDigest": sha256_hex(d.as_bytes()), "required": true})).collect::<Vec<_>>(),
            "runtimes": [{"runtimeId": "counter", "versionRequirement": "^1", "required": true}],
            "access": [], "contributions": [], "stateSchema": {}, "configurationSchema": {},
            "limits": {"maxConcurrent": 1, "queueCapacity": 1, "maxMemoryBytes": 1048576, "streamCreditBytes": 4096}, "helpFile": "help.md"});
        let p = StoredPackage {
            package_id: id.into(),
            revision: 1,
            enabled: true,
            installed: true,
            active_digest: Some(digest.clone()),
            predecessor_digest: None,
            pending_digest: None,
            state_generation: 1,
            config: None,
            config_revision: 1,
            trust: None,
            failure: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let v = StoredPackageVersion {
            package_id: id.into(),
            manifest_digest: digest.clone(),
            bundle_digest: digest,
            version: "1.0.0".into(),
            publisher_id: "acme".into(),
            signer_key: None,
            inspection: facts,
            provenance: json!({}),
            phase: VersionPhase::Installed,
            waits: vec![],
            approval: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        (p, Some(v))
    }
    fn host() -> HostPlatform {
        HostPlatform {
            target_triple: "test-target".into(),
            host_version: "0.1.0".into(),
            protocol_major: 1,
            protocol_minor: 0,
        }
    }

    #[test]
    fn shared_dependency_dags_have_bounded_work_and_failure_diagnostics() {
        let mut packages = BTreeMap::new();
        for level in (0..=8).rev() {
            for n in 0..if level == 0 { 1 } else { 10 } {
                let id = format!("level-{level}-{n}");
                let deps = if level == 8 {
                    vec![]
                } else {
                    (0..10)
                        .map(|n| format!("level-{}-{n}", level + 1))
                        .collect()
                };
                packages.insert(id.clone(), entry(&id, &deps));
            }
        }
        let host = host();
        for available in [true, false] {
            let probe = Probe {
                calls: AtomicUsize::new(0),
                budget: packages.len() * 4,
                available,
            };
            let registry = Registry {
                packages: &packages,
                runtimes: &probe,
                host: &host,
            };
            let (p, v) = &packages["level-0-0"];
            let (state, reason) = registry.availability(p, v.as_ref());
            assert!(if available {
                matches!(state, PackageAvailability::Ready)
            } else {
                matches!(state, PackageAvailability::MissingDependency)
            });
            assert!(reason.chars().count() <= 4096);
        }
    }

    #[test]
    fn dependency_memoization_preserves_depth_and_cycle_refusals() {
        let mut packages = BTreeMap::new();
        packages.insert("leaf".into(), entry("leaf", &[]));
        packages.insert("shared".into(), entry("shared", &["leaf".into()]));
        packages.insert(
            "root".into(),
            entry("root", &["shared".into(), "chain-1".into()]),
        );
        for n in 1..=7 {
            let id = format!("chain-{n}");
            let next = if n == 7 {
                "shared".into()
            } else {
                format!("chain-{}", n + 1)
            };
            packages.insert(id.clone(), entry(&id, &[next]));
        }
        let host = host();
        for cycle in [false, true] {
            if cycle {
                packages.insert("chain-7".into(), entry("chain-7", &["root".into()]));
            }
            let probe = Probe {
                calls: AtomicUsize::new(0),
                budget: packages.len() * 8,
                available: true,
            };
            let registry = Registry {
                packages: &packages,
                runtimes: &probe,
                host: &host,
            };
            let (p, v) = &packages["root"];
            let (state, why) = registry.availability(p, v.as_ref());
            assert!(matches!(state, PackageAvailability::MissingDependency));
            assert!(why.contains("too deep or cyclic"), "{why}");
        }
    }
}
