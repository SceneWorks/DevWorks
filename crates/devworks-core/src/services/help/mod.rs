//! One offline Help owner for native UI, agents and the scheduled regeneration producer.
mod bundle;
mod commands;
mod generation;
mod maintenance;
mod metadata;
mod prepared;
mod queries;
mod text;

use crate::operations::help::GenerateCapabilityDocsInput;
use crate::ports::storage::{ApplicationStore, ReadSnapshot, StorageError, WriteTransaction};
use crate::ports::{Clock, IdSource};
use crate::protocol::Revision;
use crate::services::{UseCases, artifacts::ArtifactService, packages::PackageService};
use std::collections::BTreeSet;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};

pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
pub use generation::GenerationOutcome;
pub const EFFECT_CONSUMER: &str = "help_docs";
const JOB_KIND: &str = "HelpGenerationV1";
pub const OPERATIONS: [&str; 5] = [
    "help.search",
    "help.read",
    "help.topic_for_screen",
    "help.package_guides",
    "help.generate_capability_docs",
];

#[derive(Clone)]
pub struct HelpService {
    store: Arc<dyn ApplicationStore>,
    artifacts: Arc<ArtifactService>,
    ids: Arc<dyn IdSource>,
    clock: Arc<dyn Clock>,
    packages: Option<Arc<PackageService>>,
    registered: Arc<RwLock<BTreeSet<(String, String)>>>,
    host_source: Arc<RwLock<String>>,
    stopping: Arc<AtomicBool>,
}
impl HelpService {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        artifacts: Arc<ArtifactService>,
        ids: Arc<dyn IdSource>,
        clock: Arc<dyn Clock>,
        packages: Option<Arc<PackageService>>,
    ) -> Self {
        Self {
            store,
            artifacts,
            ids,
            clock,
            packages,
            registered: Arc::new(RwLock::new(BTreeSet::new())),
            host_source: Arc::new(RwLock::new(String::new())),
            stopping: Arc::new(AtomicBool::new(false)),
        }
    }
    /// Capture the actual composed handlers, never all descriptor declarations as supported.
    pub fn set_composition(&self, cases: &UseCases) {
        let mut registered = BTreeSet::new();
        for operation in crate::registry::all() {
            if cases.has_query(operation.name) {
                registered.insert((operation.name.into(), "Query".into()));
            }
            if cases.has_command(operation.name) {
                registered.insert((operation.name.into(), "Command".into()));
            }
        }
        let mut current = self
            .registered
            .write()
            .unwrap_or_else(|lock| lock.into_inner());
        *current = registered;
        let mut sources = Vec::new();
        for (name, mode) in current.iter() {
            let descriptor = crate::registry::find(name).expect("composed operation");
            let binding = descriptor
                .binding(if mode == "Query" {
                    crate::registry::Mode::Query
                } else {
                    crate::registry::Mode::Command
                })
                .expect("composed mode");
            let revision = *descriptor.revisions.last().expect("composed revision");
            sources.push(serde_json::json!({"operation":name,"mode":mode,"owner":descriptor.owner,
                "authority":format!("{:?}",binding.authority),"effect":format!("{:?}",binding.effect),"revision":revision.0,
                "input":binding.input_schema(revision),"result":descriptor.result_schema(revision)}));
        }
        *self
            .host_source
            .write()
            .unwrap_or_else(|lock| lock.into_inner()) = crate::fingerprint::sha256_hex(
            serde_json::to_string(&sources)
                .expect("registered schema sources")
                .as_bytes(),
        );
    }
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }
    pub fn is_stopped(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }
    fn source_digest(&self, registry: i64) -> String {
        let registered = self
            .registered
            .read()
            .unwrap_or_else(|lock| lock.into_inner());
        let mut digest = crate::fingerprint::sha256_hex(bundle::CATALOG.as_bytes());
        for (path, body) in bundle::FILES {
            digest.push_str(path);
            digest.push_str(&crate::fingerprint::sha256_hex(body.as_bytes()));
        }
        let source = serde_json::json!({"app":APP_VERSION,"bundle":digest,"registry":registry,"handlers":*registered,
            "contracts":*self.host_source.read().unwrap_or_else(|lock|lock.into_inner())});
        crate::fingerprint::sha256_hex(source.to_string().as_bytes())
    }
}

fn generation_args(registry: i64, help: i64) -> Result<GenerateCapabilityDocsInput, StorageError> {
    Ok(GenerateCapabilityDocsInput {
        registry_revision: Revision::new(registry).map_err(|_| {
            StorageError::InvalidRecord("invalid capability registry revision".into())
        })?,
        expected_help_revision: Revision::new(help)
            .map_err(|_| StorageError::InvalidRecord("invalid Help revision".into()))?,
    })
}

/// Captures the current source revisions for an explicitly current-generation host request.
/// This never replaces explicitly authored arguments or grants permission to run the command.
pub fn current_generation_args(
    read: &dyn ReadSnapshot,
) -> Result<GenerateCapabilityDocsInput, StorageError> {
    generation_args(
        read.packages().registry_revision()?,
        read.help().help_head()?.revision,
    )
}

/// The scheduled job producer captures these arguments in its existing run transaction before
/// dispatch, then replays the stored invocation rather than resolving a new scope on retry.
pub fn current_generation_args_in(
    tx: &mut dyn WriteTransaction,
) -> Result<GenerateCapabilityDocsInput, StorageError> {
    let registry = tx.packages().registry_revision()?;
    generation_args(registry, tx.help().help_head()?.revision)
}

/// For the authorized native job owner, using only its actual accepted child operation.
/// A foreign/scripted handler using the same operation name does not gain this protocol.
pub(crate) fn request_generation_cancel(
    tx: &mut dyn WriteTransaction,
    operation: &crate::protocol::OperationId,
) -> Result<bool, StorageError> {
    let Some(record) = tx.operations().operation(operation)? else {
        return Ok(false);
    };
    if crate::ports::storage::is_terminal(record.state)
        || record.capability != "help.generate_capability_docs"
        || record.schema_version != 1
        || record.payload["kind"] != JOB_KIND
    {
        return Ok(false);
    }
    let delivery: crate::protocol::Id =
        serde_json::from_value(record.payload["deliveryId"].clone()).map_err(|_| {
            StorageError::InvalidRecord("invalid Help generation delivery identity".into())
        })?;
    let entry = tx
        .queue()
        .entry(&delivery)?
        .ok_or_else(|| StorageError::InvalidRecord("Help generation delivery is missing".into()))?;
    if entry.consumer != EFFECT_CONSUMER || entry.operation_id.as_ref() != Some(operation) {
        return Err(StorageError::InvalidRecord(
            "Help generation delivery binding changed".into(),
        ));
    }
    if entry.cancel_requested {
        return Ok(false);
    }
    tx.queue().cancel(&delivery)?;
    Ok(true)
}
