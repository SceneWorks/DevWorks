//! The native port uses anonymous GitHub release discovery and signed, private staging.
mod github;

use crate::fs::workspace_archive::ArchiveDirectory;
use crate::http::NativeHttp;
use devworks_core::fingerprint::{is_sha256_hex, sha256_hex};
use devworks_core::ports::storage::{Cancellation, FileDigest, StorageError};
use devworks_core::ports::update::{
    MAX_MANIFEST_BYTES, Release, ReleaseAsset, ReleaseError, ReleaseManifest, ReleasePage,
    ReleaseSource, StagedRelease, asset_name,
};
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest as _, Sha256};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

const STAGING: &str = "app-update-stages";
const MANIFEST: &str = "manifest.json";
const SIGNATURE: &str = "manifest.sig";
const ARCHIVE: &str = "app.bundle";

pub struct NativeReleaseSource {
    root: PathBuf,
    http: NativeHttp,
    feed: Option<std::sync::Arc<dyn devworks_core::ports::http::HttpTransport>>,
    trust: Vec<VerifyingKey>,
}

impl NativeReleaseSource {
    pub fn new(root: PathBuf, trust: Vec<VerifyingKey>) -> Self {
        Self {
            root,
            http: NativeHttp::new(),
            feed: None,
            trust,
        }
    }

    pub fn with_feed(
        mut self,
        feed: std::sync::Arc<dyn devworks_core::ports::http::HttpTransport>,
    ) -> Self {
        self.feed = Some(feed);
        self
    }

    fn staging(&self) -> Result<ArchiveDirectory, ReleaseError> {
        let profile = ArchiveDirectory::open(&self.root, None).map_err(storage)?;
        match profile.create_folder(STAGING) {
            Ok(stage) => Ok(stage),
            Err(_) => profile.child(STAGING).map_err(storage),
        }
    }

    fn open_stage(&self, expected: &StagedRelease) -> Result<ArchiveDirectory, ReleaseError> {
        if !is_sha256_hex(&expected.manifest_digest) {
            return Err(rejected(
                "The selected update has an invalid manifest commitment.",
            ));
        }
        let stage = self
            .staging()?
            .child(&expected.manifest_digest)
            .map_err(storage)?;
        let manifest = read_member(&stage, MANIFEST, MAX_MANIFEST_BYTES as u64)?;
        let signature = read_member(&stage, SIGNATURE, 64)?;
        if verify_manifest(&manifest, &signature, &self.trust)? != *expected {
            return Err(rejected("The staged update manifest changed."));
        }
        // Keep the parser as a fence for the persistent stage as well as the network path.
        let view = Release {
            id: expected.manifest.release_id,
            tag: expected.manifest.tag.clone(),
            prerelease: expected.manifest.tag.contains('-'),
            assets: Vec::new(),
        };
        expected.manifest.check(&view, &expected.manifest.target)?;
        Ok(stage)
    }

    /// A private directory on the installation volume already claimed by the installer.
    /// Each extraction validates the bytes read from the verified handle, including the
    /// entire archive digest and all member digests.
    pub fn materialize(
        &self,
        expected: &StagedRelease,
        destination: &Path,
        cancellation: &Cancellation,
    ) -> Result<(), ReleaseError> {
        let stage = self.open_stage(expected)?;
        let destination = ArchiveDirectory::open(destination, None).map_err(storage)?;
        inspect_archive(&stage, &expected.manifest, Some(&destination), cancellation)
    }

    fn asset_response(
        &self,
        asset: &ReleaseAsset,
    ) -> Result<ureq::http::Response<ureq::Body>, ReleaseError> {
        let mut address = github::asset_path(asset.id);
        for _ in 0..=3 {
            let response = self
                .http
                .public_asset(&address)
                .map_err(|_| rejected("The signed update asset could not be downloaded."))?;
            match response.status().as_u16() {
                200 => return Ok(response),
                301 | 302 | 303 | 307 | 308 => {
                    let location = response
                        .headers()
                        .get("location")
                        .and_then(|value| value.to_str().ok())
                        .filter(|value| value.len() <= 8192)
                        .ok_or_else(|| {
                            rejected("GitHub returned an invalid update asset redirect.")
                        })?;
                    let url = Url::parse(location).map_err(|_| {
                        rejected("GitHub returned an invalid update asset redirect.")
                    })?;
                    let trusted_cdn = [
                        "release-assets.githubusercontent.com",
                        "objects.githubusercontent.com",
                    ]
                    .iter()
                    .any(|host| github::clean_https(&url, host));
                    if !trusted_cdn {
                        return Err(rejected(
                            "GitHub redirected the update asset outside its download hosts.",
                        ));
                    }
                    address = url.into();
                }
                _ => return Err(rejected("The signed update asset is unavailable.")),
            }
        }
        Err(rejected("GitHub returned too many update asset redirects."))
    }

