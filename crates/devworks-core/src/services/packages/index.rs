//! Available packs from signed publisher indexes (ADDON-02/03), and the durable announcement
//! of every change of a package's effective availability or active version (ADDON-04).
//!
//! # Distribution model
//!
//! A publisher signs an index document (`devworks-package-index/1`) listing package identity,
//! versions, targets, manifest and bundle digests, sizes and bundle locations. A person
//! configures index sources (`packages.indexSources`: an https URL or a local directory, each
//! with the publisher's Ed25519 public key; none by default; an agent's change is only a
//! proposal). The package host fetches each source through the [`IndexTransport`] port, has the
//! [`IndexVerifier`] check its signature against that source's key, and records the verified
//! entries ([`PackageService::record_index`]); a failed fetch is recorded and keeps the last
//! verified entries. `addons.catalog` lists the offered versions next to installed ones.
//!
//! Installing from an index fetches exactly the listed bundle and accepts it only when its
//! bytes hash to the signed bundle digest and its manifest to the signed manifest digest; it
//! then goes through the ordinary inspector and approval path (`addons.install`/`.update`),
//! so the index adds an integrity anchor and a discovery channel, never authority.
//!
//! # Availability announcements
//!
//! [`PackageService::observe`] runs in the unit of work of every command that changes the
//! registry and in the host's own passes (startup failure, runtime probe and index refresh). It
//! recomputes every package's effective availability and active version, compares them with
//! the last announced observation and, for each change, appends one `PackageAvailabilityChanged`
//! history event (aggregate `package_availability`, the package's ordered partition) with the
//! package, old and new version and digest, old and new availability and the reason, plus one
//! durable delivery per registered consumer (code intelligence marks semantic data stale from
//! it), all in that same transaction.
use super::PackageService;
use super::policy::Registry;
use super::view;
use crate::fingerprint::sha256_hex;
use crate::ports::IdKind;
use crate::ports::packages::{IndexEntry, IndexSource, VerifiedIndex};
use crate::ports::storage::{
    ConfigScope, ConfigurationRead, NewEvent, NewQueueEntry, PackageRegistryRead, StorageError,
    StoredIndex, StoredObservation, WriteOptions, WriteTransaction, run_write,
};
use crate::protocol::EventId;
use crate::protocol::records::PackageAvailability;
use crate::settings::catalog::{PACKAGE_INDEX_SOURCES_KEY, descriptor};
use crate::settings::model::{chain, current_chain};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// History aggregate of availability announcements.
pub const AVAILABILITY_AGGREGATE: &str = "package_availability";
/// History event kind of an availability or active-version change.
pub const AVAILABILITY_CHANGED: &str = "PackageAvailabilityChanged";

/// The configured index sources (effective workspace value; none by default).
pub fn index_sources(read: &dyn ConfigurationRead) -> Result<Vec<IndexSource>, StorageError> {
    let d = descriptor(PACKAGE_INDEX_SOURCES_KEY).expect("catalog setting");
    let layers = current_chain(read, &ConfigScope::Workspace)?;
    let entries = chain(d, &layers).map_err(|m| {
        StorageError::InvalidRecord(format!("{} stored under another schema", m.key))
    })?;
    let value = entries
        .first()
        .map(|e| e.value.value.0.clone())
        .unwrap_or_else(|| d.default_value());
    Ok(serde_json::from_value(value).unwrap_or_default())
}

/// The identity of a source: its location and key.
pub fn source_key(s: &IndexSource) -> String {
    sha256_hex(format!("{}\n{}", s.location, s.public_key).as_bytes())
}

/// One offered version with where it was listed.
#[derive(Clone, Debug)]
pub struct Offer {
    pub entry: IndexEntry,
    pub source: String,
    pub publisher_id: String,
}

