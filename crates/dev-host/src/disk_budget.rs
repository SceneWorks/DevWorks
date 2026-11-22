//! On-disk admission ledger for builds sharing a volume. The platform adapter verifies
//! the host directory, samples the volume under the ledger lock, and proves when an
//! owner's writers have stopped.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, ErrorKind, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const LOCK: &str = "disk-budget.lock";
const STATE: &str = "disk-budget.json";
const STAGING: &str = "disk-budget.json.tmp";
const SCHEMA: u32 = 1;
const DIGEST_BYTES: usize = 32;

/// Host-provisioned identity and policy. Changing either requires a drained host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub volume: String,
    pub generation: String,
    pub minimum_free_bytes: u64,
}

/// Process identity verified by the caller. PID reuse cannot inherit a reservation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub pid: u32,
    pub creation_time: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reservation {
    pub id: String,
    pub owner: Owner,
    pub bytes: u64,
}

/// The caller must consider all writers, including descendants and post actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Writers {
    Active,
    Unknown,
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    Granted(Reservation),
    Wait {
        available_bytes: u64,
        required_bytes: u64,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    schema: u32,
    policy: Policy,
    reservations: Vec<Reservation>,
}

/// All cooperating processes must use the same administrator-provisioned directory
/// and policy. The adapter authenticates the directory and volume before using this type.
pub struct DiskBudget<F> {
    directory: PathBuf,
    policy: Policy,
    lock_timeout: Duration,
    verify_anchor: F,
}

impl<F: Fn(&File) -> io::Result<()>> DiskBudget<F> {
    /// `verify_anchor` checks the identity of the open lock file against the
    /// host's provisioned identity. Checking a pathname alone can split locking
    /// if the file is replaced while an earlier process still holds it open.
    pub fn new(
        directory: PathBuf,
        policy: Policy,
        lock_timeout: Duration,
        verify_anchor: F,
    ) -> Self {
        Self {
            directory,
            policy,
            lock_timeout,
            verify_anchor,
        }
    }

    /// Initialize a new ledger. The caller proves the host is drained first.
    ///
    /// A failed initialization can leave a lock file behind. Both normal admission and
    /// a later initialization refuse that incomplete state until an operator repairs it.
    /// The returned handle holds the lock so the caller can pin the file identity
    /// before publishing the host configuration and allowing builds.
    pub fn initialize(&self, verify_drained: impl FnOnce() -> io::Result<()>) -> io::Result<File> {
        self.check_directory()?;
        if regular_file(&self.directory.join(LOCK))?
            || regular_file(&self.directory.join(STATE))?
            || regular_file(&self.directory.join(STAGING))?
        {
            return Err(io::Error::new(
                ErrorKind::AlreadyExists,
                "disk budget state already exists",
            ));
        }
        verify_drained()?;
        let mut lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(self.directory.join(LOCK))?;
        self.wait_for_lock(&lock)?;
        self.save(
            &mut lock,
            &State {
                schema: SCHEMA,
                policy: self.policy.clone(),
                reservations: Vec::new(),
            },
        )?;
        Ok(lock)
    }

