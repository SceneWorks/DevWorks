//! The provider connections the inference router routes through.
//!
//! Connection records, authentication and secret handles belong to the provider-connection item
//! (DW-18), model discovery refreshes to it and the transports (DW-19). The router only needs to
//! know, at selection time, which connections are usable, which endpoint profile each serves and
//! (once discovered) which upstream models each lists, so it reads them through this port and
//! never through a provider. The port is synchronous and in memory: it is consulted inside the
//! routing unit of work, which must not touch the network.
//!
//! Until DW-18 plugs a directory in, the production composition uses [`NoConnections`]: nothing
//! is connected, so every route reports `NoConnection` truthfully instead of pretending a model
//! is reachable.
use crate::protocol::Id;
use std::collections::BTreeSet;

/// A connection's usability as the router sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// Authenticated and usable.
    Connected,
    /// Never signed in, or signed out.
    Disconnected,
    /// A sign-in is in progress.
    Authorizing,
    /// Refreshing its credential; requests wait for the refresh.
    Refreshing,
    /// The provider rejected the credential; the user must sign in again.
    ReauthenticationRequired,
    /// Temporarily unreachable (an availability event, not a revocation).
    Unavailable,
    /// Turned off by the user, or a subscription profile whose risk notice is not accepted.
    Disabled,
}

/// One connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionStatus {
    pub id: Id,
    /// The `EndpointKind` wire name it serves.
    pub profile: String,
    pub state: ConnectionState,
    /// Upstream model IDs of its last successful discovery; `None` before the first, when an
    /// explicitly configured model may be used with its declared capabilities.
    pub discovered: Option<BTreeSet<String>>,
}

/// The connections the router may route through.
pub trait ConnectionDirectory: Send + Sync {
    /// Every configured connection, in a stable order.
    fn connections(&self) -> Vec<ConnectionStatus>;
}

/// No provider connection is configured (the production state until DW-18).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoConnections;
impl ConnectionDirectory for NoConnections {
    fn connections(&self) -> Vec<ConnectionStatus> {
        Vec::new()
    }
}