    fn small_asset(&self, asset: &ReleaseAsset, bound: usize) -> Result<Vec<u8>, ReleaseError> {
        if asset.size == 0 || asset.size > bound as u64 {
            return Err(rejected(
                "The signed update manifest or signature exceeds its size limit.",
            ));
        }
        let body = self
            .asset_response(asset)?
            .body_mut()
            .with_config()
            .limit(bound as u64)
            .read_to_vec()
            .map_err(|_| rejected("The signed update manifest or signature could not be read."))?;
        if body.len() as u64 != asset.size {
            return Err(rejected(
                "The release asset length changed during discovery.",
            ));
        }
        Ok(body)
    }
}

impl ReleaseSource for NativeReleaseSource {
    fn discover(
        &self,
        cached: &[ReleasePage],
        cancellation: &Cancellation,
    ) -> Result<Vec<ReleasePage>, ReleaseError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        github::discover(
            self.feed.as_deref().unwrap_or(&self.http),
            cached,
            cancellation,
            now,
        )
    }

    fn stage(
        &self,
        release: &Release,
        target: &str,
        cancellation: &Cancellation,
        progress: &(dyn Fn(u64, u64) + Sync),
    ) -> Result<StagedRelease, ReleaseError> {
        if self.trust.is_empty() {
            return Err(rejected(
                "This application build has no trusted DevWorks release signing key.",
            ));
        }
        let asset = |suffix| {
            release
                .asset(&asset_name(target, suffix))
                .ok_or_else(|| rejected("This target's signed release files are unavailable."))
        };
        check_cancel(cancellation)?;
        let bytes = self.small_asset(asset("manifest.json")?, MAX_MANIFEST_BYTES)?;
        check_cancel(cancellation)?;
        let signature = self.small_asset(asset("manifest.sig")?, 64)?;
        let staged = verify_manifest(&bytes, &signature, &self.trust)?;
        staged.manifest.check(release, target)?;
        let archive = asset("bundle")?;
        if archive.size != staged.manifest.archive.bytes {
            return Err(rejected(
                "The release archive length differs from its signed manifest.",
            ));
        }
        check_cancel(cancellation)?;
        let root = self.staging()?;
        if root.child(&staged.manifest_digest).is_ok() {
            self.verify(&staged, cancellation)?;
            return Ok(staged);
        }
        let pending = format!("pending-{}", random_token()?);
        let directory = root.create_folder(&pending).map_err(storage)?;
        let result = (|| {
            for (name, body) in [
                (MANIFEST, bytes.as_slice()),
                (SIGNATURE, signature.as_slice()),
            ] {
                directory
                    .write_member(
                        name,
                        body,
                        &FileDigest {
                            sha256: sha256_hex(body),
                            bytes: body.len() as u64,
                        },
                        cancellation,
                    )
                    .map_err(storage)?;
            }
            let mut response = self.asset_response(archive)?;
            let body = response
                .body_mut()
                .with_config()
                .limit(staged.manifest.archive.bytes + 1);
            let mut body = ProgressRead {
                inner: body.reader(),
                progress,
                bytes: 0,
                total: archive.size,
            };
            directory
                .write_member(ARCHIVE, &mut body, &staged.manifest.archive, cancellation)
                .map_err(storage)?;
            inspect_archive(&directory, &staged.manifest, None, cancellation)?;
            check_cancel(cancellation)?;
            root.publish_child(&directory, &staged.manifest_digest)
                .map_err(storage)?;
            Ok(())
        })();
        if result.is_err()
            && root
                .child(&pending)
                .is_ok_and(|child| child.identity() == directory.identity())
        {
            drop(directory);
            let _ = std::fs::remove_dir_all(root.location().join(&pending));
        }
        result?;
        Ok(staged)
    }

    fn retain(&self, keep: &[&str]) {
        let Ok(root) =
            ArchiveDirectory::open(&self.root, None).and_then(|profile| profile.child(STAGING))
        else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(root.location()) else {
            return;
        };
        for entry in entries.take(256).flatten() {
            let name = entry.file_name();
            let Some(key) = name
                .to_str()
                .filter(|name| is_sha256_hex(name) && !keep.contains(name))
            else {
                continue;
            };
            let Ok(stage) = root.child(key) else { continue };
            let valid = read_member(&stage, MANIFEST, MAX_MANIFEST_BYTES as u64)
                .and_then(|document| {
                    let signature = read_member(&stage, SIGNATURE, 64)?;
                    verify_manifest(&document, &signature, &self.trust)
                })
                .is_ok_and(|found| found.manifest_digest == key);
            if valid
                && root
                    .child(key)
                    .is_ok_and(|current| current.identity() == stage.identity())
            {
                drop(stage);
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        let _ = root.flush();
    }

    fn verify(
        &self,
        expected: &StagedRelease,
        cancellation: &Cancellation,
    ) -> Result<(), ReleaseError> {
        let stage = self.open_stage(expected)?;
        inspect_archive(&stage, &expected.manifest, None, cancellation)
    }
}

pub fn digest_owned_file(
    file: std::fs::File,
    cancellation: &Cancellation,
) -> Result<FileDigest, ReleaseError> {
    let mut source =
        DigestRead::new(file.take(devworks_core::ports::update::MAX_ARCHIVE_BYTES + 1));
    let mut block = [0u8; 64 * 1024];
    loop {
        check_cancel(cancellation)?;
        if source
            .read(&mut block)
            .map_err(|_| rejected("The installed file could not be read."))?
            == 0
        {
            break;
        }
    }
    let digest = source.digest();
    if digest.bytes > devworks_core::ports::update::MAX_ARCHIVE_BYTES {
        return Err(rejected("The installed file exceeds its size limit."));
    }
    Ok(digest)
}

pub fn random_token() -> Result<String, ReleaseError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|_| rejected("A private update token could not be allocated."))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn verify_manifest(
    bytes: &[u8],
    signature: &[u8],
    keys: &[VerifyingKey],
) -> Result<StagedRelease, ReleaseError> {
    let signature = Signature::try_from(signature)
        .map_err(|_| rejected("The DevWorks release signature has the wrong format."))?;
    if !keys
        .iter()
        .any(|key| key.verify_strict(bytes, &signature).is_ok())
    {
        return Err(rejected(
            "The DevWorks release signature was not made by a trusted release key.",
        ));
    }
    let manifest = serde_json::from_slice(bytes)
        .map_err(|_| rejected("The signed DevWorks release manifest is invalid."))?;
    Ok(StagedRelease {
        manifest,
        manifest_digest: sha256_hex(bytes),
    })
}

