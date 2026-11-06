//! Price only measured components using the admitted quote. An unknown component remains
//! explicit alongside a known subtotal; a later catalog revision is never consulted here.
use super::meter::{InputSemantics, Quantities};
use super::money::{CostAccumulator, MoneyError, Rounding};
use crate::ports::usage::{CostClassification, PriceQuote};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PricingContext {
    pub input_semantics: InputSemantics,
    /// Host-declared profile capabilities, not the presence of a codec field.
    pub cache_read_possible: bool,
    pub cache_write_possible: bool,
    pub cache_write_ttl_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Estimate {
    pub tracked_micro_usd: Option<i64>,
    pub monetary_complete: bool,
    pub unknown_components: Vec<String>,
}

fn add_component(
    cost: &mut CostAccumulator,
    known: &mut bool,
    unknown: &mut Vec<String>,
    name: &str,
    quantity: Option<i64>,
    rate: Option<&str>,
) -> Result<(), MoneyError> {
    match (quantity, rate) {
        (Some(0), _) => *known = true,
        (Some(n), Some(rate)) => {
            cost.tokens(n, rate)?;
            *known = true;
        }
        (None, None) => {}
        _ => unknown.push(name.into()),
    }
    Ok(())
}

pub fn estimate(
    quote: &PriceQuote,
    quantities: &Quantities,
    context: &PricingContext,
) -> Result<Estimate, MoneyError> {
    quantities
        .validate()
        .map_err(|_| MoneyError::InvalidDecimal)?;
    if quote.classification == CostClassification::LocalFree {
        return Ok(Estimate {
            tracked_micro_usd: Some(0),
            monetary_complete: true,
            unknown_components: vec![],
        });
    }
    if quote.classification == CostClassification::Untracked || quote.currency != "USD" {
        return Ok(Estimate {
            tracked_micro_usd: None,
            monetary_complete: false,
            unknown_components: vec![
                quote
                    .untracked_reason
                    .clone()
                    .unwrap_or_else(|| "no usable USD rate".into()),
            ],
        });
    }
    let mut cost = CostAccumulator::default();
    let mut known = false;
    let mut unknown = Vec::new();
    let mut input = quantities.clone();
    // A missing category on a profile that cannot use it is inapplicable. A positive
    // contradictory observation remains unknown; it cannot disappear into a cheap estimate.
    let unexpected_read =
        !context.cache_read_possible && input.cached_read_tokens.is_some_and(|v| v > 0);
    let unexpected_write =
        !context.cache_write_possible && input.cache_write_tokens.is_some_and(|v| v > 0);
    if unexpected_read {
        unknown.push("profile contradicts observed cache read quantity".into());
    }
    if unexpected_write {
        unknown.push("profile contradicts observed cache write quantity".into());
    }
    if !context.cache_read_possible && !unexpected_read {
        input.cached_read_tokens = Some(0);
    }
    if !context.cache_write_possible && !unexpected_write {
        input.cache_write_tokens = Some(0);
    }
    if (unexpected_read || unexpected_write) && context.input_semantics == InputSemantics::Inclusive
    {
        input.input_tokens = None;
    }
    let uncached = input
        .uncached_input(context.input_semantics)
        .map_err(|_| MoneyError::InvalidDecimal)?;
    let rates = quote.rates.as_ref();
    let write = rates.and_then(|rates| {
        let matching = rates
            .cache_writes
            .iter()
            .filter(|rate| rate.ttl_ms == context.cache_write_ttl_ms)
            .collect::<Vec<_>>();
        if matching.len() == 1 {
            Some(matching[0].usd_per_million.as_str())
        } else if rates.cache_writes.is_empty() {
            rates.cache_write_usd_per_million.as_deref()
        } else {
            None
        }
    });
    for (name, kind, quantity, rate, applicable) in [
        (
            "input",
            "Input",
            uncached,
            rates.and_then(|r| r.input_usd_per_million.as_deref()),
            true,
        ),
        (
            "output",
            "Output",
            quantities.output_tokens,
            rates.and_then(|r| r.output_usd_per_million.as_deref()),
            true,
        ),
        (
            "cache read",
            "CacheRead",
            quantities.cached_read_tokens,
            rates.and_then(|r| r.cache_read_usd_per_million.as_deref()),
            context.cache_read_possible,
        ),
        (
            "cache write",
            "CacheWrite",
            quantities.cache_write_tokens,
            write,
            context.cache_write_possible,
        ),
    ] {
        if !applicable {
            continue;
        }
        let explicit = quantities
            .additional
            .iter()
            .filter(|q| q.unit == "Token" && q.charge_kind == kind)
            .collect::<Vec<_>>();
        if explicit.is_empty() {
            add_component(&mut cost, &mut known, &mut unknown, name, quantity, rate)?;
        } else if let Some(total) = quantity {
            let measured = explicit
                .iter()
                .try_fold(0_i64, |sum, q| sum.checked_add(q.quantity))
                .ok_or(MoneyError::Overflow)?;
            if measured > total {
                return Err(MoneyError::InvalidDecimal);
            }
            if measured < total {
                unknown.push(format!("unmeasured {name} modality remainder"));
            }
        }
    }
    for quantity in &quantities.additional {
        if (quantity.charge_kind == "CacheRead" && !context.cache_read_possible)
            || (quantity.charge_kind == "CacheWrite" && !context.cache_write_possible)
        {
            if quantity.quantity > 0 {
                unknown.push("profile contradicts measured cache unit".into());
            }
            continue;
        }
        let matching = quote
            .tariffs
            .iter()
            .filter(|rate| {
                rate.unit == quantity.unit
                    && rate.modality == quantity.modality
                    && rate.charge_kind == quantity.charge_kind
                    && rate
                        .cache_ttl_seconds
                        .and_then(|seconds| seconds.checked_mul(1000))
                        == quantity.cache_write_ttl_ms
            })
            .collect::<Vec<_>>();
        if quantity.quantity == 0 {
            known = true;
        } else if matching.len() == 1 {
            let rate = matching[0];
            cost.measure(quantity.quantity, &rate.amount_usd, rate.units)?;
            known = true;
        } else {
            unknown.push(format!(
                "{} {} {}",
                quantity.modality, quantity.charge_kind, quantity.unit
            ));
        }
    }
    // Non-token/modal rates in this quote require their own observed dimensional quantity.
    // Alternative TTL rows describe one write category; they are not separate charges.
    for tariff in &quote.tariffs {
        if tariff.unit == "Token" && matches!(tariff.modality.as_str(), "Text" | "Any") {
            continue;
        }
        if !quantities.additional.iter().any(|q| {
            q.unit == tariff.unit
                && q.modality == tariff.modality
                && q.charge_kind == tariff.charge_kind
        }) {
            let reason = format!(
                "unmeasured {} {} {}",
                tariff.modality, tariff.charge_kind, tariff.unit
            );
            if !unknown.contains(&reason) {
                unknown.push(reason);
            }
        }
    }
    Ok(Estimate {
        tracked_micro_usd: if known {
            Some(cost.micro_usd(Rounding::Nearest)?)
        } else {
            None
        },
        monetary_complete: unknown.is_empty(),
        unknown_components: unknown,
    })
}

/// Independently rounded component estimates for display, never summed into incurred spending.
/// Provider-reported whole charges do not establish a reported cache component charge.
pub fn cache_estimates(
    quote: &PriceQuote,
    quantities: &Quantities,
    context: &PricingContext,
) -> Result<(Option<i64>, Option<i64>), MoneyError> {
    let component =
        |kind: &str, count: Option<i64>, possible: bool| -> Result<Option<i64>, MoneyError> {
            let units = quantities
                .additional
                .iter()
                .filter(|q| q.charge_kind == kind)
                .cloned()
                .collect::<Vec<_>>();
            if count.is_none() && units.is_empty() {
                return Ok(None);
            }
            if !possible {
                return Ok(None);
            }
            let mut selected = quote.clone();
            selected.tariffs.retain(|t| t.charge_kind == kind);
            if let Some(rates) = &mut selected.rates {
                rates.input_usd_per_million = Some("0".into());
                rates.output_usd_per_million = Some("0".into());
            }
            let q = Quantities {
                input_tokens: Some(0),
                output_tokens: Some(0),
                cached_read_tokens: if kind == "CacheRead" { count } else { Some(0) },
                cache_write_tokens: if kind == "CacheWrite" { count } else { Some(0) },
                reasoning_tokens: None,
                additional: units,
            };
            let p = PricingContext {
                input_semantics: InputSemantics::Disjoint,
                cache_read_possible: kind == "CacheRead",
                cache_write_possible: kind == "CacheWrite",
                cache_write_ttl_ms: context.cache_write_ttl_ms,
            };
            let result = estimate(&selected, &q, &p)?;
            Ok(if result.monetary_complete {
                result.tracked_micro_usd
            } else {
                None
            })
        };
    Ok((
        component(
            "CacheRead",
            quantities.cached_read_tokens,
            context.cache_read_possible,
        )?,
        component(
            "CacheWrite",
            quantities.cache_write_tokens,
            context.cache_write_possible,
        )?,
    ))
}

/// Reserve validated total input plus output headroom. Cache reads never grant a provisional
/// discount; if cache writes may occur, use the highest applicable complete write/input rate.
pub fn reservation(
    quote: &PriceQuote,
    input_tokens: i64,
    output_headroom: i64,
    cache_write_possible: bool,
    unit_headroom: &[super::meter::MeasuredQuantity],
) -> Result<Estimate, MoneyError> {
    if input_tokens < 0 || output_headroom < 0 {
        return Err(MoneyError::InvalidDecimal);
    }
    Quantities {
        additional: unit_headroom.to_vec(),
        ..Default::default()
    }
    .validate()
    .map_err(|_| MoneyError::InvalidDecimal)?;
    if quote.classification == CostClassification::LocalFree {
        return Ok(Estimate {
            tracked_micro_usd: Some(0),
            monetary_complete: true,
            unknown_components: vec![],
        });
    }
    if quote.currency != "USD" || quote.classification == CostClassification::Untracked {
        return Ok(Estimate {
            tracked_micro_usd: None,
            monetary_complete: false,
            unknown_components: vec!["no usable reservation rates".into()],
        });
    }
    let mut base = CostAccumulator::default();
    let mut known = false;
    let mut unknown = Vec::new();
    for quantity in unit_headroom {
        let matching = quote
            .tariffs
            .iter()
            .filter(|rate| {
                rate.unit == quantity.unit
                    && rate.modality == quantity.modality
                    && rate.charge_kind == quantity.charge_kind
                    && (quantity.cache_write_ttl_ms.is_none()
                        || rate
                            .cache_ttl_seconds
                            .and_then(|seconds| seconds.checked_mul(1000))
                            == quantity.cache_write_ttl_ms)
            })
            .collect::<Vec<_>>();
        // For an unknown write TTL, independently price each valid option and take its
        // conservative maximum. Other ambiguous unit rates are never guessed.
        let choices = if (quantity.charge_kind == "CacheWrite"
            && quantity.cache_write_ttl_ms.is_none())
            || matching.len() == 1
        {
            matching
        } else {
            Vec::new()
        };
        if quantity.quantity == 0 {
            known = true;
            continue;
        }
        let mut highest: Option<(CostAccumulator, &crate::ports::usage::PublishedTariff)> = None;
        for rate in choices {
            let mut candidate = CostAccumulator::default();
            candidate.measure(quantity.quantity, &rate.amount_usd, rate.units)?;
            if highest
                .as_ref()
                .map(|(before, _)| candidate.compare(before))
                .transpose()?
                .is_none_or(|order| order.is_gt())
            {
                highest = Some((candidate, rate));
            }
        }
        if let Some((_, rate)) = highest {
            base.measure(quantity.quantity, &rate.amount_usd, rate.units)?;
            known = true;
        } else {
            unknown.push(format!(
                "{} {} {} reservation",
                quantity.modality, quantity.charge_kind, quantity.unit
            ));
        }
    }
    for tariff in &quote.tariffs {
        if tariff.unit == "Token" && matches!(tariff.modality.as_str(), "Text" | "Any") {
            continue;
        }
        if !unit_headroom.iter().any(|q| {
            q.unit == tariff.unit
                && q.modality == tariff.modality
                && q.charge_kind == tariff.charge_kind
        }) {
            let reason = format!(
                "unmeasured {} {} {} reservation",
                tariff.modality, tariff.charge_kind, tariff.unit
            );
            if !unknown.contains(&reason) {
                unknown.push(reason);
            }
        }
    }
    let rates = quote.rates.as_ref();
    let mut input_rates = Vec::new();
    if let Some(rate) = rates.and_then(|r| r.input_usd_per_million.as_deref()) {
        input_rates.push(rate);
    } else if input_tokens > 0 {
        unknown.push("input reservation".into());
    }
    if cache_write_possible && input_tokens > 0 {
        if let Some(rates) = rates {
            if let Some(rate) = &rates.cache_write_usd_per_million {
                input_rates.push(rate.as_str());
            }
            input_rates.extend(
                rates
                    .cache_writes
                    .iter()
                    .map(|r| r.usd_per_million.as_str()),
            );
            if rates.cache_write_usd_per_million.is_none() && rates.cache_writes.is_empty() {
                unknown.push("cache write reservation".into());
            }
        } else {
            unknown.push("cache write reservation".into());
        }
    }
    if let Some(rate) = rates.and_then(|r| r.output_usd_per_million.as_deref()) {
        base.tokens(output_headroom, rate)?;
        known = true;
    } else if output_headroom > 0 {
        unknown.push("output reservation".into());
    }
    let mut candidates = Vec::new();
    for rate in input_rates {
        let mut cost = base.clone();
        cost.tokens(input_tokens, rate)?;
        candidates.push(cost.micro_usd(Rounding::Up)?);
        known = true;
    }
    if candidates.is_empty()
        && (known || (input_tokens == 0 && output_headroom == 0 && unknown.is_empty()))
    {
        candidates.push(base.micro_usd(Rounding::Up)?);
    }
    Ok(Estimate {
        tracked_micro_usd: candidates.into_iter().max(),
        monetary_complete: unknown.is_empty(),
        unknown_components: unknown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::usage::UsageRates;
    use crate::services::usage::meter::MeasuredQuantity;
    use serde_json::json;
    fn quote(rates: Option<UsageRates>) -> PriceQuote {
        serde_json::from_value(json!({"classification":"OfficialEstimate","snapshot_id":null,"price_revision":null,"override_revision":null,"currency":"USD","rates":rates,"tariffs":[],"untracked_reason":null})).unwrap()
    }
    fn context() -> PricingContext {
        PricingContext {
            input_semantics: InputSemantics::Inclusive,
            cache_read_possible: true,
            cache_write_possible: false,
            cache_write_ttl_ms: None,
        }
    }
    #[test]
    fn cache_subsets_and_reasoning_do_not_inflate_the_price() {
        let quote = quote(Some(UsageRates {
            input_usd_per_million: Some("2".into()),
            output_usd_per_million: Some("8".into()),
            cache_read_usd_per_million: Some("0.2".into()),
            ..Default::default()
        }));
        let q = Quantities {
            input_tokens: Some(100),
            output_tokens: Some(10),
            cached_read_tokens: Some(40),
            reasoning_tokens: Some(5),
            ..Default::default()
        };
        let cost = estimate(&quote, &q, &context()).unwrap();
        assert_eq!(cost.tracked_micro_usd, Some(208));
        assert!(cost.monetary_complete);
        let partial = estimate(
            &quote,
            &Quantities {
                cached_read_tokens: None,
                ..q
            },
            &context(),
        )
        .unwrap();
        assert_eq!(partial.tracked_micro_usd, Some(80));
        assert!(!partial.monetary_complete);
    }
    #[test]
    fn missing_write_ttl_never_selects_a_cheaper_incurred_rate() {
        let quote = quote(Some(UsageRates {
            cache_writes: vec![
                crate::ports::usage::CacheWriteRate {
                    ttl_ms: Some(300_000),
                    usd_per_million: "1.25".into(),
                },
                crate::ports::usage::CacheWriteRate {
                    ttl_ms: Some(3_600_000),
                    usd_per_million: "2".into(),
                },
            ],
            ..Default::default()
        }));
        let q = Quantities {
            input_tokens: Some(0),
            output_tokens: Some(0),
            cached_read_tokens: Some(0),
            cache_write_tokens: Some(100),
            ..Default::default()
        };
        let c = PricingContext {
            input_semantics: InputSemantics::Disjoint,
            cache_read_possible: false,
            cache_write_possible: true,
            cache_write_ttl_ms: None,
        };
        let unknown = estimate(&quote, &q, &c).unwrap();
        assert!(!unknown.monetary_complete);
        assert_eq!(unknown.unknown_components, vec!["cache write"]);
        let selected = estimate(
            &quote,
            &q,
            &PricingContext {
                cache_write_ttl_ms: Some(300_000),
                ..c
            },
        )
        .unwrap();
        assert_eq!(selected.tracked_micro_usd, Some(125));
        assert!(selected.monetary_complete);
    }
    #[test]
    fn non_token_measurements_use_exact_published_denominators() {
        let mut quote = quote(None);
        quote.tariffs=serde_json::from_value(json!([{"upstream_model_id":"audio","service_tier":"Standard","context_lower_exclusive":null,"context_upper_inclusive":null,"modality":"Audio","charge_kind":"Runtime","cache_ttl_seconds":null,"unit":"Second","units":60,"amount_usd":"0.000001","region":null,"endpoint_profile":null}])).unwrap();
        let units = vec![MeasuredQuantity {
            modality: "Audio".into(),
            charge_kind: "Runtime".into(),
            unit: "Second".into(),
            cache_write_ttl_ms: None,
            quantity: 30,
        }];
        let cost = estimate(
            &quote,
            &Quantities {
                additional: units.clone(),
                ..Default::default()
            },
            &PricingContext {
                cache_read_possible: false,
                ..context()
            },
        )
        .unwrap();
        assert_eq!(cost.tracked_micro_usd, Some(1));
        assert!(cost.monetary_complete);
        let reserve = reservation(&quote, 0, 0, false, &units).unwrap();
        assert_eq!(reserve.tracked_micro_usd, Some(1));
        assert!(reserve.monetary_complete);
        assert!(
            !reservation(&quote, 0, 0, false, &[])
                .unwrap()
                .monetary_complete
        );
    }
    #[test]
    fn reservation_uses_the_highest_write_rate_without_a_cache_hit_discount() {
        let quote = quote(Some(UsageRates {
            input_usd_per_million: Some("2".into()),
            output_usd_per_million: Some("8".into()),
            cache_read_usd_per_million: Some("0.01".into()),
            cache_writes: vec![crate::ports::usage::CacheWriteRate {
                ttl_ms: Some(300_000),
                usd_per_million: "4".into(),
            }],
            ..Default::default()
        }));
        assert_eq!(
            reservation(&quote, 100, 10, true, &[]).unwrap(),
            Estimate {
                tracked_micro_usd: Some(480),
                monetary_complete: true,
                unknown_components: vec![]
            }
        );
    }
    #[test]
    fn fractional_write_choices_are_compared_before_rounding_the_whole_reservation() {
        let mut quote = quote(None);
        let tariff = |kind: &str, modality: &str, unit: &str, amount: &str, ttl: Option<i64>| {
            serde_json::from_value(serde_json::json!({
            "upstream_model_id":"priced", "service_tier":"Standard", "context_lower_exclusive":null,"context_upper_inclusive":null,
            "modality":modality,"charge_kind":kind,"cache_ttl_seconds":ttl,"unit":unit,"units":1,"amount_usd":amount,"region":null,"endpoint_profile":null})).unwrap()
        };
        quote.tariffs = vec![
            tariff("CacheWrite", "Text", "Token", "0.0000008", Some(300)),
            tariff("CacheWrite", "Text", "Token", "0.0000002", Some(3600)),
            tariff("Runtime", "Audio", "Second", "0.0000008", None),
        ];
        let units = vec![
            MeasuredQuantity {
                modality: "Text".into(),
                charge_kind: "CacheWrite".into(),
                unit: "Token".into(),
                cache_write_ttl_ms: None,
                quantity: 1,
            },
            MeasuredQuantity {
                modality: "Audio".into(),
                charge_kind: "Runtime".into(),
                unit: "Second".into(),
                cache_write_ttl_ms: None,
                quantity: 1,
            },
        ];
        let cost = reservation(&quote, 0, 0, true, &units).unwrap();
        assert_eq!(cost.tracked_micro_usd, Some(2));
        assert!(cost.monetary_complete);
    }
    #[test]
    fn contradictory_cache_observation_cannot_be_costed_as_zero() {
        let quantities = Quantities {
            input_tokens: Some(100),
            output_tokens: Some(10),
            cached_read_tokens: Some(50),
            ..Default::default()
        };
        let priced = estimate(
            &quote(Some(UsageRates {
                input_usd_per_million: Some("1".into()),
                output_usd_per_million: Some("2".into()),
                ..Default::default()
            })),
            &quantities,
            &PricingContext {
                input_semantics: InputSemantics::Inclusive,
                cache_read_possible: false,
                cache_write_possible: false,
                cache_write_ttl_ms: None,
            },
        )
        .unwrap();
        assert!(!priced.monetary_complete);
        assert!(
            priced
                .unknown_components
                .iter()
                .any(|v| v.contains("contradicts"))
        );
        assert_eq!(quantities.cached_read_tokens, Some(50));
    }
    #[test]
    fn cache_component_estimates_preserve_missing_and_ambiguous_ttl() {
        let context = PricingContext {
            input_semantics: InputSemantics::Inclusive,
            cache_read_possible: true,
            cache_write_possible: true,
            cache_write_ttl_ms: None,
        };
        let mut quantities = Quantities {
            input_tokens: Some(100),
            output_tokens: Some(0),
            cached_read_tokens: Some(50),
            cache_write_tokens: Some(10),
            ..Default::default()
        };
        let mut priced = quote(Some(UsageRates {
            cache_read_usd_per_million: Some("0.5".into()),
            ..Default::default()
        }));
        priced.rates.as_mut().unwrap().cache_writes = vec![
            crate::ports::usage::CacheWriteRate {
                ttl_ms: Some(300_000),
                usd_per_million: "2".into(),
            },
            crate::ports::usage::CacheWriteRate {
                ttl_ms: Some(3_600_000),
                usd_per_million: "3".into(),
            },
        ];
        let (read, write) = cache_estimates(&priced, &quantities, &context).unwrap();
        assert!(read.is_some());
        assert_eq!(write, None);
        quantities.cached_read_tokens = None;
        assert_eq!(
            cache_estimates(&priced, &quantities, &context).unwrap().0,
            None
        );
        quantities.cached_read_tokens = Some(0);
        assert_eq!(
            cache_estimates(&priced, &quantities, &context).unwrap().0,
            Some(0)
        );
    }
}
