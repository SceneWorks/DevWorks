//! Tariff rows an official source implies but does not tabulate: cache prices for service
//! tiers and the regional multiplier. The source adapter reports the published rows and which
//! modifiers the source declares; these rules decide the derived rows.
use crate::ports::usage::{
    DeclaredPriceModifiers, FetchedPriceCatalog, MAX_PUBLISHED_TARIFFS, OfficialPriceProvider,
    PriceFetchError, PublishedTariff,
};

fn changed(message: &str) -> PriceFetchError {
    PriceFetchError::SourceChanged(message.to_owned())
}

fn fixed_rate(amount: &str) -> Result<u128, PriceFetchError> {
    let (whole, fraction) = amount.split_once('.').unwrap_or((amount, ""));
    if fraction.len() > 12 {
        return Err(changed("rate exceeds exact precision"));
    }
    let whole: u128 = whole.parse().map_err(|_| changed("invalid rate"))?;
    let fractional: u128 = if fraction.is_empty() {
        0
    } else {
        fraction.parse().map_err(|_| changed("invalid rate"))?
    };
    whole
        .checked_mul(1_000_000_000_000)
        .and_then(|v| v.checked_add(fractional * 10_u128.pow(12 - fraction.len() as u32)))
        .ok_or_else(|| changed("rate overflow"))
}

fn scale_rate(amount: &str, numerator: &str, denominator: &str) -> Result<String, PriceFetchError> {
    let numerator = fixed_rate(amount)?
        .checked_mul(fixed_rate(numerator)?)
        .ok_or_else(|| changed("rate multiplication overflow"))?;
    let denominator = fixed_rate(denominator)?;
    if denominator == 0 || numerator % denominator != 0 {
        return Err(changed("rate modifier exceeds exact precision"));
    }
    let value = numerator / denominator;
    let fractional = format!("{:012}", value % 1_000_000_000_000);
    let fractional = fractional.trim_end_matches('0');
    Ok(if fractional.is_empty() {
        (value / 1_000_000_000_000).to_string()
    } else {
        format!("{}.{fractional}", value / 1_000_000_000_000)
    })
}

fn complete_anthropic_rates(
    modifiers: DeclaredPriceModifiers,
    tariffs: &mut Vec<PublishedTariff>,
) -> Result<(), PriceFetchError> {
    let source = tariffs.clone();
    for selected in source
        .iter()
        .filter(|r| r.charge_kind == "Input" && matches!(r.service_tier.as_str(), "Batch" | "Fast"))
    {
        let declared = if selected.service_tier == "Batch" {
            modifiers.batch_cache_multipliers_stack
        } else {
            modifiers.fast_cache_multipliers_apply
        };
        if !declared {
            return Err(changed("cache/service modifier declaration changed"));
        }
        let same = |r: &&PublishedTariff| {
            r.upstream_model_id == selected.upstream_model_id
                && r.service_tier == "Standard"
                && r.context_lower_exclusive == selected.context_lower_exclusive
                && r.context_upper_inclusive == selected.context_upper_inclusive
                && r.modality == selected.modality
        };
        let base = source
            .iter()
            .filter(same)
            .find(|r| r.charge_kind == "Input")
            .ok_or_else(|| changed("missing base rate for service modifier"))?;
        for cache in source
            .iter()
            .filter(same)
            .filter(|r| matches!(r.charge_kind.as_str(), "CacheRead" | "CacheWrite"))
        {
            let mut rate = cache.clone();
            rate.service_tier = selected.service_tier.clone();
            if tariffs.iter().any(|existing| {
                existing.upstream_model_id == rate.upstream_model_id
                    && existing.service_tier == rate.service_tier
                    && existing.context_lower_exclusive == rate.context_lower_exclusive
                    && existing.context_upper_inclusive == rate.context_upper_inclusive
                    && existing.modality == rate.modality
                    && existing.charge_kind == rate.charge_kind
                    && existing.cache_ttl_seconds == rate.cache_ttl_seconds
                    && existing.region == rate.region
                    && existing.endpoint_profile == rate.endpoint_profile
                    && existing.unit == rate.unit
                    && existing.units == rate.units
            }) {
                continue;
            }
            rate.amount_usd =
                scale_rate(&cache.amount_usd, &selected.amount_usd, &base.amount_usd)?;
            tariffs.push(rate);
        }
    }
    if modifiers.regional_multiplier {
        for row in tariffs.clone() {
            let version: Vec<_> = row
                .upstream_model_id
                .split('-')
                .filter_map(|v| v.parse::<u32>().ok())
                .collect();
            if version.first().is_some_and(|v| {
                *v > 4 || (*v == 4 && version.get(1).is_some_and(|minor| *minor >= 6))
            }) {
                let mut regional = row;
                regional.region = Some("us".into());
                regional.amount_usd = scale_rate(&regional.amount_usd, "1.1", "1")?;
                tariffs.push(regional);
            }
        }
    }
    Ok(())
}

