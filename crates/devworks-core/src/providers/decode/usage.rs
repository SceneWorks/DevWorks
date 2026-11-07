use super::*;
pub use crate::services::usage::meter::InputSemantics;
use serde_json::value::RawValue;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheWrite {
    pub tokens: u64,
    pub ttl_seconds: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportedCost {
    /// Exact plain decimal; never passes through binary floating point.
    pub amount: String,
    pub currency: Option<String>,
    pub raw_lexeme: String,
}
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedUsage {
    pub semantics: crate::services::usage::meter::ObservationSemantics,
    pub service_tier: Option<String>,
    pub processing_region: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cached_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub cache_writes: Vec<CacheWrite>,
    pub reasoning_tokens: Option<u64>,
    pub input_audio_tokens: Option<u64>,
    pub output_audio_tokens: Option<u64>,
    pub input_semantics: Option<InputSemantics>,
    pub cost: Option<ReportedCost>,
    pub other_costs: BTreeMap<String, ReportedCost>,
    pub raw_json: String,
    /// Bad independent fields do not erase valid reported quantities or prices.
    pub validation_errors: Vec<String>,
}

pub(super) fn raw_at<'a>(raw: &'a str, path: &[&str]) -> Option<&'a str> {
    if path.is_empty() {
        return Some(raw);
    }
    if raw.trim_start().starts_with('[') {
        let items: Vec<&RawValue> = serde_json::from_str(raw).ok()?;
        raw_at(items.get(path[0].parse::<usize>().ok()?)?.get(), &path[1..])
    } else {
        let fields: BTreeMap<String, &RawValue> = serde_json::from_str(raw).ok()?;
        raw_at(fields.get(path[0])?.get(), &path[1..])
    }
}

pub(super) fn raw_array_at<'a>(raw: &'a str, path: &[&str]) -> Option<Vec<&'a RawValue>> {
    serde_json::from_str(raw_at(raw, path)?).ok()
}

/// Expand JSON's decimal exponent exactly, bounded by the containing event and a finite
/// expansion limit. This retains large integers and sub-micro charges without rounding.
pub(super) fn decimal(raw: &str) -> Option<String> {
    let text = if raw.starts_with('"') {
        serde_json::from_str::<String>(raw).ok()?
    } else {
        raw.into()
    };
    if text.is_empty() || text.len() > 1024 || text.starts_with('-') {
        return None;
    }
    let (mantissa, exponent) = match text.find(['e', 'E']) {
        Some(i) => (&text[..i], text[i + 1..].parse::<i32>().ok()?),
        None => (text.as_str(), 0),
    };
    let mut digits = String::new();
    let mut fraction = 0_i32;
    let mut dot = false;
    for b in mantissa.bytes() {
        if b == b'.' && !dot {
            dot = true;
            continue;
        }
        if !b.is_ascii_digit() {
            return None;
        }
        digits.push(char::from(b));
        if dot {
            fraction += 1;
        }
    }
    if digits.is_empty() || mantissa.starts_with('.') || mantissa.ends_with('.') {
        return None;
    }
    let point = i32::try_from(digits.len())
        .ok()?
        .checked_sub(fraction)?
        .checked_add(exponent)?;
    if point.unsigned_abs() > 4096 {
        return None;
    }
    let mut value = if point <= 0 {
        format!("0.{}{}", "0".repeat((-point) as usize), digits)
    } else if point as usize >= digits.len() {
        format!("{}{}", digits, "0".repeat(point as usize - digits.len()))
    } else {
        format!(
            "{}.{}",
            &digits[..point as usize],
            &digits[point as usize..]
        )
    };
    while value.starts_with('0') && !value.starts_with("0.") && value.len() > 1 {
        value.remove(0);
    }
    if value.contains('.') {
        while value.ends_with('0') {
            value.pop();
        }
        if value.ends_with('.') {
            value.pop();
        }
    }
    Some(value)
}

