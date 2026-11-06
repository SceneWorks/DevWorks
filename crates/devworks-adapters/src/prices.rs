//! Bounded, credential-free official price maintenance. The providers publish Markdown views
//! of their documentation; parse the table headers and explicit units, never scrape a display
//! order into a flat list of model prices. Unknown/ambiguous structure refuses publication.
use devworks_core::ports::http::{HttpRequest, HttpTransport};
use devworks_core::ports::usage::{
    FetchedPriceCatalog, OfficialPriceProvider, PriceFetchError, PriceSource, PublishedTariff,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;

const PARSER_VERSION: &str = "official-markdown-v1";
const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;
const MAX_RATES: usize = 20_000;

pub struct OfficialPriceSource {
    http: Arc<dyn HttpTransport>,
}
impl OfficialPriceSource {
    pub fn new(http: Arc<dyn HttpTransport>) -> Self {
        Self { http }
    }
}
impl PriceSource for OfficialPriceSource {
    fn fetch(
        &self,
        provider: OfficialPriceProvider,
        now_ms: i64,
    ) -> Result<FetchedPriceCatalog, PriceFetchError> {
        let mut request = HttpRequest::get(format!("{}.md", provider.source_url()))
            .header("Accept", "text/markdown,text/plain;q=0.9");
        request.timeout_ms = 30_000;
        request.max_response_bytes = MAX_SOURCE_BYTES;
        let response = self
            .http
            .send(&request)
            .map_err(|e| PriceFetchError::Transport(e.to_string()))?;
        if response.status != 200 {
            return Err(PriceFetchError::Transport(format!(
                "official pricing source returned HTTP {}",
                response.status
            )));
        }
        parse_catalog(provider, &response.body, now_ms)
    }
}

fn changed(message: &str) -> PriceFetchError {
    PriceFetchError::SourceChanged(message.to_owned())
}
fn cells(line: &str) -> Vec<&str> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(str::trim)
        .collect()
}
fn plain(value: &str) -> String {
    // Remove only presentation tags and Markdown links. Unexpected syntax in a monetary cell
    // is rejected by decimal(), rather than interpreted as a numeric zero.
    let mut value = value.to_owned();
    while let Some(start) = value.find("<sup>") {
        let Some(end) = value[start..].find("</sup>") else {
            break;
        };
        value.replace_range(start..start + end + "</sup>".len(), "");
    }
    let mut out = String::new();
    let mut in_tag = false;
    let mut link_target = false;
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if in_tag {
            if c == '>' {
                in_tag = false;
            }
        } else if link_target {
            if c == ')' {
                link_target = false;
            }
        } else if c == '<' {
            in_tag = true;
        } else if c == ']' && chars.peek() == Some(&'(') {
            chars.next();
            link_target = true;
        } else if c != '[' && c != '*' && c != '`' {
            out.push(c);
        }
    }
    out.trim().to_owned()
}
fn decimal(cell: &str) -> Result<Option<String>, PriceFetchError> {
    let text = plain(cell);
    if text == "Free" {
        return Ok(Some("0".into()));
    }
    if matches!(text.as_str(), "-" | "—" | "N/A") {
        return Ok(None);
    }
    let Some(value) = text.strip_prefix('$') else {
        return Err(PriceFetchError::InvalidRate("missing USD amount".into()));
    };
    let value = value.split_whitespace().next().unwrap_or("");
    let parts: Vec<_> = value.split('.').collect();
    if parts.len() > 2
        || parts[0].is_empty()
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
        || parts[0].len() > 9
        || parts.get(1).is_some_and(|p| p.len() > 12)
    {
        return Err(PriceFetchError::InvalidRate(
            "invalid decimal USD tariff".into(),
        ));
    }
    let suffix = text[1 + value.len()..].trim();
    if !suffix.is_empty()
        && !matches!(
            suffix,
            "/ MTok" | "USD / MTok" | "/ 1M tokens" | "/ 1M characters" | "/ minute"
        )
        && !suffix.bytes().all(|c| c.is_ascii_digit())
    {
        return Err(PriceFetchError::SourceChanged(format!(
            "unrecognized monetary unit or qualifier: {}",
            text.chars().take(120).collect::<String>()
        )));
    }
    Ok(Some(value.to_owned()))
}

