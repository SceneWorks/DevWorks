//! Retained subscription windows and Retry-After. These observations never grant dispatch.
use crate::protocol::provider::EndpointProfile;
use serde_json::Value;
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LimitReading {
    pub exhausted: bool,
    pub reset_at_ms: Option<i64>,
}
fn number(s: Option<&str>) -> Option<f64> {
    s?.trim().parse::<f64>().ok().filter(|n| n.is_finite())
}
fn millis(n: f64) -> Option<i64> {
    (n.is_finite() && n > 0.0 && n <= 253_402_300_799_999.0).then_some(n as i64)
}
pub fn instant(s: &str) -> Option<i64> {
    if let Some(n) = number(Some(s)) {
        return millis(if n > 1e11 { n } else { n * 1000.0 });
    }
    s.parse::<jiff::Timestamp>()
        .ok()
        .map(|t| t.as_millisecond())
}
pub fn after(now: i64, s: &str) -> Option<i64> {
    let n = number(Some(s))?;
    if n <= 0.0 {
        return None;
    }
    now.checked_add(millis(n * 1000.0)?)
}
pub fn retry_after(now: i64, s: &str) -> Option<i64> {
    if number(Some(s)).is_some() {
        return after(now, s);
    }
    let normalized = s.trim().strip_suffix("GMT").map(|v| format!("{v}+0000"));
    jiff::Timestamp::strptime("%a, %d %b %Y %T %z", normalized.as_deref().unwrap_or(s))
        .ok()
        .map(|t| t.as_millisecond())
}
pub fn headers(profile: &EndpointProfile, headers: &[(String, String)], now: i64) -> LimitReading {
    let get = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    let mut result = LimitReading::default();
    let mut merge = |reset: Option<i64>| {
        result.exhausted = true;
        result.reset_at_ms = result.reset_at_ms.into_iter().chain(reset).max();
    };
    match profile {
        EndpointProfile::CodexSubscriptionResponses => {
            for window in ["primary", "secondary"] {
                let prefix = format!("x-codex-{window}-");
                if number(get(&format!("{prefix}used-percent"))).is_some_and(|n| n >= 100.0) {
                    merge(
                        get(&format!("{prefix}reset-at"))
                            .and_then(instant)
                            .or_else(|| {
                                get(&format!("{prefix}reset-after-seconds"))
                                    .and_then(|v| after(now, v))
                            }),
                    );
                }
            }
        }
        EndpointProfile::AnthropicMessages | EndpointProfile::ClaudeSubscriptionMessages => {
            for prefix in [
                "anthropic-ratelimit-unified-",
                "anthropic-ratelimit-unified-5h-",
                "anthropic-ratelimit-unified-7d-",
            ] {
                if get(&format!("{prefix}status"))
                    .is_some_and(|v| v.eq_ignore_ascii_case("rejected"))
                    || number(get(&format!("{prefix}utilization"))).is_some_and(|n| n >= 1.0)
                {
                    merge(get(&format!("{prefix}reset")).and_then(instant));
                }
            }
        }
        _ => {}
    }
    result
}
pub fn body_reset(value: &Value, now: i64) -> Option<i64> {
    let error = value
        .get("error")
        .or_else(|| value.get("detail"))
        .unwrap_or(value);
    let text = |key: &str| {
        error.get(key).map(|v| {
            v.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| v.to_string())
        })
    };
    text("resets_at")
        .and_then(|v| instant(&v))
        .or_else(|| text("resets_in_seconds").and_then(|v| after(now, &v)))
}
