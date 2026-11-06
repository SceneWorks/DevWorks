//! DevWorks infrastructure adapters.
pub mod browser;
pub mod diagnostics;
pub mod fs;
pub mod git_transport;
pub mod http;
pub mod index;
pub mod packages;
pub mod paths;
pub mod power;
pub mod process;
pub mod secrets;
pub mod storage;
pub mod web;

/// The OS cryptographic random generator (core `Entropy`: PKCE verifiers, sign-in state).
#[derive(Clone, Copy, Debug, Default)]
pub struct OsEntropy;
impl devworks_core::ports::http::Entropy for OsEntropy {
    fn fill(&self, buf: &mut [u8]) {
        // A failing OS generator leaves no safe way to sign in; fail loudly rather than emit a
        // predictable verifier.
        getrandom::fill(buf).expect("the OS random generator is unavailable");
    }
}
