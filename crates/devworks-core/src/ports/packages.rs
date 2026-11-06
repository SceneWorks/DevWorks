//! Ports of the package lifecycle (ADDON-01..04, EXT-01..07) that are not persistence:
//!
//! * [`PackageInspector`] verifies a received bundle before anything of it is trusted or written
//!   to its install location: container format, archive path safety, the manifest's strict
//!   decoding and structural rules, every file's digest and length, and the optional signature.
//!   It returns a core-owned [`Inspection`], so core never depends on the public wire crate
//!   (`devworks-adapters::packages` implements it with `devworks-extension-protocol`).
//! * [`PackageFiles`] publishes a verified version's files into a content-addressed install
//!   directory atomically (stage, flush, verify, rename), re-verifies them before every launch
//!   and removes retired ones.
//! * [`RuntimeProbe`] answers which declared runtimes (for example a .NET or Node runtime a
//!   third-party compiler tool needs) exist on this machine; the host never installs one.
//! * [`PackageActivity`] is the package host's in-process view: the quiescence barrier that gates
//!   new invocations of a package while an update publishes, and the post-commit hint that the
//!   registry changed.
//!
//! None of these is a security boundary for the package's own code: an installed native helper
//! runs with the desktop user's rights (see `crate::services::packages`, "Trust model").
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

/// Largest bundle the host inspects (bytes).
pub const MAX_BUNDLE_BYTES: u64 = 256 * 1024 * 1024;

/// One file of a verified version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectedFile {
    pub path: String,
    pub sha256: String,
    pub byte_length: u64,
    /// A target entrypoint (published executable).
    pub executable: bool,
}

/// A target binary of the manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectedTarget {
    pub target_triple: String,
    pub entrypoint: String,
}

/// A declared package dependency.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectedDependency {
    pub package_id: String,
    pub version_requirement: String,
    pub manifest_digest: String,
    pub required: bool,
}

/// A declared runtime requirement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectedRuntime {
    pub runtime_id: String,
    pub version_requirement: String,
    pub required: bool,
}

/// One declared contribution: its stable qualified ID, kind and public contract (input/output
/// schema references as canonical JSON), so a changed public contract is detectable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectedContribution {
    pub id: String,
    /// `Semantic`, `Tool`, `Workflow`, `EventHandler`, `ContextHook`, `Content`, `Provider` or
    /// `NativeUi`.
    pub kind: String,
    pub required: bool,
    /// SHA-256 of the canonical contribution descriptor (any contract change changes it).
    pub contract_digest: String,
    /// Original verified descriptor; absent only on older compact inspections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descriptor: Option<Value>,
    /// Languages of a semantic contribution.
    pub languages: Vec<String>,
}

/// Requested resource limits (admission inputs, not OS containment).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectedLimits {
    pub max_concurrent: u16,
    pub queue_capacity: u16,
    pub max_memory_bytes: i64,
    pub stream_credit_bytes: u32,
}

/// A manifest-bound schema document in the verified installed files.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectedSchemaAsset {
    pub reference: Value,
    pub path: String,
}