type ModelContext = (Vec<String>, Option<i64>, Option<i64>);
fn anthropic_models(cell: &str) -> Result<ModelContext, PriceFetchError> {
    let text = plain(cell);
    let (lower, upper) = if text.contains("for prompts up to 100,000 tokens") {
        (None, Some(100_000))
    } else if text.contains("for prompts over 100,000 tokens") {
        (Some(100_000), None)
    } else {
        (None, None)
    };
    let mut models = Vec::new();
    for item in text.split(" / ") {
        let name = item.split(" (").next().unwrap_or(item).trim();
        let Some(name) = name.strip_prefix("Claude ") else {
            return Err(changed("unrecognized Anthropic model identity"));
        };
        if name.len() > 128
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b' ' || b == b'.' || b == b'-')
        {
            return Err(changed("unrecognized Anthropic model identity"));
        }
        models.push(format!(
            "claude-{}",
            name.to_ascii_lowercase().replace([' ', '.'], "-")
        ));
    }
    Ok((models, lower, upper))
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
    text: &str,
    tariffs: &mut Vec<PublishedTariff>,
) -> Result<(), PriceFetchError> {
    let source = tariffs.clone();
    for selected in source
        .iter()
        .filter(|r| r.charge_kind == "Input" && matches!(r.service_tier.as_str(), "Batch" | "Fast"))
    {
        let marker = if selected.service_tier == "Batch" {
            "These multipliers stack with other pricing modifiers"
        } else {
            "apply on top of fast mode pricing"
        };
        if !text.contains(marker) {
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
    if text.contains("1.1x pricing multiplier") && text.contains("inference_geo") {
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

/// Parse a complete official document. Identical repeated tariff rows are deduplicated;
/// conflicting duplicates refuse the entire snapshot and preserve the last published catalog.
pub fn parse_catalog(
    provider: OfficialPriceProvider,
    body: &[u8],
    now_ms: i64,
) -> Result<FetchedPriceCatalog, PriceFetchError> {
    if body.len() > MAX_SOURCE_BYTES || now_ms < 0 {
        return Err(changed("source or time exceeds its bound"));
    }
    let text = std::str::from_utf8(body).map_err(|_| changed("source is not UTF-8"))?;
    if !text.contains("Pricing") {
        return Err(changed("source title changed"));
    }
    if provider == OfficialPriceProvider::OpenAi && !text.contains("Prices per 1M tokens") {
        return Err(changed("missing published token unit"));
    }
    let lines: Vec<_> = text.lines().collect();
    let mut rates = BTreeMap::<String, PublishedTariff>::new();
    let mut service = "Standard";
    let mut saw_primary = false;
    let mut i = 0;
    while i + 1 < lines.len() {
        let line = lines[i].trim().trim_start_matches('#').trim();
        service = match line.to_ascii_lowercase().as_str() {
            "standard" | "standard pricing data" | "model pricing" => "Standard",
            "batch" | "batch pricing data" | "batch processing" => "Batch",
            "flex" => "Flex",
            "fast" | "fast pricing data" | "fast mode pricing" => "Fast",
            "ultrafast" | "ultrafast pricing data" => "Ultrafast",
            "cyber models"
            | "life sciences models"
            | "legal model configurations"
            | "multimodal models"
            | "realtime and audio generation models"
            | "image generation models"
            | "transcription models"
            | "gpt-live sessions"
            | "specialized models" => "Standard",
            _ => service,
        };
        if !lines[i].trim().starts_with('|') || !lines[i + 1].contains("---") {
            i += 1;
            continue;
        }
        let headers = cells(lines[i]);
        let model_col = headers
            .iter()
            .position(|h| matches!(*h, "Model" | "Model configuration"));
        let has_prices = headers.iter().any(|h| {
            matches!(
                h.to_ascii_lowercase().as_str(),
                "input"
                    | "base input tokens"
                    | "batch input"
                    | "short context input"
                    | "price per minute"
            )
        });
        i += 2;
        let Some(model_col) = model_col.filter(|_| has_prices) else {
            while i < lines.len() && lines[i].trim().starts_with('|') {
                i += 1;
            }
            continue;
        };
        if headers.contains(&"Training") {
            // Fine-tuned inference has a different access context; its base model row cannot
            // price an ordinary base-model request. Those identities stay explicitly unmapped.
            while i < lines.len() && lines[i].trim().starts_with('|') {
                i += 1;
            }
            continue;
        }
        while i < lines.len() && lines[i].trim().starts_with('|') {
            if i + 1 < lines.len() && lines[i + 1].contains("---") {
                break;
            }
            let row = cells(lines[i]);
            i += 1;
            if row.len() != headers.len() {
                return Err(changed("pricing table width changed"));
            }
            let (models, model_lower, model_upper) = match provider {
                OfficialPriceProvider::Anthropic => anthropic_models(row[model_col])?,
                OfficialPriceProvider::OpenAi => {
                    let value = plain(row[model_col]);
                    let model = value.split(" (").next().unwrap_or(&value);
                    if model.is_empty()
                        || !model
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-._/".contains(&b))
                    {
                        return Err(changed("unrecognized OpenAI model identity"));
                    }
                    (vec![model.to_owned()], None, None)
                }
            };
            let modality = headers
                .iter()
                .position(|h| *h == "Modality")
                .map_or("Text", |c| row[c]);
            if !matches!(modality, "Text" | "Image" | "Audio" | "Video") {
                return Err(changed("unknown modality"));
            }
            for (col, header) in headers.iter().enumerate() {
                let h = header.to_ascii_lowercase();
                let (lower, upper) = if h.starts_with("short context ") {
                    if !text.contains(
                        "Short context: ≤272K input tokens. Long context: >272K input tokens.",
                    ) {
                        return Err(changed("missing context threshold"));
                    }
                    (None, Some(272_000))
                } else if h.starts_with("long context ") {
                    (Some(272_000), None)
                } else {
                    (model_lower, model_upper)
                };
                let (kind, ttl, unit, units) = if h.contains("write") {
                    let ttl = if h.contains("30m") {
                        1800
                    } else if h.contains("12h") {
                        43200
                    } else if h.starts_with("5m ") {
                        300
                    } else if h.starts_with("1h ") {
                        3600
                    } else {
                        return Err(changed("unrecognized cache-write lifetime"));
                    };
                    ("CacheWrite", Some(ttl), "Token", 1_000_000)
                } else if h.contains("cached input") || h == "cache hits and refreshes" {
                    ("CacheRead", None, "Token", 1_000_000)
                } else if h == "price per minute" {
                    ("Runtime", None, "Second", 60)
                } else if matches!(
                    h.as_str(),
                    "input"
                        | "base input tokens"
                        | "batch input"
                        | "short context input"
                        | "long context input"
                ) {
                    ("Input", None, "Token", 1_000_000)
                } else if matches!(
                    h.as_str(),
                    "output"
                        | "output tokens"
                        | "batch output"
                        | "output / cost"
                        | "short context output"
                        | "long context output"
                ) {
                    ("Output", None, "Token", 1_000_000)
                } else {
                    continue;
                };
                let amount_cell = plain(row[col]);
                let (unit, units) = if amount_cell.contains("/ 1M characters") {
                    ("Character", 1_000_000)
                } else if amount_cell.contains("/ minute") {
                    ("Second", 60)
                } else {
                    (unit, units)
                };
                let Some(amount) = decimal(row[col])? else {
                    continue;
                };
                for model in &models {
                    let rate = PublishedTariff {
                        upstream_model_id: model.clone(),
                        service_tier: service.into(),
                        context_lower_exclusive: lower,
                        context_upper_inclusive: upper,
                        modality: modality.into(),
                        charge_kind: kind.into(),
                        cache_ttl_seconds: ttl,
                        unit: unit.into(),
                        units,
                        amount_usd: amount.clone(),
                        region: None,
                        endpoint_profile: (kind == "CacheRead" && model.starts_with("gpt-image-2"))
                            .then(|| "OpenAiResponses".into()),
                    };
                    let mut identity = rate.clone();
                    identity.amount_usd.clear();
                    let key = serde_json::to_string(&identity)
                        .map_err(|_| changed("invalid rate identity"))?;
                    if let Some(previous) = rates.get(&key) {
                        if previous != &rate {
                            return Err(changed("conflicting duplicate tariff"));
                        }
                    } else {
                        rates.insert(key, rate);
                    }
                    if rates.len() > MAX_RATES {
                        return Err(changed("too many tariff rows"));
                    }
                }
                saw_primary |= kind == "Input" && service == "Standard";
            }
        }
    }
    if !saw_primary || rates.is_empty() {
        return Err(changed("no complete primary pricing table"));
    }
    let mut tariffs: Vec<_> = rates.into_values().collect();
    if provider == OfficialPriceProvider::Anthropic {
        complete_anthropic_rates(text, &mut tariffs)?;
    }
    if tariffs.len() > MAX_RATES {
        return Err(changed("too many tariff rows after declared modifiers"));
    }
    Ok(FetchedPriceCatalog {
        provider,
        source_url: provider.source_url().into(),
        source_digest: Sha256::digest(body)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        parser_version: PARSER_VERSION.into(),
        fetched_at_ms: now_ms,
        verified_at_ms: now_ms,
        effective_date: None,
        tariffs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use devworks_core::ports::http::{HttpError, HttpResponse};
    use std::sync::Mutex;

    #[test]
    fn context_cache_and_service_dimensions_are_distinct_and_reported_zero_survives() {
        let source = "# Pricing\nPrices per 1M tokens.\nShort context: ≤272K input tokens. Long context: >272K input tokens.\n### Standard pricing data\n| Model | Short context input | Short context output | Short context cached input | Short context 30m write | Long context input | Long context output |\n| --- | --- | --- | --- | --- | --- | --- |\n| gpt-test | $1.25 | $5 | $0 | $1.5625 | $2.50 | $7.50 |\n### Fast pricing data\n| Model | Input | Output |\n| --- | --- | --- |\n| gpt-test | $4 | $20 |\n";
        let parsed = parse_catalog(OfficialPriceProvider::OpenAi, source.as_bytes(), 17).unwrap();
        assert_eq!(parsed.tariffs.len(), 8);
        assert!(
            parsed
                .tariffs
                .iter()
                .any(|r| r.charge_kind == "CacheRead" && r.amount_usd == "0")
        );
        assert!(parsed.tariffs.iter().any(
            |r| r.cache_ttl_seconds == Some(1800) && r.context_upper_inclusive == Some(272_000)
        ));
        assert!(
            parsed
                .tariffs
                .iter()
                .any(|r| r.context_lower_exclusive == Some(272_000) && r.amount_usd == "2.50")
        );
        assert_eq!(
            parsed
                .tariffs
                .iter()
                .filter(|r| r.service_tier == "Fast")
                .count(),
            2
        );
        assert_eq!(parsed.source_digest.len(), 64);
        assert_eq!(parsed.fetched_at_ms, 17);
    }

    #[test]
    fn anthropic_uses_exact_model_rows_ttl_rates_and_prompt_thresholds() {
        let source = "# Pricing\nPrices per 1M tokens.\n## Model pricing\n| Model | Base input tokens | 5m cache writes | 1h cache writes | Cache hits and refreshes | Output tokens |\n| --- | --- | --- | --- | --- | --- |\n| Claude Unit 5.5 (for prompts up to 100,000 tokens) | $0.10 / MTok | $0.125 / MTok | $0.20 / MTok | $0.005 / MTok<sup>1</sup> | $0.50 / MTok |\n| Claude Unit 5.5 (for prompts over 100,000 tokens) | $0.50 / MTok | $0.625 / MTok | $1 / MTok | $0.025 / MTok | $2.50 / MTok |\n";
        let parsed = parse_catalog(OfficialPriceProvider::Anthropic, source.as_bytes(), 0).unwrap();
        assert_eq!(parsed.tariffs.len(), 10);
        assert!(
            parsed
                .tariffs
                .iter()
                .all(|r| r.upstream_model_id == "claude-unit-5-5")
        );
        assert!(
            parsed
                .tariffs
                .iter()
                .any(|r| r.context_upper_inclusive == Some(100_000) && r.amount_usd == "0.005")
        );
        assert!(parsed.tariffs.iter().any(
            |r| r.cache_ttl_seconds == Some(3600) && r.context_lower_exclusive == Some(100_000)
        ));
    }

    #[test]
    fn ambiguous_or_changed_sources_are_refused_without_fabricating_rates() {
        let good = "# Pricing\nPrices per 1M tokens.\n| Model | Input | Output |\n| --- | --- | --- |\n| gpt-test | $1 | $2 |\n";
        for bad in [
            good.replace("$1", "€1"),
            good.replace("$1", "$1 / 1000"),
            format!("{good}| gpt-test | $3 | $2 |\n"),
            good.replace("| $1 | $2 |", "| $1 |"),
            good.replace("# Pricing", "# Sign in"),
        ] {
            assert!(
                parse_catalog(OfficialPriceProvider::OpenAi, bad.as_bytes(), 0).is_err(),
                "{bad}"
            );
        }
        let repeated = format!("{good}| gpt-test | $1 | $2 |\n");
        assert_eq!(
            parse_catalog(OfficialPriceProvider::OpenAi, repeated.as_bytes(), 0)
                .unwrap()
                .tariffs
                .len(),
            2
        );
    }

    #[test]
    fn runtime_units_and_modality_endpoint_constraints_remain_explicit() {
        let source = "# Pricing\nPrices per 1M tokens.\n| Model | Input | Output |\n| --- | --- | --- |\n| gpt-test | $1 | $2 |\n| Model | Price per minute |\n| --- | --- |\n| gpt-live-test | $0.05 |\n| Model | Modality | Input | Output | Cached input |\n| --- | --- | --- | --- | --- |\n| gpt-image-2-test | Image | $8 | $30 | $2 |\n";
        let parsed = parse_catalog(OfficialPriceProvider::OpenAi, source.as_bytes(), 0).unwrap();
        assert!(
            parsed
                .tariffs
                .iter()
                .any(|r| r.unit == "Second" && r.units == 60 && r.amount_usd == "0.05")
        );
        assert!(parsed.tariffs.iter().any(|r| r.modality == "Image"
            && r.charge_kind == "CacheRead"
            && r.endpoint_profile.as_deref() == Some("OpenAiResponses")));
    }

    struct Scripted {
        requests: Mutex<Vec<String>>,
        status: u16,
    }
    #[test]
    fn service_modifiers_preserve_model_specific_cache_prices() {
        let source = "# Pricing\n## Model pricing\n| Model | Base input tokens | 5m cache writes | 1h cache writes | Cache hits and refreshes | Output tokens |\n| --- | --- | --- | --- | --- | --- |\n| Claude Unit 5.5 | $4 / MTok | $5 / MTok | $8 / MTok | $0.20 / MTok | $20 / MTok |\n### Fast mode pricing\n| Model | Input | Output |\n| --- | --- | --- |\n| Claude Unit 5.5 | $8 USD / MTok | $40 USD / MTok |\nCache multipliers apply on top of fast mode pricing.\n### Batch processing\n| Model | Batch input | Batch output |\n| --- | --- | --- |\n| Claude Unit 5.5 | $2 / MTok | $10 / MTok |\nThese multipliers stack with other pricing modifiers.\nUsing inference_geo incurs a 1.1x pricing multiplier.\n";
        let parsed = parse_catalog(OfficialPriceProvider::Anthropic, source.as_bytes(), 0).unwrap();
        let rate = |tier: &str, region: Option<&str>, kind: &str, ttl: Option<i64>| {
            &parsed
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
        };
        assert_eq!(rate("Fast", None, "CacheRead", None), "0.4");
        assert_eq!(rate("Batch", None, "CacheRead", None), "0.1");
        assert_eq!(rate("Fast", Some("us"), "CacheWrite", Some(3600)), "17.6");
        assert!(
            parse_catalog(
                OfficialPriceProvider::Anthropic,
                source
                    .replace("apply on top of fast mode pricing", "changed rule")
                    .as_bytes(),
                0
            )
            .is_err()
        );
    }
    #[test]
    fn explicit_character_units_and_published_free_rates_are_not_missing_values() {
        let source = "# Pricing\nPrices per 1M tokens unless noted.\n| Model | Input | Output |\n| --- | --- | --- |\n| unit-free | Free | - |\n| unit-tts | - | $15.00 / 1M characters |\n";
        let parsed = parse_catalog(OfficialPriceProvider::OpenAi, source.as_bytes(), 0).unwrap();
        assert_eq!(parsed.tariffs.len(), 2);
        assert!(
            parsed
                .tariffs
                .iter()
                .any(|r| r.upstream_model_id == "unit-free" && r.amount_usd == "0")
        );
        assert!(
            parsed
                .tariffs
                .iter()
                .any(|r| r.unit == "Character" && r.units == 1_000_000)
        );
        assert!(
            parse_catalog(
                OfficialPriceProvider::OpenAi,
                source.replace("1M tokens", "1K tokens").as_bytes(),
                0
            )
            .is_err()
        );
    }
    impl HttpTransport for Scripted {
        fn send(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
            assert_eq!(request.timeout_ms, 30_000);
            assert_eq!(request.max_response_bytes, MAX_SOURCE_BYTES);
            assert!(request.header_value("Authorization").is_none());
            assert!(request.body.is_none());
            self.requests.lock().unwrap().push(request.url.clone());
            Ok(HttpResponse { status: self.status, body: b"# Pricing\nPrices per 1M tokens.\n| Model | Input | Output |\n| --- | --- | --- |\n| gpt-test | $1 | $2 |\n".to_vec() })
        }
    }
    #[test]
    fn fetch_is_credential_free_bounded_and_refuses_redirects() {
        let http = Arc::new(Scripted {
            requests: Mutex::new(Vec::new()),
            status: 200,
        });
        OfficialPriceSource::new(http.clone())
            .fetch(OfficialPriceProvider::OpenAi, 0)
            .unwrap();
        assert_eq!(
            *http.requests.lock().unwrap(),
            vec!["https://developers.openai.com/api/docs/pricing.md"]
        );
        let redirect = Arc::new(Scripted {
            requests: Mutex::new(Vec::new()),
            status: 302,
        });
        assert!(
            OfficialPriceSource::new(redirect.clone())
                .fetch(OfficialPriceProvider::Anthropic, 0)
                .is_err()
        );
        assert_eq!(redirect.requests.lock().unwrap().len(), 1);
    }
}
