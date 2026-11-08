//! An isolated, short-lived native registry login context for one image pull.
use super::backend::Backend;

pub trait RegistryAuthSession: Send {
    /// Environment overrides for the selected CLI. Values are paths, never credentials.
    fn environment(&self) -> Vec<(String, String)>;
    fn redactions(&self) -> Vec<String>;
}

pub trait RegistryAuth: Send + Sync {
    /// Prepare a private credential context. Dropping the returned session deletes it.
    fn prepare(
        &self,
        backend: Backend,
        image: &str,
        credential: &str,
    ) -> Result<Box<dyn RegistryAuthSession>, &'static str>;
}