/// The verified facts of one bundle. Everything here was checked by the inspector; nothing
/// here is authority (approval and trust are decided by the package service).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Inspection {
    pub package_id: String,
    pub version: String,
    pub publisher_id: String,
    /// SHA-256 of the exact manifest bytes.
    pub manifest_digest: String,
    /// SHA-256 of the whole bundle.
    pub bundle_digest: String,
    /// Fingerprint (SHA-256 of the Ed25519 public key) of the verified signature; `None` when the
    /// bundle is unsigned. A present but invalid signature is an inspection error, never `None`.
    pub signer_key: Option<String>,
    pub protocol_min: (u16, u16),
    pub protocol_max: (u16, u16),
    pub minimum_host_version: String,
    pub targets: Vec<InspectedTarget>,
    pub files: Vec<InspectedFile>,
    pub dependencies: Vec<InspectedDependency>,
    pub runtimes: Vec<InspectedRuntime>,
    /// The normalized effective capability envelope, sorted and unique: `worktree.read`,
    /// `worktree.write`, `network:<endpoint>`, `process:<runtime>`, `inference`, `state.write`,
    /// `ui`. Compared as sets, never by display label.
    pub access: Vec<String>,
    pub contributions: Vec<InspectedContribution>,
    #[serde(default)]
    pub schema_assets: Vec<InspectedSchemaAsset>,
    /// The state schema reference (wire `SchemaRef` JSON).
    pub state_schema: Value,
    /// The configuration schema reference (wire `SchemaRef` JSON).
    pub configuration_schema: Value,
    pub limits: InspectedLimits,
    pub help_file: String,
    /// The configuration schema document the package ships (the schema asset whose reference
    /// is `configuration_schema`), parsed; `None` when it ships none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration_schema_document: Option<Value>,
}
impl Inspection {
    /// Languages of every semantic contribution, sorted and unique.
    pub fn languages(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .contributions
            .iter()
            .flat_map(|c| c.languages.iter().cloned())
            .collect();
        v.sort();
        v.dedup();
        v
    }
    /// `LanguagePack` when it contributes a semantic indexer, otherwise `HarnessPackage`.
    pub fn kind(&self) -> &'static str {
        if self.contributions.iter().any(|c| c.kind == "Semantic") {
            "LanguagePack"
        } else {
            "HarnessPackage"
        }
    }
    pub fn entrypoint(&self, target_triple: &str) -> Option<&str> {
        self.targets
            .iter()
            .find(|t| t.target_triple == target_triple)
            .map(|t| t.entrypoint.as_str())
    }
    pub fn contribution_ids(&self) -> Vec<String> {
        self.contributions.iter().map(|c| c.id.clone()).collect()
    }
}

/// A file's bytes as received in the bundle (only after inspection verified them).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleFile {
    pub path: String,
    pub bytes: Vec<u8>,
    pub executable: bool,
}

/// A verified bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectedBundle {
    pub inspection: Inspection,
    pub files: Vec<BundleFile>,
}

/// Why a bundle was refused. Nothing of it was written or executed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InspectError {
    /// Not a bundle, a truncated or oversized container, duplicate or extra entries.
    Format(String),
    /// A traversal, absolute, link-like or case-aliased path.
    UnsafePath(String),
    /// The manifest does not decode strictly or breaks a structural rule.
    Manifest(String),
    /// A file's bytes do not match the manifest's digest or length, or a manifest file is
    /// missing.
    Integrity(String),
    /// A signature is present but does not verify (never treated as unsigned).
    Signature(String),
}
impl InspectError {
    /// Stable refusal code (`Rejected { code }`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Format(_) => "BundleFormat",
            Self::UnsafePath(_) => "UnsafePath",
            Self::Manifest(_) => "InvalidManifest",
            Self::Integrity(_) => "IntegrityMismatch",
            Self::Signature(_) => "InvalidSignature",
        }
    }
    pub fn detail(&self) -> &str {
        match self {
            Self::Format(d)
            | Self::UnsafePath(d)
            | Self::Manifest(d)
            | Self::Integrity(d)
            | Self::Signature(d) => d,
        }
    }
}

pub trait PackageInspector: Send + Sync {
    fn inspect(&self, bundle: &[u8]) -> Result<InspectedBundle, InspectError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackageFileError {
    /// Installed bytes no longer match the verified manifest (tampering or corruption).
    Mismatch(String),
    Missing(String),
    Io(String),
}
impl std::fmt::Display for PackageFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mismatch(d) => write!(f, "installed files do not match the manifest: {d}"),
            Self::Missing(d) => write!(f, "installed files are missing: {d}"),
            Self::Io(d) => write!(f, "package files: {d}"),
        }
    }
}

