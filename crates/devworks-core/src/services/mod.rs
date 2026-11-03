//! Application use cases and the registry every entry point dispatches through.
//!
//! [`UseCases`] maps a host-selected operation to its one registered use case and submits it to
//! the [`Gateway`]; a UI action, an agent tool, a schedule or the package host therefore reach
//! the same validation, authority, receipts and implementation (ARCH-02). An operation with no
//! registered use case is reported as [`GatewayFailure::NoUseCase`] before any store access, so
//! nothing unimplemented is advertised or recorded.
pub mod artifacts;
pub mod cursor;
pub mod history;
pub mod queue;
pub mod sessions;

use crate::context::InvocationContext;
use crate::gateway::{CommandOutcome, Gateway, GatewayFailure, QueryOutcome};
use crate::ports::storage::WriteOptions;
use crate::ports::{CommandHandler, QueryHandler};
use crate::protocol::support::BoundedList;
use crate::protocol::{CorrelationId, DomainErrorCode, DomainFault, RetryAdvice};
use crate::registry::{self, Mode};
use crate::settings;
use std::collections::BTreeMap;
use std::sync::Arc;

/// A use-case fault without target details, correlated with the request (a query uses its
/// host invocation identity; a command allocates one in its transaction).
pub(crate) fn fault(
    code: DomainErrorCode,
    retry: RetryAdvice,
    correlation: &CorrelationId,
) -> DomainFault {
    DomainFault {
        code,
        retry,
        correlation_id: correlation.clone(),
        target_id: None,
        target_kind: None,
        revision_dimension: None,
        expected_revision: None,
        current_revision: None,
        blocker_ids: BoundedList(Vec::new()),
        diagnostic_id: None,
    }
}

/// Registered use cases by operation name.
#[derive(Clone, Default)]
pub struct UseCases {
    queries: BTreeMap<&'static str, Arc<dyn QueryHandler>>,
    commands: BTreeMap<&'static str, Arc<dyn CommandHandler>>,
}

impl UseCases {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `handler` for every mode `operation` declares. Panics on an unknown operation
    /// (a composition-root programming error, never request data).
    pub fn register(
        &mut self,
        operation: &'static str,
        query: Option<Arc<dyn QueryHandler>>,
        command: Option<Arc<dyn CommandHandler>>,
    ) -> &mut Self {
        let descriptor = registry::find(operation)
            .unwrap_or_else(|| panic!("{operation} is not a catalog operation"));
        if let Some(q) = query
            && descriptor.binding(Mode::Query).is_some()
        {
            self.queries.insert(descriptor.name, q);
        }
        if let Some(c) = command
            && descriptor.binding(Mode::Command).is_some()
        {
            self.commands.insert(descriptor.name, c);
        }
        self
    }

    /// Registers the queue family (`QueueDispatcher`).
    pub fn with_queue(mut self) -> Self {
        let service = Arc::new(queue::QueueService);
        for op in queue::OPERATIONS {
            self.register(op, Some(service.clone()), Some(service.clone()));
        }
        self
    }

    /// Registers the settings family (`ConfigurationService`).
    pub fn with_settings(mut self, service: Arc<settings::service::SettingsService>) -> Self {
        for op in settings::service::OPERATIONS {
            self.register(op, Some(service.clone()), Some(service.clone()));
        }
        self
    }

    /// Registers the artifacts family (`ArtifactService`).
    pub fn with_artifacts(mut self, service: Arc<artifacts::ArtifactService>) -> Self {
        for op in artifacts::OPERATIONS {
            self.register(op, Some(service.clone()), Some(service.clone()));
        }
        self
    }

    /// Registers the sessions family (`ConversationService`).
    pub fn with_sessions(mut self, service: Arc<sessions::SessionService>) -> Self {
        for op in sessions::OPERATIONS {
            self.register(op, Some(service.clone()), Some(service.clone()));
        }
        self
    }

    /// Registers the history family (`HistoryService`).
    pub fn with_history(mut self, service: Arc<history::HistoryService>) -> Self {
        for op in history::OPERATIONS {
            self.register(op, Some(service.clone()), None);
        }
        self
    }

    pub fn has_query(&self, operation: &str) -> bool {
        self.queries.contains_key(operation)
    }
    pub fn has_command(&self, operation: &str) -> bool {
        self.commands.contains_key(operation)
    }

    pub fn query(
        &self,
        gateway: &Gateway<'_>,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
    ) -> QueryOutcome {
        match self.queries.get(operation) {
            Some(handler) => gateway.query(ctx, operation, raw, handler.as_ref()),
            None => Err(GatewayFailure::NoUseCase(operation.to_owned())),
        }
    }

    pub fn command(
        &self,
        gateway: &Gateway<'_>,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
    ) -> CommandOutcome {
        self.command_with(gateway, ctx, operation, raw, WriteOptions::default())
    }

    pub fn command_with(
        &self,
        gateway: &Gateway<'_>,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
        options: WriteOptions,
    ) -> CommandOutcome {
        match self.commands.get(operation) {
            Some(handler) => gateway.command_with(ctx, operation, raw, handler.clone(), options),
            None => Err(GatewayFailure::NoUseCase(operation.to_owned())),
        }
    }
}
