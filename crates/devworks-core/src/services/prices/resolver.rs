use crate::ports::storage::{ReadSnapshot, StorageError, StoredConnection};
use crate::ports::usage::{
    CacheWriteRate, CostClassification, OfficialPriceProvider, PriceQuery, PriceQuote,
    PriceResolver, PublishedTariff, UsageRates,
};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Default)]
pub struct CatalogPriceResolver;

/// Official price-source identity is distinct from the configured billing transport. A
/// compatible protocol at an arbitrary proxy does not establish that proxy's prices.
pub fn official_price_source<'a>(
    connection: &StoredConnection,
    upstream: &'a str,
) -> Option<(OfficialPriceProvider, &'a str)> {
    if connection.local_free {
        return None;
    }
    let is_origin = |host: &str| {
        url::Url::parse(&connection.endpoint).is_ok_and(|u| {
            u.scheme() == "https"
                && u.host_str() == Some(host)
                && u.port_or_known_default() == Some(443)
        })
    };
    match connection.profile.as_str() {
        "CodexSubscriptionResponses" => Some((OfficialPriceProvider::OpenAi, upstream)),
        "ClaudeSubscriptionMessages" => Some((OfficialPriceProvider::Anthropic, upstream)),
        "OpenAiResponses" | "CompatibleChat" | "Embedding" if is_origin("api.openai.com") => {
            Some((OfficialPriceProvider::OpenAi, upstream))
        }
        "AnthropicMessages" if is_origin("api.anthropic.com") => {
            Some((OfficialPriceProvider::Anthropic, upstream))
        }
        "OpenRouterChat" if is_origin("openrouter.ai") => match upstream.split_once('/') {
            Some(("openai", model)) => Some((OfficialPriceProvider::OpenAi, model)),
            Some(("anthropic", model)) => Some((OfficialPriceProvider::Anthropic, model)),
            _ => None,
        },
        _ => None,
    }
}

impl PriceResolver for CatalogPriceResolver {
    fn quote(
        &self,
        read: &dyn ReadSnapshot,
        query: &PriceQuery,
    ) -> Result<PriceQuote, StorageError> {
        let mut quote = PriceQuote {
            classification: CostClassification::Untracked,
            connection_revision: None,
            publication_provider: None,
            snapshot_id: None,
            price_revision: None,
            override_revision: None,
            currency: "USD".into(),
            rates: None,
            tariffs: Vec::new(),
            untracked_reason: Some("connection_unavailable".into()),
        };
        let Some(connection) = read.router().connection(&query.connection_id)? else {
            return Ok(quote);
        };
        quote.connection_revision = Some(connection.revision);
        if connection.profile != query.endpoint_profile {
            quote.untracked_reason = Some("route_profile_changed".into());
            return Ok(quote);
        }
        quote.override_revision = Some(
            read.usage()
                .price_override_revision(&query.catalog_model_id)?,
        );
        let subscription = matches!(
            connection.profile.as_str(),
            "CodexSubscriptionResponses" | "ClaudeSubscriptionMessages"
        );
        if connection.local_free {
            quote.classification = CostClassification::LocalFree;
            quote.untracked_reason = None;
            return Ok(quote);
        }
        if query.required_modalities.is_empty() {
            quote.untracked_reason = Some("input_modality_unavailable".into());
            return Ok(quote);
        }
        let Some(upstream) = query.provider_model_id.as_deref() else {
            quote.untracked_reason = Some("resolved_model_unavailable".into());
            return Ok(quote);
        };
        let configured = read.usage().price_override(&query.catalog_model_id)?;
        let (source, classification) = if let Some(override_) =
            configured.filter(|v| v.mode == "Set")
        {
            (
                override_.tariffs,
                if subscription {
                    CostClassification::SubscriptionApiEquivalent
                } else {
                    CostClassification::ConfiguredEstimate
                },
            )
        } else {
            let Some((provider, canonical)) = official_price_source(&connection, upstream) else {
                quote.untracked_reason = Some(
                    if connection.profile == "OpenRouterChat" {
                        "awaiting_reported_or_configured_cost"
                    } else {
                        "custom_price_not_configured"
                    }
                    .into(),
                );
                return Ok(quote);
            };
            // Capture even a missing publication. A later first refresh must force a requote
            // before admission rather than silently admitting this old Untracked quotation.
            quote.publication_provider = Some(provider);
            let Some(snapshot) = read.usage().current_price_snapshot(provider.id())? else {
                quote.untracked_reason = Some("official_catalog_unavailable".into());
                return Ok(quote);
            };
            quote.snapshot_id = Some(snapshot.id);
            quote.price_revision = Some(snapshot.revision);
            let mut source = snapshot.tariffs;
            // An explicit OpenRouter provider namespace identifies the requested upstream. It
            // does not make OpenRouter's reported charge equal to an official API estimate.
            if canonical != upstream {
                for row in &mut source {
                    if row.upstream_model_id == canonical {
                        row.upstream_model_id = upstream.into();
                    }
                }
            }
            let class = if matches!(
                connection.profile.as_str(),
                "CodexSubscriptionResponses" | "ClaudeSubscriptionMessages"
            ) {
                CostClassification::SubscriptionApiEquivalent
            } else {
                CostClassification::OfficialEstimate
            };
            (source, class)
        };
        let (tariffs, reason) = applicable(&source, query, upstream);
        if tariffs.is_empty() {
            quote.untracked_reason = Some(reason.into());
            return Ok(quote);
        }
        quote.classification = classification;
        quote.rates = token_rates(&tariffs, query.cache_write_ttl_ms);
        quote.tariffs = tariffs;
        quote.untracked_reason = None;
        Ok(quote)
    }
}

