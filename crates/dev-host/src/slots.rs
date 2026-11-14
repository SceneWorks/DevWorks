//! Sticky build slots under `build/<project>/<key>/`.
//!
//! Each worktree path gets one slot per build key, recorded in `slots.json` beside the slots
//! and guarded by an exclusive lock on `slots.lock`. The same worktree gets the same slot on
//! every call; a second worktree gets a different one. When all `cap` slots are taken, the
//! least recently used slot whose cargo lock is free is reassigned. Every assignment rewrites
//! the slot's stamp file, which the cleanup job reads for age.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const REGISTRY_FILE: &str = "slots.json";
pub const REGISTRY_LOCK: &str = "slots.lock";
/// Touched in the slot on every assignment; its mtime is the slot's last use.
pub const STAMP_FILE: &str = ".dev-host-stamp";
/// The lock file cargo holds while it builds into a profile folder.
pub const CARGO_LOCK: &str = ".cargo-lock";

#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    slots: Vec<Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    slot: u32,
    worktree: String,
    /// Unix seconds of the last assignment.
    last_used: u64,
}

/// Decides whether a build is running in a slot.
pub trait BusyProbe {
    fn is_busy(&self, slot_dir: &Path) -> bool;
}

/// Looks for cargo's `.cargo-lock` files (`<slot>/<profile>/` and `<slot>/<triple>/<profile>/`)
/// and reports the slot busy when any is held, using a non-blocking exclusive lock attempt.
pub struct CargoLockProbe;

impl BusyProbe for CargoLockProbe {
    fn is_busy(&self, slot_dir: &Path) -> bool {
        lock_files(slot_dir, 3).iter().any(|path| {
            let Ok(file) = File::open(path) else {
                return false;
            };
            matches!(file.try_lock(), Err(TryLockError::WouldBlock))
        })
    }
}

fn lock_files(dir: &Path, depth: u32) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_file() && entry.file_name() == CARGO_LOCK {
            out.push(path);
        } else if kind.is_dir() && depth > 1 {
            out.extend(lock_files(&path, depth - 1));
        }
    }
    out
}

pub fn slot_dir(key_dir: &Path, slot: u32) -> PathBuf {
    key_dir.join(format!("slot-{slot}"))
}

/// Assigns `worktree` its slot under `key_dir` (created if needed) and returns the slot folder.
pub fn assign(
    key_dir: &Path,
    worktree: &str,
    cap: u32,
    now: u64,
    busy: &dyn BusyProbe,
) -> Result<PathBuf, String> {
    let io = |what: &str, path: &Path, e: std::io::Error| format!("{what} {}: {e}", path.display());
    std::fs::create_dir_all(key_dir).map_err(|e| io("cannot create", key_dir, e))?;
    let lock_path = key_dir.join(REGISTRY_LOCK);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| io("cannot open", &lock_path, e))?;
    lock.lock().map_err(|e| io("cannot lock", &lock_path, e))?;

    let registry_path = key_dir.join(REGISTRY_FILE);
    let mut registry: Registry = match std::fs::read_to_string(&registry_path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            format!(
                "{} is not a valid slot registry ({e}); remove it to reset assignments",
                registry_path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Registry::default(),
        Err(e) => return Err(io("cannot read", &registry_path, e)),
    };

    let slot = choose(&mut registry, key_dir, worktree, cap, now, busy)?;

    let tmp = key_dir.join(format!("{REGISTRY_FILE}.tmp"));
    let text = serde_json::to_string_pretty(&registry).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| io("cannot write", &tmp, e))?;
    std::fs::rename(&tmp, &registry_path).map_err(|e| io("cannot replace", &registry_path, e))?;

    let dir = slot_dir(key_dir, slot);
    std::fs::create_dir_all(&dir).map_err(|e| io("cannot create", &dir, e))?;
    let stamp = dir.join(STAMP_FILE);
    std::fs::write(&stamp, format!("{worktree}\n{now}\n"))
        .map_err(|e| io("cannot write", &stamp, e))?;
    drop(lock);
    Ok(dir)
}

