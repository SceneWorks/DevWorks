//! Signed DevWorks release metadata shared by discovery, approval and the native installer.
use super::storage::{Cancellation, FileDigest, MigrationWitness};
use crate::fingerprint::is_sha256_hex;
use crate::protocol::records::{UpdateChannel, UpdateRecord};
use crate::protocol::{ActorId, Id, OperationId, StoreId};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_RELEASE_PAGES: usize = 12;
pub const RELEASES_PER_PAGE: usize = 30;
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
pub const MAX_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_FILES: usize = 8192;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallOwner {
    MacApplication,
    WindowsPerUser,
    LinuxStandalone,
}

impl InstallOwner {
    pub const fn entrypoint(self) -> &'static str {
        match self {
            Self::MacApplication => "Contents/MacOS/devworks-desktop",
            Self::WindowsPerUser => "devworks-desktop.exe",
            Self::LinuxStandalone => "devworks-desktop",
        }
    }

    pub fn matches_target(self, target: &str) -> bool {
        let Some((arch, system)) = target.split_once('-') else {
            return false;
        };
        matches!(arch, "aarch64" | "x86_64")
            && matches!(
                (self, system),
                (Self::MacApplication, "apple-darwin")
                    | (Self::WindowsPerUser, "pc-windows-msvc")
                    | (Self::LinuxStandalone, "unknown-linux-gnu")
            )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReleaseFile {
    pub path: String,
    pub content: FileDigest,
    pub executable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReleaseManifest {
    pub format: u16,
    pub product: String,
    pub release_id: u64,
    pub tag: String,
    pub build: String,
    pub target: String,
    pub owner: InstallOwner,
    pub archive: FileDigest,
    pub archive_asset: String,
    pub help_digest: String,
    pub migrations: Vec<MigrationWitness>,
    pub files: Vec<ReleaseFile>,
    pub destructive_migration: bool,
}

impl ReleaseManifest {
    /// GitHub metadata supplies discovery and revocation. Only the verified signed
    /// manifest supplies content authority after identity is compared to this observation.
    pub fn check(&self, release: &Release, target: &str) -> Result<(), ReleaseError> {
        if self.format != 1
            || self.product != "SceneWorks/DevWorks"
            || self.release_id != release.id
            || self.tag != release.tag
            || self.target != target
            || !self.owner.matches_target(target)
            || !self.archive_asset.eq(&asset_name(target, "bundle"))
            || !is_sha256_hex(&self.archive.sha256)
            || self.archive.bytes == 0
            || self.archive.bytes > MAX_ARCHIVE_BYTES
            || !is_sha256_hex(&self.help_digest)
            || self.build.len() != 40
            || !self.build.bytes().all(lower_hex)
            || self.migrations.is_empty()
            || self.migrations.len() > 1024
            || self.files.is_empty()
            || self.files.len() > MAX_FILES
        {
            return Err(ReleaseError::rejected(
                "The signed release identity or target is invalid.",
            ));
        }

        let mut prior_reader = 0;
        let mut prior_writer = 0;
        for (index, migration) in self.migrations.iter().enumerate() {
            if migration.version != index as i64 + 1
                || !(prior_reader.max(1)..=migration.minimum_writer)
                    .contains(&migration.minimum_reader)
                || !(prior_writer.max(1)..=migration.version).contains(&migration.minimum_writer)
                || !is_sha256_hex(&migration.checksum)
            {
                return Err(ReleaseError::rejected(
                    "The signed migration ledger is invalid.",
                ));
            }
            prior_reader = migration.minimum_reader;
            prior_writer = migration.minimum_writer;
        }

        let mut paths = BTreeSet::new();
        let mut directory_spellings = BTreeMap::new();
        let mut total = 0u64;
        let mut entrypoint = false;
        for file in &self.files {
            let path = file.path.to_ascii_lowercase();
            if !safe_file_path(&file.path)
                || !paths.insert(path)
                || !is_sha256_hex(&file.content.sha256)
            {
                return Err(ReleaseError::rejected(
                    "The signed file inventory is invalid.",
                ));
            }
            for (at, _) in file.path.match_indices('/') {
                let exact = &file.path[..at];
                if directory_spellings
                    .insert(exact.to_ascii_lowercase(), exact)
                    .is_some_and(|old| old != exact)
                {
                    return Err(ReleaseError::rejected(
                        "The signed inventory has an aliased directory.",
                    ));
                }
            }
            total = total
                .checked_add(file.content.bytes)
                .ok_or_else(|| ReleaseError::rejected("The signed archive is too large."))?;
            entrypoint |= file.executable && file.path == self.owner.entrypoint();
        }
        if !entrypoint
            || total != self.archive.bytes
            || !self
                .files
                .windows(2)
                .all(|pair| pair[0].path < pair[1].path)
            || paths.iter().any(|path| {
                path.match_indices('/')
                    .any(|(at, _)| paths.contains(&path[..at]))
            })
        {
            return Err(ReleaseError::rejected(
                "The signed inventory and archive disagree.",
            ));
        }
        Ok(())
    }

    pub fn entrypoint(&self) -> &ReleaseFile {
        self.files
            .iter()
            .find(|file| file.path == self.owner.entrypoint())
            .expect("a validated manifest contains its entrypoint")
    }
}

fn lower_hex(c: u8) -> bool {
    matches!(c, b'0'..=b'9' | b'a'..=b'f')
}

/// No alternate data streams, device aliases, non-ASCII normalization aliases, or
/// special components. Use the same grammar on all targets before any archive I/O.
fn safe_file_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 512
        && path.split('/').all(|part| {
            !part.is_empty()
                && part.len() <= 120
                && !part.starts_with('.')
                && !part.ends_with('.')
                && part
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
                && !windows_reserved(part.split('.').next().unwrap_or_default())
        })
}

fn windows_reserved(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    matches!(name.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (name.len() == 4
            && matches!(&name[..3], "COM" | "LPT")
            && name.as_bytes()[3].is_ascii_digit())
}

pub fn asset_name(target: &str, suffix: &str) -> String {
    format!("DevWorks-{target}.{suffix}")
}

/// Only names, sizes and numeric IDs from the release response are retained. The native
/// source builds an asset URL on the known origin from the ID, never from a supplied URL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseAsset {
    pub id: u64,
    pub name: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub id: u64,
    pub tag: String,
    pub prerelease: bool,
    pub assets: Vec<ReleaseAsset>,
}

impl Release {
    pub fn version(&self) -> Option<Version> {
        let version = Version::parse(self.tag.strip_prefix('v')?).ok()?;
        (version.pre.is_empty() != self.prerelease).then_some(version)
    }

    pub fn channel(&self) -> UpdateChannel {
        if self.prerelease {
            UpdateChannel::Prerelease
        } else {
            UpdateChannel::Stable
        }
    }

    pub fn asset(&self, name: &str) -> Option<&ReleaseAsset> {
        let mut matches = self.assets.iter().filter(|asset| asset.name == name);
        let asset = matches.next().filter(|asset| asset.id > 0)?;
        matches.next().is_none().then_some(asset)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReleasePage {
    pub number: usize,
    pub etag: Option<String>,
    pub next: Option<usize>,
    pub releases: Vec<Release>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedRelease {
    pub manifest: ReleaseManifest,
    pub manifest_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseError {
    pub message: String,
    pub retry_after_ms: Option<i64>,
    pub cancelled: bool,
}

impl ReleaseError {
    pub fn rejected(message: &str) -> Self {
        Self {
            message: message.to_owned(),
            retry_after_ms: None,
            cancelled: false,
        }
    }

    pub fn cancelled() -> Self {
        Self {
            message: "Update preparation was cancelled.".into(),
            retry_after_ms: None,
            cancelled: true,
        }
    }
}

pub trait ReleaseSource: Send + Sync {
    fn discover(
        &self,
        cached: &[ReleasePage],
        cancel: &Cancellation,
    ) -> Result<Vec<ReleasePage>, ReleaseError>;

    /// Verify the signed manifest, bound download size, validate all files, and publish
    /// a private content-addressed stage. Report progress only against the selected asset.
    fn stage(
        &self,
        release: &Release,
        target: &str,
        cancel: &Cancellation,
        progress: &(dyn Fn(u64, u64) + Sync),
    ) -> Result<StagedRelease, ReleaseError>;

    /// Retire private archived stages not referenced by the durable update state.
    fn retain(&self, _stages: &[&str]) {}

    /// Reopen the actual staged bytes under the pinned release trust.
    fn verify(&self, stage: &StagedRelease, cancel: &Cancellation) -> Result<(), ReleaseError>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadedUpdate {
    pub record: UpdateRecord,
    pub stage: StagedRelease,
    pub proposal_id: Id,
}

/// These approval fields are saved in the same transaction as the command receipt.
/// The installer can observe them after the approved boot and SQLite writer have exited.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallApproval {
    pub operation: OperationId,
    pub proposal: Id,
    pub update: Id,
    pub revision: i64,
    pub store: StoreId,
    pub epoch: i64,
    pub actor: ActorId,
    pub approved_boot: Id,
    pub request_sha256: String,
    pub expires_at_ms: i64,
    pub old_build: String,
    pub old_binary_digest: String,
    pub old_version: String,
    pub old_schema_capability: i64,
    pub profile_schema: i64,
    pub profile_migrations: Vec<MigrationWitness>,
    pub stage: StagedRelease,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateState {
    pub generation: i64,
    pub profile: Option<(StoreId, i64)>,
    pub channel: UpdateChannel,
    pub checked_at_ms: Option<i64>,
    pub retry_at_ms: i64,
    pub failures: u32,
    pub issue: Option<String>,
    pub pages: Vec<ReleasePage>,
    pub update: Option<DownloadedUpdate>,
    pub approval: Option<InstallApproval>,
    pub last: Option<UpdateRecord>,
    pub last_detail: Option<String>,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            generation: 1,
            profile: None,
            channel: UpdateChannel::Stable,
            checked_at_ms: None,
            retry_at_ms: 0,
            failures: 0,
            issue: None,
            pages: Vec::new(),
            update: None,
            approval: None,
            last: None,
            last_detail: None,
        }
    }
}

#[derive(Clone)]
pub struct RunningApplication {
    pub version: Version,
    pub build: String,
    pub target: String,
    pub migrations: Vec<MigrationWitness>,
    pub installed_binary_digest: Option<String>,
}

pub struct UpdateProfile {
    pub schema: i64,
    pub minimum_reader: i64,
    pub minimum_writer: i64,
    pub migrations: Vec<(i64, String)>,
}

impl UpdateProfile {
    pub fn reviewed_prefix<'a>(
        &self,
        signed: &'a [MigrationWitness],
    ) -> Option<&'a [MigrationWitness]> {
        let count = usize::try_from(self.schema).ok()?;
        let prefix = signed.get(..count)?;
        let current = prefix.last()?;
        (current.minimum_reader == self.minimum_reader
            && current.minimum_writer == self.minimum_writer
            && self.migrations.len() == count
            && self
                .migrations
                .iter()
                .zip(prefix)
                .all(|((version, checksum), planned)| {
                    *version == planned.version && checksum == &planned.checksum
                }))
        .then_some(prefix)
    }
}

pub trait UpdateRead {
    fn update_profile(&self) -> Result<UpdateProfile, super::storage::StorageError>;
    fn update_state(&self) -> Result<UpdateState, super::storage::StorageError>;
}

pub trait UpdateRepository: UpdateRead {
    fn save_update_state(
        &mut self,
        state: &UpdateState,
    ) -> Result<(), super::storage::StorageError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_file_names_are_portable_and_confined() {
        assert!(safe_file_path("Contents/_CodeSignature/CodeResources"));
        for path in [
            "", "/a", "../a", "a/./b", "a\\b", "a:zone", "a//b", "AUX.txt", "bin/LpT9", "A.", "Å",
        ] {
            assert!(!safe_file_path(path), "{path}");
        }
    }
}