impl Decoder {
    pub(super) fn usage(&mut self, raw: &str, path: &[&str], out: &mut Vec<DecodedFrame>) {
        let enclosing =
            raw_at(raw, &path[..path.len().saturating_sub(1)]).and_then(|raw| json_value(raw).ok());
        let Some(raw) = raw_at(raw, path).filter(|s| *s != "null") else {
            return;
        };
        let Ok(value) = json_value(raw) else {
            return;
        };
        if !value.is_object() {
            self.error(DecodeErrorKind::InvalidContent, "usage_not_object", out);
            return;
        }
        let service_tier = string(&value, "service_tier")
            .or_else(|| enclosing.as_ref().and_then(|v| string(v, "service_tier")));
        let processing_region = string(&value, "inference_geo")
            .or_else(|| string(&value, "processing_region"))
            .or_else(|| {
                enclosing
                    .as_ref()
                    .and_then(|v| string(v, "processing_region"))
            });
        let mut errors = Vec::new();
        let mut quantity = |paths: &[&[&str]]| -> Option<u64> {
            for path in paths {
                if let Some(v) = raw_at(raw, path).filter(|s| *s != "null") {
                    return match v.parse::<u64>() {
                        Ok(n) => Some(n),
                        Err(_) => {
                            errors.push(path.join("."));
                            None
                        }
                    };
                }
            }
            None
        };
        let input_tokens = quantity(&[&["input_tokens"], &["prompt_tokens"]]);
        let output_tokens = quantity(&[&["output_tokens"], &["completion_tokens"]]);
        let total_tokens = quantity(&[&["total_tokens"]]);
        let cached_read_tokens = quantity(&[
            &["cache_read_input_tokens"],
            &["input_tokens_details", "cached_tokens"],
            &["prompt_tokens_details", "cached_tokens"],
        ]);
        let cache_write_tokens = quantity(&[
            &["cache_creation_input_tokens"],
            &["prompt_tokens_details", "cache_write_tokens"],
        ]);
        let reasoning_tokens = quantity(&[
            &["output_tokens_details", "reasoning_tokens"],
            &["completion_tokens_details", "reasoning_tokens"],
        ]);
        let input_audio_tokens = quantity(&[
            &["prompt_tokens_details", "audio_tokens"],
            &["input_tokens_details", "audio_tokens"],
        ]);
        let output_audio_tokens = quantity(&[
            &["completion_tokens_details", "audio_tokens"],
            &["output_tokens_details", "audio_tokens"],
        ]);
        let five = quantity(&[&["cache_creation", "ephemeral_5m_input_tokens"]]);
        let hour = quantity(&[&["cache_creation", "ephemeral_1h_input_tokens"]]);
        let mut cache_writes = Vec::new();
        if let Some(tokens) = five {
            cache_writes.push(CacheWrite {
                tokens,
                ttl_seconds: Some(300),
            });
        }
        if let Some(tokens) = hour {
            cache_writes.push(CacheWrite {
                tokens,
                ttl_seconds: Some(3600),
            });
        }
        if cache_writes.is_empty()
            && let Some(tokens) = cache_write_tokens
        {
            cache_writes.push(CacheWrite {
                tokens,
                ttl_seconds: None,
            });
        }
        let currency = string(&value, "currency").or_else(|| {
            matches!(self.profile, EndpointProfile::OpenRouterChat).then(|| "USD".into())
        });
        let mut reported = |path: &[&str]| {
            let lexeme = raw_at(raw, path).filter(|v| *v != "null")?;
            match decimal(lexeme) {
                Some(amount) => Some(ReportedCost {
                    amount,
                    raw_lexeme: lexeme.into(),
                    currency: currency.clone(),
                }),
                None => {
                    errors.push(path.join("."));
                    None
                }
            }
        };
        let cost = reported(&["cost"]);
        let mut other_costs = BTreeMap::new();
        for path in [
            &["cost_details", "upstream_inference_cost"][..],
            &["cost_details", "upstream_inference_prompt_cost"],
            &["cost_details", "upstream_inference_completions_cost"],
        ] {
            if let Some(cost) = reported(path) {
                other_costs.insert(path.join("."), cost);
            }
        }
        let input_semantics = match self.profile {
            EndpointProfile::AnthropicMessages | EndpointProfile::ClaudeSubscriptionMessages => {
                Some(InputSemantics::Disjoint)
            }
            EndpointProfile::CompatibleChat => None,
            _ => Some(InputSemantics::Inclusive),
        };
        out.push(DecodedFrame::Usage {
            observation: DecodedUsage {
                semantics: crate::services::usage::meter::ObservationSemantics::Cumulative,
                service_tier,
                processing_region,
                input_tokens,
                output_tokens,
                total_tokens,
                cached_read_tokens,
                cache_write_tokens,
                cache_writes,
                reasoning_tokens,
                input_audio_tokens,
                output_audio_tokens,
                input_semantics,
                cost,
                other_costs,
                raw_json: raw.into(),
                validation_errors: errors,
            },
        });
    }
}
