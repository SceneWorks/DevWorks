//! Package bundles and install directories: the [`BundleInspector`] (`PackageInspector`), the
//! [`NativePackageFiles`] install store (`PackageFiles`) and the bundle writer local package
//! development and tests use ([`write_bundle`]).
//!
//! # Bundle format (`devworks-package-bundle/1`)
//!
//! A bundle is one immutable artifact, little-endian and length-prefixed, with nothing after its
//! last file:
//!
//! ```text
//! magic            8 bytes  "DWPKG\0\x01\0"
//! manifest         u32 length, then the manifest JSON (at most 1 MiB)
//! signature        u32 length (0: unsigned), then {"algorithm":"ed25519","publicKey":hex,"signature":hex}
//! file count       u32 (at most 2048)
//! each file        u16 path length, the UTF-8 path (at most 240 bytes), u64 length, the bytes
//! ```
//!
//! The signature is Ed25519 over `"devworks-package-manifest-v1\n" ‖ manifest bytes`; the
//! manifest binds every file by SHA-256 and length, so the signature covers the whole content.
//! There are no links, directories, permissions or installer scripts in the format: a file is a
//! regular file and an entrypoint is made executable by the host.
//!
//! # Inspection order
//!
//! Size bound, container bounds, the strict manifest (one JSON value, no duplicate members, no
//! unknown fields, `PackageManifest::validate`), the signature when present (a present but
//! invalid signature is refused — never treated as unsigned), then every entry: a safe relative
//! path (no `..`, `.`, absolute, drive, backslash, control character, trailing dot or space, or
//! Windows device name), no duplicate or case-aliased entry, no entry the manifest does not
//! list, and every listed file present with its exact digest and length.
use devworks_core::authority::destructive::{EntryKind, FilesystemResolver};
use devworks_core::authority::reads::{ReadRequest, ReadRoot, VerifiedReader, VerifiedRewindFile};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::framing;
use devworks_core::jcs;
use devworks_core::ports::packages::VerifiedPackageFile;
use devworks_core::ports::packages::{
    BundleFile, InspectError, InspectedBundle, InspectedContribution, InspectedDependency,
    InspectedFile, InspectedLimits, InspectedRuntime, InspectedTarget, Inspection,
    MAX_BUNDLE_BYTES, PackageFileError, PackageFiles, PackageInspector,
};
use devworks_core::ports::packages::{
    FieldError, IndexEntry, IndexSource, IndexTransport, IndexVerifier, SchemaValidator,
    VerifiedIndex,
};
use devworks_extension_protocol::package::{Contribution, PackageManifest, PermissionRequest};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Bundle magic and format version.
pub const MAGIC: &[u8; 8] = b"DWPKG\0\x01\0";
/// Media type of a bundle artifact.
pub const MEDIA_TYPE: &str = "application/vnd.devworks.package";
/// Domain separation of the manifest signature.
pub const SIGNING_CONTEXT: &[u8] = b"devworks-package-manifest-v1\n";
const MAX_MANIFEST: usize = 1024 * 1024;
const MAX_SIGNATURE: usize = 4096;
const MAX_FILES: usize = 2048;
const MAX_PATH: usize = 240;
/// Largest configuration schema document a package ships.
const MAX_SCHEMA: usize = 64 * 1024;
/// Install directory names that are never package IDs.
const STAGING: &str = ".staging";
const TRASH: &str = ".trash";

/// The Rust target triple prebuilt helpers must match on this host (`<arch>-apple-darwin`,
/// `<arch>-pc-windows-msvc` or `<arch>-unknown-linux-gnu`).
pub fn host_target_triple() -> String {
    let arch = std::env::consts::ARCH;
    match std::env::consts::OS {
        "macos" => format!("{arch}-apple-darwin"),
        "windows" => format!("{arch}-pc-windows-msvc"),
        "linux" => format!("{arch}-unknown-linux-gnu"),
        os => format!("{arch}-unknown-{os}"),
    }
}

/// The fingerprint of an Ed25519 public key (SHA-256 of its 32 bytes, hex).
pub fn key_fingerprint(key: &VerifyingKey) -> String {
    sha256_hex(key.as_bytes())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Builds a bundle from manifest bytes and files, signed with `key` when given. The caller is
/// responsible for a manifest that lists exactly these files with their digests.
pub fn write_bundle(manifest: &[u8], files: &[(&str, &[u8])], key: Option<&SigningKey>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
    out.extend_from_slice(manifest);
    let signature = key.map(|k| {
        let mut msg = SIGNING_CONTEXT.to_vec();
        msg.extend_from_slice(manifest);
        let sig = k.sign(&msg);
        json!({
            "algorithm": "ed25519",
            "publicKey": hex(k.verifying_key().as_bytes()),
            "signature": hex(&sig.to_bytes()),
        })
        .to_string()
    });
    let sig = signature.unwrap_or_default();
    out.extend_from_slice(&(sig.len() as u32).to_le_bytes());
    out.extend_from_slice(sig.as_bytes());
    out.extend_from_slice(&(files.len() as u32).to_le_bytes());
    for (path, data) in files {
        out.extend_from_slice(&(path.len() as u16).to_le_bytes());
        out.extend_from_slice(path.as_bytes());
        out.extend_from_slice(&(data.len() as u64).to_le_bytes());
        out.extend_from_slice(data);
    }
    out
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], InspectError> {
        let end = self
            .at
            .checked_add(n)
            .filter(|e| *e <= self.bytes.len())
            .ok_or_else(|| InspectError::Format("the bundle is truncated".into()))?;
        let s = &self.bytes[self.at..end];
        self.at = end;
        Ok(s)
    }
    fn u16(&mut self) -> Result<usize, InspectError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().expect("2 bytes")) as usize)
    }
    fn u32(&mut self) -> Result<usize, InspectError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")) as usize)
    }
    fn u64(&mut self) -> Result<u64, InspectError> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }
}

