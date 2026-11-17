//! Serial, journaled desktop input. An unknown result is observed as unknown: replaying a
//! possible click or keypress would act twice on the user's desktop.
use crate::dispatch::{Consumer, RetryPolicy};
use crate::effect_consumer::{PanicReasons, effect_consumer};
use devworks_core::services::desktop::{DesktopService, INPUT_CONSUMER};
use std::sync::Arc;

pub fn consumer(service: Arc<DesktopService>) -> Consumer {
    effect_consumer(
        INPUT_CONSUMER,
        service,
        PanicReasons {
            execute: "desktop input worker panicked",
            observe: "desktop input observation panicked",
        },
        RetryPolicy {
            base_delay_ms: 500,
            max_delay_ms: 500,
            max_attempts: 1,
        },
        Some(1),
    )
}
