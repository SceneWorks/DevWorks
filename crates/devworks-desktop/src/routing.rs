//! The composer's route and effort selectors (UI-10, ROUTE-19, ROUTE-20), shared by Home and the
//! coordinator sidebar because both render the same conversation key.
//!
//! The route selector offers Tier Max, High, Mid and Low first, then the named-model group from
//! `router.catalog`: the eight stable latest-family choices, Open Router Auto, and available
//! concrete versions (custom models included). Unavailable records remain in Settings. Effort
//! is a separate selector: Provider default, plus the explicit efforts the
//! selected route declares (a tier offers what its members declare; picking one makes the tier's
//! other members ineligible rather than silently dropping it). Labels never encode effort.
//!
//! A conversation's selection is its durable pin once it has one (`router.availability`
//! revision 2 reports it); a change is the explicit `router.pin` command. Before the first pin
//! (a new chat, or one no request has routed yet) the selection is view state kept in
//! `ui-state.json` and sent with the conversation's first routed request. The status line shows
//! the pinned model or the truthful availability of the selection: its blockers, and the
//! earliest cooldown expiry when that is all it waits for (rechecked then, never earlier).
use devworks_core::protocol::records::{
    EffortName, RouteAvailability, RouteCatalogModel, RouteChoice, RouteIntent, TierName,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The selection of a conversation that has no pin yet.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Selection {
    pub intent: RouteIntent,
    pub effort: EffortName,
}
impl PartialEq for Selection {
    fn eq(&self, other: &Self) -> bool {
        same_intent(&self.intent, &other.intent) && wire(&self.effort) == wire(&other.effort)
    }
}
impl Default for Selection {
    /// Tier High with the provider's default effort (the reference router's default tier).
    fn default() -> Self {
        Self {
            intent: RouteIntent::Tier {
                tier: TierName::High,
            },
            effort: EffortName::Default,
        }
    }
}

/// What the router last said about a conversation's selection.
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub availability: Option<RouteAvailability>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct Routing {
    /// The complete query catalog; selection options filter unavailable concrete models.
    pub catalog: Vec<RouteCatalogModel>,
    pub catalog_loaded: bool,
    pub catalog_error: Option<String>,
    /// Selections by conversation key (a session ID, or the new-chat draft key).
    pub selections: BTreeMap<String, Selection>,
    pub status: BTreeMap<String, Status>,
    /// Conversations whose first message is saved but whose routing waits for the router to
    /// say whether they are pinned yet (kept across a restart).
    pub first_route: std::collections::BTreeSet<String>,
    /// The cooldown expiry each conversation's status was last rechecked for, so a router
    /// that still reports it (clocks differ slightly) is not asked again every frame.
    pub rechecked: BTreeMap<String, i64>,
    /// The catalog load being gathered page by page (the shown catalog changes only when it is
    /// complete), and the generation of the latest load started.
    pub catalog_load: Option<CatalogLoad>,
    pub catalog_generation: u64,
    /// Conversations whose first route is being saved: their selection is the one it carries,
    /// so it cannot change until the pin is confirmed (or the route refused).
    pub first_in_flight: std::collections::BTreeSet<String>,
    /// Conversations whose explicit pin change is submitted and not yet decided (kept while its
    /// outcome is unknown, and restored with it after a relaunch).
    pub pin_in_flight: std::collections::BTreeSet<String>,
    /// The selection each conversation's availability query in flight asks about: a refresh
    /// for the same selection waits for it instead of asking again, so a slow router never
    /// fills the client's few workers with identical queries.
    pub status_asked: BTreeMap<String, Selection>,
}

/// One catalog load: the pages gathered so far.
#[derive(Clone, Debug, Default)]
pub struct CatalogLoad {
    pub generation: u64,
    pub items: Vec<RouteCatalogModel>,
}

/// What a catalog page did to the load it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogPage {
    /// A page of a load a newer one replaced: ignored.
    Stale,
    /// Gathered; the next page continues after this cursor.
    More(String),
    /// The last page: the gathered catalog is now shown.
    Complete,
}
impl Routing {
    /// Forgets everything derived from the catalog and routing settings (after they changed):
    /// the catalog is reloaded and each conversation's availability asked again.
    pub fn invalidate(&mut self) {
        self.catalog_loaded = false;
        self.status.clear();
        self.rechecked.clear();
    }
    /// Starts a new catalog load, replacing any load still gathering pages.
    pub fn begin_catalog_load(&mut self) -> u64 {
        self.catalog_generation += 1;
        self.catalog_load = Some(CatalogLoad {
            generation: self.catalog_generation,
            items: Vec::new(),
        });
        self.catalog_generation
    }
    /// Takes one page of load `generation`; pages of any other load never mix in.
    pub fn catalog_page(
        &mut self,
        generation: u64,
        items: Vec<RouteCatalogModel>,
        next: Option<String>,
    ) -> CatalogPage {
        let Some(load) = self
            .catalog_load
            .as_mut()
            .filter(|l| l.generation == generation)
        else {
            return CatalogPage::Stale;
        };
        load.items.extend(items);
        match next {
            Some(cursor) => CatalogPage::More(cursor),
            None => {
                self.catalog = self
                    .catalog_load
                    .take()
                    .map(|l| l.items)
                    .unwrap_or_default();
                self.catalog_loaded = true;
                self.catalog_error = None;
                CatalogPage::Complete
            }
        }
    }
    /// Load `generation` stopped without a reply (its next page could not be submitted): it is
    /// no longer in progress, and the catalog stays as shown (and unloaded if it never was).
    pub fn abandon_catalog_load(&mut self, generation: u64) {
        if self
            .catalog_load
            .as_ref()
            .is_some_and(|l| l.generation == generation)
        {
            self.catalog_load = None;
        }
    }
    /// A failed page of load `generation` ends that load (the shown catalog is kept).
    pub fn catalog_failed(&mut self, generation: u64, error: String) {
        if self
            .catalog_load
            .as_ref()
            .is_some_and(|l| l.generation == generation)
        {
            self.catalog_load = None;
            self.catalog_loaded = true;
            self.catalog_error = Some(error);
        }
    }
    /// Whether the conversation's selection may change now: not while its first route, which
    /// carries the current selection, is waiting or being saved.
    pub fn selection_locked(&self, key: &str) -> bool {
        self.first_route.contains(key)
            || self.first_in_flight.contains(key)
            || self.pin_in_flight.contains(key)
    }
}

