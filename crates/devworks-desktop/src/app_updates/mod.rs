//! Installed-owner boundaries and the durable native replacement journal.
mod exchange;
mod startup;

pub use startup::{Candidate, enter_application, run_internal};

use devworks_adapters::app_updates::{NativeReleaseSource, random_token};
use devworks_core::fingerprint::{is_lower_hex, is_sha256_hex};
use devworks_core::ports::storage::Cancellation;
use devworks_core::ports::update::{InstallApproval, InstallOwner, RunningApplication};
use devworks_core::services::update::InstallOutcome;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const JOURNAL: &str = "journal.json";
const MAX_JOURNAL_BYTES: u64 = 3 * 1024 * 1024;
const BUILD: Option<&str> = option_env!("DEVWORKS_RELEASE_BUILD");
const TRUST: Option<&str> = option_env!("DEVWORKS_RELEASE_PUBLIC_KEYS");

/// A build declares release identity at compile time. Runtime settings and release assets
/// cannot add a key or declare themselves the application owner.
pub fn source(profile: &Path) -> NativeReleaseSource {
    let keys = TRUST
        .and_then(|input| input.split(',').map(decode_key).collect::<Option<Vec<_>>>())
        .unwrap_or_default();
    NativeReleaseSource::new(profile.to_owned(), keys)
}

fn decode_key(value: &str) -> Option<ed25519_dalek::VerifyingKey> {
    if !is_lower_hex(value, 64) {
        return None;
    }
    let mut raw = [0u8; 32];
    for (at, byte) in raw.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[at * 2..][..2], 16).ok()?;
    }
    ed25519_dalek::VerifyingKey::from_bytes(&raw)
        .ok()
        .filter(|key| !key.is_weak())
}

pub fn running() -> RunningApplication {
    RunningApplication {
        version: semver::Version::parse(crate::VERSION).expect("Cargo version is semantic"),
        build: BUILD.unwrap_or("development").to_owned(),
        target: devworks_adapters::packages::host_target_triple(),
        migrations: devworks_adapters::storage::sqlite::update::shipped_witness(),
        installed_binary_digest: Installation::locate()
            .and_then(|installation| installation.verify_running())
            .and_then(|path| digest_executable(&path))
            .ok(),
    }
}

pub fn release_evidence() -> serde_json::Value {
    serde_json::json!({
        "version": crate::VERSION,
        "build": BUILD,
        "target": devworks_adapters::packages::host_target_triple(),
        "helpDigest": bundled_help_digest(),
        "migrations": devworks_adapters::storage::sqlite::update::shipped_witness(),
    })
}

pub fn bundled_help_digest() -> String {
    devworks_core::services::help::bundled_release_digest()
}

#[derive(Clone, Debug)]
struct Installation {
    owner: InstallOwner,
    root: PathBuf,
    control: PathBuf,
}