fn check_cancel(cancellation: &Cancellation) -> Result<(), ReleaseError> {
    if cancellation.is_cancelled() {
        Err(ReleaseError::cancelled())
    } else {
        Ok(())
    }
}

fn read_member(
    directory: &ArchiveDirectory,
    name: &str,
    limit: u64,
) -> Result<Vec<u8>, ReleaseError> {
    let mut reader = directory.open_member(name, limit).map_err(storage)?.reader;
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|_| rejected("The staged release record could not be read."))?;
    Ok(bytes)
}

struct ProgressRead<'a, R> {
    inner: R,
    progress: &'a (dyn Fn(u64, u64) + Sync),
    bytes: u64,
    total: u64,
}
impl<R: Read> Read for ProgressRead<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let len = self.inner.read(buffer)?;
        self.bytes += len as u64;
        (self.progress)(self.bytes, self.total);
        Ok(len)
    }
}

struct DigestRead<R> {
    inner: R,
    hash: Sha256,
    bytes: u64,
}
impl<R> DigestRead<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hash: Sha256::new(),
            bytes: 0,
        }
    }
    fn digest(self) -> FileDigest {
        FileDigest {
            sha256: self
                .hash
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            bytes: self.bytes,
        }
    }
}
impl<R: Read> Read for DigestRead<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let len = self.inner.read(buffer)?;
        self.hash.update(&buffer[..len]);
        self.bytes += len as u64;
        Ok(len)
    }
}

fn inspect_archive(
    stage: &ArchiveDirectory,
    manifest: &ReleaseManifest,
    destination: Option<&ArchiveDirectory>,
    cancellation: &Cancellation,
) -> Result<(), ReleaseError> {
    let mut archive = DigestRead::new(
        stage
            .open_member(ARCHIVE, manifest.archive.bytes)
            .map_err(storage)?
            .reader,
    );
    for item in &manifest.files {
        let name = &item.path;
        check_cancel(cancellation)?;
        let bytes = archive.by_ref().take(item.content.bytes);
        if let Some(root) = destination {
            let (parent, filename) = destination_parent(root, name)?;
            parent
                .write_member(filename, bytes, &item.content, cancellation)
                .map_err(storage)?;
            #[cfg(unix)]
            if item.executable {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(
                    parent.location().join(filename),
                    std::fs::Permissions::from_mode(0o700),
                )
                .map_err(|_| rejected("The staged executable could not be made runnable."))?;
                parent.flush().map_err(storage)?;
            }
        } else {
            let mut file = DigestRead::new(bytes);
            let mut block = [0u8; 64 * 1024];
            loop {
                check_cancel(cancellation)?;
                if file
                    .read(&mut block)
                    .map_err(|_| rejected("A staged update file could not be read."))?
                    == 0
                {
                    break;
                }
            }
            if file.digest() != item.content {
                return Err(rejected(
                    "A staged update file differs from its signed digest.",
                ));
            }
        }
    }
    if archive
        .read(&mut [0u8; 1])
        .map_err(|_| rejected("The staged update archive could not be read."))?
        != 0
        || archive.digest() != manifest.archive
    {
        return Err(rejected(
            "The staged update archive differs from its signed digest.",
        ));
    }
    Ok(())
}