/// Every verified offer, by package ID (newest first by semantic version).
pub fn offers(
    read: &dyn PackageRegistryRead,
) -> Result<BTreeMap<String, Vec<Offer>>, StorageError> {
    let mut out: BTreeMap<String, Vec<Offer>> = BTreeMap::new();
    for x in read.indexes()? {
        let Some(index) = x
            .index
            .as_ref()
            .and_then(|i| serde_json::from_value::<VerifiedIndex>(i.clone()).ok())
        else {
            continue;
        };
        for e in index.entries {
            out.entry(e.package_id.clone()).or_default().push(Offer {
                entry: e,
                source: x.location.clone(),
                publisher_id: index.publisher_id.clone(),
            });
        }
    }
    for list in out.values_mut() {
        list.sort_by(|a, b| {
            let v = |o: &Offer| semver::Version::parse(&o.entry.version).ok();
            v(b).cmp(&v(a))
        });
        list.dedup_by(|a, b| a.entry.manifest_digest == b.entry.manifest_digest);
    }
    Ok(out)
}

/// The offer of exactly `manifest_digest` for `package_id`, if a verified index lists it.
pub fn offer(
    read: &dyn PackageRegistryRead,
    package_id: &str,
    manifest_digest: &str,
) -> Result<Option<Offer>, StorageError> {
    Ok(offers(read)?.remove(package_id).and_then(|l| {
        l.into_iter()
            .find(|o| o.entry.manifest_digest == manifest_digest)
    }))
}

fn name(a: &PackageAvailability) -> &'static str {
    match a {
        PackageAvailability::NotInstalled => "NotInstalled",
        PackageAvailability::Disabled => "Disabled",
        PackageAvailability::MissingDependency => "MissingDependency",
        PackageAvailability::Incompatible => "Incompatible",
        PackageAvailability::Ready => "Ready",
        PackageAvailability::Failed => "Failed",
    }
}

impl PackageService {
    /// Announces every change of effective availability or active version since the last
    /// announcement, in the caller's unit of work (see the module documentation). `cause`
    /// names what changed it (the operation, `StartupFailed`, `RuntimeProbe`, ...).
    pub fn observe(
        &self,
        tx: &mut dyn WriteTransaction,
        cause: &str,
    ) -> Result<usize, StorageError> {
        let read: &dyn PackageRegistryRead = &*tx.packages();
        let reg = view::load(read)?;
        let registry_revision = read.registry_revision()?;
        let viewer = Registry {
            packages: &reg,
            runtimes: &*self.runtimes,
            host: &self.platform,
        };
        let mut changes = Vec::new();
        for (id, (p, active)) in &reg {
            let (a, why) = viewer.availability(p, active.as_ref());
            let now = StoredObservation {
                package_id: id.clone(),
                availability: name(&a).into(),
                version: active.as_ref().map(|v| v.version.clone()),
                digest: active.as_ref().map(|v| v.manifest_digest.clone()),
                revision: 0,
            };
            let before = tx.packages().observation(id)?;
            let unchanged = before
                .as_ref()
                .is_some_and(|b| b.availability == now.availability && b.digest == now.digest);
            // A package never announced and never installed has nothing to announce.
            if unchanged || (before.is_none() && matches!(a, PackageAvailability::NotInstalled)) {
                continue;
            }
            changes.push((before, now, why));
        }
        let consumers = self.consumers();
        let actor = tx.store().host_actor.clone();
        for (before, now, why) in &changes {
            tx.packages().put_observation(now)?;
            let revision = tx
                .packages()
                .observation(&now.package_id)?
                .map_or(1, |o| o.revision);
            let event = EventId(tx.new_id(IdKind::Event));
            let payload = json!({
                "packageId": now.package_id,
                "fromVersion": before.as_ref().and_then(|b| b.version.clone()),
                "toVersion": now.version,
                "fromDigest": before.as_ref().and_then(|b| b.digest.clone()),
                "toDigest": now.digest,
                "fromAvailability": before.as_ref().map(|b| b.availability.clone()),
                "toAvailability": now.availability,
                "reason": cause,
                "detail": why,
                "registryRevision": registry_revision,
            });
            tx.history().append(NewEvent {
                id: event.clone(),
                actor: actor.clone(),
                project_id: None,
                root_id: None,
                operation_id: None,
                aggregate_kind: AVAILABILITY_AGGREGATE.into(),
                aggregate_id: now.package_id.clone(),
                aggregate_revision: revision,
                kind: AVAILABILITY_CHANGED.into(),
                payload: payload.clone(),
            })?;
            for consumer in &consumers {
                let id = tx.new_id(IdKind::QueueEntry);
                let ready_at_ms = tx.now_ms();
                tx.queue().enqueue(NewQueueEntry {
                    id,
                    consumer: consumer.clone(),
                    event_id: event.clone(),
                    root_id: None,
                    operation_id: None,
                    // Per-package order: a consumer sees a package's changes in sequence.
                    partition: Some(now.package_id.clone()),
                    priority: 0,
                    ready_at_ms,
                    payload: payload.clone(),
                })?;
            }
        }
        Ok(changes.len())
    }

