//! Private, disposable Git client configuration. This does not confine a remote server.
use std::path::Path;

/// Owns private configuration until all synchronous supervised helper calls have returned.
/// Implementations must create a fresh owner-only directory, never reuse a caller path.
pub trait GitTransportContext: Send + Sync {
    fn directory(&self) -> &Path;
}
/// Host filesystem boundary; configuration may contain credentials and must never be logged.
pub trait GitTransportFiles: Send + Sync {
    /// Resolve actual config file identity; a global include inside a repository is not a
    /// trusted host authentication source. No secrets are read by this method.
    fn trusted_origin(&self, origin: &Path, repositories: &[&Path]) -> Result<bool, String>;
    fn create(&self, configuration: &[u8]) -> Result<Box<dyn GitTransportContext>, String>;
}