impl Installation {
    fn locate() -> Result<Self, String> {
        if !BUILD.is_some_and(|build| is_lower_hex(build, 40))
            || !TRUST.is_some_and(|keys| {
                !keys.is_empty() && keys.split(',').all(|key| decode_key(key).is_some())
            })
        {
            return Err("This build has no provisioned app release identity.".into());
        }
        let owner = if cfg!(target_os = "macos") {
            if !option_env!("DEVWORKS_SIGNING_TEAM").is_some_and(|team| {
                team.len() == 10
                    && team
                        .bytes()
                        .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
            }) {
                return Err("This macOS build has no qualified signing owner.".into());
            }
            InstallOwner::MacApplication
        } else if cfg!(windows) {
            if !option_env!("DEVWORKS_SIGNING_CERT_SHA256").is_some_and(is_sha256_hex) {
                return Err("This Windows build has no qualified signing owner.".into());
            }
            InstallOwner::WindowsPerUser
        } else if cfg!(target_os = "linux") {
            InstallOwner::LinuxStandalone
        } else {
            return Err("This platform's installation owner is unqualified.".into());
        };
        if !owner.matches_target(&devworks_adapters::packages::host_target_triple()) {
            return Err("This build target has no qualified installer.".into());
        }
        let user = |name| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .ok_or_else(|| "The per-user installation root is unavailable.".to_owned())
        };
        let path = match owner {
            InstallOwner::MacApplication => user("HOME")?.join("Applications/DevWorks.app"),
            InstallOwner::WindowsPerUser => user("LOCALAPPDATA")?.join("Programs/DevWorks"),
            InstallOwner::LinuxStandalone => user("HOME")?.join(".local/opt/devworks"),
        };
        Self::at(owner, &path)
    }

    fn at(owner: InstallOwner, path: &Path) -> Result<Self, String> {
        let root = path
            .canonicalize()
            .map_err(|_| "The managed per-user app installation is unavailable.")?;
        if (!cfg!(windows) && root != path) || !private_to_owner(path, false)? {
            return Err("The installation does not have a verified user-owned path.".into());
        }
        let parent = root
            .parent()
            .ok_or("The installation has no enclosing folder.")?;
        let name = root
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("The installation has no valid name.")?;
        if !private_to_owner(parent, false)? {
            return Err("The installation's enclosing folder is writable by another owner.".into());
        }
        let control = parent.join(format!(".{name}-updates"));
        match create_private_dir(&control) {
            Ok(()) => sync_directory(parent)?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(native_error(error)),
        }
        if !private_to_owner(&control, true)? {
            return Err("The installer journal folder is not private.".into());
        }
        Ok(Self {
            owner,
            root,
            control,
        })
    }

    fn canonical_entry(&self) -> Result<PathBuf, String> {
        if self.owner != InstallOwner::WindowsPerUser {
            return Ok(self.root.join(self.owner.entrypoint()));
        }
        let version = read_small(&self.root.join("current"), 64)?;
        let version = std::str::from_utf8(&version)
            .map_err(|_| "The installed app version pointer is invalid.")?;
        if !is_sha256_hex(version) {
            return Err("The installed app version pointer is invalid.".into());
        }
        Ok(self
            .root
            .join("versions")
            .join(version)
            .join(self.owner.entrypoint()))
    }

    fn verify_running(&self) -> Result<PathBuf, String> {
        let entry = self
            .canonical_entry()?
            .canonicalize()
            .map_err(native_error)?;
        if std::env::current_exe()
            .and_then(|path| path.canonicalize())
            .map_err(native_error)?
            != entry
        {
            return Err("The running executable is outside the managed installation.".into());
        }
        Ok(entry)
    }

    fn gate(&self, exclusive: bool) -> Result<File, TryLockError> {
        let lock =
            lock_file(&self.control.join("installation.lock")).map_err(TryLockError::Error)?;
        if exclusive {
            lock.try_lock()?;
        } else {
            lock.try_lock_shared()?;
        }
        Ok(lock)
    }

    fn prepared_root(&self, transaction: &str) -> PathBuf {
        let suffix = if self.owner == InstallOwner::MacApplication {
            ".app"
        } else {
            ""
        };
        self.control
            .join(format!("candidate-{transaction}{suffix}"))
    }

    fn journal(&self) -> Result<Option<Journal>, String> {
        match fs::symlink_metadata(self.control.join(JOURNAL)) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(native_error(error)),
        }
        let bytes = read_small(&self.control.join(JOURNAL), MAX_JOURNAL_BYTES)?;
        let journal: Journal =
            serde_json::from_slice(&bytes).map_err(|_| "The installer journal is unreadable.")?;
        if journal.format != 1
            || !is_lower_hex(&journal.transaction, 32)
            || !is_sha256_hex(&journal.old_digest)
            || !is_sha256_hex(&journal.approval.stage.manifest_digest)
            || journal.approval.stage.manifest.owner != self.owner
            || !journal.profile.is_absolute()
        {
            return Err("The installer journal has an unrecognized approval or owner.".into());
        }
        Ok(Some(journal))
    }

    fn save(&self, journal: &Journal) -> Result<(), String> {
        let bytes = serde_json::to_vec(journal)
            .map_err(|_| "The installer journal could not be encoded.")?;
        if bytes.len() as u64 > MAX_JOURNAL_BYTES {
            return Err("The installer journal exceeds its size limit.".into());
        }
        self.replace_private(&self.control, JOURNAL, &bytes)
    }

    fn replace_private(&self, parent: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
        let token = random_token().map_err(|error| error.message)?;
        let path = parent.join(format!("pending-{token}"));
        let mut file = new_private_file(&path).map_err(native_error)?;
        let result = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file);
        let result = result.and_then(|()| exchange::replace(&path, &parent.join(name)));
        if result.is_err() {
            let _ = fs::remove_file(&path);
        }
        result.map_err(native_error)?;
        sync_directory(parent)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Stage {
    ApprovedShutdown,
    ReplaceIntent,
    LaunchIntent,
    StoreStartupIntent,
    Healthy,
    RolledBack,
    Unchanged,
    RecoveryRequired,
}