fn applicable(
    source: &[PublishedTariff],
    query: &PriceQuery,
    upstream: &str,
) -> (Vec<PublishedTariff>, &'static str) {
    let mut rows: Vec<_> = source
        .iter()
        .filter(|r| r.upstream_model_id == upstream)
        .cloned()
        .collect();
    if rows.is_empty() {
        return (rows, "upstream_model_unmapped");
    }
    rows.retain(|r| r.service_tier == query.service_tier.as_deref().unwrap_or("Standard"));
    if rows.is_empty() {
        return (rows, "service_tier_unpriced");
    }
    rows.retain(|r| {
        r.region.as_deref().unwrap_or("global")
            == query.processing_region.as_deref().unwrap_or("global")
    });
    if rows.is_empty() {
        return (rows, "processing_region_unpriced");
    }
    rows.retain(|r| {
        r.endpoint_profile
            .as_deref()
            .is_none_or(|p| p == query.endpoint_profile)
    });
    if rows.is_empty() {
        return (rows, "endpoint_profile_unpriced");
    }
    rows.retain(|r| r.modality == "Any" || query.required_modalities.contains(&r.modality));
    if rows.is_empty() {
        return (rows, "modality_unpriced");
    }
    rows.retain(|r| match query.input_tokens {
        Some(n) => {
            r.context_lower_exclusive.is_none_or(|lower| n > lower)
                && r.context_upper_inclusive.is_none_or(|upper| n <= upper)
        }
        None => r.context_lower_exclusive.is_none() && r.context_upper_inclusive.is_none(),
    });
    if rows.is_empty() {
        return (rows, "input_quantity_unavailable_or_outside_tariff");
    }
    rows.retain(|r| match query.cache_write_ttl_ms {
        Some(ttl) if r.charge_kind == "CacheWrite" => r
            .cache_ttl_seconds
            .is_none_or(|seconds| seconds.checked_mul(1000) == Some(ttl)),
        _ => true,
    });
    (rows, "cache_lifetime_unpriced")
}

