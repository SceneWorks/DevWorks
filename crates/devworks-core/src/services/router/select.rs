//! Pure route eligibility (ROUTE-03, ROUTE-13, ROUTE-19, ROUTE-20): what a request needs, which
//! models a route intent pools, and why a pooled model is or is not eligible right now.
//!
//! Selection first filters the enabled catalog model, its declared capabilities and effort,
//! then a usable connection of its endpoint profile (connected, and listing the model once
//! discovered), then cooldowns. Nothing here reads a clock, a provider or the network: the
//! caller passes the catalog rows, the connection directory snapshot, the cooldowns and `now`.
use crate::ports::router::{ConnectionState, ConnectionStatus};
use crate::ports::storage::{StoredCooldown, StoredRouteModel};
use crate::protocol::Id;
use std::collections::{BTreeMap, BTreeSet};

/// Capability label: the request carries images (ROUTE-13).
pub const VISION: &str = "vision";
/// Capability label: the request offers tools.
pub const TOOLS: &str = "tools";
/// Capability label prefix: the request needs a context window of at least N tokens
/// (`context:N`).
pub const CONTEXT_PREFIX: &str = "context:";
/// The provider's own default effort (never an eligibility constraint).
pub const DEFAULT_EFFORT: &str = "Default";
/// Upstream identity of OpenRouter's automatic route on an `OpenRouterChat` connection.
pub const OPEN_ROUTER_AUTO_UPSTREAM: &str = "openrouter/auto";
/// The endpoint profile Open Router Auto is served by.
pub const OPEN_ROUTER_PROFILE: &str = "OpenRouterChat";

/// What one request needs from a model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Need {
    /// `EffortName` wire name; anything but `Default` is an eligibility constraint.
    pub effort: String,
    pub vision: bool,
    pub tools: bool,
    pub context_tokens: Option<i64>,
    /// The normalized labels (sorted, unique), as recorded on a pin.
    pub labels: Vec<String>,
}
impl Need {
    /// Parses capability labels. An unknown label is refused rather than ignored, so a
    /// requirement can never silently disappear.
    pub fn parse<'a>(
        effort: &str,
        labels: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, String> {
        let mut need = Self {
            effort: effort.to_owned(),
            vision: false,
            tools: false,
            context_tokens: None,
            labels: Vec::new(),
        };
        for label in labels {
            match label {
                VISION => need.vision = true,
                TOOLS => need.tools = true,
                l if l.starts_with(CONTEXT_PREFIX) => {
                    let digits = &l[CONTEXT_PREFIX.len()..];
                    let n = digits
                        .parse::<i64>()
                        .ok()
                        .filter(|n| *n > 0 && digits.bytes().all(|b| b.is_ascii_digit()))
                        .ok_or_else(|| format!("invalid context requirement '{l}'"))?;
                    need.context_tokens = Some(need.context_tokens.map_or(n, |c| c.max(n)));
                }
                l => return Err(format!("unknown capability '{l}'")),
            }
        }
        if need.vision {
            need.labels.push(VISION.into());
        }
        if need.tools {
            need.labels.push(TOOLS.into());
        }
        if let Some(n) = need.context_tokens {
            need.labels.push(format!("{CONTEXT_PREFIX}{n}"));
        }
        need.labels.sort();
        Ok(need)
    }
    /// The same requirements with another requested effort.
    pub fn with_effort(&self, effort: &str) -> Self {
        Self {
            effort: effort.to_owned(),
            ..self.clone()
        }
    }
}