    /// [`Self::observe`] in its own unit of work (host passes).
    pub fn observe_now(&self, cause: &str) -> Result<usize, StorageError> {
        let svc = self.clone_ports();
        let cause = cause.to_owned();
        run_write(&*self.store, WriteOptions::default(), move |tx| {
            svc.observe(tx, &cause)
        })
    }

    /// The configured index sources.
    pub fn configured_indexes(&self) -> Result<Vec<IndexSource>, StorageError> {
        crate::ports::storage::read_snapshot(&*self.store, &Default::default(), |s| {
            index_sources(s.configuration())
        })
    }

    /// Records the outcome of fetching `source` (verified entries, or why it failed, keeping
    /// the last verified entries), forgets sources no longer configured, and announces any
    /// availability change. Host unit of work.
    pub fn record_index(
        &self,
        configured: &[IndexSource],
        source: &IndexSource,
        outcome: Result<VerifiedIndex, String>,
    ) -> Result<(), StorageError> {
        let svc = self.clone_ports();
        let keys: Vec<String> = configured.iter().map(source_key).collect();
        let source = source.clone();
        run_write(&*self.store, WriteOptions::default(), move |tx| {
            tx.packages().retain_indexes(&keys)?;
            let key = source_key(&source);
            let earlier = tx
                .packages()
                .indexes()?
                .into_iter()
                .find(|x| x.source_key == key);
            let now = tx.now_ms();
            let record = match outcome {
                Ok(index) => StoredIndex {
                    source_key: key,
                    location: source.location.clone(),
                    public_key: source.public_key.clone(),
                    status: "Verified".into(),
                    index: Some(
                        serde_json::to_value(&index)
                            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
                    ),
                    error: None,
                    fetched_at_ms: now,
                },
                Err(why) => StoredIndex {
                    source_key: key,
                    location: source.location.clone(),
                    public_key: source.public_key.clone(),
                    status: "Failed".into(),
                    index: earlier.and_then(|e| e.index),
                    error: Some(why.clone()),
                    fetched_at_ms: now,
                },
            };
            let failed = record.status == "Failed";
            let error = record.error.clone().unwrap_or_default();
            tx.packages().put_index(&record)?;
            if failed {
                svc.log_in(
                    tx,
                    "index",
                    crate::protocol::records::Severity::Warning,
                    "IndexUnavailable",
                    &format!(
                        "Package index {} could not be used: {error}",
                        source.location
                    ),
                    None,
                )?;
            }
            svc.observe(tx, "IndexRefreshed")?;
            Ok(())
        })
    }

    /// Forgets every recorded source that is no longer configured.
    pub fn retain_configured_indexes(
        &self,
        configured: &[IndexSource],
    ) -> Result<(), StorageError> {
        let keys: Vec<String> = configured.iter().map(source_key).collect();
        run_write(&*self.store, WriteOptions::default(), move |tx| {
            tx.packages().retain_indexes(&keys)
        })
    }
}

/// The index entry fields of an offer (catalog projection).
pub fn offer_json(o: &Offer) -> Value {
    json!({"source": o.source, "publisherId": o.publisher_id, "entry": o.entry})
}

/// Whether `entry` runs on `host_target`.
pub fn supported(entry: &IndexEntry, host_target: &str) -> bool {
    entry.targets.iter().any(|t| t == host_target)
}

/// Whether `candidate` is newer than `installed` (semantic versions).
pub fn newer(candidate: &str, installed: Option<&str>) -> bool {
    match (
        semver::Version::parse(candidate),
        installed.map(semver::Version::parse),
    ) {
        (Ok(c), Some(Ok(i))) => c > i,
        (Ok(_), None) => true,
        _ => false,
    }
}