/// Windows device names, which name a device rather than a file in any directory.
const DEVICES: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// A path that names a file inside the install directory on every supported platform.
pub fn safe_path(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= MAX_PATH
        && !p.starts_with('/')
        && !p.contains(['\\', ':', '\0'])
        && !p.chars().any(char::is_control)
        && p.split('/').all(|c| {
            let stem = c.split('.').next().unwrap_or("").to_ascii_lowercase();
            !c.is_empty()
                && c != "."
                && c != ".."
                && !c.ends_with('.')
                && !c.ends_with(' ')
                && !DEVICES.contains(&stem.as_str())
        })
}

fn access(p: &PermissionRequest) -> String {
    match p {
        PermissionRequest::WorktreeRead => "worktree.read".into(),
        PermissionRequest::WorktreeWrite => "worktree.write".into(),
        PermissionRequest::Network { endpoint_id } => format!("network:{}", endpoint_id.as_str()),
        PermissionRequest::Process { runtime_id } => format!("process:{}", runtime_id.as_str()),
        PermissionRequest::Inference => "inference".into(),
        PermissionRequest::StateWrite => "state.write".into(),
        PermissionRequest::UiContribution => "ui".into(),
    }
}
fn kind(c: &Contribution) -> &'static str {
    match c {
        Contribution::Semantic { .. } => "Semantic",
        Contribution::Tool { .. } => "Tool",
        Contribution::Workflow { .. } => "Workflow",
        Contribution::EventHandler { .. } => "EventHandler",
        Contribution::ContextHook { .. } => "ContextHook",
        Contribution::Content { .. } => "Content",
        Contribution::Provider { .. } => "Provider",
        Contribution::NativeUi { .. } => "NativeUi",
    }
}
fn canonical_digest(v: &Value) -> Result<String, InspectError> {
    jcs::canonicalize(v)
        .map(|c| sha256_hex(c.as_bytes()))
        .map_err(|e| InspectError::Manifest(format!("{e:?}")))
}

/// Verifies bundles (`devworks-package-bundle/1`).
#[derive(Default)]
pub struct BundleInspector;