/// Why a pooled model is not eligible. Codes are the wire `blockedReasonCodes`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Block {
    /// The pool names a model the catalog does not have.
    NotInCatalog,
    Disabled,
    EffortUnsupported,
    VisionUnsupported,
    ToolsUnsupported,
    /// A context requirement and a model whose window is not declared.
    ContextUnknown,
    ContextTooSmall,
    /// No connection serves the model's endpoint profile.
    NoConnection,
    /// Its connections' model constraints (allowed/excluded lists) exclude it.
    NotAllowed,
    /// Its connections need a sign-in (disconnected, authorizing or rejected).
    CredentialsRequired,
    /// Its connections are temporarily unavailable or refreshing.
    ConnectionUnavailable,
    /// Its connections are turned off (or a risk notice is not accepted).
    ConnectionDisabled,
    /// Connected, but the last discovery does not list the model.
    NotDiscovered,
    /// Cooling until this UTC ms.
    Cooling(i64),
}
impl Block {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotInCatalog => "NotInCatalog",
            Self::Disabled => "Disabled",
            Self::EffortUnsupported => "EffortUnsupported",
            Self::VisionUnsupported => "VisionUnsupported",
            Self::ToolsUnsupported => "ToolsUnsupported",
            Self::ContextUnknown => "ContextUnknown",
            Self::ContextTooSmall => "ContextTooSmall",
            Self::NoConnection => "NoConnection",
            Self::NotAllowed => "NotAllowed",
            Self::CredentialsRequired => "CredentialsRequired",
            Self::ConnectionUnavailable => "ConnectionUnavailable",
            Self::ConnectionDisabled => "ConnectionDisabled",
            Self::NotDiscovered => "NotDiscovered",
            Self::Cooling(_) => "Cooling",
        }
    }
    /// A block the model itself declares (never cleared by waiting or signing in).
    pub fn is_static(&self) -> bool {
        matches!(
            self,
            Self::NotInCatalog
                | Self::Disabled
                | Self::EffortUnsupported
                | Self::VisionUnsupported
                | Self::ToolsUnsupported
                | Self::ContextUnknown
                | Self::ContextTooSmall
                | Self::NotAllowed
        )
    }
    pub fn is_credentials(&self) -> bool {
        matches!(self, Self::NoConnection | Self::CredentialsRequired)
    }
}

/// One pooled model and its eligibility.
#[derive(Clone, Debug)]
pub struct Assessed {
    pub model_id: Id,
    pub model: Option<StoredRouteModel>,
    /// The connection it would be served by; `None` when it is not eligible.
    pub connection: Option<Id>,
    /// Empty when eligible.
    pub blocks: Vec<Block>,
}
impl Assessed {
    pub fn eligible(&self) -> bool {
        self.blocks.is_empty()
    }
    /// The cooldown expiry when cooling is the only thing keeping it from being eligible.
    pub fn only_cooling_until(&self) -> Option<i64> {
        match self.blocks.as_slice() {
            [Block::Cooling(until)] => Some(*until),
            _ => None,
        }
    }
}

/// The current cooldown expiries (those still in the future) by model.
pub fn active_cooldowns(cooldowns: &[StoredCooldown], now: i64) -> BTreeMap<String, i64> {
    cooldowns
        .iter()
        .filter(|c| c.until_ms > now)
        .map(|c| (c.model_id.as_str().to_owned(), c.until_ms))
        .collect()
}

/// Whether `model` declares the requested effort: `Default` always (the provider decides);
/// any explicit effort only when declared, never mapped to a neighbouring level.
pub fn supports_effort(model: &StoredRouteModel, effort: &str) -> bool {
    effort == DEFAULT_EFFORT || model.efforts.iter().any(|e| e == effort)
}

/// The model's static capability blocks for `need`.
fn capability_blocks(model: &StoredRouteModel, need: &Need) -> Vec<Block> {
    let mut blocks = Vec::new();
    if !model.enabled {
        blocks.push(Block::Disabled);
    }
    if !supports_effort(model, &need.effort) {
        blocks.push(Block::EffortUnsupported);
    }
    if need.vision && !model.image_input {
        blocks.push(Block::VisionUnsupported);
    }
    if need.tools && !model.tools {
        blocks.push(Block::ToolsUnsupported);
    }
    if let Some(required) = need.context_tokens {
        match model.context_tokens {
            None => blocks.push(Block::ContextUnknown),
            Some(window) if window < required => blocks.push(Block::ContextTooSmall),
            Some(_) => {}
        }
    }
    blocks
}

