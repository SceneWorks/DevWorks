//! One bounded conditional API read. The service persists observations and chooses when to
//! request another page; this client performs no automatic retry or pagination loop.
use super::response::ResponseControl;
use crate::ports::http::{HttpExchange, HttpRequest};
use crate::ports::storage::ApplicationStore;
use crate::services::git::github::{Fault, GitHubClient};
use serde_json::Value;

/// An owner-selected endpoint, including a known server-ID alias when GitHub canonicalizes
/// `/repos/owner/name` links to `/repositories/id`. Pagination cannot change the endpoint.
#[derive(Clone)]
pub struct Endpoint {
    first: url::Url,
    canonical_path: Option<String>,
}
impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("path", &self.first.path())
            .finish()
    }
}
fn api_url(value: &str) -> Result<url::Url, Fault> {
    // Reject lexical controls rather than accepting URL-parser trimming/normalization.
    if value.len() > 8192 || value.bytes().any(|b| b <= b' ' || b == 127 || b == b'\\') {
        return Err(Fault::InvalidResponse);
    }
    let u = url::Url::parse(value).map_err(|_| Fault::InvalidResponse)?;
    if u.scheme() != "https"
        || u.host_str() != Some("api.github.com")
        || u.port().is_some()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.fragment().is_some()
    {
        return Err(Fault::InvalidResponse);
    }
    Ok(u)
}
impl Endpoint {
    pub fn new(first: &str, canonical_path: Option<&str>) -> Result<Self, Fault> {
        let first = api_url(first)?;
        let canonical_path = canonical_path
            .map(|path| {
                let u = api_url(&format!("https://api.github.com{path}"))?;
                if !path.starts_with('/') || u.path() != path || u.query().is_some() {
                    return Err(Fault::InvalidResponse);
                }
                Ok(path.to_owned())
            })
            .transpose()?;
        Ok(Self {
            first,
            canonical_path,
        })
    }
    fn page_url(&self, next: Option<&str>) -> Result<url::Url, Fault> {
        let u = match next {
            Some(next) => api_url(next)?,
            None => self.first.clone(),
        };
        if u.path() != self.first.path() && self.canonical_path.as_deref() != Some(u.path()) {
            return Err(Fault::InvalidResponse);
        }
        Ok(u)
    }
}

#[derive(Clone, Default)]
pub struct Validators {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}
impl std::fmt::Debug for Validators {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Validators")
            .field("etag", &self.etag.is_some())
            .field("last_modified", &self.last_modified.is_some())
            .finish()
    }
}
fn conditional(
    mut request: HttpRequest,
    validators: Option<&Validators>,
) -> Result<HttpRequest, Fault> {
    if let Some(validators) = validators {
        for (name, value) in [
            ("If-None-Match", &validators.etag),
            ("If-Modified-Since", &validators.last_modified),
        ] {
            if let Some(value) = value {
                if value.len() > 8192 || value.bytes().any(|b| matches!(b, 0 | b'\r' | b'\n')) {
                    return Err(Fault::InvalidResponse);
                }
                request = request.header(name, value);
            }
        }
    }
    Ok(request)
}

/// Non-success HTTP responses still carry the exact pacing/authentication observation.
/// The service classifies them without dropping Retry-After or resetting a durable cursor.
pub struct ApiPage {
    pub status: u16,
    pub control: ResponseControl,
    pub value: Option<Value>,
    pub next_page: Option<String>,
}
impl std::fmt::Debug for ApiPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiPage")
            .field("status", &self.status)
            .field("control", &self.control)
            .field("has_value", &self.value.is_some())
            .field("has_next_page", &self.next_page.is_some())
            .finish()
    }
}

impl GitHubClient {
    pub fn observe_page(
        &self,
        store: &dyn ApplicationStore,
        endpoint: &Endpoint,
        page: Option<&str>,
        validators: Option<&Validators>,
        now_ms: i64,
        minimum_poll_ms: i64,
    ) -> Result<ApiPage, Fault> {
        if now_ms < 0 || minimum_poll_ms < 0 || now_ms.checked_add(minimum_poll_ms).is_none() {
            return Err(Fault::InvalidResponse);
        }
        let mut request = conditional(
            HttpRequest::get(endpoint.page_url(page)?.as_str()),
            validators,
        )?
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .secret_header("Authorization", self.token(store)?);
        request.timeout_ms = 15_000;
        request.max_response_bytes = 8 * 1024 * 1024;
        let response = self
            .http
            .send_with_headers(&request)
            .map_err(|_| Fault::Unavailable)?;
        decode(endpoint, response, now_ms, minimum_poll_ms)
    }
}

fn decode(
    endpoint: &Endpoint,
    response: HttpExchange,
    now_ms: i64,
    minimum_poll_ms: i64,
) -> Result<ApiPage, Fault> {
    let control = ResponseControl::observe(&response, now_ms, minimum_poll_ms)
        .map_err(|_| Fault::InvalidResponse)?;
    let status = response.response.status;
    let value = if response.response.success() && status != 204 {
        Some(serde_json::from_slice(&response.response.body).map_err(|_| Fault::InvalidResponse)?)
    } else {
        None
    };
    let next_page = if status == 200 {
        next_link(endpoint, &control.links)?
    } else {
        None
    };
    Ok(ApiPage {
        status,
        control,
        value,
        next_page,
    })
}

