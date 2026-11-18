//! Durable outgoing MCP connection effects. Dispatch owns the attempt and recovery state;
//! the core service rechecks the saved configuration and authority before native I/O.
use crate::dispatch::Consumer;
use crate::effect_consumer::{PanicReasons, STANDARD_RETRY, effect_consumer};
use devworks_core::services::mcp::{EFFECT_CONSUMER, McpClientService};
use std::sync::Arc;

pub fn consumer(service: Arc<McpClientService>) -> Consumer {
    effect_consumer(
        EFFECT_CONSUMER,
        Arc::new(service),
        PanicReasons {
            execute: "outgoing MCP effect panicked",
            observe: "outgoing MCP observation panicked",
        },
        STANDARD_RETRY,
        None,
    )
}