/// Whether inventory lists this exact concrete upstream identity. A dated snapshot and an
/// undated provider alias are distinct identities; neither can keep the other's pin alive.
pub fn lists(profile: &str, discovered: &BTreeSet<String>, upstream: &str) -> bool {
    if profile == OPEN_ROUTER_PROFILE {
        return upstream == OPEN_ROUTER_AUTO_UPSTREAM || discovered.contains(upstream);
    }
    discovered
        .iter()
        .any(|id| id.eq_ignore_ascii_case(upstream))
}

/// Exact provider inventory availability, independent of enablement, credentials, cooldowns
/// and request requirements. Unknown remains unknown until every applicable permitted scope
/// has a complete inventory, unless one already proves the identity is listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inventory {
    pub available: Option<bool>,
    pub reason: Option<&'static str>,
}

pub fn inventory(model: &StoredRouteModel, connections: &[ConnectionStatus]) -> Inventory {
    let serving: Vec<_> = connections
        .iter()
        .filter(|c| c.profile == model.endpoint_profile)
        .filter(|c| model.connection_id.as_ref().is_none_or(|id| id == &c.id))
        .collect();
    if serving.is_empty() {
        return Inventory {
            available: None,
            reason: Some("NoInventory"),
        };
    }
    let permitted: Vec<_> = serving
        .into_iter()
        .filter(|c| c.permits(&model.upstream_model_id))
        .collect();
    if permitted.is_empty() {
        return Inventory {
            available: Some(false),
            reason: Some("Excluded"),
        };
    }
    if model.endpoint_profile == OPEN_ROUTER_PROFILE
        && model.upstream_model_id == OPEN_ROUTER_AUTO_UPSTREAM
        || permitted.iter().any(|c| {
            c.discovered
                .as_ref()
                .is_some_and(|d| lists(&model.endpoint_profile, d, &model.upstream_model_id))
        })
    {
        Inventory {
            available: Some(true),
            reason: None,
        }
    } else if permitted.iter().any(|c| c.discovered.is_none()) {
        Inventory {
            available: None,
            reason: Some("NoInventory"),
        }
    } else {
        Inventory {
            available: Some(false),
            reason: Some("NotDiscovered"),
        }
    }
}

/// The usable connection of `model` (preferring `preferred`), or why there is none.
fn connection_for(
    model: &StoredRouteModel,
    connections: &[ConnectionStatus],
    preferred: Option<&Id>,
) -> Result<Id, Block> {
    // A model bound to one connection is served by that connection only (never by another
    // endpoint of the same profile, which could hold another account or billing).
    let serving: Vec<&ConnectionStatus> = connections
        .iter()
        .filter(|c| c.profile == model.endpoint_profile)
        .filter(|c| model.connection_id.as_ref().is_none_or(|b| b == &c.id))
        .collect();
    if serving.is_empty() {
        return Err(Block::NoConnection);
    }
    // A connection's allowed/excluded model constraints are never routed around: a model they
    // exclude everywhere is ineligible, whatever the connections' state.
    let serving: Vec<&ConnectionStatus> = serving
        .into_iter()
        .filter(|c| c.permits(&model.upstream_model_id))
        .collect();
    if serving.is_empty() {
        return Err(Block::NotAllowed);
    }
    let lists = |c: &ConnectionStatus| {
        c.discovered
            .as_ref()
            .is_none_or(|d| lists(&model.endpoint_profile, d, &model.upstream_model_id))
    };
    let usable: Vec<&ConnectionStatus> = serving
        .iter()
        .copied()
        .filter(|c| c.state == ConnectionState::Connected && lists(c))
        .collect();
    if let Some(p) = preferred
        && let Some(c) = usable.iter().find(|c| &c.id == p)
    {
        return Ok(c.id.clone());
    }
    if let Some(c) = usable
        .iter()
        .min_by(|a, b| a.id.as_str().cmp(b.id.as_str()))
    {
        return Ok(c.id.clone());
    }
    let has = |s: &[ConnectionState]| serving.iter().any(|c| s.contains(&c.state));
    Err(if has(&[ConnectionState::Connected]) {
        Block::NotDiscovered
    } else if has(&[ConnectionState::Unavailable, ConnectionState::Refreshing]) {
        Block::ConnectionUnavailable
    } else if has(&[
        ConnectionState::Disconnected,
        ConnectionState::Authorizing,
        ConnectionState::ReauthenticationRequired,
    ]) {
        Block::CredentialsRequired
    } else {
        Block::ConnectionDisabled
    })
}

