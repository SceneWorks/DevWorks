//! Durable container effects share the process supervisor and exact current authority.
use crate::dispatch::Consumer;
use crate::effect_consumer::{PanicReasons, STANDARD_RETRY, effect_consumer};
use devworks_core::ports::AuthorityPort;
use devworks_core::services::containers::{
    ContainerEffects, EFFECT_CONSUMER, STOP_CONSUMER, service::ContainerService,
};
use std::sync::Arc;

fn named(
    name: &str,
    service: Arc<ContainerService>,
    authority: Arc<dyn AuthorityPort>,
) -> Consumer {
    effect_consumer(
        name,
        Arc::new(ContainerEffects { service, authority }),
        PanicReasons {
            execute: "container effect panicked",
            observe: "container observation panicked",
        },
        STANDARD_RETRY,
        Some(2),
    )
}

pub fn consumer(service: Arc<ContainerService>, authority: Arc<dyn AuthorityPort>) -> Consumer {
    named(EFFECT_CONSUMER, service, authority)
}

/// Stop dispatch stays available while long builds, pulls or execs occupy every regular slot.
pub fn stop_consumer(
    service: Arc<ContainerService>,
    authority: Arc<dyn AuthorityPort>,
) -> Consumer {
    named(STOP_CONSUMER, service, authority)
}