/// Accessible names of the two selectors.
pub const ROUTE_LABEL: &str = "Model or tier";
pub const EFFORT_LABEL: &str = "Effort";
/// The tiers, in selector order.
pub const TIERS: [(TierName, &str); 4] = [
    (TierName::Max, "Tier Max"),
    (TierName::High, "Tier High"),
    (TierName::Mid, "Tier Mid"),
    (TierName::Low, "Tier Low"),
];
/// Stable logical routes are always offered, including before discovery completes.
pub const LATEST_FAMILIES: [(&str, &str); 8] = [
    ("astra-latest", "Astra Latest"),
    ("sol-latest", "Sol Latest"),
    ("terra-latest", "Terra Latest"),
    ("luna-latest", "Luna Latest"),
    ("fable-latest", "Fable Latest"),
    ("opus-latest", "Opus Latest"),
    ("sonnet-latest", "Sonnet Latest"),
    ("haiku-latest", "Haiku Latest"),
];

pub fn is_latest(m: &RouteCatalogModel) -> bool {
    devworks_core::services::router::latest::alias_family(m.id.as_str()).is_some()
}

pub fn selectable(m: &RouteCatalogModel) -> bool {
    is_latest(m) || (m.enabled && (is_open_router_auto(m) || m.available != Some(false)))
}

const OPEN_ROUTER_AUTO_UPSTREAM: &str = "openrouter/auto";

fn wire<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Whether a catalog model is OpenRouter's own automatic route.
pub fn is_open_router_auto(m: &RouteCatalogModel) -> bool {
    wire(&m.endpoint_profile) == "OpenRouterChat"
        && m.upstream_model_id.0 == OPEN_ROUTER_AUTO_UPSTREAM
}

/// The intent a catalog entry selects: Open Router Auto is its own intent, every other model is
/// named directly (never translated to a tier).
pub fn intent_of(m: &RouteCatalogModel) -> RouteIntent {
    if is_open_router_auto(m) {
        RouteIntent::OpenRouterAuto {}
    } else {
        RouteIntent::Model {
            model_id: m.id.clone(),
        }
    }
}