fn choose(
    registry: &mut Registry,
    key_dir: &Path,
    worktree: &str,
    cap: u32,
    now: u64,
    busy: &dyn BusyProbe,
) -> Result<u32, String> {
    // A lowered cap applies to existing assignments too: slots at or above it are dropped from
    // the registry, so they are neither kept nor handed out. Their folders age out in cleanup.
    registry.slots.retain(|e| e.slot < cap);
    if let Some(entry) = registry.slots.iter_mut().find(|e| e.worktree == worktree) {
        entry.last_used = now;
        return Ok(entry.slot);
    }
    if let Some(free) = (0..cap).find(|n| registry.slots.iter().all(|e| e.slot != *n)) {
        registry.slots.push(Entry {
            slot: free,
            worktree: worktree.to_owned(),
            last_used: now,
        });
        return Ok(free);
    }
    let mut by_age: Vec<&mut Entry> = registry.slots.iter_mut().collect();
    by_age.sort_by_key(|e| (e.last_used, e.slot));
    for entry in by_age {
        if !busy.is_busy(&slot_dir(key_dir, entry.slot)) {
            entry.worktree = worktree.to_owned();
            entry.last_used = now;
            return Ok(entry.slot);
        }
    }
    Err(format!(
        "all {cap} build slots under {} are building right now; wait for one to finish or \
         raise slots_per_key",
        key_dir.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;

    struct Busy(Vec<u32>);
    impl BusyProbe for Busy {
        fn is_busy(&self, slot_dir: &Path) -> bool {
            self.0
                .iter()
                .any(|n| slot_dir.ends_with(format!("slot-{n}")))
        }
    }

    #[test]
    fn same_worktree_is_sticky_and_others_differ() {
        let scratch = Scratch::new("sticky");
        let key = scratch.path().join("key");
        let a1 = assign(&key, "/w/a", 8, 10, &Busy(vec![])).unwrap();
        let a2 = assign(&key, "/w/a", 8, 20, &Busy(vec![])).unwrap();
        let b = assign(&key, "/w/b", 8, 30, &Busy(vec![])).unwrap();
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
        assert!(a1.join(STAMP_FILE).is_file());
        assert!(b.join(STAMP_FILE).is_file());
    }

    #[test]
    fn full_cap_reassigns_least_recently_used_idle_slot() {
        let scratch = Scratch::new("lru");
        let key = scratch.path().join("key");
        let a = assign(&key, "/w/a", 2, 10, &Busy(vec![])).unwrap();
        let b = assign(&key, "/w/b", 2, 20, &Busy(vec![])).unwrap();
        // a is oldest and idle: c takes it.
        let c = assign(&key, "/w/c", 2, 30, &Busy(vec![])).unwrap();
        assert_eq!(c, a);
        // b is now oldest but busy, c is busy too: nothing to take.
        let err = assign(&key, "/w/d", 2, 40, &Busy(vec![0, 1])).unwrap_err();
        assert!(err.contains("building right now"), "{err}");
        // b is oldest; a busy c leaves b to d.
        let d = assign(&key, "/w/d", 2, 50, &Busy(vec![0])).unwrap();
        assert_eq!(d, b);
        // a lost its slot and gets a reassigned one next time.
        let a_again = assign(&key, "/w/a", 2, 60, &Busy(vec![])).unwrap();
        assert_eq!(a_again, c);
    }

    #[test]
    fn lowered_cap_applies_to_existing_assignments() {
        let scratch = Scratch::new("lowered");
        let key = scratch.path().join("key");
        for (n, wt) in ["/w/0", "/w/1", "/w/2", "/w/3"].iter().enumerate() {
            let dir = assign(&key, wt, 4, 10 + n as u64, &Busy(vec![])).unwrap();
            assert_eq!(dir, slot_dir(&key, n as u32));
        }
        // With the cap lowered to 2, the worktree holding slot-3 moves into range: slot-3 is
        // neither kept sticky nor counted for reassignment, so the oldest idle slot-0 is taken.
        let moved = assign(&key, "/w/3", 2, 20, &Busy(vec![])).unwrap();
        assert_eq!(moved, slot_dir(&key, 0));
        // slot-2 and slot-3 are no longer handed out, even as the least recently used.
        let next = assign(&key, "/w/4", 2, 30, &Busy(vec![])).unwrap();
        assert_eq!(next, slot_dir(&key, 1));
        let again = assign(&key, "/w/2", 2, 40, &Busy(vec![])).unwrap();
        assert_eq!(again, slot_dir(&key, 0));
    }

    #[test]
    fn cargo_lock_probe_sees_a_held_build_lock() {
        let scratch = Scratch::new("cargolock");
        let slot = scratch.path().join("slot-0");
        let profile = slot.join("aarch64-apple-darwin").join("debug");
        std::fs::create_dir_all(&profile).unwrap();
        let lock_path = profile.join(CARGO_LOCK);
        std::fs::write(&lock_path, "").unwrap();
        assert!(!CargoLockProbe.is_busy(&slot));
        let held = OpenOptions::new().write(true).open(&lock_path).unwrap();
        held.lock().unwrap();
        assert!(CargoLockProbe.is_busy(&slot));
        drop(held);
        // Other tests spawn processes; a child between fork and exec briefly shares the lock's
        // open file description, so allow a moment for the release to become visible.
        let released = (0..50).any(|_| {
            let idle = !CargoLockProbe.is_busy(&slot);
            if !idle {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            idle
        });
        assert!(released);
    }

    #[test]
    fn corrupt_registry_is_reported() {
        let scratch = Scratch::new("corrupt");
        let key = scratch.path().join("key");
        std::fs::create_dir_all(&key).unwrap();
        std::fs::write(key.join(REGISTRY_FILE), "not json").unwrap();
        let err = assign(&key, "/w/a", 2, 1, &Busy(vec![])).unwrap_err();
        assert!(err.contains("not a valid slot registry"), "{err}");
    }
}