fn token_rates(rows: &[PublishedTariff], ttl_ms: Option<i64>) -> Option<UsageRates> {
    let text: Vec<_> = rows
        .iter()
        .filter(|r| {
            r.unit == "Token"
                && r.units == 1_000_000
                && matches!(r.modality.as_str(), "Text" | "Any")
        })
        .collect();
    if text.is_empty() {
        return None;
    }
    let unique = |kind: &str| {
        let amounts: BTreeSet<_> = text
            .iter()
            .filter(|r| r.charge_kind == kind)
            .map(|r| r.amount_usd.clone())
            .collect();
        (amounts.len() == 1).then(|| amounts.into_iter().next().unwrap())
    };
    let cache_writes: Vec<_> = text
        .iter()
        .filter(|r| r.charge_kind == "CacheWrite")
        .map(|r| CacheWriteRate {
            ttl_ms: r.cache_ttl_seconds.and_then(|s| s.checked_mul(1000)),
            usd_per_million: r.amount_usd.clone(),
        })
        .collect();
    let cache_write = if ttl_ms.is_some() || cache_writes.len() == 1 {
        unique("CacheWrite")
    } else {
        None
    };
    Some(UsageRates {
        input_usd_per_million: unique("Input"),
        output_usd_per_million: unique("Output"),
        cache_read_usd_per_million: unique("CacheRead"),
        cache_write_usd_per_million: cache_write,
        cache_writes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Id;
    fn query() -> PriceQuery {
        PriceQuery {
            catalog_model_id: Id::new("model-1").unwrap(),
            provider_model_id: Some("unit-model".into()),
            connection_id: Id::new("connection-1").unwrap(),
            endpoint_profile: "OpenAiResponses".into(),
            access_mode: "Api".into(),
            service_tier: None,
            input_tokens: Some(272_000),
            cache_write_ttl_ms: None,
            processing_region: None,
            required_modalities: vec!["Text".into()],
        }
    }
    fn tariff() -> PublishedTariff {
        PublishedTariff {
            upstream_model_id: "unit-model".into(),
            service_tier: "Standard".into(),
            context_lower_exclusive: None,
            context_upper_inclusive: Some(272_000),
            modality: "Text".into(),
            charge_kind: "Input".into(),
            cache_ttl_seconds: None,
            unit: "Token".into(),
            units: 1_000_000,
            amount_usd: "1.25".into(),
            region: None,
            endpoint_profile: Some("OpenAiResponses".into()),
        }
    }
    fn connection(profile: &str, endpoint: &str) -> StoredConnection {
        StoredConnection {
            id: Id::new("connection-1").unwrap(),
            name: "Test".into(),
            profile: profile.into(),
            endpoint: endpoint.into(),
            enabled: true,
            local_free: false,
            open_router: None,
            credential_ref: None,
            credential_revision: 0,
            account_revision: 1,
            refresh_operation_id: None,
            credential_epoch: None,
            lifecycle: "Disconnected".into(),
            state_reason: None,
            account_digest: None,
            access_expires_at_ms: None,
            risk: None,
            discovered: None,
            discovered_at_ms: None,
            healthy: None,
            checked_at_ms: None,
            revision: 1,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }
    #[test]
    fn official_source_uses_actual_origin_without_declaring_a_proxy_free() {
        let proxy = connection("OpenAiResponses", "https://proxy.example/v1");
        assert!(official_price_source(&proxy, "unit-model").is_none());
        for endpoint in [
            "https://api.openai.com.evil.example/v1",
            "http://api.openai.com/v1",
            "https://api.openai.com:8443/v1",
        ] {
            assert!(
                official_price_source(&connection("OpenAiResponses", endpoint), "unit-model")
                    .is_none()
            );
        }
        let direct_embedding = connection("Embedding", "https://api.openai.com/v1");
        assert_eq!(
            official_price_source(&direct_embedding, "text-embedding-3-small"),
            Some((OfficialPriceProvider::OpenAi, "text-embedding-3-small"))
        );
        let mut local = direct_embedding;
        local.local_free = true;
        assert!(official_price_source(&local, "text-embedding-3-small").is_none());
    }
    #[test]
    fn namespaces_describe_price_sources_only_at_the_declared_transport() {
        let openrouter = connection("OpenRouterChat", "https://openrouter.ai/api/v1");
        assert_eq!(
            official_price_source(&openrouter, "anthropic/claude-unit"),
            Some((OfficialPriceProvider::Anthropic, "claude-unit"))
        );
        assert!(official_price_source(&openrouter, "openrouter/auto").is_none());
        assert!(
            official_price_source(
                &connection("OpenRouterChat", "https://proxy.example"),
                "anthropic/claude-unit"
            )
            .is_none()
        );
        // The subscription profile explicitly requests an API-equivalent estimate, not the
        // endpoint's asserted subscription charge. Availability/accepted risk is a router gate.
        assert_eq!(
            official_price_source(
                &connection("ClaudeSubscriptionMessages", "https://api.anthropic.com"),
                "claude-unit"
            ),
            Some((OfficialPriceProvider::Anthropic, "claude-unit"))
        );
    }
    #[test]
    fn context_region_and_profile_never_silently_fall_back_to_a_cheaper_tariff() {
        let short = tariff();
        let mut long = short.clone();
        long.context_upper_inclusive = None;
        long.context_lower_exclusive = Some(272_000);
        long.amount_usd = "2.50".into();
        let source = vec![short, long];
        let mut q = query();
        assert_eq!(
            applicable(&source, &q, "unit-model").0[0].amount_usd,
            "1.25"
        );
        q.input_tokens = Some(272_001);
        assert_eq!(
            applicable(&source, &q, "unit-model").0[0].amount_usd,
            "2.50"
        );
        q.input_tokens = None;
        assert!(applicable(&source, &q, "unit-model").0.is_empty());
        q.input_tokens = Some(100);
        q.processing_region = Some("us".into());
        assert_eq!(
            applicable(&source, &q, "unit-model").1,
            "processing_region_unpriced"
        );
        q.processing_region = None;
        q.endpoint_profile = "CompatibleChat".into();
        assert_eq!(
            applicable(&source, &q, "unit-model").1,
            "endpoint_profile_unpriced"
        );
    }
    #[test]
    fn unknown_cache_lifetime_retains_both_rates_without_inventing_a_discount() {
        let mut short = tariff();
        short.charge_kind = "CacheWrite".into();
        short.cache_ttl_seconds = Some(1800);
        let mut long = short.clone();
        long.cache_ttl_seconds = Some(43200);
        long.amount_usd = "2.50".into();
        let source = vec![tariff(), short, long];
        let mut q = query();
        let applicable = super::applicable(&source, &q, "unit-model").0;
        let rates = token_rates(&applicable, None).unwrap();
        assert_eq!(rates.input_usd_per_million.as_deref(), Some("1.25"));
        assert!(rates.cache_write_usd_per_million.is_none());
        assert_eq!(rates.cache_writes.len(), 2);
        q.cache_write_ttl_ms = Some(43_200_000);
        let selected = super::applicable(&source, &q, "unit-model").0;
        assert_eq!(
            token_rates(&selected, q.cache_write_ttl_ms)
                .unwrap()
                .cache_write_usd_per_million
                .as_deref(),
            Some("2.50")
        );
    }
}
