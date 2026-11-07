//! Conditional validators and pacing from the exact received API response.
use crate::ports::http::{HttpExchange, MAX_RESPONSE_METADATA_BYTES};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidControl {
    Metadata,
    Time,
}

/// Permission scopes describe the remote credential; they grant no local capability.
#[derive(Clone, PartialEq, Eq)]
pub struct ResponseControl {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub links: Vec<String>,
    pub poll_after_ms: i64,
    pub backoff_until_ms: Option<i64>,
    pub remaining: Option<u64>,
    pub resource: Option<String>,
    pub granted_scopes: Option<Vec<String>>,
}
impl std::fmt::Debug for ResponseControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseControl")
            .field("has_etag", &self.etag.is_some())
            .field("has_last_modified", &self.last_modified.is_some())
            .field("links", &self.links.len())
            .field("poll_after_ms", &self.poll_after_ms)
            .field("backoff_until_ms", &self.backoff_until_ms)
            .finish()
    }
}
fn single<'a>(response: &'a HttpExchange, name: &str) -> Result<Option<&'a str>, InvalidControl> {
    let mut values = response
        .headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name));
    let first = values.next().map(|(_, value)| value.as_str());
    if values.next().is_some() {
        return Err(InvalidControl::Metadata);
    }
    Ok(first)
}
fn integer(value: &str) -> Result<u64, InvalidControl> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(InvalidControl::Metadata);
    }
    value.parse().map_err(|_| InvalidControl::Metadata)
}
fn seconds(value: &str) -> Result<i64, InvalidControl> {
    i64::try_from(integer(value)?)
        .ok()
        .and_then(|n| n.checked_mul(1000))
        .ok_or(InvalidControl::Time)
}
impl ResponseControl {
    pub fn observe(
        response: &HttpExchange,
        now_ms: i64,
        minimum_poll_ms: i64,
    ) -> Result<Self, InvalidControl> {
        if now_ms < 0 || minimum_poll_ms < 0 {
            return Err(InvalidControl::Time);
        }
        let mut bytes = 0usize;
        for (key, value) in &response.headers {
            bytes = bytes
                .checked_add(key.len())
                .and_then(|n| n.checked_add(value.len()))
                .filter(|n| *n <= MAX_RESPONSE_METADATA_BYTES)
                .ok_or(InvalidControl::Metadata)?;
            if key.is_empty()
                || key
                    .bytes()
                    .chain(value.bytes())
                    .any(|b| matches!(b, 0 | b'\r' | b'\n'))
            {
                return Err(InvalidControl::Metadata);
            }
        }
        let add = |duration| now_ms.checked_add(duration).ok_or(InvalidControl::Time);
        let remaining = single(response, "x-ratelimit-remaining")?
            .map(integer)
            .transpose()?;
        let retry = single(response, "retry-after")?
            .map(|value| {
                if value.bytes().all(|b| b.is_ascii_digit()) && !value.is_empty() {
                    add(seconds(value)?)
                } else {
                    crate::providers::limits::retry_after(now_ms, value).ok_or(InvalidControl::Time)
                }
            })
            .transpose()?;
        let reset = single(response, "x-ratelimit-reset")?
            .map(seconds)
            .transpose()?;
        let fallback = matches!(response.response.status, 403 | 429) || remaining == Some(0);
        let backoff_until_ms = if fallback {
            // A missing/stale reset cannot create a tight retry loop. A supplied retry
            // deadline and exhausted primary reset both apply, so take the later one.
            let primary = (remaining == Some(0))
                .then_some(reset)
                .flatten()
                .filter(|t| *t > now_ms);
            let deadline = match retry.into_iter().chain(primary).max() {
                Some(deadline) => deadline,
                None => add(60_000)?,
            };
            Some(deadline.max(now_ms))
        } else {
            retry.map(|t| t.max(now_ms))
        };
        let interval = single(response, "x-poll-interval")?
            .map(seconds)
            .transpose()?
            .unwrap_or(0)
            .max(minimum_poll_ms);
        let poll_after_ms = add(interval)?.max(backoff_until_ms.unwrap_or(now_ms));
        let scopes = single(response, "x-oauth-scopes")?.map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect()
        });
        Ok(Self {
            etag: single(response, "etag")?.map(str::to_owned),
            last_modified: single(response, "last-modified")?.map(str::to_owned),
            links: response
                .headers
                .iter()
                .filter(|(key, _)| key.eq_ignore_ascii_case("link"))
                .map(|(_, value)| value.clone())
                .collect(),
            poll_after_ms,
            backoff_until_ms,
            remaining,
            resource: single(response, "x-ratelimit-resource")?.map(str::to_owned),
            granted_scopes: scopes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::http::HttpResponse;
    fn exchange(status: u16, headers: &[(&str, &str)]) -> HttpExchange {
        HttpExchange {
            response: HttpResponse {
                status,
                body: vec![],
            },
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }
    #[test]
    fn validators_are_exact_and_pacing_uses_the_latest_applicable_deadline() {
        let r = exchange(
            304,
            &[
                ("ETag", "W/\"opaque\""),
                ("Last-Modified", "Fri, 06 Nov 2026 10:00:00 GMT"),
                ("Link", "first"),
                ("link", "second"),
                ("X-Poll-Interval", "120"),
                ("Retry-After", "15"),
                ("X-RateLimit-Remaining", "0"),
                ("X-RateLimit-Reset", "180"),
                ("X-OAuth-Scopes", "repo, read:org, repo"),
            ],
        );
        let c = ResponseControl::observe(&r, 100_000, 60_000).unwrap();
        assert_eq!(c.etag.as_deref(), Some("W/\"opaque\""));
        assert_eq!(
            c.last_modified.as_deref(),
            Some("Fri, 06 Nov 2026 10:00:00 GMT")
        );
        assert_eq!(c.links, ["first", "second"]);
        assert_eq!(c.backoff_until_ms, Some(180_000));
        assert_eq!(c.poll_after_ms, 220_000);
        assert_eq!(
            c.granted_scopes,
            Some(vec!["read:org".into(), "repo".into()])
        );
        assert!(!format!("{c:?}").contains("opaque"));
    }
    #[test]
    fn secondary_and_stale_primary_limits_wait_and_http_date_retry_is_preserved() {
        for r in [
            exchange(429, &[]),
            exchange(403, &[("x-ratelimit-remaining", "10")]),
            exchange(
                200,
                &[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "1")],
            ),
        ] {
            assert_eq!(
                ResponseControl::observe(&r, 100_000, 0)
                    .unwrap()
                    .backoff_until_ms,
                Some(160_000)
            );
        }
        let r = exchange(429, &[("Retry-After", "Fri, 06 Nov 2026 10:00:00 GMT")]);
        let expected = "2026-11-06T10:00:00Z"
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_millisecond();
        assert_eq!(
            ResponseControl::observe(&r, 100_000, 0)
                .unwrap()
                .backoff_until_ms,
            Some(expected)
        );
        assert!(
            ResponseControl::observe(&exchange(200, &[]), 100_000, 0)
                .unwrap()
                .granted_scopes
                .is_none()
        );
    }
    #[test]
    fn ambiguous_invalid_or_overflowing_metadata_never_becomes_a_retry_instruction() {
        for r in [
            exchange(200, &[("ETag", "one"), ("etag", "two")]),
            exchange(429, &[("retry-after", "NaN")]),
            exchange(200, &[("x-poll-interval", "18446744073709551615")]),
            exchange(200, &[("etag", "injected\r\nvalue")]),
        ] {
            assert!(ResponseControl::observe(&r, 100_000, 0).is_err());
        }
        assert!(ResponseControl::observe(&exchange(200, &[]), i64::MAX, 1).is_err());
        assert_eq!(
            ResponseControl::observe(&exchange(429, &[("retry-after", "0")]), 100_000, 0)
                .unwrap()
                .backoff_until_ms,
            Some(100_000)
        );
    }
}
