//! An isolated, short-lived native registry login context for one image pull.
use super::backend::Backend;

pub trait RegistryAuthSession: Send {
    /// Environment overrides for the selected CLI. Values are paths, never credentials.
    fn environment(&self) -> Vec<(String, String)>;
    fn redactions(&self) -> Vec<String>;
}

pub trait RegistryAuth: Send + Sync {
    /// Prepare an isolated client configuration with no registry credentials.
    fn prepare_anonymous(
        &self,
        backend: Backend,
    ) -> Result<Box<dyn RegistryAuthSession>, &'static str>;
    /// Prepare a private credential context. Dropping the returned session deletes it.
    fn prepare(
        &self,
        backend: Backend,
        image: &str,
        credential: &str,
    ) -> Result<Box<dyn RegistryAuthSession>, &'static str>;
}

pub trait WorkloadEnvironmentSession: Send {
    fn path(&self) -> &str;
}

pub trait WorkloadEnvironment: Send + Sync {
    /// Store values in a private CLI env file, without placing them in the host CLI environment.
    fn prepare_workload(
        &self,
        environment: &[(String, String)],
    ) -> Result<Box<dyn WorkloadEnvironmentSession>, &'static str>;
}
