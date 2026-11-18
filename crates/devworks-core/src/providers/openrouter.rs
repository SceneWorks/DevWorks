//! The OpenRouter request fields a stored routing policy adds to a chat request.
use crate::protocol::records::{OpenRouterConstraints, OpenRouterPercentiles};
use serde::Serialize;
use serde_json::{Value, json};

/// The upstream model name of OpenRouter's automatic route.
pub const OPEN_ROUTER_AUTO_UPSTREAM: &str = "openrouter/auto";

fn wire<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The members an OpenRouter Chat Completions request carries for `constraints` (for the
/// transport, DW-19): the `provider` routing object (OpenRouter's snake_case wire names) and,
/// for the automatic route, the auto-router plugin with the allowed/excluded model patterns and
/// cost tier (reference `OpenRouterRequest.Translate`).
pub fn open_router_request_fields(
    constraints: &OpenRouterConstraints,
    upstream_model: &str,
) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    let plugin_id = match upstream_model {
        OPEN_ROUTER_AUTO_UPSTREAM => Some("auto-router"),
        "openrouter/auto-beta" => Some("auto-beta-router"),
        _ => None,
    };
    if let Some(plugin_id) = plugin_id {
        let mut plugin = json!({"id": plugin_id});
        if let Some(t) = &constraints.cost_tier {
            plugin["cost_tier"] = json!(wire(t));
        }
        if !constraints.allowed_models.0.is_empty() {
            plugin["allowed_models"] = json!(
                constraints
                    .allowed_models
                    .0
                    .iter()
                    .map(|m| &m.0)
                    .collect::<Vec<_>>()
            );
        }
        if !constraints.excluded_models.0.is_empty() {
            plugin["excluded_models"] = json!(
                constraints
                    .excluded_models
                    .0
                    .iter()
                    .map(|m| &m.0)
                    .collect::<Vec<_>>()
            );
        }
        out.insert("plugins".into(), json!([plugin]));
    }
    if let Some(p) = &constraints.provider {
        let mut provider = serde_json::Map::new();
        let labels = |l: &crate::protocol::support::Items<crate::protocol::support::Label>| {
            json!(l.0.iter().map(|x| &x.0).collect::<Vec<_>>())
        };
        if let Some(v) = &p.order {
            provider.insert("order".into(), labels(v));
        }
        if let Some(v) = p.allow_fallbacks {
            provider.insert("allow_fallbacks".into(), json!(v));
        }
        if let Some(v) = p.require_parameters {
            provider.insert("require_parameters".into(), json!(v));
        }
        if let Some(v) = &p.data_collection {
            provider.insert("data_collection".into(), json!(wire(v)));
        }
        if let Some(v) = p.zdr {
            provider.insert("zdr".into(), json!(v));
        }
        if let Some(v) = p.enforce_distillable_text {
            provider.insert("enforce_distillable_text".into(), json!(v));
        }
        if let Some(v) = &p.only {
            provider.insert("only".into(), labels(v));
        }
        if let Some(v) = &p.ignore {
            provider.insert("ignore".into(), labels(v));
        }
        if let Some(v) = &p.quantizations {
            provider.insert(
                "quantizations".into(),
                json!(v.0.iter().map(wire).collect::<Vec<_>>()),
            );
        }
        if let Some(s) = &p.sort {
            let mut sort = json!({"by": wire(&s.by)});
            if let Some(part) = &s.partition {
                sort["partition"] = json!(wire(part));
            }
            provider.insert("sort".into(), sort);
        }
        let percentiles =
            |q: &OpenRouterPercentiles| serde_json::to_value(q).unwrap_or(Value::Null);
        if let Some(v) = &p.preferred_min_throughput {
            provider.insert("preferred_min_throughput".into(), percentiles(v));
        }
        if let Some(v) = &p.preferred_max_latency {
            provider.insert("preferred_max_latency".into(), percentiles(v));
        }
        if let Some(m) = &p.max_price {
            // USD amounts; OpenRouter takes numbers.
            let mut price = serde_json::Map::new();
            for (k, v) in [
                ("prompt", &m.prompt),
                ("completion", &m.completion),
                ("request", &m.request),
                ("image", &m.image),
            ] {
                if let Some(v) = v
                    && let Ok(n) = v.0.parse::<f64>()
                {
                    price.insert(k.into(), json!(n));
                }
            }
            provider.insert("max_price".into(), Value::Object(price));
        }
        if !provider.is_empty() {
            out.insert("provider".into(), Value::Object(provider));
        }
    }
    out
}