impl PackageInspector for BundleInspector {
    fn inspect(&self, bundle: &[u8]) -> Result<InspectedBundle, InspectError> {
        if bundle.len() as u64 > MAX_BUNDLE_BYTES {
            return Err(InspectError::Format("the bundle is too large".into()));
        }
        let mut r = Reader {
            bytes: bundle,
            at: 0,
        };
        if r.take(8)? != MAGIC {
            return Err(InspectError::Format("not a DevWorks package bundle".into()));
        }
        let n = r.u32()?;
        if n == 0 || n > MAX_MANIFEST {
            return Err(InspectError::Format("manifest length out of bounds".into()));
        }
        let manifest_bytes = r.take(n)?;
        let n = r.u32()?;
        if n > MAX_SIGNATURE {
            return Err(InspectError::Format(
                "signature length out of bounds".into(),
            ));
        }
        let signature = r.take(n)?;
        let count = r.u32()?;
        if count > MAX_FILES {
            return Err(InspectError::Format("too many files".into()));
        }
        let mut entries: Vec<(String, &[u8])> = Vec::with_capacity(count);
        for _ in 0..count {
            let n = r.u16()?;
            if n == 0 || n > MAX_PATH {
                return Err(InspectError::UnsafePath(
                    "a path length is out of bounds".into(),
                ));
            }
            let path = std::str::from_utf8(r.take(n)?)
                .map_err(|_| InspectError::UnsafePath("a path is not UTF-8".into()))?
                .to_owned();
            let len = r.u64()?;
            let data = r.take(
                usize::try_from(len).map_err(|_| InspectError::Format("file too large".into()))?,
            )?;
            entries.push((path, data));
        }
        if r.at != bundle.len() {
            return Err(InspectError::Format(
                "trailing data after the last file".into(),
            ));
        }

        // The manifest: strict framing (one value, no duplicate members), then the strict DTO.
        let value =
            framing::parse(manifest_bytes).map_err(|e| InspectError::Manifest(e.to_string()))?;
        let manifest: PackageManifest = serde_json::from_value(value.clone())
            .map_err(|e| InspectError::Manifest(e.to_string()))?;
        manifest
            .validate()
            .map_err(|e| InspectError::Manifest(e.to_owned()))?;
        let manifest_digest = sha256_hex(manifest_bytes);

        // The signature, when present, must verify; there is no unsigned fallback.
        let signer_key = if signature.is_empty() {
            None
        } else {
            let s =
                framing::parse(signature).map_err(|e| InspectError::Signature(e.to_string()))?;
            let o = s
                .as_object()
                .filter(|o| {
                    o.len() == 3 && o.get("algorithm").and_then(Value::as_str) == Some("ed25519")
                })
                .ok_or_else(|| InspectError::Signature("unsupported signature block".into()))?;
            let key: [u8; 32] = o
                .get("publicKey")
                .and_then(Value::as_str)
                .and_then(unhex)
                .and_then(|k| k.try_into().ok())
                .ok_or_else(|| InspectError::Signature("invalid public key".into()))?;
            let sig: [u8; 64] = o
                .get("signature")
                .and_then(Value::as_str)
                .and_then(unhex)
                .and_then(|k| k.try_into().ok())
                .ok_or_else(|| InspectError::Signature("invalid signature encoding".into()))?;
            let key = VerifyingKey::from_bytes(&key)
                .map_err(|_| InspectError::Signature("invalid public key".into()))?;
            let mut msg = SIGNING_CONTEXT.to_vec();
            msg.extend_from_slice(manifest_bytes);
            key.verify_strict(&msg, &Signature::from_bytes(&sig))
                .map_err(|_| {
                    InspectError::Signature("the signature does not match the manifest".into())
                })?;
            Some(key_fingerprint(&key))
        };

        // Entries: safe, unique (also ignoring case), listed, and exactly the listed bytes.
        let listed: BTreeMap<&str, (String, u64)> = manifest
            .files
            .iter()
            .map(|f| {
                (
                    f.path.as_str(),
                    (
                        String::from(f.sha256.clone()),
                        String::from(f.byte_length.clone())
                            .parse::<u64>()
                            .unwrap_or(u64::MAX),
                    ),
                )
            })
            .collect();
        let entrypoints: BTreeSet<&str> = manifest
            .targets
            .iter()
            .map(|t| t.entrypoint.as_str())
            .collect();
        let mut seen = BTreeSet::new();
        let mut files = Vec::with_capacity(entries.len());
        for (path, data) in &entries {
            if !safe_path(path) {
                return Err(InspectError::UnsafePath(format!("unsafe path {path:?}")));
            }
            if !seen.insert(path.to_lowercase()) {
                return Err(InspectError::Format(format!("duplicate entry {path:?}")));
            }
            let Some((sha, len)) = listed.get(path.as_str()) else {
                return Err(InspectError::Format(format!(
                    "entry {path:?} is not in the manifest"
                )));
            };
            if data.len() as u64 != *len || sha256_hex(data) != *sha {
                return Err(InspectError::Integrity(format!(
                    "{path:?} does not match the manifest"
                )));
            }
            files.push(BundleFile {
                path: path.clone(),
                bytes: data.to_vec(),
                executable: entrypoints.contains(path.as_str()),
            });
        }
        for f in &manifest.files {
            if !safe_path(&f.path) {
                return Err(InspectError::UnsafePath(format!(
                    "unsafe path {:?}",
                    f.path
                )));
            }
            if !seen.contains(&f.path.to_lowercase()) {
                return Err(InspectError::Integrity(format!(
                    "{:?} is missing from the bundle",
                    f.path
                )));
            }
        }

        // The configuration schema document the package ships: the schema asset whose
        // reference is the declared configuration schema (its bytes were verified above).
        let configuration_schema_document = match manifest.schema_assets.iter().find(|a| {
            serde_json::to_value(&a.reference).ok()
                == serde_json::to_value(&manifest.configuration_schema).ok()
        }) {
            None => None,
            Some(asset) => {
                let bytes = files
                    .iter()
                    .find(|f| f.path == asset.path)
                    .map(|f| f.bytes.as_slice())
                    .ok_or_else(|| {
                        InspectError::Integrity("the configuration schema is missing".into())
                    })?;
                if bytes.len() > MAX_SCHEMA {
                    return Err(InspectError::Manifest(
                        "the configuration schema is too large".into(),
                    ));
                }
                Some(framing::parse(bytes).map_err(|e| {
                    InspectError::Manifest(format!("the configuration schema is not JSON: {e}"))
                })?)
            }
        };
        let mut access_set: Vec<String> =
            manifest.requested_permissions.iter().map(access).collect();
        access_set.sort();
        access_set.dedup();
        let contributions = manifest
            .contributions
            .iter()
            .map(|c| {
                let v =
                    serde_json::to_value(c).map_err(|e| InspectError::Manifest(e.to_string()))?;
                Ok(InspectedContribution {
                    id: c.meta().id.as_str().to_owned(),
                    kind: kind(c).into(),
                    required: c.meta().required,
                    contract_digest: canonical_digest(&v)?,
                    descriptor: Some(v),
                    languages: match c {
                        Contribution::Semantic { languages, .. } => {
                            languages.iter().map(|l| l.as_str().to_owned()).collect()
                        }
                        _ => Vec::new(),
                    },
                })
            })
            .collect::<Result<Vec<_>, InspectError>>()?;
        let to_value =
            |v: &devworks_extension_protocol::scalars::SchemaRef| -> Result<Value, InspectError> {
                serde_json::to_value(v).map_err(|e| InspectError::Manifest(e.to_string()))
            };
        let inspection = Inspection {
            package_id: manifest.package_id.as_str().to_owned(),
            version: manifest.version.clone(),
            publisher_id: manifest.publisher_id.as_str().to_owned(),
            manifest_digest,
            bundle_digest: sha256_hex(bundle),
            signer_key,
            protocol_min: (manifest.protocol_min.major, manifest.protocol_min.minor),
            protocol_max: (manifest.protocol_max.major, manifest.protocol_max.minor),
            minimum_host_version: manifest.minimum_host_version.clone(),
            targets: manifest
                .targets
                .iter()
                .map(|t| InspectedTarget {
                    target_triple: t.target_triple.as_str().to_owned(),
                    entrypoint: t.entrypoint.clone(),
                })
                .collect(),
            files: manifest
                .files
                .iter()
                .map(|f| InspectedFile {
                    path: f.path.clone(),
                    sha256: String::from(f.sha256.clone()),
                    byte_length: String::from(f.byte_length.clone()).parse().unwrap_or(0),
                    executable: entrypoints.contains(f.path.as_str()),
                })
                .collect(),
            dependencies: manifest
                .dependencies
                .iter()
                .map(|d| InspectedDependency {
                    package_id: d.package_id.as_str().to_owned(),
                    version_requirement: d.version_requirement.clone(),
                    manifest_digest: String::from(d.manifest_digest.clone()),
                    required: d.required,
                })
                .collect(),
            runtimes: manifest
                .runtimes
                .iter()
                .map(|r| InspectedRuntime {
                    runtime_id: r.runtime_id.as_str().to_owned(),
                    version_requirement: r.version_requirement.clone(),
                    required: r.required,
                })
                .collect(),
            access: access_set,
            contributions,
            schema_assets: manifest
                .schema_assets
                .iter()
                .map(|a| {
                    Ok(devworks_core::ports::packages::InspectedSchemaAsset {
                        reference: to_value(&a.reference)?,
                        path: a.path.clone(),
                    })
                })
                .collect::<Result<Vec<_>, InspectError>>()?,
            state_schema: to_value(&manifest.state_schema)?,
            configuration_schema: to_value(&manifest.configuration_schema)?,
            limits: InspectedLimits {
                max_concurrent: manifest.requested_limits.max_concurrent,
                queue_capacity: manifest.requested_limits.queue_capacity,
                max_memory_bytes: String::from(manifest.requested_limits.max_memory_bytes.clone())
                    .parse()
                    .unwrap_or(i64::MAX),
                stream_credit_bytes: manifest.requested_limits.stream_credit_bytes,
            },
            help_file: manifest.help_file.clone(),
            configuration_schema_document,
        };
        Ok(InspectedBundle { inspection, files })
    }
}

