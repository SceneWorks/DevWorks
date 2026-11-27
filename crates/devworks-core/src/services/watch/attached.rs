//! External process watchers own read-only registrations. Their identities survive a restart.
use super::{WatchService, finish_in, outcome};
use crate::locks::LockExt;
use crate::ports::process::{
    ExternalExit, ExternalProcessStatus as Status, ExternalProcessWatch, NativeIdentity,
    ProcessHost,
};
use crate::ports::storage::{StorageError, StoredWatcher, WriteOptions, run_write};
use crate::protocol::Id;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub(super) type PreparedAttachment = Result<Mutex<Option<Box<dyn ExternalProcessWatch>>>, Status>;

pub(super) struct Attachments {
    host: Arc<dyn ProcessHost>,
    registrations: Mutex<HashMap<Id, Registration>>,
}

enum Registration {
    Watching(Box<dyn ExternalProcessWatch>),
    // A one-shot OS notification must survive a failed store transaction and a retry.
    Exited(ExternalExit),
}

impl Attachments {
    pub(super) fn new(host: Arc<dyn ProcessHost>) -> Self {
        Self {
            host,
            registrations: Mutex::new(HashMap::new()),
        }
    }

    pub(super) fn prepare(&self, identity: &NativeIdentity) -> PreparedAttachment {
        // Retain the verified registration across the create transaction so a fast exit is
        // observable even if the target PID disappears before the first tick.
        self.host
            .watch_external(identity)
            .map(|watch| Mutex::new(Some(watch)))
    }

    pub(super) fn register(&self, id: Id, watch: Box<dyn ExternalProcessWatch>) {
        self.registrations
            .locked()
            .entry(id)
            .or_insert(Registration::Watching(watch));
    }

    pub(super) fn snapshot(&self) -> HashSet<Id> {
        self.registrations.locked().keys().cloned().collect()
    }

    pub(super) fn forget(&self, id: &Id) {
        self.registrations.locked().remove(id);
    }

    pub(super) fn forget_missing(&self, missing: HashSet<Id>) {
        if !missing.is_empty() {
            self.registrations
                .locked()
                .retain(|id, _| !missing.contains(id));
        }
    }

    pub(super) fn close(&self) {
        self.registrations.locked().clear();
    }

    fn observe(&self, id: &Id, identity: &NativeIdentity) -> Status {
        let mut map = self.registrations.locked();
        let registration = match map.entry(id.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let registration = match self.host.watch_external(identity) {
                    Ok(watch) => Registration::Watching(watch),
                    Err(Status::Missing) => Registration::Exited(ExternalExit::default()),
                    Err(Status::Exited(exit)) => Registration::Exited(exit),
                    Err(status) => return status,
                };
                entry.insert(registration)
            }
        };
        match registration {
            Registration::Exited(exit) => Status::Exited(*exit),
            Registration::Watching(watch) => match watch.poll_exit() {
                Ok(None) => Status::Running,
                Ok(Some(exit)) => {
                    *registration = Registration::Exited(exit);
                    Status::Exited(exit)
                }
                Err(_) => {
                    map.remove(id);
                    Status::Unavailable
                }
            },
        }
    }
}

impl WatchService {
    pub(super) fn advance_attachment(
        &self,
        watcher: &StoredWatcher,
        identity: &NativeIdentity,
    ) -> Result<bool, StorageError> {
        let observation = self.attachments.observe(&watcher.id, identity);
        self.settle_attachment(watcher, observation)
    }

    pub(super) fn backup_attachment(
        &self,
        watcher: &StoredWatcher,
        identity: &NativeIdentity,
    ) -> Result<Option<&'static str>, StorageError> {
        // Probe through a fresh OS observation even when a registration is healthy or stuck.
        let observation = self.attachments.host.inspect_external(identity);
        if self.settle_attachment(watcher, observation)? {
            return Ok(None);
        }
        Ok(match observation {
            Status::Different => Some("its attached PID now belongs to another process"),
            Status::Unavailable => Some("its attached process's status cannot be established"),
            _ => None,
        })
    }

    fn settle_attachment(
        &self,
        watcher: &StoredWatcher,
        observation: Status,
    ) -> Result<bool, StorageError> {
        let exit = match observation {
            Status::Exited(exit) => Some(exit),
            // The identity was verified at creation. Confirmed absence after reopening means
            // that process is gone, even when the host missed the original exit notification.
            Status::Missing => Some(ExternalExit::default()),
            _ => None,
        };
        if exit.is_none()
            && watcher
                .expires_at_ms
                .is_none_or(|at| self.clock.now_ms().get() < at)
        {
            return Ok(false);
        }
        let id = watcher.id.clone();
        let settled = run_write(&*self.store, WriteOptions::host_state(), {
            let id = id.clone();
            move |tx| {
                let Some(row) = tx.watchers().watcher(&id)? else {
                    return Ok(false);
                };
                if !row.phase.is_open() || row.attached_process.is_none() {
                    return Ok(false);
                }
                let now = tx.now_ms();
                let observed = match exit {
                    Some(exit) => outcome(
                        "Exited",
                        exit.exit_code.map(i64::from),
                        None,
                        exit.exit_code.is_none().then_some("exit status unknown"),
                    ),
                    None if row.expires_at_ms.is_some_and(|at| now >= at) => outcome(
                        "TimedOut",
                        None,
                        None,
                        Some("the attached process watch reached its maximum lifetime"),
                    ),
                    None => return Ok(false),
                };
                let duration = Some((now - row.created_at_ms).max(0));
                finish_in(tx, row, observed, duration, None, false, None)?;
                Ok(true)
            }
        })?;
        if settled {
            self.attachments.forget(&id);
        }
        Ok(settled)
    }
}
