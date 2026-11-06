//! Wire records of the package registry: `PackageRecord` (with the revision-2 availability,
//! prerequisite, access, pending and rollback members), `ProposalRecord`, the catalog page and the
//! package log page.
use super::COMPONENT;
#[cfg(doc)]
use super::MAX_PACKAGES;
use super::policy::{self, Registry};
use crate::context::InvocationContext;
use crate::operations::addons as ops;
use crate::ports::packages::{HostPlatform, Inspection, RuntimeProbe};
use crate::ports::storage::{
    DiagnosticFilter, PackageRegistryRead, ReadSnapshot, StorageError, StoredPackage,
    StoredPackageProposal, StoredPackageVersion,
};
use crate::protocol::Id;
use crate::protocol::provider::{ArtifactRef, Digest, SchemaRef};
use crate::protocol::records::{
    ApprovalState, Diagnostic, PackageAvailability, PackageOffer, PackagePrerequisite,
    PackageRecord, PrerequisiteState, ProposalRecord, SchemaValue,
};
use crate::protocol::results::OperationResult;
use crate::protocol::support::{BoundedList, BoundedText, Page};
use crate::services::cursor::CursorBinding;
use crate::services::sessions::{Failure, content, counter, label, rev};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Every package with its active version.
pub type RegistryView = BTreeMap<String, (StoredPackage, Option<StoredPackageVersion>)>;

/// Loads the whole registry. Its size is bounded on admission ([`MAX_PACKAGES`] identities, a
/// new one beyond it is refused), never by cutting the view short.
pub fn load(read: &dyn PackageRegistryRead) -> Result<RegistryView, StorageError> {
    let mut out = RegistryView::new();
    let mut after: Option<String> = None;
    loop {
        let page = read.packages(after.as_deref(), 500)?;
        let done = page.len() < 500;
        for p in page {
            after = Some(p.package_id.clone());
            let active = match &p.active_digest {
                Some(d) => read.package_version(&p.package_id, d)?,
                None => None,
            };
            out.insert(p.package_id.clone(), (p, active));
        }
        if done {
            return Ok(out);
        }
    }
}

fn digest(s: &str) -> Option<Digest> {
    Digest::try_from(s.to_owned()).ok()
}
fn labels(v: impl IntoIterator<Item = String>) -> BoundedList<BoundedText<1, 256>, 0, 128> {
    BoundedList(v.into_iter().take(128).map(|s| label(&s, "-")).collect())
}

/// How the package's contents are trusted, for the record's `trustState`.
fn trust_state(p: &StoredPackage, shown: Option<&StoredPackageVersion>) -> &'static str {
    match shown {
        None => "NotInstalled",
        Some(v) if v.approval.is_none() => "AwaitingApproval",
        Some(_) if p.trust.is_some() => "TrustedPublisher",
        Some(v)
            if v.approval.as_ref().and_then(|a| a["kind"].as_str()) == Some("TrustedPublisher") =>
        {
            "TrustedPublisher"
        }
        Some(v) if v.signer_key.is_none() => "ApprovedUnsigned",
        Some(_) => "ApprovedSigned",
    }
}