fn destination_parent<'a>(
    root: &ArchiveDirectory,
    path: &'a str,
) -> Result<(ArchiveDirectory, &'a str), ReleaseError> {
    let (dirs, file) = path.rsplit_once('/').unwrap_or(("", path));
    let mut current =
        ArchiveDirectory::open(root.location(), Some(root.identity())).map_err(storage)?;
    for dir in dirs.split('/').filter(|dir| !dir.is_empty()) {
        current = match current.child(dir) {
            Ok(existing) => existing,
            Err(_) => current.create_folder(dir).map_err(storage)?,
        };
    }
    Ok((current, file))
}

fn rejected(message: &str) -> ReleaseError {
    ReleaseError::rejected(message)
}

fn storage(error: StorageError) -> ReleaseError {
    if matches!(error, StorageError::Cancelled) {
        ReleaseError::cancelled()
    } else {
        rejected(&format!(
            "The private update stage could not be verified or saved. {error}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devworks_core::ports::update::{InstallOwner, ReleaseFile};
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn signed_stage_materializes_exact_bytes_and_rechecks_every_read() {
        let base = devworks_test_support::TempRoot::new("verified-app-release").created();
        let profile = base.path();
        let key = SigningKey::from_bytes(&[31; 32]);
        let source = NativeReleaseSource::new(profile.to_owned(), vec![key.verifying_key()]);
        let contents: &[u8] = b"fixture executable";
        let hash = FileDigest {
            bytes: contents.len() as u64,
            sha256: sha256_hex(contents),
        };
        let target = "aarch64-apple-darwin";
        let manifest = ReleaseManifest {
            format: 1,
            product: "SceneWorks/DevWorks".into(),
            release_id: 10,
            tag: "v1.2.0".into(),
            build: "a".repeat(40),
            target: target.into(),
            owner: InstallOwner::MacApplication,
            archive: hash.clone(),
            archive_asset: asset_name(target, "bundle"),
            help_digest: "b".repeat(64),
            migrations: crate::storage::sqlite::update::shipped_witness(),
            files: vec![ReleaseFile {
                path: InstallOwner::MacApplication.entrypoint().into(),
                content: hash.clone(),
                executable: true,
            }],
            destructive_migration: false,
        };
        let encoded = serde_json::to_vec(&manifest).unwrap();
        let signature = key.sign(&encoded).to_bytes();
        let staged = verify_manifest(&encoded, &signature, &source.trust).unwrap();
        assert!(
            verify_manifest(
                &encoded,
                &signature,
                &[SigningKey::from_bytes(&[32; 32]).verifying_key()]
            )
            .is_err()
        );
        assert!(verify_manifest(&[b' '; 4], &signature, &source.trust).is_err());
        let cancel = Cancellation::default();
        let directory = source
            .staging()
            .unwrap()
            .create_folder(&staged.manifest_digest)
            .unwrap();
        for (name, bytes) in [
            (MANIFEST, encoded.as_slice()),
            (SIGNATURE, &signature[..]),
            (ARCHIVE, contents),
        ] {
            let digest = FileDigest {
                sha256: sha256_hex(bytes),
                bytes: bytes.len() as u64,
            };
            directory
                .write_member(name, bytes, &digest, &cancel)
                .unwrap();
        }
        source.verify(&staged, &cancel).unwrap();
        let path = profile.join("new-install");
        std::fs::create_dir(&path).unwrap();
        source.materialize(&staged, &path, &cancel).unwrap();
        assert_eq!(
            std::fs::read(path.join(manifest.owner.entrypoint())).unwrap(),
            contents
        );
        assert!(
            source.materialize(&staged, &path, &cancel).is_err(),
            "existing files are never overwritten"
        );
        cancel.cancel();
        assert!(source.verify(&staged, &cancel).unwrap_err().cancelled);
        std::fs::write(directory.location().join(ARCHIVE), b"corrupt executable").unwrap();
        assert!(source.verify(&staged, &Cancellation::default()).is_err());
    }
}