/// Content-addressed install directories `<root>/<package>/<manifest digest>/`.
pub trait PackageFiles: Send + Sync {
    /// Publishes a verified version: writes into a private staging directory, flushes, verifies
    /// every digest, then renames it into place. Idempotent: an existing directory whose
    /// contents verify is kept; a staging directory never becomes visible half-written.
    fn publish(
        &self,
        package_id: &str,
        manifest_digest: &str,
        files: &[BundleFile],
        expected: &[InspectedFile],
    ) -> Result<(), PackageFileError>;
    /// Re-verifies an installed version's files (every byte, before a launch).
    fn verify(
        &self,
        package_id: &str,
        manifest_digest: &str,
        expected: &[InspectedFile],
    ) -> Result<(), PackageFileError>;
    /// The absolute path of a file of an installed version.
    fn path(&self, package_id: &str, manifest_digest: &str, relative: &str) -> PathBuf;
    /// The version's install directory.
    fn directory(&self, package_id: &str, manifest_digest: &str) -> PathBuf;
    /// Removes an installed version's directory; a missing one is success.
    fn remove(&self, package_id: &str, manifest_digest: &str) -> Result<(), PackageFileError>;
    /// How long since the version's directory was last published or reused (`None`: unknown or
    /// missing). Unreferenced directories are collected only once idle long enough, so a
    /// publication whose unit of work has not committed yet is never removed under it.
    fn idle_for(&self, package_id: &str, manifest_digest: &str) -> Option<Duration>;
    /// Every installed `(package, manifest digest)` directory on disk.
    fn installed(&self) -> Result<Vec<(String, String)>, PackageFileError>;
    /// Removes interrupted staging directories older than `older_than` (`None`: all of them,
    /// only before any publication can be in progress, at boot); returns how many.
    fn clean_staging(&self, older_than: Option<Duration>) -> Result<usize, PackageFileError>;
}

/// Runtimes available on this machine (a declared runtime is never installed by the host).
pub trait RuntimeProbe: Send + Sync {
    /// The available version of `runtime_id`, if any.
    fn runtime(&self, runtime_id: &str) -> Option<String>;
    /// Why `runtime_id` is not available, when it was probed and is not.
    fn missing_reason(&self, _runtime_id: &str) -> Option<String> {
        None
    }
}

/// One probed runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeObservation {
    /// Found, with its version normalized to `major.minor.patch`.
    Found(String),
    /// Not found, timed out or its version could not be read (the reason).
    Missing(String),
}

/// Probed runtimes, refreshed by the package host off any UI or write thread (it runs each
/// runtime's version command as an owned process with a short timeout) and read by the
/// availability rules.
#[derive(Default)]
pub struct RuntimeCache {
    seen: std::sync::RwLock<std::collections::BTreeMap<String, RuntimeObservation>>,
    refreshed_at: std::sync::atomic::AtomicU64,
}
impl RuntimeCache {
    pub fn new() -> Self {
        Self::default()
    }
    /// Replaces the observations of a probe pass.
    pub fn set(&self, observed: std::collections::BTreeMap<String, RuntimeObservation>) {
        *self.seen.write().unwrap_or_else(|p| p.into_inner()) = observed;
        self.refreshed_at
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    pub fn observations(&self) -> std::collections::BTreeMap<String, RuntimeObservation> {
        self.seen.read().unwrap_or_else(|p| p.into_inner()).clone()
    }
    /// How many probe passes have completed.
    pub fn passes(&self) -> u64 {
        self.refreshed_at.load(std::sync::atomic::Ordering::SeqCst)
    }
}
impl RuntimeProbe for RuntimeCache {
    fn runtime(&self, runtime_id: &str) -> Option<String> {
        match self
            .seen
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(runtime_id)
        {
            Some(RuntimeObservation::Found(v)) => Some(v.clone()),
            _ => None,
        }
    }
    fn missing_reason(&self, runtime_id: &str) -> Option<String> {
        match self
            .seen
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(runtime_id)
        {
            Some(RuntimeObservation::Missing(why)) => Some(why.clone()),
            Some(RuntimeObservation::Found(_)) => None,
            None => Some("not probed yet".into()),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Publisher indexes (available packs)

/// A configured index source (`packages.indexSources`, applied only by a person): an https URL
/// of an index document or a local directory holding `devworks-index.json`, and the Ed25519
/// public key (hex) its signature must verify against.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexSource {
    pub location: String,
    pub public_key: String,
}

/// One version a verified index offers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexEntry {
    pub package_id: String,
    pub version: String,
    pub manifest_digest: String,
    pub bundle_digest: String,
    pub bundle_bytes: u64,
    pub targets: Vec<String>,
    pub kind: String,
    pub languages: Vec<String>,
    /// Where the bundle is fetched from (an https URL, or a file path inside the source
    /// directory), already resolved and checked by the verifier.
    pub bundle: String,
}

/// An index whose signature verified against its source's key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifiedIndex {
    pub publisher_id: String,
    /// Fingerprint (SHA-256) of the key that signed it.
    pub key_fingerprint: String,
    pub generated_at_ms: i64,
    pub entries: Vec<IndexEntry>,
}

/// Largest index document fetched.
pub const MAX_INDEX_BYTES: u64 = 4 * 1024 * 1024;

/// Fetches index documents and bundles (https, or a local directory). Tests use a fake; nothing
/// in the core reaches the network.
pub trait IndexTransport: Send + Sync {
    /// The bytes at `location`, at most `max_bytes` (more is an error).
    fn fetch(&self, location: &str, max_bytes: u64) -> Result<Vec<u8>, String>;
}

/// Verifies an index document against its source (signature, bounds, entries, bundle
/// locations).
pub trait IndexVerifier: Send + Sync {
    /// The index location of a source (`<dir>/devworks-index.json` for a directory).
    fn index_location(&self, source: &IndexSource) -> Result<String, String>;
    fn verify(&self, source: &IndexSource, document: &[u8]) -> Result<VerifiedIndex, String>;
}

// ---------------------------------------------------------------------------------------------
// Configuration schemas

/// One field-level validation error: the JSON pointer of the offending value and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldError {
    pub pointer: String,
    pub message: String,
}

