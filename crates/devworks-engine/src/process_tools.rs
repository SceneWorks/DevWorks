//! Durable process-tool effects share the existing owned-process supervisor. Dispatch records
//! the attempt before launch; observation never launches a replacement after an unknown result.
use crate::dispatch::Consumer;
use crate::effect_consumer::{PanicReasons, STANDARD_RETRY, effect_consumer};
use devworks_core::services::process::tools::{
    CANCEL_CONSUMER, EFFECT_CONSUMER, ProcessToolService, WATCH_CONSUMER,
};
use std::sync::Arc;

fn named(name: &str, service: Arc<ProcessToolService>) -> Consumer {
    effect_consumer(
        name,
        Arc::new(service),
        PanicReasons {
            execute: "process effect panicked",
            observe: "process observation panicked",
        },
        STANDARD_RETRY,
        Some(2),
    )
}

pub fn consumer(service: Arc<ProcessToolService>) -> Consumer {
    named(EFFECT_CONSUMER, service)
}

/// Cancellation is isolated from long-running execute/install slots, so a full worker pool
/// cannot hold a stop request behind the processes it needs to stop.
pub fn cancel_consumer(service: Arc<ProcessToolService>) -> Consumer {
    named(CANCEL_CONSUMER, service)
}

/// Watched processes run long by design (a dev server, a build). Their effect ends once the
/// process is launched (a monitor thread records the outcome), so they hold a dispatcher worker
/// only for the launch and never starve cancellation or other consumers.
pub fn watch_consumer(service: Arc<ProcessToolService>) -> Consumer {
    named(WATCH_CONSUMER, service)
}