/// The wire record of `p`. `project` narrows enablement to a project's own override.
pub fn package_record(
    read: &dyn PackageRegistryRead,
    registry: &RegistryView,
    p: &StoredPackage,
    host: &HostPlatform,
    runtimes: &dyn RuntimeProbe,
    project: Option<&str>,
) -> Result<PackageRecord, StorageError> {
    let version = |d: &Option<String>| -> Result<Option<StoredPackageVersion>, StorageError> {
        match d {
            Some(d) => read.package_version(&p.package_id, d),
            None => Ok(None),
        }
    };
    let active = version(&p.active_digest)?;
    let pending = version(&p.pending_digest)?;
    let predecessor = version(&p.predecessor_digest)?;
    let latest = match (&active, &pending) {
        (None, None) => read.package_versions(&p.package_id)?.pop(),
        _ => None,
    };
    let shown = active.as_ref().or(pending.as_ref()).or(latest.as_ref());
    let view = Registry {
        packages: registry,
        runtimes,
        host,
    };
    let project_disabled = project.is_some_and(|pr| {
        p.config
            .as_ref()
            .and_then(|c| c["disabledProjects"].as_array())
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(pr)))
    });
    let (mut availability, mut reason) = view.availability(p, active.as_ref());
    if project_disabled && !matches!(availability, PackageAvailability::NotInstalled) {
        availability = PackageAvailability::Disabled;
        reason = "The package is disabled for this project".into();
    }
    // A first install waiting for compatibility or prerequisites shows why it cannot run.
    if active.is_none()
        && let Some(i) = pending.as_ref().and_then(policy::inspection)
        && let Some(why) = policy::static_incompatibility(&i, host)
    {
        availability = PackageAvailability::Incompatible;
        reason = why;
    }
    let inspection: Option<Inspection> = shown.and_then(policy::inspection);
    let prerequisites = inspection
        .as_ref()
        .map(|i| view.prerequisites(i))
        .unwrap_or_default();
    let effective = match (&inspection, availability.clone()) {
        (Some(i), PackageAvailability::Ready) => i.access.clone(),
        _ => Vec::new(),
    };
    let contributions: Vec<String> = inspection
        .as_ref()
        .map(Inspection::contribution_ids)
        .unwrap_or_default();
    let registry_revision = read.registry_revision()?;
    let (version_label, version_digest) = match shown {
        Some(v) => (v.version.clone(), v.manifest_digest.clone()),
        None => ("0.0.0".into(), "0".repeat(64)),
    };
    let pending_waits = pending.as_ref().map(|v| {
        BoundedList(
            v.waits
                .iter()
                .filter_map(|w| policy::parse_wait(w))
                .take(128)
                .collect(),
        )
    });
    let pending_proposal = match &pending {
        Some(v) => read
            .proposal_for(&p.package_id, &v.manifest_digest)?
            .map(|pr| {
                if pr.state == "Pending" && pr.origin.get("sourceContext").is_none() {
                    reason.push_str(" Original invocation was not recorded; submit a fresh proposal to request saved-operation approval.");
                }
                pr.id
            }),
        None => None,
    };
    let configuration_schema = inspection
        .as_ref()
        .and_then(|i| serde_json::from_value::<SchemaRef>(i.configuration_schema.clone()).ok());
    let disabled_projects = p
        .config
        .as_ref()
        .and_then(|c| c["disabledProjects"].as_array())
        .into_iter()
        .flatten()
        .map(|v| {
            v.as_str()
                .and_then(|s| Id::new(s).ok())
                .ok_or_else(|| StorageError::InvalidRecord("package disabled project id".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let configuration = p
        .config
        .as_ref()
        .filter(|c| c.get("schema").is_some() && c.get("value").is_some())
        .map(|c| {
            serde_json::from_value::<SchemaValue>(
                json!({"schema": c["schema"], "value": c["value"]}),
            )
        })
        .transpose()
        .map_err(|_| StorageError::InvalidRecord("package configuration".into()))?;
    Ok(PackageRecord {
        configuration,
        disabled_projects: Some(BoundedList(disabled_projects)),
        id: Id::new(&p.package_id).map_err(|_| StorageError::InvalidRecord("package id".into()))?,
        version: label(&version_label, "0.0.0"),
        digest: digest(&version_digest).unwrap_or_else(|| Digest("0".repeat(64))),
        revision: rev(p.revision),
        publisher_id: shown.and_then(|v| Id::new(&v.publisher_id).ok()),
        enabled: p.enabled && !project_disabled,
        trust_state: label(trust_state(p, shown), "NotInstalled"),
        // Revision 1 lists contribution IDs as opaque IDs: the local names that are valid IDs.
        contribution_ids: BoundedList(
            contributions
                .iter()
                .filter_map(|c| c.split_once('/').and_then(|(_, n)| Id::new(n).ok()))
                .take(128)
                .collect(),
        ),
        registry_revision: Some(rev(registry_revision)),
        availability: Some(availability),
        availability_reason: Some(BoundedText(reason.chars().take(4096).collect())),
        package_kind: inspection
            .as_ref()
            .map(|i| label(i.kind(), "HarnessPackage")),
        contributions: Some(BoundedList(
            contributions
                .iter()
                .take(500)
                .map(|c| label(c, "-"))
                .collect(),
        )),
        languages: inspection.as_ref().map(|i| labels(i.languages())),
        targets: inspection
            .as_ref()
            .map(|i| labels(i.targets.iter().map(|t| t.target_triple.clone()))),
        host_target_supported: inspection
            .as_ref()
            .map(|i| i.entrypoint(&host.target_triple).is_some()),
        prerequisites: Some(BoundedList(
            prerequisites
                .into_iter()
                .take(128)
                .map(|q| PackagePrerequisite {
                    kind: label(q.kind, "package"),
                    id: label(&q.id, "-"),
                    requirement: label(&q.requirement, "*"),
                    required: q.required,
                    satisfied: q.satisfied,
                    state: Some(match q.state {
                        "Ready" => PrerequisiteState::Ready,
                        "Incompatible" => PrerequisiteState::Incompatible,
                        _ => PrerequisiteState::Missing,
                    }),
                    detected_version: q.detected.as_ref().map(|v| label(v, "-")),
                })
                .collect(),
        )),
        requested_access: inspection.as_ref().map(|i| labels(i.access.clone())),
        effective_access: Some(labels(effective)),
        configuration_revision: Some(rev(p.config_revision.max(1))),
        configuration_schema,
        state_generation: Some(counter(p.state_generation)),
        pending_version: pending.as_ref().map(|v| label(&v.version, "0.0.0")),
        pending_digest: pending.as_ref().and_then(|v| digest(&v.manifest_digest)),
        pending_waits,
        pending_proposal_id: pending_proposal,
        // The exact bundle a person approves by installing it (EXT-02 approval access).
        pending_bundle: pending.as_ref().and_then(|v| {
            serde_json::from_value::<ArtifactRef>(v.provenance["source"].clone()).ok()
        }),
        predecessor_version: predecessor.as_ref().map(|v| label(&v.version, "0.0.0")),
        predecessor_digest: predecessor
            .as_ref()
            .and_then(|v| digest(&v.manifest_digest)),
        publisher_trusted: Some(p.trust.is_some()),
        signer_key: shown
            .and_then(|v| v.signer_key.as_ref())
            .map(|k| label(k, "-")),
        // Only the active version's failure (another version's record guards rollback).
        failure_code: super::active_failure(p)
            .and_then(|f| f["code"].as_str())
            .map(|c| label(c, "Failed")),
        available: None,
        // The fact (an active, installed version), not derived from availability: a first
        // install that is incompatible is not installed.
        installed: Some(p.installed && p.active_digest.is_some()),
    })
}

/// The versions verified indexes offer for `p`, next to its installed version.
pub fn attach_offers(
    record: &mut PackageRecord,
    offers: Option<&Vec<super::index::Offer>>,
    installed: Option<&str>,
    host: &HostPlatform,
) {
    let list: Vec<PackageOffer> = offers
        .map(|l| {
            l.iter()
                .take(128)
                .filter_map(|o| {
                    Some(PackageOffer {
                        version: label(&o.entry.version, "0.0.0"),
                        manifest_digest: digest(&o.entry.manifest_digest)?,
                        bundle_digest: digest(&o.entry.bundle_digest)?,
                        bundle_bytes: counter(o.entry.bundle_bytes.min(i64::MAX as u64) as i64),
                        publisher_id: Id::new(&o.publisher_id).ok()?,
                        source: label(&o.source, "index"),
                        host_target_supported: super::index::supported(
                            &o.entry,
                            &host.target_triple,
                        ),
                        newer: super::index::newer(&o.entry.version, installed),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    record.available = Some(BoundedList(list));
}

/// The record of a package only an index offers (not installed, never proposed).
pub fn offered_record(
    package_id: &str,
    offers: &[super::index::Offer],
    registry_revision: i64,
    host: &HostPlatform,
) -> Option<PackageRecord> {
    let best = offers
        .iter()
        .find(|o| super::index::supported(&o.entry, &host.target_triple))
        .or_else(|| offers.first())?;
    let supported = super::index::supported(&best.entry, &host.target_triple);
    let mut r = PackageRecord {
        configuration: None,
        disabled_projects: Some(BoundedList(Vec::new())),
        installed: Some(false),
        id: Id::new(package_id).ok()?,
        version: label(&best.entry.version, "0.0.0"),
        digest: digest(&best.entry.manifest_digest)?,
        revision: rev(1),
        publisher_id: Id::new(&best.publisher_id).ok(),
        enabled: false,
        trust_state: label("Available", "Available"),
        contribution_ids: BoundedList(Vec::new()),
        registry_revision: Some(rev(registry_revision)),
        availability: Some(if supported {
            PackageAvailability::NotInstalled
        } else {
            PackageAvailability::Incompatible
        }),
        availability_reason: Some(BoundedText(if supported {
            format!("Available from {}", best.source)
        } else {
            format!(
                "No prebuilt helper for this platform ({})",
                host.target_triple
            )
        })),
        package_kind: Some(label(&best.entry.kind, "HarnessPackage")),
        contributions: None,
        languages: Some(labels(best.entry.languages.clone())),
        targets: Some(labels(best.entry.targets.clone())),
        host_target_supported: Some(supported),
        prerequisites: None,
        requested_access: None,
        effective_access: Some(BoundedList(Vec::new())),
        configuration_revision: None,
        configuration_schema: None,
        state_generation: None,
        pending_version: None,
        pending_digest: None,
        pending_waits: None,
        pending_proposal_id: None,
        pending_bundle: None,
        predecessor_version: None,
        predecessor_digest: None,
        publisher_trusted: Some(false),
        signer_key: None,
        failure_code: None,
        available: None,
    };
    attach_offers(&mut r, Some(&offers.to_vec()), None, host);
    Some(r)
}

/// The wire record of a proposal.
pub fn proposal_record(
    pr: &StoredPackageProposal,
    package_revision: i64,
    update: bool,
) -> ProposalRecord {
    let state = match pr.state.as_str() {
        "Approved" => ApprovalState::Approved,
        "Rejected" => ApprovalState::Rejected,
        "Superseded" => ApprovalState::Superseded,
        "Expired" => ApprovalState::Expired,
        _ => ApprovalState::Pending,
    };
    let s = &pr.summary;
    let list = |k: &str| -> String {
        s[k].as_array()
            .map(|a| {
                a.iter()
                    .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "none".into())
    };
    let summary = format!(
        "Proposed {} {} ({}). Requested access: {}. Contributions: {}. Changes: {}. Originated by {} ({}).",
        pr.package_id,
        pr.version,
        if s["signed"].as_bool() == Some(true) {
            "signed"
        } else {
            "unsigned"
        },
        list("requestedAccess"),
        list("contributions"),
        s["changes"]["summary"].as_str().unwrap_or("new package"),
        pr.originator.0.as_str(),
        pr.origin["kind"].as_str().unwrap_or("unknown"),
    );
    let summary = if pr.state == "Pending" && pr.origin.get("sourceContext").is_none() {
        format!(
            "{summary} Original invocation was not recorded; submit a fresh proposal to request saved-operation approval."
        )
    } else {
        summary
    };
    ProposalRecord {
        id: pr.id.clone(),
        revision: rev(pr.revision),
        content_digest: Digest(pr.manifest_digest.clone()),
        state,
        operation_kind: label(
            if update {
                "addons.update"
            } else {
                "addons.install"
            },
            "addons.install",
        ),
        target_id: Id::new(&pr.package_id)
            .unwrap_or_else(|_| Id::new("package").expect("valid id")),
        target_revision: rev(package_revision),
        summary: content(&summary),
        artifact: serde_json::from_value::<ArtifactRef>(s["source"].clone()).ok(),
    }
}

/// What a candidate changes against the active version, for the reviewer.
pub fn changes(base: Option<&Inspection>, next: &Inspection) -> Value {
    let Some(base) = base else {
        return json!({"summary": "new package", "addedAccess": next.access});
    };
    let added: Vec<&String> = next
        .access
        .iter()
        .filter(|a| !base.access.contains(a))
        .collect();
    let removed: Vec<&String> = base
        .access
        .iter()
        .filter(|a| !next.access.contains(a))
        .collect();
    let before: BTreeMap<&str, &str> = base
        .contributions
        .iter()
        .map(|c| (c.id.as_str(), c.contract_digest.as_str()))
        .collect();
    let mut added_c = Vec::new();
    let mut changed_c = Vec::new();
    for c in &next.contributions {
        match before.get(c.id.as_str()) {
            None => added_c.push(c.id.clone()),
            Some(d) if *d != c.contract_digest => changed_c.push(c.id.clone()),
            _ => {}
        }
    }
    let removed_c: Vec<&str> = base
        .contributions
        .iter()
        .filter(|c| !next.contributions.iter().any(|n| n.id == c.id))
        .map(|c| c.id.as_str())
        .collect();
    json!({
        "summary": format!(
            "{} -> {}; access +{} -{}; contributions +{} ~{} -{}",
            base.version, next.version, added.len(), removed.len(), added_c.len(), changed_c.len(), removed_c.len()
        ),
        "fromVersion": base.version,
        "toVersion": next.version,
        "addedAccess": added,
        "removedAccess": removed,
        "addedContributions": added_c,
        "changedContributions": changed_c,
        "removedContributions": removed_c,
        "stateSchemaChanged": base.state_schema != next.state_schema,
    })
}

/// `addons.catalog`: every installed, uninstalled-with-history or proposed package, by ID.
pub fn catalog(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    i: &ops::CatalogInput,
    host: &HostPlatform,
    runtimes: &dyn RuntimeProbe,
) -> Result<OperationResult, Failure> {
    let reg = read.packages();
    let registry_revision = reg.registry_revision()?;
    let binding = CursorBinding::new(ctx, "addons.catalog", &[], 1);
    let after = i
        .page
        .cursor
        .as_ref()
        .map(|c| binding.decode(&c.0))
        .transpose()?;
    let limit = (i.page.limit.get() as usize).clamp(1, 500);
    let all = load(reg as &dyn PackageRegistryRead)?;
    let offered = super::index::offers(reg as &dyn PackageRegistryRead)?;
    // Installed or proposed packages and packages only an index offers, in one ID order.
    let ids: std::collections::BTreeSet<&String> = all.keys().chain(offered.keys()).collect();
    let rows: Vec<&String> = ids
        .into_iter()
        .filter(|id| after.as_deref().is_none_or(|a| id.as_str() > a))
        .take(limit + 1)
        .collect();
    let has_more = rows.len() > limit;
    let rows = &rows[..rows.len().min(limit)];
    let project = ctx.project_id().map(|p| p.0.as_str());
    let mut items = Vec::with_capacity(rows.len());
    for id in rows {
        match all.get(*id) {
            Some((p, active)) => {
                let mut r = package_record(reg, &all, p, host, runtimes, project)?;
                let installed = active.as_ref().map(|v| v.version.as_str());
                attach_offers(&mut r, offered.get(*id), installed, host);
                items.push(r);
            }
            None => {
                if let Some(r) = offered
                    .get(*id)
                    .and_then(|o| offered_record(id, o, registry_revision, host))
                {
                    items.push(r);
                }
            }
        }
    }
    let next = has_more.then(|| BoundedText(binding.encode(rows[rows.len() - 1])));
    Ok(OperationResult::AddonsCatalog {
        value: Page {
            items: BoundedList(items),
            next_cursor: next,
            has_more,
        },
    })
}

/// `addons.logs`: the package's retained host diagnostics, oldest first after `afterSequence`.
pub fn logs(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    i: &ops::LogsInput,
) -> Result<OperationResult, Failure> {
    let binding = CursorBinding::new(
        ctx,
        "addons.logs",
        &[i.package_id.as_str(), &i.after_sequence.get().to_string()],
        1,
    );
    let after = match &i.page.cursor {
        Some(c) => binding
            .decode(&c.0)?
            .parse::<i64>()
            .map_err(|_| StorageError::CursorExpired)?,
        None => i.after_sequence.get(),
    };
    let limit = (i.page.limit.get() as usize).clamp(1, 500);
    let filter = DiagnosticFilter {
        after_sequence: after,
        component: Some(COMPONENT.into()),
        target_id: Some(i.package_id.clone()),
        ..Default::default()
    };
    let rows = read
        .diagnostics()
        .diagnostics(&filter, (limit + 1) as u16)?;
    let has_more = rows.len() > limit;
    let rows = &rows[..rows.len().min(limit)];
    let items = rows
        .iter()
        .map(|d| -> Result<Diagnostic, StorageError> {
            let e = crate::services::diagnostics::entry(d)?;
            Ok(Diagnostic {
                id: e.id,
                severity: e.severity,
                component: e.component,
                code: e.code,
                message: e.message,
                correlation_id: e.correlation_id,
                evidence: e.evidence,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let next =
        has_more.then(|| BoundedText(binding.encode(&rows[rows.len() - 1].sequence.to_string())));
    Ok(OperationResult::AddonsLogs {
        value: Page {
            items: BoundedList(items),
            next_cursor: next,
            has_more,
        },
    })
}