// ---------------------------------------------------------------------------------------------
// Install directories

/// Install directories under `<root>/<package>/<manifest digest>/`.
pub struct NativePackageFiles {
    root: PathBuf,
    serial: AtomicU64,
}
impl NativePackageFiles {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            serial: AtomicU64::new(0),
        }
    }
    fn io(e: impl std::fmt::Display) -> PackageFileError {
        PackageFileError::Io(e.to_string())
    }
    fn fresh(&self, under: &str) -> PathBuf {
        let n = self.serial.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        self.root
            .join(under)
            .join(format!("{}-{nanos}-{n}", std::process::id()))
    }
    /// Directory fsync makes a rename durable on Unix; Windows has no directory handles here.
    /// A failure is the publication's failure: the caller cannot claim a durable install.
    fn sync_dir(dir: &Path) -> Result<(), PackageFileError> {
        #[cfg(unix)]
        std::fs::File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(Self::io)?;
        #[cfg(not(unix))]
        let _ = dir;
        Ok(())
    }
    /// The marker recording the last publication or reuse of a version directory.
    fn marker(&self, package: &str, digest: &str) -> PathBuf {
        self.root.join(package).join(format!(".{digest}.published"))
    }
    fn touch(&self, package: &str, digest: &str) {
        let _ = std::fs::write(self.marker(package, digest), b"");
    }
    fn valid_name(package: &str, digest: &str) -> bool {
        devworks_core::protocol::Id::new(package).is_ok()
            && digest.len() == 64
            && digest.bytes().all(|b| b.is_ascii_hexdigit())
    }
    fn stopped(stopped: &mut dyn FnMut() -> bool) -> Result<(), PackageFileError> {
        if stopped() {
            Err(PackageFileError::Io(
                "package verification interrupted".into(),
            ))
        } else {
            Ok(())
        }
    }
    fn read_root(&self, dir: &Path) -> Result<ReadRoot, PackageFileError> {
        let location = dir
            .to_str()
            .ok_or_else(|| Self::io("unsupported install path"))?;
        let resolved = crate::fs::NativeFilesystem::new()
            .resolve(location, None)
            .map_err(|_| PackageFileError::Missing("installed package directory".into()))?;
        if resolved.entry_kind == EntryKind::Missing {
            return Err(PackageFileError::Missing(
                "installed package directory".into(),
            ));
        }
        if resolved.entry_kind != EntryKind::Directory
            || resolved
                .linked_ancestors
                .iter()
                .any(|p| p.starts_with(&self.root))
        {
            return Err(PackageFileError::Mismatch(
                "installed package directory changed".into(),
            ));
        }
        Ok(ReadRoot {
            location: resolved.entry.path,
            identity: resolved.entry.identity,
        })
    }
    fn opened(
        root: &ReadRoot,
        file: &InspectedFile,
    ) -> Result<VerifiedRewindFile, PackageFileError> {
        if !safe_path(&file.path) || file.byte_length > MAX_BUNDLE_BYTES {
            return Err(PackageFileError::Mismatch("invalid package file".into()));
        }
        crate::fs::NativeFilesystem::new()
            .open_rewindable(&ReadRequest {
                root,
                relative: &file.path,
                max_bytes: file.byte_length,
                allow_hard_links: false,
            })
            .map_err(|error| match error {
                devworks_core::authority::reads::ReadRefusal::NotFound => {
                    PackageFileError::Missing(file.path.clone())
                }
                devworks_core::authority::reads::ReadRefusal::Io(reason) => {
                    PackageFileError::Io(reason)
                }
                _ => {
                    PackageFileError::Mismatch(format!("{:?} is unavailable or changed", file.path))
                }
            })
    }
    fn hash(
        reader: &mut dyn Read,
        file: &InspectedFile,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<(), PackageFileError> {
        let mut buffer = [0u8; 64 * 1024];
        let mut hash = Sha256::new();
        let mut bytes = 0u64;
        loop {
            Self::stopped(stopped)?;
            let n = reader.read(&mut buffer).map_err(Self::io)?;
            if n == 0 {
                break;
            }
            bytes = bytes.saturating_add(n as u64);
            if bytes > file.byte_length {
                return Err(PackageFileError::Mismatch(format!(
                    "{:?} length changed",
                    file.path
                )));
            }
            hash.update(&buffer[..n]);
        }
        Self::stopped(stopped)?;
        if bytes != file.byte_length || hex(hash.finalize().as_slice()) != file.sha256 {
            return Err(PackageFileError::Mismatch(format!(
                "{:?} was modified",
                file.path
            )));
        }
        Ok(())
    }
    fn check(&self, dir: &Path, expected: &[InspectedFile]) -> Result<(), PackageFileError> {
        let root = self.read_root(dir)?;
        self.check_open(&root, expected, None, &mut || false)
    }
    fn check_open(
        &self,
        root: &ReadRoot,
        expected: &[InspectedFile],
        already_hashed: Option<&str>,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<(), PackageFileError> {
        if expected.len() > MAX_FILES {
            return Err(PackageFileError::Mismatch(
                "invalid package file count".into(),
            ));
        }
        let mut want = BTreeSet::new();
        let mut entry_bound = 0usize;
        let mut total = 0u64;
        for file in expected {
            Self::stopped(stopped)?;
            total = total
                .checked_add(file.byte_length)
                .filter(|total| *total <= MAX_BUNDLE_BYTES)
                .ok_or_else(|| PackageFileError::Mismatch("invalid package byte count".into()))?;
            if !safe_path(&file.path) || !want.insert(root.location.join(&file.path)) {
                return Err(PackageFileError::Mismatch(
                    "invalid package file list".into(),
                ));
            }
            entry_bound += Path::new(&file.path).components().count();
            if already_hashed != Some(file.path.as_str()) {
                let mut opened = Self::opened(root, file)?;
                Self::hash(&mut opened.reader, file, stopped)?;
            }
        }
        // Extra entries and all links are refused. Only manifest-declared ancestor directories
        // are traversed, so added empty directory trees cannot turn verification unbounded.
        let mut stack = vec![root.location.clone()];
        let mut entries = 0usize;
        while let Some(directory) = stack.pop() {
            Self::stopped(stopped)?;
            let now = self.read_root(&directory)?;
            if now.location != directory {
                return Err(PackageFileError::Mismatch(
                    "package directory changed".into(),
                ));
            }
            for entry in std::fs::read_dir(&directory).map_err(Self::io)? {
                Self::stopped(stopped)?;
                let entry = entry.map_err(Self::io)?;
                let kind = entry.file_type().map_err(Self::io)?;
                let path = entry.path();
                entries += 1;
                if entries > entry_bound {
                    return Err(PackageFileError::Mismatch(
                        "package directory changed during verification".into(),
                    ));
                }
                let declared_directory = kind.is_dir()
                    && want
                        .range(path.clone()..)
                        .next()
                        .is_some_and(|file| file.starts_with(&path) && file != &path);
                if declared_directory {
                    stack.push(path);
                } else if !kind.is_file() || !want.contains(&path) {
                    return Err(PackageFileError::Mismatch(format!(
                        "unexpected entry {}",
                        path.strip_prefix(&root.location).unwrap_or(&path).display()
                    )));
                }
            }
        }
        let now = self.read_root(&root.location)?;
        if now.identity != root.identity {
            return Err(PackageFileError::Mismatch(
                "installed package directory changed".into(),
            ));
        }
        Self::stopped(stopped)
    }
}

impl PackageFiles for NativePackageFiles {
    fn open_verified(
        &self,
        package_id: &str,
        manifest_digest: &str,
        expected: &InspectedFile,
        all_files: &[InspectedFile],
        max: u64,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<VerifiedPackageFile, PackageFileError> {
        Self::stopped(stopped)?;
        if !Self::valid_name(package_id, manifest_digest)
            || expected.byte_length > max
            || !all_files.iter().any(|file| file == expected)
        {
            return Err(PackageFileError::Mismatch(
                "invalid installed package file identity".into(),
            ));
        }
        let root = self.read_root(&self.directory(package_id, manifest_digest))?;
        let mut opened = Self::opened(&root, expected)?;
        Self::hash(&mut opened.reader, expected, stopped)?;
        self.check_open(&root, all_files, Some(&expected.path), stopped)?;
        let current = crate::fs::NativeFilesystem::new()
            .resolve(
                root.location
                    .join(&expected.path)
                    .to_str()
                    .ok_or_else(|| Self::io("unsupported installed path"))?,
                None,
            )
            .map_err(|_| PackageFileError::Mismatch("installed package file changed".into()))?;
        if current.entry_kind != EntryKind::File
            || current.entry.identity != opened.identity
            || current
                .linked_ancestors
                .iter()
                .any(|p| p.starts_with(&root.location))
        {
            return Err(PackageFileError::Mismatch(
                "installed package file changed".into(),
            ));
        }
        Self::stopped(stopped)?;
        opened.reader.rewind().map_err(Self::io)?;
        Ok(VerifiedPackageFile {
            reader: opened.reader,
            sha256: expected.sha256.clone(),
            byte_length: expected.byte_length,
        })
    }
    fn read_verified(
        &self,
        package_id: &str,
        manifest_digest: &str,
        expected: &InspectedFile,
        max: u64,
    ) -> Result<Vec<u8>, PackageFileError> {
        use devworks_core::authority::destructive::{EntryKind, FilesystemResolver};
        use devworks_core::authority::reads::{ReadRequest, ReadRoot, VerifiedReader};
        if !Self::valid_name(package_id, manifest_digest)
            || !safe_path(&expected.path)
            || expected.byte_length > max
        {
            return Err(PackageFileError::Mismatch(
                "invalid or oversized package file".into(),
            ));
        }
        let filesystem = crate::fs::NativeFilesystem::new();
        let directory = self.directory(package_id, manifest_digest);
        let root = filesystem
            .resolve(
                directory
                    .to_str()
                    .ok_or_else(|| PackageFileError::Io("unsupported install path".into()))?,
                None,
            )
            .map_err(|_| PackageFileError::Missing("installed package directory".into()))?;
        if root.entry_kind != EntryKind::Directory
            || root
                .linked_ancestors
                .iter()
                .any(|path| path.starts_with(&self.root))
        {
            return Err(PackageFileError::Mismatch(
                "installed package directory changed".into(),
            ));
        }
        let root = ReadRoot {
            location: root.entry.path,
            identity: root.entry.identity,
        };
        let bytes = filesystem
            .read(&ReadRequest {
                root: &root,
                relative: &expected.path,
                max_bytes: max,
                allow_hard_links: false,
            })
            .map_err(|_| {
                PackageFileError::Mismatch(
                    "installed package file is unavailable or changed".into(),
                )
            })?
            .bytes;
        if bytes.len() as u64 != expected.byte_length || sha256_hex(&bytes) != expected.sha256 {
            return Err(PackageFileError::Mismatch(
                "installed package file digest changed".into(),
            ));
        }
        Ok(bytes)
    }
    fn publish(
        &self,
        package_id: &str,
        manifest_digest: &str,
        files: &[BundleFile],
        expected: &[InspectedFile],
    ) -> Result<(), PackageFileError> {
        if !Self::valid_name(package_id, manifest_digest) {
            return Err(PackageFileError::Io("invalid install identity".into()));
        }
        let dest = self.directory(package_id, manifest_digest);
        if dest.exists() {
            if self.check(&dest, expected).is_ok() {
                self.touch(package_id, manifest_digest);
                return Ok(());
            }
            // A damaged copy of these exact contents is replaced by the verified bytes.
            let trash = self.fresh(TRASH);
            std::fs::create_dir_all(trash.parent().expect("parent")).map_err(Self::io)?;
            std::fs::rename(&dest, &trash).map_err(Self::io)?;
            let _ = std::fs::remove_dir_all(&trash);
        }
        let stage = self.fresh(STAGING);
        std::fs::create_dir_all(&stage).map_err(Self::io)?;
        let written = (|| -> Result<(), PackageFileError> {
            for f in files {
                if !safe_path(&f.path) {
                    return Err(PackageFileError::Mismatch(format!(
                        "unsafe path {:?}",
                        f.path
                    )));
                }
                let p = stage.join(&f.path);
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent).map_err(Self::io)?;
                }
                let mut out = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&p)
                    .map_err(Self::io)?;
                out.write_all(&f.bytes).map_err(Self::io)?;
                #[cfg(unix)]
                if f.executable {
                    use std::os::unix::fs::PermissionsExt;
                    out.set_permissions(std::fs::Permissions::from_mode(0o755))
                        .map_err(Self::io)?;
                }
                out.sync_all().map_err(Self::io)?;
            }
            self.check(&stage, expected)?;
            Self::sync_dir(&stage)?;
            let parent = dest.parent().expect("package directory");
            std::fs::create_dir_all(parent).map_err(Self::io)?;
            match std::fs::rename(&stage, &dest) {
                Ok(()) => {}
                // Another publication of the same contents won the rename.
                Err(_) if dest.exists() && self.check(&dest, expected).is_ok() => {
                    let _ = std::fs::remove_dir_all(&stage);
                }
                Err(e) => return Err(Self::io(e)),
            }
            Self::sync_dir(parent)?;
            self.touch(package_id, manifest_digest);
            Ok(())
        })();
        if written.is_err() {
            let _ = std::fs::remove_dir_all(&stage);
        }
        written
    }
    fn verify(
        &self,
        package_id: &str,
        manifest_digest: &str,
        expected: &[InspectedFile],
    ) -> Result<(), PackageFileError> {
        if !Self::valid_name(package_id, manifest_digest) {
            return Err(PackageFileError::Missing("invalid install identity".into()));
        }
        self.check(&self.directory(package_id, manifest_digest), expected)
    }
    fn path(&self, package_id: &str, manifest_digest: &str, relative: &str) -> PathBuf {
        self.directory(package_id, manifest_digest).join(relative)
    }
    fn directory(&self, package_id: &str, manifest_digest: &str) -> PathBuf {
        self.root.join(package_id).join(manifest_digest)
    }
    fn remove(&self, package_id: &str, manifest_digest: &str) -> Result<(), PackageFileError> {
        if !Self::valid_name(package_id, manifest_digest) {
            return Ok(());
        }
        let dir = self.directory(package_id, manifest_digest);
        if !dir.exists() {
            return Ok(());
        }
        // Rename first so a half-removed directory is never mistaken for an installed version.
        let trash = self.fresh(TRASH);
        std::fs::create_dir_all(trash.parent().expect("parent")).map_err(Self::io)?;
        std::fs::rename(&dir, &trash).map_err(Self::io)?;
        let _ = std::fs::remove_file(self.marker(package_id, manifest_digest));
        std::fs::remove_dir_all(&trash).map_err(Self::io)?;
        if let Some(parent) = dir.parent()
            && std::fs::read_dir(parent).is_ok_and(|mut d| d.next().is_none())
        {
            let _ = std::fs::remove_dir(parent);
        }
        Ok(())
    }
    fn idle_for(&self, package_id: &str, manifest_digest: &str) -> Option<Duration> {
        let age = |p: PathBuf| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
        };
        let dir = age(self.directory(package_id, manifest_digest))?;
        Some(match age(self.marker(package_id, manifest_digest)) {
            Some(m) => m.min(dir),
            None => dir,
        })
    }
    fn installed(&self) -> Result<Vec<(String, String)>, PackageFileError> {
        let mut out = Vec::new();
        let Ok(top) = std::fs::read_dir(&self.root) else {
            return Ok(out);
        };
        for p in top.flatten() {
            let name = p.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !p.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            for d in std::fs::read_dir(p.path()).map_err(Self::io)?.flatten() {
                let digest = d.file_name().to_string_lossy().into_owned();
                if Self::valid_name(&name, &digest) && d.file_type().is_ok_and(|t| t.is_dir()) {
                    out.push((name.clone(), digest));
                }
            }
        }
        out.sort();
        Ok(out)
    }
    fn clean_staging(&self, older_than: Option<Duration>) -> Result<usize, PackageFileError> {
        let mut n = 0;
        for under in [STAGING, TRASH] {
            if let Ok(d) = std::fs::read_dir(self.root.join(under)) {
                for e in d.flatten() {
                    let old = older_than.is_none_or(|age| {
                        e.metadata()
                            .and_then(|m| m.modified())
                            .ok()
                            .and_then(|t| t.elapsed().ok())
                            .is_some_and(|el| el >= age)
                    });
                    if old && std::fs::remove_dir_all(e.path()).is_ok() {
                        n += 1;
                    }
                }
            }
        }
        Ok(n)
    }
}

