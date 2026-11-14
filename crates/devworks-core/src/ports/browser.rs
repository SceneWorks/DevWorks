//! The owned-browser boundary (DW-047): browser discovery and per-target qualification, owned
//! profile directories, the launch command and the Chrome DevTools Protocol transport.
//!
//! `devworks-adapters::browser` implements [`BrowserPlatform`] natively: it finds an installed
//! Chromium-family browser (Chrome, Chromium, Edge) or a configured executable, creates an empty
//! owned profile (user-data directory) per session under the data root, and speaks CDP over the
//! browser's private DevTools pipe (`--remote-debugging-pipe`): the process owner hands the host
//! the anonymous pipes the browser inherited ([`crate::ports::process::ControlPipe`]), so no
//! port, socket or name exists that another local process could attach to. No Node, Python or
//! driver service is involved. The protocol client itself (request/response correlation, flattened
//! target sessions, event buffering) lives in core over [`CdpTransport`], so the same client is
//! exercised by deterministic fakes and by the native transport.
//!
//! The platform never launches anything: [`BrowserPlatform::command`] only describes the process
//! the browser service then starts through the process owner
//! ([`crate::services::process::ProcessSupervisor`]), so every browser is an owned process tree
//! with bounded cancellation and boot recovery.
use crate::protocol::Id;
use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::process::{ControlChannel, ProcessSpec};

/// What the platform knows about one browser asset on this target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Qualification {
    /// An executable was found; it is verified only by a successful launch handshake.
    Present,
    /// No compatible browser executable was found where this asset is looked for.
    Missing(String),
    /// This asset is not offered on this OS/CPU target.
    Unsupported(String),
    /// The executable was found but cannot be used (not a regular executable file).
    Failed(String),
}

/// One browser asset of this target (an installed Chromium-family browser or a configured one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserAsset {
    /// Host identifier used by `browser.sessions` Create (`chrome`, `chromium`, `edge`,
    /// `configured`).
    pub id: String,
    /// Product family shown with the asset.
    pub product: String,
    /// The executable, when one was found.
    pub program: Option<PathBuf>,
    pub qualification: Qualification,
}

/// An owned, initially empty profile (user-data) directory of one browser session. The session
/// root also holds the session's staged upload copies, so removing it removes every file the
/// session ever owned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub session_id: Id,
    /// The session's owned root directory.
    pub root: PathBuf,
    /// The browser's user-data directory inside `root`.
    pub user_data: PathBuf,
}

/// A transport or platform failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrowserFault {
    /// The browser did not offer a DevTools endpoint within the bound, or exited first.
    NotReady(String),
    /// The transport is closed (the browser exited or the connection dropped).
    Closed(String),
    /// A filesystem operation on an owned directory failed.
    Io(String),
    /// A path or name was refused (escape, link, existing entry).
    Refused(String),
    /// The protocol peer sent something this client cannot interpret.
    Protocol(String),
}
impl std::fmt::Display for BrowserFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotReady(e) => write!(f, "browser not ready: {e}"),
            Self::Closed(e) => write!(f, "browser connection closed: {e}"),
            Self::Io(e) => write!(f, "browser file operation failed: {e}"),
            Self::Refused(e) => write!(f, "browser path refused: {e}"),
            Self::Protocol(e) => write!(f, "browser protocol error: {e}"),
        }
    }
}

/// One CDP connection (text frames carrying JSON messages). Implementations buffer incoming
/// frames so a busy page never blocks the browser; they bound that buffer.
pub trait CdpTransport: Send {
    /// Sends one message.
    fn send(&mut self, message: &str) -> Result<(), BrowserFault>;
    /// The next incoming message, waiting at most `timeout`; `Ok(None)` when none arrived.
    fn recv(&mut self, timeout: Duration) -> Result<Option<String>, BrowserFault>;
    /// Closes the connection (idempotent).
    fn close(&mut self);
}