/// Add the rows a provider's declared modifiers imply to the parsed catalog. A declaration a
/// published tier depends on that is missing refuses the whole catalog.
pub fn derive_official_tariffs(catalog: &mut FetchedPriceCatalog) -> Result<(), PriceFetchError> {
    if catalog.provider == OfficialPriceProvider::Anthropic {
        complete_anthropic_rates(catalog.modifiers, &mut catalog.tariffs)?;
    }
    if catalog.tariffs.len() > MAX_PUBLISHED_TARIFFS {
        return Err(changed("too many tariff rows after declared modifiers"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(tier: &str, kind: &str, ttl: Option<i64>, amount: &str) -> PublishedTariff {
        PublishedTariff {
            upstream_model_id: "claude-unit-5-5".into(),
            service_tier: tier.into(),
            context_lower_exclusive: None,
            context_upper_inclusive: None,
            modality: "Text".into(),
            charge_kind: kind.into(),
            cache_ttl_seconds: ttl,
            unit: "Token".into(),
            units: 1_000_000,
            amount_usd: amount.into(),
            region: None,
            endpoint_profile: None,
        }
    }

    fn catalog(modifiers: DeclaredPriceModifiers) -> FetchedPriceCatalog {
        FetchedPriceCatalog {
            provider: OfficialPriceProvider::Anthropic,
            source_url: OfficialPriceProvider::Anthropic.source_url().into(),
            source_digest: "0".repeat(64),
            parser_version: "official-markdown-v1".into(),
            fetched_at_ms: 0,
            verified_at_ms: 0,
            effective_date: None,
            tariffs: vec![
                row("Standard", "Input", None, "4"),
                row("Standard", "CacheWrite", Some(300), "5"),
                row("Standard", "CacheWrite", Some(3600), "8"),
                row("Standard", "CacheRead", None, "0.20"),
                row("Standard", "Output", None, "20"),
                row("Fast", "Input", None, "8"),
                row("Fast", "Output", None, "40"),
                row("Batch", "Input", None, "2"),
                row("Batch", "Output", None, "10"),
            ],
            modifiers,
        }
    }

    const ALL: DeclaredPriceModifiers = DeclaredPriceModifiers {
        batch_cache_multipliers_stack: true,
        fast_cache_multipliers_apply: true,
        regional_multiplier: true,
    };

    #[test]
    fn service_modifiers_preserve_model_specific_cache_prices() {
        let mut parsed = catalog(ALL);
        derive_official_tariffs(&mut parsed).unwrap();
        let rate = |tier: &str, region: Option<&str>, kind: &str, ttl: Option<i64>| {
            parsed
                .tariffs
                .iter()
                .find(|r| {
                    r.service_tier == tier
                        && r.region.as_deref() == region
                        && r.charge_kind == kind
                        && r.cache_ttl_seconds == ttl
                })
                .unwrap()
                .amount_usd
                .clone()
        };
        assert_eq!(rate("Fast", None, "CacheRead", None), "0.4");
        assert_eq!(rate("Batch", None, "CacheRead", None), "0.1");
        assert_eq!(rate("Fast", Some("us"), "CacheWrite", Some(3600)), "17.6");
        // 9 published + 3 Fast cache + 3 Batch cache rows, then one regional row each.
        assert_eq!(parsed.tariffs.len(), 30);

        let mut undeclared = catalog(DeclaredPriceModifiers {
            fast_cache_multipliers_apply: false,
            ..ALL
        });
        assert_eq!(
            derive_official_tariffs(&mut undeclared),
            Err(changed("cache/service modifier declaration changed"))
        );
    }

    #[test]
    fn openai_rows_and_undeclared_regions_are_not_derived() {
        let mut openai = catalog(ALL);
        openai.provider = OfficialPriceProvider::OpenAi;
        let published = openai.tariffs.clone();
        derive_official_tariffs(&mut openai).unwrap();
        assert_eq!(openai.tariffs, published);

        let mut local = catalog(DeclaredPriceModifiers {
            regional_multiplier: false,
            ..ALL
        });
        derive_official_tariffs(&mut local).unwrap();
        assert_eq!(local.tariffs.len(), 15);
        assert!(local.tariffs.iter().all(|r| r.region.is_none()));
    }

    #[test]
    fn exact_decimal_scaling_refuses_lossy_modifiers() {
        assert_eq!(scale_rate("0.20", "8", "4").unwrap(), "0.4");
        assert_eq!(scale_rate("16", "1.1", "1").unwrap(), "17.6");
        assert!(scale_rate("1", "1", "3").is_err());
        assert!(scale_rate("1", "1", "0").is_err());
        assert!(fixed_rate("1.0000000000001").is_err());
    }

    #[test]
    fn a_tier_without_its_standard_base_is_refused() {
        let mut parsed = catalog(ALL);
        parsed
            .tariffs
            .retain(|r| !(r.service_tier == "Standard" && r.charge_kind == "Input"));
        assert_eq!(
            derive_official_tariffs(&mut parsed),
            Err(changed("missing base rate for service modifier"))
        );
    }
}