// ---------------------------------------------------------------------------------------------
// Signed publisher indexes (`devworks-package-index/1`)

/// Domain separation of the index signature.
pub const INDEX_SIGNING_CONTEXT: &[u8] = b"devworks-package-index-v1\n";
/// The index document a directory source holds.
pub const INDEX_FILE: &str = "devworks-index.json";
const MAX_ENTRIES: usize = 1024;

/// Builds a signed index document from the index JSON (a publisher tool and the tests use it):
/// `{"format", "index": <the exact index text>, "signature": {...}}`, the Ed25519 signature
/// over `"devworks-package-index-v1\n" ‖ index text`.
pub fn write_index(index: &Value, key: &SigningKey) -> Vec<u8> {
    let text = index.to_string();
    let mut msg = INDEX_SIGNING_CONTEXT.to_vec();
    msg.extend_from_slice(text.as_bytes());
    let sig = key.sign(&msg);
    json!({
        "format": "devworks-package-index/1",
        "index": text,
        "signature": {"algorithm": "ed25519", "publicKey": hex(key.verifying_key().as_bytes()),
            "signature": hex(&sig.to_bytes())},
    })
    .to_string()
    .into_bytes()
}

fn is_digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Verifies index documents against their source.
///
/// The signature must verify with the source's configured key (an index signed by any other
/// key is refused, even if valid); the document is strictly decoded and bounded; every entry
/// needs a valid package ID, semantic version, digests, size, targets and a bundle location: a
/// relative path inside a directory source, or an https URL (absolute, or relative to the
/// index URL) for a URL source. Plain http, other schemes and traversal are refused.
#[derive(Default)]
pub struct BundleIndexVerifier;