    /// Ensure this build has at least the requested reservation. Replays by the same
    /// owner reuse it. A request for more bytes is admitted as a single transaction.
    ///
    /// Charge each active reservation in full. The sample is the live free space,
    /// so disk usage outside the ledger is reflected in the decision too.
    pub fn reserve(
        &self,
        request: Reservation,
        available_bytes: impl FnOnce() -> io::Result<u64>,
    ) -> io::Result<Admission> {
        if !valid_reservation(&request) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "disk budget requires a reservation id, process identity and positive size",
            ));
        }
        let (mut lock, mut state) = self.locked_state()?;
        let existing = find_owner(&state, &request.id, &request.owner)?;
        if let Some(index) = existing
            && state.reservations[index].bytes >= request.bytes
        {
            return Ok(Admission::Granted(state.reservations[index].clone()));
        }

        let mut required_bytes = sum_with_floor(&state, existing)?;
        required_bytes = required_bytes
            .checked_add(request.bytes)
            .ok_or_else(|| invalid_data("disk budget required bytes overflow"))?;
        let available_bytes = available_bytes()?;
        if available_bytes < required_bytes {
            return Ok(Admission::Wait {
                available_bytes,
                required_bytes,
            });
        }

        match existing {
            Some(index) => state.reservations[index] = request.clone(),
            None => state.reservations.push(request.clone()),
        }
        self.save(&mut lock, &state)?;
        Ok(Admission::Granted(request))
    }

    /// Read verified reservations for the adapter to test process membership.
    pub fn reservations(&self) -> io::Result<Vec<Reservation>> {
        self.locked_state().map(|(_, state)| state.reservations)
    }

    /// Release an exact reservation once the caller proves its writers stopped.
    /// Returns false for an already released id. An unknown writer keeps its charge.
    pub fn release(
        &self,
        id: &str,
        owner: &Owner,
        writers: impl FnOnce(&Reservation) -> Writers,
    ) -> io::Result<bool> {
        let (mut lock, mut state) = self.locked_state()?;
        let Some(index) = find_owner(&state, id, owner)? else {
            return Ok(false);
        };
        if writers(&state.reservations[index]) != Writers::Stopped {
            return Err(io::Error::new(
                ErrorKind::WouldBlock,
                "disk budget reservation still has possible writers",
            ));
        }
        state.reservations.remove(index);
        self.save(&mut lock, &state)?;
        Ok(true)
    }

    /// Reclaim reservations whose writers the caller positively proves stopped.
    /// A missing owner process alone does not establish that its descendants stopped.
    pub fn reconcile(&self, mut writers: impl FnMut(&Reservation) -> Writers) -> io::Result<usize> {
        let (mut lock, mut state) = self.locked_state()?;
        let before = state.reservations.len();
        state
            .reservations
            .retain(|reservation| writers(reservation) != Writers::Stopped);
        let removed = before - state.reservations.len();
        if removed > 0 {
            self.save(&mut lock, &state)?;
        }
        Ok(removed)
    }

    fn check_directory(&self) -> io::Result<()> {
        if self.policy.volume.is_empty() || self.policy.generation.is_empty() {
            return Err(invalid_data("disk budget policy identity is incomplete"));
        }
        if !fs::symlink_metadata(&self.directory)?.is_dir() {
            return Err(invalid_data("disk budget requires an ordinary directory"));
        }
        Ok(())
    }

    fn locked_state(&self) -> io::Result<(File, State)> {
        self.check_directory()?;
        let lock_path = self.directory.join(LOCK);
        require_file(&lock_path)?;
        let mut lock = OpenOptions::new().read(true).write(true).open(&lock_path)?;
        self.wait_for_lock(&lock)?;
        (self.verify_anchor)(&lock)?;

        let path = self.directory.join(STATE);
        require_file(&path)?;
        let bytes = fs::read(path)?;
        let mut committed_digest = [0; DIGEST_BYTES];
        if lock.metadata()?.len() != DIGEST_BYTES as u64 {
            return Err(invalid_data("invalid disk budget commit marker"));
        }
        lock.rewind()?;
        lock.read_exact(&mut committed_digest)?;
        if Sha256::digest(&bytes)[..] != committed_digest {
            return Err(invalid_data(
                "disk budget state does not match the commit marker",
            ));
        }
        let state: State = serde_json::from_slice(&bytes)
            .map_err(|error| invalid_data(format!("invalid disk budget state: {error}")))?;
        if state.schema != SCHEMA || state.policy != self.policy {
            return Err(invalid_data(
                "disk budget schema, volume, generation or free-space policy does not match",
            ));
        }
        let mut ids = std::collections::BTreeSet::new();
        for reservation in &state.reservations {
            if !valid_reservation(reservation) || !ids.insert(&reservation.id) {
                return Err(invalid_data("invalid or duplicate disk budget reservation"));
            }
        }
        sum_with_floor(&state, None)?;
        Ok((lock, state))
    }

    fn wait_for_lock(&self, lock: &File) -> io::Result<()> {
        let start = Instant::now();
        loop {
            match lock.try_lock() {
                Ok(()) => return Ok(()),
                Err(TryLockError::Error(error)) => return Err(error),
                Err(TryLockError::WouldBlock) => {
                    let remaining = self.lock_timeout.saturating_sub(start.elapsed());
                    if remaining.is_zero() {
                        return Err(io::Error::new(
                            ErrorKind::TimedOut,
                            "timed out acquiring the disk budget lock",
                        ));
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(10)));
                }
            }
        }
    }

    fn save(&self, lock: &mut File, state: &State) -> io::Result<()> {
        let staged = self.directory.join(STAGING);
        if regular_file(&staged)? {
            // A prior interrupted commit may leave its staging entry. Removing it
            // instead of truncating it avoids writing through a hard-linked file.
            fs::remove_file(&staged)?;
        }
        let bytes = serde_json::to_vec(state)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);

        // Publish the digest through the stable lock handle before replacing the state.
        // A crash in between leaves a mismatch that requires drained-host repair.
        lock.rewind()?;
        lock.write_all(&Sha256::digest(&bytes))?;
        lock.sync_all()?;
        fs::rename(staged, self.directory.join(STATE))
    }
}