/// Validates a value against a JSON Schema document (the package's declared configuration
/// schema). A schema that cannot be compiled is reported as one error at the root.
pub trait SchemaValidator: Send + Sync {
    fn validate(&self, schema: &Value, value: &Value) -> Result<(), Vec<FieldError>>;
}

/// What the running host is, for compatibility checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPlatform {
    /// The Rust target triple prebuilt helpers must match (for example `aarch64-apple-darwin`).
    pub target_triple: String,
    /// The host's semantic version.
    pub host_version: String,
    /// The package protocol major and highest minor this host speaks.
    pub protocol_major: u16,
    pub protocol_minor: u16,
}

/// Held while a package's new invocations are gated: no invocation of the package was in flight
/// when it was granted, and none starts until it drops.
pub trait QuiescenceGuard: Send + Sync {}

/// The package host's in-process activity (implemented by `devworks-engine`).
pub trait PackageActivity: Send + Sync {
    /// Gates new invocations of `package_id` and waits up to `wait` for its in-flight
    /// invocations to finish. `None` when it did not quiesce within `wait` (the gate is lifted
    /// again and nothing was interrupted).
    fn quiesce(&self, package_id: &str, wait: Duration) -> Option<Box<dyn QuiescenceGuard>>;
    /// Gates new invocations of `package_id` without waiting for in-flight ones (disable,
    /// uninstall, enable, configuration): no invocation is admitted against the registry the
    /// command is replacing while it commits.
    fn fence(&self, package_id: &str) -> Box<dyn QuiescenceGuard>;
    /// Whether any invocation of `package_id` is in flight.
    fn busy(&self, package_id: &str) -> bool;
    /// The registry may have changed (after a command committed, before its gate lifts):
    /// admissions re-read it and processes are reconciled.
    fn published(&self);
}

/// No package host is running (nothing is ever in flight).
pub struct Idle;
struct IdleGuard;
impl QuiescenceGuard for IdleGuard {}
impl PackageActivity for Idle {
    fn quiesce(&self, _: &str, _: Duration) -> Option<Box<dyn QuiescenceGuard>> {
        Some(Box::new(IdleGuard))
    }
    fn fence(&self, _: &str) -> Box<dyn QuiescenceGuard> {
        Box::new(IdleGuard)
    }
    fn busy(&self, _: &str) -> bool {
        false
    }
    fn published(&self) {}
}