/// The four tiers, eight stable family choices, then offered concrete versions and upstream auto.
pub fn options(catalog: &[RouteCatalogModel]) -> Vec<(String, RouteIntent)> {
    let mut out: Vec<(String, RouteIntent)> = TIERS
        .iter()
        .map(|(t, l)| ((*l).to_owned(), RouteIntent::Tier { tier: t.clone() }))
        .collect();
    out.extend(LATEST_FAMILIES.iter().map(|(id, label)| {
        (
            catalog
                .iter()
                .find(|m| m.id.as_str() == *id)
                .map(|m| m.display_name.0.clone())
                .unwrap_or_else(|| (*label).into()),
            RouteIntent::Model {
                model_id: devworks_core::protocol::Id::new(*id).expect("stable alias"),
            },
        )
    }));
    out.extend(
        catalog
            .iter()
            .filter(|m| !is_latest(m) && selectable(m))
            .map(|m| {
                let label = if m.available.is_none() {
                    format!("{} (availability unknown)", m.display_name.0)
                } else {
                    m.display_name.0.clone()
                };
                (label, intent_of(m))
            }),
    );
    out
}

pub fn same_intent(a: &RouteIntent, b: &RouteIntent) -> bool {
    serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
}

/// The label of an intent.
pub fn intent_label(catalog: &[RouteCatalogModel], intent: &RouteIntent) -> String {
    options(catalog)
        .into_iter()
        .find(|(_, i)| same_intent(i, intent))
        .map(|(l, _)| l)
        .unwrap_or_else(|| match intent {
            RouteIntent::Model { model_id } => catalog
                .iter()
                .find(|m| &m.id == model_id)
                .map(|m| m.display_name.0.clone())
                .unwrap_or_else(|| model_id.as_str().to_owned()),
            RouteIntent::OpenRouterAuto {} => "Open Router Auto".into(),
            RouteIntent::Tier { tier } => format!("Tier {}", wire(tier)),
        })
}

fn models_of<'c>(
    catalog: &'c [RouteCatalogModel],
    intent: &RouteIntent,
) -> Vec<&'c RouteCatalogModel> {
    catalog
        .iter()
        .filter(|m| m.enabled && m.available != Some(false))
        .filter(|m| match intent {
            RouteIntent::Tier { tier } => {
                m.tier_memberships.0.iter().any(|t| wire(t) == wire(tier))
            }
            RouteIntent::Model { model_id } => {
                if devworks_core::services::router::latest::alias_family(model_id.as_str())
                    .is_some()
                {
                    catalog
                        .iter()
                        .find(|a| &a.id == model_id)
                        .filter(|a| a.available == Some(true))
                        .and_then(|a| a.resolves_to_model_ids.as_ref())
                        .is_some_and(|ids| ids.0.contains(&m.id))
                } else {
                    &m.id == model_id
                }
            }
            RouteIntent::OpenRouterAuto {} => is_open_router_auto(m),
        })
        .collect()
}