fn valid_reservation(reservation: &Reservation) -> bool {
    !reservation.id.is_empty()
        && reservation.owner.pid != 0
        && reservation.owner.creation_time != 0
        && reservation.bytes != 0
}

fn sum_with_floor(state: &State, except: Option<usize>) -> io::Result<u64> {
    state
        .reservations
        .iter()
        .enumerate()
        .filter(|(index, _)| Some(*index) != except)
        .try_fold(state.policy.minimum_free_bytes, |sum, (_, entry)| {
            sum.checked_add(entry.bytes)
                .ok_or_else(|| invalid_data("disk budget reservation total overflow"))
        })
}

fn find_owner(state: &State, id: &str, owner: &Owner) -> io::Result<Option<usize>> {
    let Some(index) = state.reservations.iter().position(|entry| entry.id == id) else {
        return Ok(None);
    };
    if &state.reservations[index].owner != owner {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "disk budget reservation belongs to another process identity",
        ));
    }
    Ok(Some(index))
}

fn regular_file(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(invalid_data(format!(
            "disk budget requires an ordinary file at {}",
            path.display()
        ))),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn require_file(path: &Path) -> io::Result<()> {
    if regular_file(path)? {
        return Ok(());
    }
    Err(io::Error::new(
        ErrorKind::NotFound,
        format!("disk budget metadata is missing at {}", path.display()),
    ))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;

    fn budget(path: &Path) -> DiskBudget<impl Fn(&File) -> io::Result<()>> {
        DiskBudget::new(
            path.to_owned(),
            Policy {
                volume: "test-volume".into(),
                generation: "bootstrap-1".into(),
                minimum_free_bytes: 20,
            },
            Duration::from_secs(5),
            |_| Ok(()),
        )
    }

    fn initialized(scratch: &Scratch) -> DiskBudget<impl Fn(&File) -> io::Result<()>> {
        let budget = budget(scratch.path());
        budget.initialize(|| Ok(())).unwrap();
        budget
    }

    fn build(id: &str, pid: u32, bytes: u64) -> Reservation {
        Reservation {
            id: id.into(),
            owner: Owner {
                pid,
                creation_time: 1,
            },
            bytes,
        }
    }

    #[test]
    fn parallel_builds_fit_and_a_larger_stage_waits() {
        let scratch = Scratch::new("disk-parallel");
        let budget = initialized(&scratch);
        let first = build("ci", 10, 50);
        let second = build("local", 11, 30);
        assert_eq!(
            budget.reserve(first.clone(), || Ok(100)).unwrap(),
            Admission::Granted(first.clone())
        );
        assert!(matches!(
            budget.reserve(second.clone(), || Ok(100)).unwrap(),
            Admission::Granted(_)
        ));
        let bigger = build("ci", 10, 60);
        assert_eq!(
            budget.reserve(bigger.clone(), || Ok(100)).unwrap(),
            Admission::Wait {
                available_bytes: 100,
                required_bytes: 110
            }
        );
        // The rejected top-up left the first reservation at its previous size.
        assert_eq!(
            budget.reserve(first.clone(), || unreachable!()).unwrap(),
            Admission::Granted(first)
        );
        assert!(
            budget
                .release(&second.id, &second.owner, |_| Writers::Stopped)
                .unwrap()
        );
        assert_eq!(
            budget.reserve(bigger.clone(), || Ok(100)).unwrap(),
            Admission::Granted(bigger)
        );
    }

    #[test]
    fn replay_does_not_shrink_and_pid_reuse_cannot_release() {
        let scratch = Scratch::new("disk-owner");
        let budget = initialized(&scratch);
        let first = build("ci", 10, 70);
        budget.reserve(first.clone(), || Ok(100)).unwrap();
        let mut stale = build("ci", 10, 1);
        assert_eq!(
            budget.reserve(stale.clone(), || unreachable!()).unwrap(),
            Admission::Granted(first.clone())
        );
        stale.owner.creation_time += 1;
        assert_eq!(
            budget
                .reserve(stale.clone(), || unreachable!())
                .unwrap_err()
                .kind(),
            ErrorKind::PermissionDenied
        );
        assert_eq!(
            budget
                .release(&stale.id, &stale.owner, |_| unreachable!())
                .unwrap_err()
                .kind(),
            ErrorKind::PermissionDenied
        );
        assert!(
            budget
                .release(&first.id, &first.owner, |_| Writers::Stopped)
                .unwrap()
        );
        assert!(
            !budget
                .release(&first.id, &first.owner, |_| unreachable!())
                .unwrap()
        );
    }

    #[test]
    fn unknown_and_live_writers_keep_their_bytes_charged() {
        let scratch = Scratch::new("disk-writers");
        let budget = initialized(&scratch);
        let active = build("active", 10, 40);
        let unknown = build("unknown", 11, 30);
        budget.reserve(active, || Ok(100)).unwrap();
        budget.reserve(unknown.clone(), || Ok(100)).unwrap();
        assert_eq!(budget.reconcile(|_| Writers::Unknown).unwrap(), 0);
        assert_eq!(
            budget
                .release(&unknown.id, &unknown.owner, |_| Writers::Unknown)
                .unwrap_err()
                .kind(),
            ErrorKind::WouldBlock
        );
        let waiting = build("waiting", 12, 30);
        assert_eq!(
            budget.reserve(waiting.clone(), || Ok(100)).unwrap(),
            Admission::Wait {
                available_bytes: 100,
                required_bytes: 120,
            }
        );
        assert_eq!(
            budget
                .reconcile(|entry| {
                    if entry.id == unknown.id {
                        Writers::Stopped
                    } else {
                        Writers::Active
                    }
                })
                .unwrap(),
            1
        );
        assert_eq!(
            budget.reserve(waiting.clone(), || Ok(100)).unwrap(),
            Admission::Granted(waiting)
        );
    }

    #[test]
    fn initialization_requires_a_drained_host_and_runtime_never_bootstraps() {
        let scratch = Scratch::new("disk-missing");
        let budget = budget(scratch.path());
        let request = build("ci", 10, 10);
        assert_eq!(
            budget
                .reserve(request.clone(), || unreachable!())
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound
        );
        assert!(fs::read_dir(scratch.path()).unwrap().next().is_none());
        assert!(
            budget
                .initialize(|| Err(io::Error::other("live build")))
                .is_err()
        );
        assert!(fs::read_dir(scratch.path()).unwrap().next().is_none());
        budget.initialize(|| Ok(())).unwrap();

        // Losing metadata with a lock file present cannot create an empty ledger.
        fs::remove_file(scratch.path().join(STATE)).unwrap();
        assert_eq!(
            budget
                .reserve(request, || unreachable!())
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound
        );
        assert_eq!(
            budget.initialize(|| unreachable!()).unwrap_err().kind(),
            ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn foreign_or_corrupt_state_never_grants_capacity() {
        let scratch = Scratch::new("disk-state");
        let original = initialized(&scratch);
        let request = build("ci", 10, 10);
        for policy in [
            Policy {
                volume: "other-volume".into(),
                ..original.policy.clone()
            },
            Policy {
                generation: "other-generation".into(),
                ..original.policy.clone()
            },
            Policy {
                minimum_free_bytes: 0,
                ..original.policy.clone()
            },
        ] {
            let foreign = DiskBudget::new(
                scratch.path().to_owned(),
                policy,
                original.lock_timeout,
                |_| Ok(()),
            );
            assert_eq!(
                foreign
                    .reserve(request.clone(), || unreachable!())
                    .unwrap_err()
                    .kind(),
                ErrorKind::InvalidData
            );
        }

        for invalid in ["{}", "bad json"] {
            fs::write(scratch.path().join(STATE), invalid).unwrap();
            assert_eq!(
                original
                    .reserve(request.clone(), || unreachable!())
                    .unwrap_err()
                    .kind(),
                ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn replaying_a_same_generation_state_keeps_admission_closed() {
        let scratch = Scratch::new("disk-replay");
        let budget = initialized(&scratch);
        let state_path = scratch.path().join(STATE);
        let stale = fs::read(&state_path).unwrap();
        budget.reserve(build("active", 10, 50), || Ok(100)).unwrap();
        fs::write(&state_path, stale).unwrap();

        let error = budget
            .reserve(build("waiting", 11, 50), || unreachable!())
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert!(error.to_string().contains("commit marker"));
    }

    #[test]
    fn foreign_lock_handle_keeps_admission_closed() {
        let scratch = Scratch::new("disk-anchor");
        let budget = initialized(&scratch);
        let untrusted_anchor = DiskBudget::new(
            scratch.path().to_owned(),
            budget.policy,
            budget.lock_timeout,
            |_| {
                Err(io::Error::new(
                    ErrorKind::PermissionDenied,
                    "replaced lock file",
                ))
            },
        );
        assert_eq!(
            untrusted_anchor
                .reserve(build("waiting", 11, 50), || unreachable!())
                .unwrap_err()
                .kind(),
            ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn failed_sample_or_stage_write_does_not_change_the_ledger() {
        let scratch = Scratch::new("disk-write");
        let budget = initialized(&scratch);
        let active = build("active", 10, 50);
        budget.reserve(active.clone(), || Ok(100)).unwrap();
        let original = fs::read(scratch.path().join(STATE)).unwrap();

        let second = build("second", 11, 30);
        assert!(
            budget
                .reserve(second.clone(), || Err(io::Error::other(
                    "volume query failed"
                )))
                .is_err()
        );
        fs::create_dir(scratch.path().join(STAGING)).unwrap();
        assert!(budget.reserve(second, || Ok(100)).is_err());
        assert!(
            budget
                .release(&active.id, &active.owner, |_| Writers::Stopped)
                .is_err()
        );
        assert_eq!(fs::read(scratch.path().join(STATE)).unwrap(), original);
    }

    #[test]
    fn stale_staging_entry_cannot_truncate_a_hard_link() {
        let scratch = Scratch::new("disk-stage-link");
        let budget = initialized(&scratch);
        let retained = scratch.path().join("retained");
        fs::write(&retained, "retain this file").unwrap();
        fs::hard_link(&retained, scratch.path().join(STAGING)).unwrap();

        assert!(matches!(
            budget.reserve(build("ci", 10, 10), || Ok(100)).unwrap(),
            Admission::Granted(_)
        ));
        assert_eq!(fs::read_to_string(retained).unwrap(), "retain this file");
    }

    #[test]
    fn locked_ledger_has_a_bounded_wait() {
        let scratch = Scratch::new("disk-lock");
        let mut budget = initialized(&scratch);
        budget.lock_timeout = Duration::ZERO;
        let held = OpenOptions::new()
            .write(true)
            .open(scratch.path().join(LOCK))
            .unwrap();
        held.lock().unwrap();
        assert_eq!(
            budget
                .reserve(build("ci", 10, 10), || unreachable!())
                .unwrap_err()
                .kind(),
            ErrorKind::TimedOut
        );
    }

    #[test]
    fn separate_processes_cannot_charge_the_same_bytes() {
        use std::process::Command;
        const WORKER: &str = "disk_budget::tests::subprocess_reservation";
        let scratch = Scratch::new("disk-processes");
        let budget = initialized(&scratch);
        let mut children = Vec::new();

        for id in ["first", "second"] {
            children.push(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", WORKER])
                    .env("DEV_HOST_DISK_TEST_DIRECTORY", scratch.path())
                    .env("DEV_HOST_DISK_TEST_ID", id)
                    .spawn()
                    .unwrap(),
            );
        }
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }
        let (_lock, state) = budget.locked_state().unwrap();
        assert_eq!(state.reservations.len(), 1);
        assert_eq!(state.reservations[0].bytes, 50);
    }

    #[test]
    fn subprocess_reservation() {
        let Some(dir) = std::env::var_os("DEV_HOST_DISK_TEST_DIRECTORY") else {
            return;
        };
        let id = std::env::var("DEV_HOST_DISK_TEST_ID").unwrap();
        budget(Path::new(&dir))
            .reserve(build(&id, std::process::id(), 50), || Ok(100))
            .unwrap();
    }
}
