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

/// The configured value, the held request and the last status, changed together.
struct State {
    enabled: bool,
    held: Option<Box<dyn PowerAssertion>>,
    status: KeepAwakeStatus,
}

/// The keep-awake controller. Every read and change runs under one lock, so a status check
/// that re-acquires a lapsed request can never overtake a disable: a disable waits for it and
/// then releases what it acquired.
pub struct KeepAwake {
    port: Arc<dyn PowerPort>,
    state: Mutex<State>,
}

impl KeepAwake {
    pub fn new(port: Arc<dyn PowerPort>) -> Self {
        Self {
            port,
            state: Mutex::new(State {
                enabled: false,
                held: None,
                status: KeepAwakeStatus::Off,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Makes the held request match the configured value: none when disabled; when enabled, the
    /// held request if it still works, otherwise a new one.
    fn settle(&self, st: &mut State) -> KeepAwakeStatus {
        let status = if !st.enabled {
            // Dropping the assertion releases the OS request.
            st.held = None;
            KeepAwakeStatus::Off
        } else {
            if let Some(PowerRequest::Effective { mechanism }) =
                st.held.as_mut().map(|a| a.status())
            {
                return self.record(st, KeepAwakeStatus::Effective { mechanism });
            }
            // A request that stopped working is released and requested again.
            st.held = None;
            match self.port.prevent_idle_sleep() {
                Ok(mut a) => {
                    let s = match a.status() {
                        PowerRequest::Effective { mechanism } => {
                            KeepAwakeStatus::Effective { mechanism }
                        }
                        PowerRequest::Unavailable(e) => KeepAwakeStatus::Unavailable(e),
                    };
                    if matches!(s, KeepAwakeStatus::Effective { .. }) {
                        st.held = Some(a);
                    }
                    s
                }
                Err(e) => KeepAwakeStatus::Unavailable(e),
            }
        };
        self.record(st, status)
    }
    fn record(&self, st: &mut State, status: KeepAwakeStatus) -> KeepAwakeStatus {
        st.status = status.clone();
        status
    }

    /// Applies the effective setting: requests prevention of idle sleep when enabled (keeping a
    /// working request), releases it when disabled.
    pub fn apply(&self, enabled: bool) -> KeepAwakeStatus {
        let mut st = self.lock();
        st.enabled = enabled;
        self.settle(&mut st)
    }

    /// Reads the effective setting and applies it.
    pub fn refresh(&self, read: &dyn ConfigurationRead) -> Result<KeepAwakeStatus, StorageError> {
        Ok(self.apply(keep_awake_enabled(read)?))
    }

    /// The current status, re-checking (and, while enabled, re-requesting) the OS request. It
    /// never requests anything while the setting is off.
    pub fn status(&self) -> KeepAwakeStatus {
        let mut st = self.lock();
        if !st.enabled {
            return KeepAwakeStatus::Off;
        }
        self.settle(&mut st)
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
