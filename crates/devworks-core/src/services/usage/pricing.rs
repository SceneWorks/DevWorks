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

#[derive(Clone, Copy)]
struct Rate<'a> {
    amount_usd: &'a str,
    units: i64,
    ttl_ms: Option<i128>,
}
struct RateOptions<'a> {
    declared: bool,
    choices: Vec<Rate<'a>>,
}

// Exact tariffs are authoritative. Legacy per-million rates are a fallback for older quotes,
// never a second charge or a way around an ambiguous/missing canonical tariff match.
fn token_rates<'a>(
    quote: &'a PriceQuote,
    kind: &str,
    ttl_ms: Option<i64>,
    reserve: bool,
) -> RateOptions<'a> {
    let mut choices = quote
        .tariffs
        .iter()
        .filter(|t| {
            t.unit == "Token"
                && matches!(t.modality.as_str(), "Text" | "Any")
                && t.charge_kind == kind
        })
        .map(|t| Rate {
            amount_usd: &t.amount_usd,
            units: t.units,
            ttl_ms: t.cache_ttl_seconds.map(|s| i128::from(s) * 1000),
        })
        .collect::<Vec<_>>();
    if choices.is_empty()
        && let Some(rates) = &quote.rates
    {
        if kind == "CacheWrite" && !rates.cache_writes.is_empty() {
            choices.extend(rates.cache_writes.iter().map(|r| Rate {
                amount_usd: &r.usd_per_million,
                units: 1_000_000,
                ttl_ms: r.ttl_ms.map(i128::from),
            }));
        } else {
            let rate = match kind {
                "Input" => rates.input_usd_per_million.as_deref(),
                "Output" => rates.output_usd_per_million.as_deref(),
                "CacheRead" => rates.cache_read_usd_per_million.as_deref(),
                "CacheWrite" => rates.cache_write_usd_per_million.as_deref(),
                _ => None,
            };
            choices.extend(rate.map(|amount_usd| Rate {
                amount_usd,
                units: 1_000_000,
                ttl_ms: None,
            }));
        }
    }
    let declared = !choices.is_empty();
    if !(reserve && kind == "CacheWrite" && ttl_ms.is_none()) {
        // A missing lifetime cannot distinguish a specific write row from an overlapping
        // unrestricted row. Only an entirely lifetime-independent schedule is then usable.
        if kind == "CacheWrite" && ttl_ms.is_none() && choices.iter().any(|r| r.ttl_ms.is_some()) {
            choices.clear();
        } else {
            choices.retain(|r| {
                r.ttl_ms
                    .is_none_or(|ttl| Some(ttl) == ttl_ms.map(i128::from))
            });
        }
    }
    RateOptions { declared, choices }
}

fn add_component(
    cost: &mut CostAccumulator,
    known: &mut bool,
    unknown: &mut Vec<String>,
    name: &str,
    quantity: Option<i64>,
    rates: &RateOptions<'_>,
) -> Result<(), MoneyError> {
    match quantity {
        Some(0) => *known = true,
        Some(n) if rates.choices.len() == 1 => {
            let rate = rates.choices[0];
            cost.measure(n, rate.amount_usd, rate.units)?;
            *known = true;
        }
        None if !rates.declared => {}
        _ => unknown.push(name.into()),
    }
    Ok(())
}

fn highest_rate<'a>(
    rates: &[Rate<'a>],
    ttl_alternatives: bool,
) -> Result<Option<Rate<'a>>, MoneyError> {
    if rates.is_empty() || (!ttl_alternatives && rates.len() != 1) {
        return Ok(None);
    }
    // Different known lifetimes are valid reservation alternatives. Overlapping rows at
    // any lifetime are ambiguous, even when one of them has no TTL restriction.
    if ttl_alternatives
        && rates.iter().enumerate().any(|(i, a)| {
            rates[..i]
                .iter()
                .any(|b| a.ttl_ms.is_none() || b.ttl_ms.is_none() || a.ttl_ms == b.ttl_ms)
        })
    {
        return Ok(None);
    }
    most_expensive(rates)
}

fn most_expensive<'a>(rates: &[Rate<'a>]) -> Result<Option<Rate<'a>>, MoneyError> {
    let mut highest: Option<(CostAccumulator, Rate<'a>)> = None;
    for &rate in rates {
        let mut exact = CostAccumulator::default();
        exact.measure(1, rate.amount_usd, rate.units)?;
        if highest
            .as_ref()
            .map(|(old, _)| exact.compare(old))
            .transpose()?
            .is_none_or(|ordering| ordering.is_gt())
        {
            highest = Some((exact, rate));
        }
    }
    Ok(highest.map(|(_, rate)| rate))
}