/// Assesses one pooled model.
pub fn assess(
    model_id: &Id,
    model: Option<&StoredRouteModel>,
    connections: &[ConnectionStatus],
    cooling: &BTreeMap<String, i64>,
    need: &Need,
    preferred_connection: Option<&Id>,
) -> Assessed {
    let Some(model) = model else {
        return Assessed {
            model_id: model_id.clone(),
            model: None,
            connection: None,
            blocks: vec![Block::NotInCatalog],
        };
    };
    let mut blocks = capability_blocks(model, need);
    let connection = match connection_for(model, connections, preferred_connection) {
        Ok(c) => Some(c),
        Err(b) => {
            blocks.push(b);
            None
        }
    };
    if let Some(until) = cooling.get(model.id.as_str()) {
        blocks.push(Block::Cooling(*until));
    }
    Assessed {
        model_id: model_id.clone(),
        model: Some(model.clone()),
        connection: if blocks.is_empty() { connection } else { None },
        blocks,
    }
}

/// Why nothing in a pool is eligible, from the most to the least recoverable cause.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// The pool has no member at all.
    EmptyPool,
    /// Something would be eligible once its cooldown ends; the earliest such expiry.
    Cooling(i64),
    /// Something would be eligible after a sign-in or connection.
    Credentials,
    /// Something would be eligible once its connection recovers or is turned on.
    Connection,
    /// No member can serve this request as configured.
    Unsupported,
}

/// The earliest expiry among members kept out only by a cooldown.
pub fn earliest_cooldown(pool: &[Assessed]) -> Option<i64> {
    pool.iter().filter_map(Assessed::only_cooling_until).min()
}

/// Why `pool` (with no eligible member) is unavailable.
pub fn unavailable(pool: &[Assessed]) -> Unavailable {
    if pool.is_empty() {
        return Unavailable::EmptyPool;
    }
    if let Some(until) = earliest_cooldown(pool) {
        return Unavailable::Cooling(until);
    }
    let dynamic_only = |a: &&Assessed| a.blocks.iter().all(|b| !b.is_static());
    let recoverable: Vec<&Assessed> = pool.iter().filter(dynamic_only).collect();
    if recoverable
        .iter()
        .any(|a| a.blocks.iter().any(Block::is_credentials))
    {
        Unavailable::Credentials
    } else if !recoverable.is_empty() {
        Unavailable::Connection
    } else {
        Unavailable::Unsupported
    }
}