impl IndexVerifier for BundleIndexVerifier {
    fn index_location(&self, source: &IndexSource) -> Result<String, String> {
        if source.location.starts_with("https://") {
            return Ok(source.location.clone());
        }
        let dir = Path::new(&source.location);
        if !dir.is_absolute() {
            return Err("an index source is an https:// URL or an absolute directory".into());
        }
        Ok(dir.join(INDEX_FILE).display().to_string())
    }

    fn verify(&self, source: &IndexSource, document: &[u8]) -> Result<VerifiedIndex, String> {
        let doc = framing::parse(document).map_err(|e| format!("the index is not JSON: {e}"))?;
        let o = doc
            .as_object()
            .filter(|o| {
                o.len() == 3
                    && o.get("format").and_then(Value::as_str) == Some("devworks-package-index/1")
            })
            .ok_or("not a devworks-package-index/1 document")?;
        let text = o
            .get("index")
            .and_then(Value::as_str)
            .ok_or("the index text is missing")?;
        let sig = o
            .get("signature")
            .and_then(Value::as_object)
            .filter(|s| s.len() == 3)
            .ok_or("the index signature is missing")?;
        if sig.get("algorithm").and_then(Value::as_str) != Some("ed25519") {
            return Err("unsupported index signature algorithm".into());
        }
        let key_hex = sig.get("publicKey").and_then(Value::as_str).unwrap_or("");
        if key_hex != source.public_key {
            return Err("the index is signed by a key this source does not trust".into());
        }
        let key: [u8; 32] = unhex(key_hex)
            .and_then(|k| k.try_into().ok())
            .ok_or("invalid index key")?;
        let key = VerifyingKey::from_bytes(&key).map_err(|_| "invalid index key")?;
        let signature: [u8; 64] = sig
            .get("signature")
            .and_then(Value::as_str)
            .and_then(unhex)
            .and_then(|s| s.try_into().ok())
            .ok_or("invalid index signature encoding")?;
        let mut msg = INDEX_SIGNING_CONTEXT.to_vec();
        msg.extend_from_slice(text.as_bytes());
        key.verify_strict(&msg, &Signature::from_bytes(&signature))
            .map_err(|_| "the index signature does not verify")?;

        let index = framing::parse(text.as_bytes())
            .map_err(|e| format!("the index body is not JSON: {e}"))?;
        let publisher = index["publisherId"]
            .as_str()
            .filter(|p| devworks_core::protocol::Id::new(*p).is_ok())
            .ok_or("the index names no valid publisher")?;
        let generated = index["generatedAtMs"]
            .as_i64()
            .filter(|n| *n >= 0)
            .unwrap_or(0);
        let entries = index["entries"]
            .as_array()
            .ok_or("the index has no entries list")?;
        if entries.len() > MAX_ENTRIES {
            return Err("the index lists too many entries".into());
        }
        let mut out = Vec::with_capacity(entries.len());
        for (n, e) in entries.iter().enumerate() {
            let bad = |what: &str| format!("entry {}: {what}", n + 1);
            let text = |k: &str| e[k].as_str().map(str::to_owned);
            let package_id = text("packageId")
                .filter(|p| devworks_core::protocol::Id::new(p).is_ok())
                .ok_or_else(|| bad("invalid package ID"))?;
            let version = text("version")
                .filter(|v| semver::Version::parse(v).is_ok())
                .ok_or_else(|| bad("invalid version"))?;
            let manifest_digest = text("manifestDigest")
                .filter(|d| is_digest(d))
                .ok_or_else(|| bad("invalid manifest digest"))?;
            let bundle_digest = text("bundleDigest")
                .filter(|d| is_digest(d))
                .ok_or_else(|| bad("invalid bundle digest"))?;
            let bundle_bytes = e["bundleBytes"]
                .as_u64()
                .filter(|b| *b > 0 && *b <= devworks_core::ports::packages::MAX_BUNDLE_BYTES)
                .ok_or_else(|| bad("invalid bundle size"))?;
            let list = |k: &str| -> Option<Vec<String>> {
                e[k].as_array()?
                    .iter()
                    .take(64)
                    .map(|v| {
                        v.as_str()
                            .filter(|s| !s.is_empty() && s.len() <= 128)
                            .map(str::to_owned)
                    })
                    .collect()
            };
            let targets = list("targets")
                .filter(|t| !t.is_empty())
                .ok_or_else(|| bad("invalid targets"))?;
            let languages = list("languages").unwrap_or_default();
            let kind = match text("kind").as_deref() {
                Some("LanguagePack") => "LanguagePack",
                _ => "HarnessPackage",
            };
            let location = text("bundle").ok_or_else(|| bad("no bundle location"))?;
            let bundle = resolve(&source.location, &location)
                .ok_or_else(|| bad("unsafe bundle location"))?;
            out.push(IndexEntry {
                package_id,
                version,
                manifest_digest,
                bundle_digest,
                bundle_bytes,
                targets,
                kind: kind.into(),
                languages,
                bundle,
            });
        }
        Ok(VerifiedIndex {
            publisher_id: publisher.to_owned(),
            key_fingerprint: key_fingerprint(&key),
            generated_at_ms: generated,
            entries: out,
        })
    }
}