impl Stage {
    fn outcome(self) -> Option<InstallOutcome> {
        match self {
            Self::Healthy => Some(InstallOutcome::Installed),
            Self::RolledBack => Some(InstallOutcome::Failed),
            Self::Unchanged => Some(InstallOutcome::NotStarted),
            Self::RecoveryRequired => Some(InstallOutcome::Uncertain),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    format: u16,
    transaction: String,
    approval: InstallApproval,
    profile: PathBuf,
    parent: u32,
    parent_birth: String,
    old_pointer: String,
    old_digest: String,
    stage: Stage,
    detail: String,
}

impl Journal {
    fn new(
        profile: PathBuf,
        approval: InstallApproval,
        old_digest: String,
        old_pointer: String,
    ) -> Result<Self, String> {
        use devworks_core::ports::process::ProcessHost;
        let parent = devworks_adapters::process::NativeProcessHost
            .current_identity()
            .ok_or("The installed process identity could not be observed.")?;
        Ok(Self {
            format: 1,
            transaction: random_token().map_err(|error| error.message)?,
            approval,
            profile,
            parent: parent.pid,
            parent_birth: parent.birth,
            old_pointer,
            old_digest,
            stage: Stage::ApprovedShutdown,
            detail: "DevWorks verified that work was held for Continue.".into(),
        })
    }

    fn record(&mut self, install: &Installation, stage: Stage, detail: &str) -> Result<(), String> {
        let mut next = self.clone();
        next.stage = stage;
        next.detail = detail.to_owned();
        install.save(&next)?;
        *self = next;
        Ok(())
    }
}

pub fn launch_helper(profile: &Path, approval: InstallApproval) -> Result<(), String> {
    let install = Installation::locate()?;
    let entry = install.verify_running()?;
    if BUILD != Some(&approval.old_build) || crate::VERSION != approval.old_version {
        return Err("The installed application build changed since approval.".into());
    }
    let profile = profile.canonicalize().map_err(native_error)?;
    let old_digest = digest_executable(&entry)?;
    if approval.old_binary_digest != old_digest {
        return Err("The installed executable changed since approval.".into());
    }
    let old_pointer = if install.owner == InstallOwner::WindowsPerUser {
        String::from_utf8(read_small(&install.root.join("current"), 64)?)
            .map_err(|_| "The installed Windows app pointer is invalid.")?
    } else {
        String::new()
    };
    let journal = Journal::new(profile, approval, old_digest, old_pointer)?;
    if install.journal()?.is_some_and(|other| {
        other.stage.outcome().is_none() || other.stage == Stage::RecoveryRequired
    }) {
        return Err("Resolve the previous installation before starting a new update.".into());
    }
    startup::spawn_helper(&install, &journal, &entry)
}

pub fn previous_outcome(
    profile: &Path,
    approval: &InstallApproval,
) -> Result<(InstallOutcome, String), String> {
    let install = Installation::locate()?;
    let Some(journal) = install.journal()? else {
        return Ok((
            InstallOutcome::NotStarted,
            "The approved update did not reach installation.".into(),
        ));
    };
    if profile.canonicalize().map_err(native_error)? != journal.profile
        || &journal.approval != approval
    {
        return Ok((
            InstallOutcome::NotStarted,
            "The approved update did not reach installation.".into(),
        ));
    }
    if let Some(outcome) = journal.stage.outcome() {
        let expected = if journal.stage == Stage::Healthy {
            &approval.stage.manifest.entrypoint().content.sha256
        } else {
            &approval.old_binary_digest
        };
        if journal.stage != Stage::RecoveryRequired
            && digest_executable(&install.canonical_entry()?)? != *expected
        {
            return Ok((
                InstallOutcome::Uncertain,
                "The installed app no longer matches its last update outcome.".into(),
            ));
        }
        Ok((outcome, journal.detail))
    } else {
        Ok((
            InstallOutcome::Uncertain,
            "The interrupted installation requires recovery.".into(),
        ))
    }
}

fn digest_executable(path: &Path) -> Result<String, String> {
    digest_file(path).map(|value| value.sha256)
}

fn digest_file(path: &Path) -> Result<devworks_core::ports::storage::FileDigest, String> {
    // The installation is owned, final names are no-follow, and the install-wide gate fences
    // other DevWorks processes. Archive extraction verifies signed bytes on different handles.
    let file = open_regular(path)?;
    devworks_adapters::app_updates::digest_owned_file(file, &Cancellation::default())
        .map_err(|value| value.message)
}

fn lock_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    private_options(&mut options);
    let file = options.open(path)?;
    if !regular(&file.metadata()?) {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(file)
}

fn open_regular(path: &Path) -> Result<File, String> {
    if !regular(&fs::symlink_metadata(path).map_err(native_error)?) {
        return Err("An installation member is not a regular file.".into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    private_options(&mut options);
    let file = options.open(path).map_err(native_error)?;
    if !regular(&file.metadata().map_err(native_error)?) {
        return Err("An installation member is not a regular file.".into());
    }
    Ok(file)
}

fn regular(meta: &fs::Metadata) -> bool {
    if !meta.is_file() || meta.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return false;
        }
    }
    true
}

fn read_small(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let mut file = open_regular(path)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(native_error)?;
    if bytes.len() as u64 > limit {
        return Err("An installer control record exceeds its size limit.".into());
    }
    Ok(bytes)
}

fn new_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    private_options(&mut options);
    options.open(path)
}

fn private_options(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut build = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        build.mode(0o700);
    }
    build.create(path)
}

fn private_to_owner(path: &Path, private: bool) -> Result<bool, String> {
    let info = path.symlink_metadata().map_err(native_error)?;
    if !info.is_dir() || info.file_type().is_symlink() {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: no pointer arguments and no mutation.
        let me = unsafe { libc::geteuid() };
        let mask = if private { 0o077 } else { 0o022 };
        Ok(info.uid() == me && info.mode() & mask == 0)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let _ = private;
        // The owner is the current user's native LocalAppData path. Never admit reparse points.
        Ok(info.file_attributes() & 0x400 == 0)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = private;
        Ok(false)
    }
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(native_error)?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn native_error(error: io::Error) -> String {
    format!("The installed application files could not be accessed safely. {error}")
}
