//! Offline Help publication uses the application operation journal and durable queue.
use crate::dispatch::Consumer;
use crate::effect_consumer::{PanicReasons, STANDARD_RETRY, effect_consumer};
use devworks_core::services::help::{EFFECT_CONSUMER, HelpService};
use std::sync::Arc;

pub fn consumer(service: Arc<HelpService>) -> Consumer {
    effect_consumer(
        EFFECT_CONSUMER,
        service,
        PanicReasons {
            execute: "Help publication panicked after dispatch",
            observe: "Help observation panicked",
        },
        STANDARD_RETRY,
        Some(1),
    )
}