/// The native browser platform.
pub trait BrowserPlatform: Send + Sync {
    /// The OS/CPU target this platform reports for (for example `macos-aarch64`).
    fn target(&self) -> String;
    /// The browser assets of this target and whether each is present. Discovery reads the
    /// filesystem only; it never starts a process.
    fn assets(&self) -> Vec<BrowserAsset>;
    /// Creates the owned, empty session root and profile directory for `session`. An existing
    /// entry is refused (never reused or followed).
    fn create_profile(&self, session: &Id) -> Result<Profile, BrowserFault>;
    /// Removes the session root (no-follow: links inside are removed, never traversed). A root
    /// that does not exist is not an error.
    fn remove_profile(&self, session: &Id) -> Result<(), BrowserFault>;
    /// Session roots present on disk (for the sweep of roots no live session owns).
    fn profiles(&self) -> Result<Vec<Id>, BrowserFault>;
    /// The process that runs `asset` with `args` (the browser service's launch flags, including
    /// the profile directory), with a private control pipe for its DevTools channel. The
    /// service launches it through the process owner.
    fn command(&self, asset: &BrowserAsset, profile: &Profile, args: &[String]) -> ProcessSpec;
    /// The DevTools transport over the launched browser's control channel (the process
    /// owner's [`crate::services::process::ProcessSupervisor::take_control`]). The browser is
    /// ready once its first request is answered.
    fn connect(
        &self,
        profile: &Profile,
        control: Option<ControlChannel>,
    ) -> Result<Box<dyn CdpTransport>, BrowserFault>;
    /// Materializes one staged file of the session (an upload or an artifact page) from
    /// `source`: a new file `name` (a single safe component) in a new directory `batch` (a single
    /// safe component) under the session root, written with create-new semantics and flushed.
    /// Returns the absolute path the browser is given.
    fn stage_file(
        &self,
        profile: &Profile,
        batch: &str,
        name: &str,
        source: &mut dyn Read,
        max_bytes: u64,
    ) -> Result<PathBuf, BrowserFault>;
    /// Removes one staged batch directory (no-follow); absent is not an error.
    fn remove_staged(&self, profile: &Profile, batch: &str) -> Result<(), BrowserFault>;
    /// Reads a staged file of the session (a regular file under its root, not followed through
    /// a link), at most `max_bytes`; a larger one is refused.
    fn read_staged(
        &self,
        profile: &Profile,
        path: &Path,
        max_bytes: u64,
    ) -> Result<Vec<u8>, BrowserFault>;
    /// Starts the session's egress proxy on a loopback port: the browser is launched to send
    /// every connection through it. For each connection the proxy asks `policy` once for the
    /// addresses of the requested host and port and connects only to one of those addresses,
    /// so the address that was checked is the address that is reached. Dropping the returned
    /// handle stops the proxy accepting connections.
    fn start_egress_proxy(
        &self,
        policy: Arc<dyn ConnectPolicy>,
    ) -> Result<Box<dyn RunningProxy>, BrowserFault>;
}

/// Decides where the browser's egress proxy may connect.
pub trait ConnectPolicy: Send + Sync {
    /// The admitted addresses of `host` (a domain or an IP literal, as the browser asked for
    /// it) at `port`, every one already checked; `Err` says why the connection is refused.
    fn addresses(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, String>;
}

/// A running egress proxy. Dropping it stops the proxy accepting connections.
pub trait RunningProxy: Send + Sync {
    /// The loopback address the proxy listens on.
    fn address(&self) -> SocketAddr;
}

/// Most open targets one session keeps (live and recorded alike).
pub const MAX_TARGETS: usize = 128;

/// The longest staged file or batch name a platform accepts, in UTF-8 bytes.
pub const STAGED_NAME_MAX_BYTES: usize = 128;

/// The profile directory a session root holds.
pub fn user_data_of(root: &Path) -> PathBuf {
    root.join("profile")
}