/// The efforts offered for `intent`: Provider default, then every explicit effort a pooled
/// model declares (lowest first).
pub fn efforts(catalog: &[RouteCatalogModel], intent: &RouteIntent) -> Vec<EffortName> {
    const ORDER: [EffortName; 6] = [
        EffortName::Minimal,
        EffortName::Low,
        EffortName::Medium,
        EffortName::High,
        EffortName::XHigh,
        EffortName::Max,
    ];
    let declared: Vec<String> = models_of(catalog, intent)
        .iter()
        .flat_map(|m| {
            if is_latest(m) {
                m.resolves_to_model_ids
                    .as_ref()
                    .map(|ids| {
                        ids.0
                            .iter()
                            .filter_map(|id| {
                                catalog.iter().find(|target| {
                                    &target.id == id
                                        && target.enabled
                                        && target.available != Some(false)
                                })
                            })
                            .flat_map(|target| target.efforts.0.iter().map(wire))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            } else {
                m.efforts.0.iter().map(wire).collect()
            }
        })
        .collect();
    let mut out = vec![EffortName::Default];
    out.extend(
        ORDER
            .into_iter()
            .filter(|e| declared.iter().any(|d| *d == wire(e))),
    );
    out
}

pub fn effort_label(e: &EffortName) -> String {
    match e {
        EffortName::Default => "Provider default".into(),
        EffortName::XHigh => "Extra high".into(),
        other => wire(other),
    }
}

/// A blocker code in words.
pub fn reason(code: &str) -> &'static str {
    match code {
        "NoConnection" => "no provider connection (add one in Settings)",
        "NotAllowed" => "excluded by its connection's model constraints",
        "CredentialsRequired" => "sign-in required",
        "ConnectionUnavailable" => "provider temporarily unavailable",
        "ConnectionDisabled" => "connection disabled",
        "NotDiscovered" => "not offered by its connection",
        "Cooling" => "cooling down after a provider limit",
        "Disabled" => "turned off in the catalog",
        "NotInCatalog" => "not in the catalog",
        "EffortUnsupported" => "the selected effort is not supported",
        "VisionUnsupported" => "images are not supported",
        "ToolsUnsupported" => "tools are not supported",
        "ContextUnknown" => "context window not declared",
        "ContextTooSmall" => "context window too small",
        "EmptyPool" => "no members are configured",
        "LatestUnavailable" => "no available version of this family",
        "InventoryIncomplete" => "waiting for complete provider inventory",
        _ => "unavailable",
    }
}

/// Local wall-clock time of a UTC millisecond instant.
pub fn local_time(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| "an unknown time".into())
}

/// The status line of a selection: the pinned route, or the selection's truthful availability.
pub fn status_line(
    catalog: &[RouteCatalogModel],
    selection: &Selection,
    status: Option<&Status>,
) -> String {
    let route = intent_label(catalog, &selection.intent);
    let effort = effort_label(&selection.effort);
    let Some(status) = status else {
        return format!("{route} · {effort} · checking availability…");
    };
    if let Some(e) = &status.error {
        return format!("{route} · {effort} · availability unknown: {e}");
    }
    let Some(a) = &status.availability else {
        return format!("{route} · {effort} · checking availability…");
    };
    if let (Some(pin), Some(true)) = (&a.pin, a.pin_eligible) {
        return format!("{route} · Pinned: {} · {effort}", model_label(pin));
    }
    let pinned = a
        .pin
        .as_ref()
        .map(|p| format!("Pinned: {} (not eligible now) · ", model_label(p)))
        .unwrap_or_default();
    if a.available {
        let names: Vec<String> = a
            .eligible_model_ids
            .0
            .iter()
            .map(|id| {
                catalog
                    .iter()
                    .find(|m| &m.id == id)
                    .map_or_else(|| id.as_str().to_owned(), |m| m.display_name.0.clone())
            })
            .collect();
        return format!(
            "{pinned}{route} · {effort} · available: {}",
            names.join(", ")
        );
    }
    let reasons: Vec<&str> = a
        .blocked_reason_codes
        .0
        .iter()
        .map(|c| reason(&c.0))
        .collect();
    let wait = a
        .retry_at_ms
        .as_ref()
        .map(|t| format!("; rechecked at {}", local_time(t.get())))
        .unwrap_or_default();
    format!(
        "{pinned}{route} · {effort} · no eligible model: {}{wait}",
        reasons.join("; ")
    )
}

