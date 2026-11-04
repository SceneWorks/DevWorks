//! Native network adapters of the shared web tools (DW-51): the rustls HTTP/1.1 transport, the
//! system resolver and the search providers. Every egress decision is core's
//! ([`devworks_core::services::web`]); these only move bytes to the address core checked.
pub mod brave;
pub mod resolve;
pub mod transport;

use devworks_core::ports::secrets::SecretResolver;
use devworks_core::ports::web::{HttpTransport, Resolver, SearchProvider};
use devworks_core::services::web::WebPorts;
use std::sync::Arc;

/// The production web ports: the system resolver, the native transport with Mozilla's roots and
/// every search provider this build supports, resolving credentials through `secrets`.
pub fn native_ports(secrets: Arc<dyn SecretResolver>) -> WebPorts {
    let resolver: Arc<dyn Resolver> = Arc::new(resolve::SystemResolver);
    let transport: Arc<dyn HttpTransport> = Arc::new(transport::NativeTransport::new());
    let providers: Vec<Arc<dyn SearchProvider>> = vec![Arc::new(brave::BraveSearch::new(
        resolver.clone(),
        transport.clone(),
    ))];
    WebPorts {
        resolver,
        transport,
        providers,
        secrets,
    }
}