fn measured_rates<'a>(
    quote: &'a PriceQuote,
    quantity: &super::meter::MeasuredQuantity,
    reserve: bool,
) -> RateOptions<'a> {
    if quantity.unit == "Token" && matches!(quantity.modality.as_str(), "Text" | "Any") {
        return token_rates(
            quote,
            &quantity.charge_kind,
            quantity.cache_write_ttl_ms,
            reserve,
        );
    }
    let rows = quote
        .tariffs
        .iter()
        .filter(|r| {
            r.unit == quantity.unit
                && r.modality == quantity.modality
                && r.charge_kind == quantity.charge_kind
        })
        .collect::<Vec<_>>();
    RateOptions {
        declared: !rows.is_empty(),
        choices: rows
            .into_iter()
            .filter(|r| {
                (reserve
                    && quantity.charge_kind == "CacheWrite"
                    && quantity.cache_write_ttl_ms.is_none())
                    || r.cache_ttl_seconds
                        .is_none_or(|s| s.checked_mul(1000) == quantity.cache_write_ttl_ms)
            })
            .map(|r| Rate {
                amount_usd: &r.amount_usd,
                units: r.units,
                ttl_ms: r.cache_ttl_seconds.map(|s| i128::from(s) * 1000),
            })
            .collect(),
    }
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
    let ambiguous_audio_cache = quantities.additional.iter().any(|q| {
        q.modality == "Audio" && q.charge_kind == "Input" && q.unit == "Token" && q.quantity > 0
    }) && ((context.cache_read_possible
        && quantities.cached_read_tokens != Some(0))
        || (context.cache_write_possible && quantities.cache_write_tokens != Some(0)));
    if ambiguous_audio_cache {
        unknown.push("input_modality_cache_breakdown".into());
    }
    // Provider audio counters are subsets of aggregate token counts, not a second complete
    // input/output. Preserve the aggregate in the meter while pricing disjoint components.
    for (kind, aggregate) in [
        ("Input", &mut input.input_tokens),
        ("Output", &mut input.output_tokens),
    ] {
        let audio = quantities
            .additional
            .iter()
            .find(|q| q.modality == "Audio" && q.charge_kind == kind && q.unit == "Token");
        if let Some(audio) = audio {
            *aggregate = aggregate
                .and_then(|total| total.checked_sub(audio.quantity))
                .filter(|n| *n >= 0);
            if aggregate.is_none() {
                unknown.push(format!("{kind}_modality_breakdown"));
            }
        }
    }
    if ambiguous_audio_cache {
        input.input_tokens = None;
    }
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
    for (name, kind, quantity, applicable) in [
        ("input", "Input", uncached, true),
        ("output", "Output", input.output_tokens, true),
        (
            "cache read",
            "CacheRead",
            quantities.cached_read_tokens,
            context.cache_read_possible && !ambiguous_audio_cache,
        ),
        (
            "cache write",
            "CacheWrite",
            quantities.cache_write_tokens,
            context.cache_write_possible && !ambiguous_audio_cache,
        ),
    ] {
        if !applicable {
            continue;
        }
        let explicit = quantities
            .additional
            .iter()
            .filter(|q| {
                q.unit == "Token"
                    && q.charge_kind == kind
                    && !(q.modality == "Audio" && matches!(kind, "Input" | "Output"))
            })
            .collect::<Vec<_>>();
        if explicit.is_empty() {
            let rates = token_rates(quote, kind, context.cache_write_ttl_ms, false);
            add_component(&mut cost, &mut known, &mut unknown, name, quantity, &rates)?;
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
        if ambiguous_audio_cache
            && quantity.modality == "Audio"
            && quantity.charge_kind == "Input"
            && quantity.unit == "Token"
        {
            continue;
        }

        if (quantity.charge_kind == "CacheRead" && !context.cache_read_possible)
            || (quantity.charge_kind == "CacheWrite" && !context.cache_write_possible)
        {
            if quantity.quantity > 0 {
                unknown.push("profile contradicts measured cache unit".into());
            }
            continue;
        }
        let rates = measured_rates(quote, quantity, false);
        add_component(
            &mut cost,
            &mut known,
            &mut unknown,
            &format!(
                "{} {} {}",
                quantity.modality, quantity.charge_kind, quantity.unit
            ),
            Some(quantity.quantity),
            &rates,
        )?;
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
    let mut input_remainder = input_tokens;
    let mut output_remainder = output_headroom;
    for quantity in unit_headroom {
        if quantity.unit == "Token" {
            let aggregate = match quantity.charge_kind.as_str() {
                "Input" => Some(&mut input_remainder),
                "Output" => Some(&mut output_remainder),
                _ => None,
            };
            if let Some(aggregate) = aggregate {
                *aggregate = aggregate
                    .checked_sub(quantity.quantity)
                    .filter(|n| *n >= 0)
                    .ok_or(MoneyError::InvalidDecimal)?;
            }
        }
        if quantity.quantity == 0 {
            known = true;
            continue;
        }
        let options = measured_rates(quote, quantity, true);
        let rate = highest_rate(
            &options.choices,
            quantity.charge_kind == "CacheWrite" && quantity.cache_write_ttl_ms.is_none(),
        )?;
        let mut alternatives = rate.into_iter().collect::<Vec<_>>();
        if alternatives.is_empty() {
            unknown.push(format!(
                "{} {} {} reservation",
                quantity.modality, quantity.charge_kind, quantity.unit
            ));
        }
        if cache_write_possible && quantity.unit == "Token" && quantity.charge_kind == "Input" {
            let write = super::meter::MeasuredQuantity {
                charge_kind: "CacheWrite".into(),
                cache_write_ttl_ms: None,
                ..quantity.clone()
            };
            let options = measured_rates(quote, &write, true);
            if let Some(write) = highest_rate(&options.choices, true)? {
                alternatives.push(write);
            } else {
                unknown.push(format!("{} cache write reservation", quantity.modality));
            }
        }
        if let Some(rate) = most_expensive(&alternatives)? {
            base.measure(quantity.quantity, rate.amount_usd, rate.units)?;
            known = true;
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
    let output = token_rates(quote, "Output", None, false);
    if let Some(rate) = highest_rate(&output.choices, false)? {
        base.measure(output_remainder, rate.amount_usd, rate.units)?;
        known = true;
    } else if output_remainder > 0 {
        unknown.push("output reservation".into());
    }
    let mut alternatives = Vec::new();
    let input = token_rates(quote, "Input", None, false);
    if let Some(rate) = highest_rate(&input.choices, false)? {
        alternatives.push(rate);
    } else if input_remainder > 0 {
        unknown.push("input reservation".into());
    }
    if cache_write_possible && input_remainder > 0 {
        let write = token_rates(quote, "CacheWrite", None, true);
        if let Some(rate) = highest_rate(&write.choices, true)? {
            alternatives.push(rate);
        } else {
            unknown.push("cache write reservation".into());
        }
    }
    if let Some(rate) = most_expensive(&alternatives)? {
        base.measure(input_remainder, rate.amount_usd, rate.units)?;
        known = true;
    }

    Ok(Estimate {
        tracked_micro_usd: if known
            || (input_tokens == 0 && output_headroom == 0 && unknown.is_empty())
        {
            Some(base.micro_usd(Rounding::Up)?)
        } else {
            None
        },
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
    #[test]
    fn legal_dimensional_tariffs_keep_large_exact_intermediates_until_final_rounding() {
        let mut priced = quote(None);
        let mut quantities = Quantities {
            input_tokens: Some(0),
            output_tokens: Some(0),
            ..Default::default()
        };
        for (modality, units) in ["Text", "Image", "Audio", "Video", "Any"].into_iter().zip([
            1_000_000_007,
            1_000_000_009,
            1_000_000_021,
            1_000_000_033,
            1_000_000_087,
        ]) {
            priced.tariffs.push(serde_json::from_value(json!({"upstream_model_id":"priced","service_tier":"Standard","context_lower_exclusive":null,"context_upper_inclusive":null,"modality":modality,"charge_kind":"Runtime","cache_ttl_seconds":null,"unit":"Second","units":units,"amount_usd":"1","region":null,"endpoint_profile":null})).unwrap());
            quantities.additional.push(MeasuredQuantity {
                modality: modality.into(),
                charge_kind: "Runtime".into(),
                unit: "Second".into(),
                cache_write_ttl_ms: None,
                quantity: 1_000_000_000,
            });
        }
        let cost = estimate(
            &priced,
            &quantities,
            &PricingContext {
                input_semantics: InputSemantics::Inclusive,
                cache_read_possible: false,
                cache_write_possible: false,
                cache_write_ttl_ms: None,
            },
        )
        .unwrap();
        assert_eq!(cost.tracked_micro_usd, Some(5_000_000));
        assert!(cost.monetary_complete);
    }
}