fn model_label(pin: &RouteChoice) -> String {
    match &pin.resolved_upstream_model_id {
        Some(upstream) => {
            let provenance = match pin.resolved_identity_origin.as_ref().map(|s| s.0.as_str()) {
                Some("Migration25CurrentCatalog") => " (version captured during upgrade)",
                _ => "",
            };
            format!("{}{provenance}", upstream.0)
        }
        None => format!("{} (version unknown)", pin.model_id.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str) -> RouteCatalogModel {
        serde_json::from_value(serde_json::json!({
            "id": id, "displayName": id, "endpointProfile": "CompatibleChat",
            "upstreamModelId": id, "enabled": true, "tierMemberships": [], "efforts": [],
            "imageInput": false, "tools": true, "revision": "1"
        }))
        .unwrap()
    }
    fn ids(r: &Routing) -> Vec<String> {
        r.catalog.iter().map(|m| m.id.as_str().to_owned()).collect()
    }

    #[test]
    fn logical_options_are_stable_and_unavailable_concrete_rows_remain_inspectable() {
        let mut available = model("available-version");
        available.available = Some(true);
        let mut gone = model("retired-version");
        gone.available = Some(false);
        let unknown = model("manual-version");
        let catalog = vec![available, gone, unknown];
        let choices = options(&catalog);
        for (id, label) in LATEST_FAMILIES {
            assert!(choices.iter().any(|(l, i)| l == label
                && matches!(i, RouteIntent::Model { model_id } if model_id.as_str() == id)));
        }
        assert!(choices.iter().any(|(l, _)| l == "available-version"));
        assert!(!choices.iter().any(|(l, _)| l == "retired-version"));
        assert!(
            choices
                .iter()
                .any(|(l, _)| l == "manual-version (availability unknown)")
        );
        let retired = RouteIntent::Model {
            model_id: catalog[1].id.clone(),
        };
        assert_eq!(intent_label(&catalog, &retired), "retired-version");
        assert_eq!(
            catalog.len(),
            3,
            "query records remain available to Settings"
        );
    }

    #[test]
    fn latest_effort_comes_only_from_its_current_targets_and_pin_keeps_both_names() {
        let mut concrete = model("astra-v2");
        concrete.available = Some(true);
        concrete.efforts.0 = vec![EffortName::High];
        let mut alias = model("astra-latest");
        alias.available = Some(true);
        alias.resolves_to_model_ids =
            Some(serde_json::from_value(serde_json::json!([concrete.id])).unwrap());
        let selection = Selection {
            intent: intent_of(&alias),
            effort: EffortName::High,
        };
        let mut catalog = vec![alias, concrete];
        assert_eq!(efforts(&catalog, &selection.intent).len(), 2);
        catalog[0].available = Some(false);
        assert_eq!(efforts(&catalog, &selection.intent).len(), 1);
        assert_eq!(
            effort_label(&selection.effort),
            "High",
            "constraints are retained"
        );
        let availability: RouteAvailability = serde_json::from_value(serde_json::json!({
            "intent": {"kind":"Model", "modelId":"astra-latest"},
            "available":true, "eligibleModelIds":["astra-v1"], "blockedReasonCodes":[],
            "pinEligible":true,
            "pin":{"intent":{"kind":"Model","modelId":"astra-latest"},"modelId":"astra-v1",
                "connectionId":"codex", "requestedEffort":"High", "effectiveEffort":"High",
                "pinId":"pin-1", "pinRevision":"1", "resolvedUpstreamModelId":"astra-v1",
                "resolvedEndpointProfile":"CodexSubscriptionResponses", "resolvedIdentityOrigin":"AtResolution"}
        }))
        .unwrap();
        let text = status_line(
            &catalog,
            &selection,
            Some(&Status {
                availability: Some(availability),
                error: None,
            }),
        );
        assert!(
            text.contains("Astra Latest") && text.contains("Pinned: astra-v1"),
            "{text}"
        );
    }

    #[test]
    fn catalog_pages_are_gathered_and_never_mixed_across_loads() {
        let mut r = Routing::default();
        let first = r.begin_catalog_load();
        assert_eq!(
            r.catalog_page(first, vec![model("a1")], Some("c1".into())),
            CatalogPage::More("c1".into())
        );
        assert!(!r.catalog_loaded, "not loaded until every page arrived");
        // A refresh starts while the first load is still paging.
        let second = r.begin_catalog_load();
        assert_eq!(
            r.catalog_page(first, vec![model("a2")], None),
            CatalogPage::Stale
        );
        assert!(!r.catalog_loaded && r.catalog.is_empty());
        r.catalog_page(second, vec![model("b1")], Some("c2".into()));
        r.catalog_failed(first, "late".into());
        assert!(
            r.catalog_load.is_some(),
            "a stale failure does not end the newer load"
        );
        assert_eq!(
            r.catalog_page(second, vec![model("b2")], None),
            CatalogPage::Complete
        );
        assert!(r.catalog_loaded);
        assert_eq!(ids(&r), ["b1", "b2"]);
    }
}