// Link values can contain commas in angle-bracket URLs or quoted extension parameters.
// Split only outside both, then require a unique next relation and the selected endpoint.
fn next_link(endpoint: &Endpoint, links: &[String]) -> Result<Option<String>, Fault> {
    let mut next = None;
    for link in links {
        let mut start = 0;
        let (mut angle, mut quote, mut escape) = (false, false, false);
        for (i, b) in link.bytes().chain(std::iter::once(b',')).enumerate() {
            if escape {
                escape = false;
                continue;
            }
            if quote && b == b'\\' {
                escape = true;
                continue;
            }
            if b == b'"' && !angle {
                quote = !quote;
            }
            if !quote {
                match b {
                    b'<' if !angle => angle = true,
                    b'>' if angle => angle = false,
                    _ => {}
                }
            }
            if b != b',' || angle || quote {
                continue;
            }
            let part = link[start..i].trim();
            start = i + 1;
            let Some(rest) = part.strip_prefix('<') else {
                return Err(Fault::InvalidResponse);
            };
            let (target, params) = rest.split_once('>').ok_or(Fault::InvalidResponse)?;
            let mut is_next = false;
            let mut relation_seen = false;
            for parameter in parameters(params)? {
                let Some((key, value)) = parameter.trim().split_once('=') else {
                    continue;
                };
                if key.eq_ignore_ascii_case("rel") {
                    if relation_seen {
                        return Err(Fault::Ambiguous);
                    }
                    relation_seen = true;
                    let value = value.trim();
                    let value = if let Some(value) = value.strip_prefix('"') {
                        value.strip_suffix('"').ok_or(Fault::InvalidResponse)?
                    } else {
                        value
                    };
                    is_next = value.split_ascii_whitespace().any(|s| s == "next");
                }
            }
            if is_next {
                let target = endpoint.page_url(Some(target))?.to_string();
                if next.replace(target).is_some() {
                    return Err(Fault::Ambiguous);
                }
            }
        }
        if angle || quote || escape || start != link.len() + 1 {
            return Err(Fault::InvalidResponse);
        }
    }
    Ok(next)
}

fn parameters(value: &str) -> Result<Vec<&str>, Fault> {
    let mut parts = Vec::new();
    let (mut start, mut quote, mut escape) = (0, false, false);
    for (i, b) in value.bytes().chain(std::iter::once(b';')).enumerate() {
        if escape {
            escape = false;
            continue;
        }
        if quote && b == b'\\' {
            escape = true;
            continue;
        }
        if b == b'"' {
            quote = !quote;
        }
        if b == b';' && !quote {
            let part = value[start..i].trim();
            if start == 0 && !part.is_empty() {
                return Err(Fault::InvalidResponse);
            }
            if start != 0 {
                parts.push(part);
            }
            start = i + 1;
        }
    }
    if quote || escape {
        return Err(Fault::InvalidResponse);
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::http::HttpResponse;
    fn endpoint() -> Endpoint {
        Endpoint::new(
            "https://api.github.com/repos/a/b/issues?state=all&per_page=100",
            Some("/repositories/42/issues"),
        )
        .unwrap()
    }
    #[test]
    fn pagination_retains_server_cursor_and_accepts_only_the_selected_endpoint() {
        let endpoint = endpoint();
        let target =
            "https://api.github.com/repositories/42/issues?after=opaque%2Ccursor&per_page=100";
        let links = [format!(
            "<{target}>; title=\"a,b;rel=prev\"; rel=\"next\", <https://api.github.com/repos/a/b/issues?page=1>; rel=\"first\""
        )];
        assert_eq!(
            next_link(&endpoint, &links).unwrap().as_deref(),
            Some(target)
        );
        assert!(
            next_link(
                &endpoint,
                &[format!("<{target}>; rel=next, <{target}>; rel=next")]
            )
            .is_err()
        );
        assert_eq!(
            next_link(&endpoint, &[format!("<{target}>; title=\"x;rel=next\"")]).unwrap(),
            None
        );
        for target in [
            "https://example.com/repos/a/b/issues?page=2",
            "http://api.github.com/repos/a/b/issues?page=2",
            "https://token@api.github.com/repos/a/b/issues",
            "https://api.github.com/repos/other/b/issues?page=2",
            "https://api.github.com/repos/a/b/issues#fragment",
            "https://api.github.com/repos/a/b/issues\n",
        ] {
            assert!(
                next_link(&endpoint, &[format!("<{target}>; rel=next")]).is_err(),
                "{target}"
            );
        }
    }
    #[test]
    fn conditional_not_modified_and_throttled_responses_preserve_metadata_without_parsing_error_bodies()
     {
        for status in [304, 403, 429] {
            let result = decode(
                &endpoint(),
                HttpExchange {
                    response: HttpResponse {
                        status,
                        body: b"not JSON".to_vec(),
                    },
                    headers: vec![
                        ("ETag".into(), "W/\"exact\"".into()),
                        ("Retry-After".into(), "120".into()),
                    ],
                },
                100_000,
                60_000,
            )
            .unwrap();
            assert_eq!(result.status, status);
            assert!(result.value.is_none());
            assert_eq!(result.control.etag.as_deref(), Some("W/\"exact\""));
            assert_eq!(result.control.poll_after_ms, 220_000);
        }
        let request = conditional(
            HttpRequest::get("https://api.github.com/user"),
            Some(&Validators {
                etag: Some("W/\"exact\"".into()),
                last_modified: Some("Fri, 06 Nov 2026 10:00:00 GMT".into()),
            }),
        )
        .unwrap();
        assert_eq!(request.header_value("If-None-Match"), Some("W/\"exact\""));
        assert_eq!(
            request.header_value("If-Modified-Since"),
            Some("Fri, 06 Nov 2026 10:00:00 GMT")
        );
        assert!(
            conditional(
                request,
                Some(&Validators {
                    etag: Some("a\r\nAuthorization: injected".into()),
                    last_modified: None
                })
            )
            .is_err()
        );
    }
}
