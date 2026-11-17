//! Portable convention imports execute through the actual operation journal and durable queue.
use crate::dispatch::Consumer;
use crate::effect_consumer::{PanicReasons, STANDARD_RETRY, effect_consumer};
use devworks_core::services::conventions::importing::{EFFECT_CONSUMER, ImportService};
use std::sync::Arc;

pub fn consumer(service: Arc<ImportService>) -> Consumer {
    effect_consumer(
        EFFECT_CONSUMER,
        service,
        PanicReasons {
            execute: "Convention import panicked after dispatch",
            observe: "Convention import observation panicked",
        },
        STANDARD_RETRY,
        Some(1),
    )
}