/// The distinct block codes of a pool, in a stable order (`EmptyPool` for an empty pool).
pub fn block_codes(pool: &[Assessed]) -> Vec<&'static str> {
    if pool.is_empty() {
        return vec!["EmptyPool"];
    }
    let mut codes: Vec<&'static str> = pool
        .iter()
        .flat_map(|a| a.blocks.iter().map(Block::code))
        .collect();
    codes.sort_unstable();
    codes.dedup();
    codes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(efforts: &[&str], vision: bool, context: Option<i64>) -> StoredRouteModel {
        StoredRouteModel {
            id: Id::new("m").unwrap(),
            display_name: "M".into(),
            endpoint_profile: "CompatibleChat".into(),
            upstream_model_id: "up/m".into(),
            enabled: true,
            efforts: efforts.iter().map(|e| (*e).to_owned()).collect(),
            image_input: vision,
            tools: true,
            context_tokens: context,
            connection_id: None,
            position: 1,
            revision: 1,
            updated_at_ms: 0,
        }
    }
    fn connected(discovered: Option<&[&str]>) -> Vec<ConnectionStatus> {
        vec![ConnectionStatus {
            id: Id::new("c").unwrap(),
            profile: "CompatibleChat".into(),
            state: ConnectionState::Connected,
            discovered: discovered.map(|d| d.iter().map(|s| (*s).to_owned()).collect()),
            allowed: Vec::new(),
            excluded: Vec::new(),
        }]
    }

    #[test]
    fn needs_parse_known_labels_and_refuse_unknown_ones() {
        let n = Need::parse("High", ["tools", "context:1000", "vision", "context:4000"]).unwrap();
        assert!(n.vision && n.tools);
        assert_eq!(n.context_tokens, Some(4000), "the largest requirement");
        assert_eq!(n.labels, ["context:4000", "tools", "vision"]);
        for bad in [
            "telepathy",
            "context:",
            "context:0",
            "context:-1",
            "context:1e3",
            "Vision",
        ] {
            assert!(Need::parse("Default", [bad]).is_err(), "{bad}");
        }
    }

    #[test]
    fn explicit_effort_must_be_declared_exactly_and_default_always_fits() {
        let m = model(&["Low", "High"], false, None);
        assert!(supports_effort(&m, "Default"));
        assert!(supports_effort(&m, "High"));
        assert!(!supports_effort(&m, "Medium"), "no nearest-level mapping");
        let need = Need::parse("Medium", []).unwrap();
        let a = assess(
            &m.id,
            Some(&m),
            &connected(None),
            &BTreeMap::new(),
            &need,
            None,
        );
        assert_eq!(a.blocks, [Block::EffortUnsupported]);
    }

    #[test]
    fn blocks_order_cooling_before_credentials_and_unsupported() {
        let m = model(&[], true, Some(10));
        let need = Need::parse("Default", []).unwrap();
        let cooling: BTreeMap<String, i64> = [("m".to_owned(), 99)].into();
        let a = assess(&m.id, Some(&m), &connected(None), &cooling, &need, None);
        assert_eq!(a.only_cooling_until(), Some(99));
        assert_eq!(
            unavailable(std::slice::from_ref(&a)),
            Unavailable::Cooling(99)
        );
        // Not discovered by its connection: a connection-side block, not a credential one.
        let a = assess(
            &m.id,
            Some(&m),
            &connected(Some(&["other"])),
            &BTreeMap::new(),
            &need,
            None,
        );
        assert_eq!(a.blocks, [Block::NotDiscovered]);
        assert_eq!(
            unavailable(std::slice::from_ref(&a)),
            Unavailable::Connection
        );
        // A cooling model that also lacks a capability does not set a wait.
        let big = Need::parse("Default", ["context:100"]).unwrap();
        let a = assess(&m.id, Some(&m), &connected(None), &cooling, &big, None);
        assert_eq!(a.only_cooling_until(), None);
        assert_eq!(
            unavailable(std::slice::from_ref(&a)),
            Unavailable::Unsupported
        );
        assert_eq!(unavailable(&[]), Unavailable::EmptyPool);
        assert_eq!(block_codes(&[]), ["EmptyPool"]);
        // No connection of the profile at all.
        let a = assess(&m.id, Some(&m), &[], &BTreeMap::new(), &need, None);
        assert_eq!(a.blocks, [Block::NoConnection]);
        assert_eq!(
            unavailable(std::slice::from_ref(&a)),
            Unavailable::Credentials
        );
    }
}
