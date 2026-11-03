//! Keep-awake (POWER-01/02): while the `power.keepAwake` setting is on and the app is open, hold
//! one OS request that prevents idle system sleep and still allows display sleep.
//!
//! The setting is the existing workspace setting of the ConfigurationService (off by default);
//! this controller only applies its effective value. [`KeepAwake::apply`] holds the request
//! while enabled and drops it when disabled; dropping the controller (closing the app) drops
//! it too, and every native mechanism also ends with the process. The status separates the
//! configured value from whether the OS request is effective or unavailable, for Settings
//! ([`KeepAwake::health`]). It never claims to prevent explicit sleep, lid close or shutdown.
use crate::ports::process::{PowerAssertion, PowerPort, PowerRequest};
use crate::ports::storage::{ConfigScope, ConfigurationRead, StorageError};
use crate::settings::catalog::descriptor;
use crate::settings::model::{chain, current_chain};
use crate::settings::view::Health;
use std::sync::{Arc, Mutex};

/// The keep-awake setting key.
pub const KEEP_AWAKE_KEY: &str = "power.keepAwake";

/// The effective (workspace) keep-awake value.
pub fn keep_awake_enabled(read: &dyn ConfigurationRead) -> Result<bool, StorageError> {
    let d = descriptor(KEEP_AWAKE_KEY).expect("catalog setting");
    let layers = current_chain(read, &ConfigScope::Workspace)?;
    let entries = chain(d, &layers).map_err(|m| {
        StorageError::InvalidRecord(format!("{} stored under another schema", m.key))
    })?;
    Ok(entries
        .first()
        .and_then(|e| e.value.value.0.as_bool())
        .unwrap_or(false))
}

/// The configured value and what the OS did with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeepAwakeStatus {
    /// The setting is off; nothing is requested.
    Off,
    /// The setting is on and the OS request is held.
    Effective { mechanism: &'static str },
    /// The setting is on but no request could be made.
    Unavailable(String),
}

pub struct KeepAwake {
    port: Arc<dyn PowerPort>,
    held: Mutex<Option<Box<dyn PowerAssertion>>>,
    status: Mutex<KeepAwakeStatus>,
}

impl KeepAwake {
    pub fn new(port: Arc<dyn PowerPort>) -> Self {
        Self {
            port,
            held: Mutex::new(None),
            status: Mutex::new(KeepAwakeStatus::Off),
        }
    }

    /// Applies the effective setting: requests prevention of idle sleep when enabled (keeping an
    /// existing request), releases it when disabled.
    pub fn apply(&self, enabled: bool) -> KeepAwakeStatus {
        let mut held = self.held.lock().unwrap_or_else(|p| p.into_inner());
        let status = if !enabled {
            // Dropping the assertion releases the OS request.
            *held = None;
            KeepAwakeStatus::Off
        } else {
            if held.is_none() {
                match self.port.prevent_idle_sleep() {
                    Ok(a) => *held = Some(a),
                    Err(e) => {
                        let s = KeepAwakeStatus::Unavailable(e);
                        *self.status.lock().unwrap_or_else(|p| p.into_inner()) = s.clone();
                        return s;
                    }
                }
            }
            match held.as_mut().map(|a| a.status()) {
                Some(PowerRequest::Effective { mechanism }) => {
                    KeepAwakeStatus::Effective { mechanism }
                }
                Some(PowerRequest::Unavailable(e)) => {
                    // A request that stopped working is released so the next apply retries.
                    *held = None;
                    KeepAwakeStatus::Unavailable(e)
                }
                None => KeepAwakeStatus::Unavailable("no request".into()),
            }
        };
        *self.status.lock().unwrap_or_else(|p| p.into_inner()) = status.clone();
        status
    }

    /// Reads the effective setting and applies it.
    pub fn refresh(&self, read: &dyn ConfigurationRead) -> Result<KeepAwakeStatus, StorageError> {
        Ok(self.apply(keep_awake_enabled(read)?))
    }

    /// The last applied status, re-checking a held request.
    pub fn status(&self) -> KeepAwakeStatus {
        let current = self
            .status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        match current {
            KeepAwakeStatus::Off => KeepAwakeStatus::Off,
            _ => self.apply(true),
        }
    }

    /// Runtime health for the Settings view (distinct from the configured value).
    pub fn health(&self) -> Health {
        match self.status() {
            KeepAwakeStatus::Off => Health::NotChecked,
            KeepAwakeStatus::Effective { .. } => Health::Available,
            KeepAwakeStatus::Unavailable(e) => Health::Unavailable(e),
        }
    }
}