/// Resolves a bundle location against its source: a safe relative path inside a directory,
/// or an https URL (absolute, or relative to the index URL's directory).
fn resolve(source: &str, location: &str) -> Option<String> {
    if source.starts_with("https://") {
        if location.starts_with("https://") {
            return (!location.contains(['\\', ' ', '\n', '\r'])).then(|| location.to_owned());
        }
        if location.contains("://") || location.starts_with('/') || !safe_path(location) {
            return None;
        }
        let base = &source[..source.rfind('/')? + 1];
        return Some(format!("{base}{location}"));
    }
    // Joined component by component, so the result uses the platform's separator throughout
    // (`safe_path` already refused absolute, drive, backslash and traversal forms).
    safe_path(location).then(|| {
        location
            .split('/')
            .fold(PathBuf::from(source), |p, c| p.join(c))
            .display()
            .to_string()
    })
}

/// Fetches index documents and bundles: `https://` with ureq (rustls, bundled roots, a
/// bounded body, no redirects to plain http), or a regular file under a local directory.
pub struct NativeIndexTransport {
    agent: package_ureq::Agent,
}
impl Default for NativeIndexTransport {
    fn default() -> Self {
        Self {
            agent: package_ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(60))
                .redirects(5)
                .https_only(true)
                .build(),
        }
    }
}
impl IndexTransport for NativeIndexTransport {
    fn fetch(&self, location: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
        use std::io::Read;
        let mut out = Vec::new();
        if location.starts_with("https://") {
            let resp = self
                .agent
                .get(location)
                .call()
                .map_err(|e| format!("fetching {location}: {e}"))?;
            resp.into_reader()
                .take(max_bytes + 1)
                .read_to_end(&mut out)
                .map_err(|e| format!("reading {location}: {e}"))?;
        } else {
            let path = Path::new(location);
            let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{location}: {e}"))?;
            if !meta.is_file() {
                return Err(format!("{location} is not a regular file"));
            }
            std::fs::File::open(path)
                .and_then(|f| f.take(max_bytes + 1).read_to_end(&mut out))
                .map_err(|e| format!("{location}: {e}"))?;
        }
        if out.len() as u64 > max_bytes {
            return Err(format!("{location} is larger than {max_bytes} bytes"));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------------------------
// Configuration schema validation

/// JSON Schema validation (jsonschema, drafts 4–2020-12; no remote or file references are
/// resolved, so a schema cannot reach the network or the file system).
#[derive(Default)]
pub struct JsonSchemaValidator;

impl SchemaValidator for JsonSchemaValidator {
    fn validate(&self, schema: &Value, value: &Value) -> Result<(), Vec<FieldError>> {
        let validator = jsonschema::validator_for(schema).map_err(|e| {
            vec![FieldError {
                pointer: String::new(),
                message: format!("the package's configuration schema is invalid: {e}"),
            }]
        })?;
        let errors: Vec<FieldError> = validator
            .iter_errors(value)
            .take(64)
            .map(|e| FieldError {
                pointer: e.instance_path.to_string(),
                message: e.to_string(),
            })
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}
