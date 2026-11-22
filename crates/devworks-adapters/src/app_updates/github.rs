//! Public, credential-free DevWorks release discovery.
use devworks_core::ports::http::{HttpRequest, HttpTransport};
use devworks_core::ports::storage::Cancellation;
use devworks_core::ports::update::{
    MAX_RELEASE_PAGES, RELEASES_PER_PAGE, Release, ReleaseAsset, ReleaseError, ReleasePage,
};
use devworks_core::services::github::response::ResponseControl;
use serde::Deserialize;
use url::Url;

pub(super) const ORIGIN: &str = "https://api.github.com";
const PATH: &str = "/repos/SceneWorks/DevWorks/releases";
const MAX_RESPONSE: usize = 1024 * 1024;

#[derive(Deserialize)]
struct GithubRelease {
    id: u64,
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<ReleaseAsset>,
}

pub(super) fn discover(
    transport: &dyn HttpTransport,
    cached: &[ReleasePage],
    cancellation: &Cancellation,
    now: i64,
) -> Result<Vec<ReleasePage>, ReleaseError> {
    let mut pages = Vec::new();
    let mut number = 1;
    loop {
        if cancellation.is_cancelled() {
            return Err(ReleaseError::cancelled());
        }
        if number > MAX_RELEASE_PAGES {
            return Err(ReleaseError::rejected(
                "The DevWorks release list exceeded the complete scan limit.",
            ));
        }
        let old = cached.iter().find(|page| page.number == number);
        let mut request = HttpRequest::get(format!(
            "{ORIGIN}{PATH}?per_page={RELEASES_PER_PAGE}&page={number}"
        ))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
        request.timeout_ms = 12_000;
        request.max_response_bytes = MAX_RESPONSE;
        if let Some(etag) = old.and_then(|page| page.etag.as_ref()) {
            request = request.header("If-None-Match", etag);
        }
        let exchange = transport.send_with_headers(&request).map_err(|_| {
            ReleaseError::rejected("The DevWorks release feed could not be reached.")
        })?;
        let control = ResponseControl::observe(&exchange, now, 0).map_err(|_| {
            ReleaseError::rejected("GitHub returned invalid release response headers.")
        })?;
        let next = next_page(&control.links, number)?;
        let page = match exchange.response.status {
            304 => {
                let mut page =
                    old.filter(|page| page.etag.is_some())
                        .cloned()
                        .ok_or_else(|| {
                            ReleaseError::rejected(
                                "GitHub reported an unchanged page with no cached release page.",
                            )
                        })?;
                if control.etag.is_some() {
                    page.etag = control.etag;
                }
                if !control.links.is_empty() {
                    page.next = next;
                }
                page
            }
            200 => ReleasePage {
                number,
                etag: control.etag,
                next,
                releases: decode(&exchange.response.body)?,
            },
            403 | 429 => {
                return Err(ReleaseError {
                    message: "GitHub limited the DevWorks release check. It will retry when due."
                        .into(),
                    retry_after_ms: control.backoff_until_ms,
                    cancelled: false,
                });
            }
            _ => {
                return Err(ReleaseError::rejected(
                    "GitHub could not return the DevWorks release list.",
                ));
            }
        };
        let next = page.next;
        pages.push(page);
        match next {
            Some(next) if next == number + 1 => number = next,
            Some(_) => return Err(bad_link()),
            None => return Ok(pages),
        }
    }
}

fn decode(bytes: &[u8]) -> Result<Vec<Release>, ReleaseError> {
    let entries: Vec<GithubRelease> = serde_json::from_slice(bytes)
        .map_err(|_| ReleaseError::rejected("GitHub returned an invalid DevWorks release page."))?;
    if entries.len() > RELEASES_PER_PAGE {
        return Err(ReleaseError::rejected(
            "GitHub returned too many releases on a page.",
        ));
    }
    Ok(entries
        .into_iter()
        .filter(|release| {
            !release.draft && release.tag_name.starts_with('v') && release.tag_name.len() <= 128
        })
        .map(|release| Release {
            id: release.id,
            tag: release.tag_name,
            prerelease: release.prerelease,
            assets: release
                .assets
                .into_iter()
                .filter(|asset| asset.name.starts_with("DevWorks-") && asset.name.len() <= 128)
                .collect(),
        })
        .collect())
}

fn bad_link() -> ReleaseError {
    ReleaseError::rejected("GitHub returned an invalid release pagination link.")
}

fn next_page(links: &[String], current: usize) -> Result<Option<usize>, ReleaseError> {
    let mut next = None;
    for link in links.iter().flat_map(|value| value.split(',')) {
        let mut parts = link.split(';').map(str::trim);
        let address = parts.next().unwrap_or_default();
        if !parts.any(|part| part == r#"rel="next""#) {
            continue;
        }
        let raw = address
            .strip_prefix('<')
            .and_then(|url| url.strip_suffix('>'))
            .ok_or_else(bad_link)?;
        let url = Url::parse(raw).map_err(|_| bad_link())?;
        let scoped = url
            .path()
            .strip_prefix("/repositories/")
            .and_then(|path| path.strip_suffix("/releases"))
            .is_some_and(|id| !id.is_empty() && id.bytes().all(|c| c.is_ascii_digit()));
        if !clean_https(&url, "api.github.com")
            || (!scoped && !url.path().eq_ignore_ascii_case(PATH))
        {
            return Err(bad_link());
        }
        let mut pair = url.query_pairs().filter(|(key, _)| key == "page");
        let target = pair
            .next()
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .filter(|number| *number == current + 1)
            .ok_or_else(bad_link)?;
        if pair.next().is_some() || next.replace(target).is_some() {
            return Err(bad_link());
        }
    }
    Ok(next)
}

pub(super) fn clean_https(url: &Url, host: &str) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some(host)
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
}

pub(super) fn asset_path(id: u64) -> String {
    format!("{ORIGIN}{PATH}/assets/{id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use devworks_core::ports::http::{HttpError, HttpExchange, HttpResponse};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct Feed(Mutex<VecDeque<(&'static str, Option<&'static str>, HttpExchange)>>);
    impl HttpTransport for Feed {
        fn send(&self, _: &HttpRequest) -> Result<HttpResponse, HttpError> {
            unreachable!("release discovery uses response headers");
        }
        fn send_with_headers(&self, request: &HttpRequest) -> Result<HttpExchange, HttpError> {
            let (page, etag, response) = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .expect("expected page request");
            assert!(request.url.ends_with(page));
            assert_eq!(request.header_value("If-None-Match"), etag);
            assert!(request.header_value("Authorization").is_none());
            Ok(response)
        }
    }

    fn response(status: u16, body: &str, headers: &[(&str, &str)]) -> HttpExchange {
        HttpExchange {
            response: HttpResponse {
                status,
                body: body.as_bytes().into(),
            },
            headers: headers
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        }
    }

    #[test]
    fn conditional_first_page_does_not_refresh_an_unchecked_later_page() {
        let next =
            r#"<https://api.github.com/repositories/77/releases?per_page=30&page=2>; rel="next""#;
        let feed = Feed(Mutex::new(VecDeque::from([
            (
                "page=1",
                None,
                response(200, "[]", &[("ETag", "1"), ("Link", next)]),
            ),
            (
                "page=2",
                None,
                response(
                    200,
                    r#"[{"id":2,"tag_name":"v1.2.0","draft":false,"prerelease":false,"assets":[]}]"#,
                    &[("ETag", "2")],
                ),
            ),
            ("page=1", Some("1"), response(304, "", &[])),
            ("page=2", Some("2"), response(200, "[]", &[("ETag", "3")])),
            ("page=1", Some("1"), response(304, "", &[])),
            (
                "page=2",
                Some("3"),
                response(429, "", &[("Retry-After", "35")]),
            ),
        ])));
        let cancel = Cancellation::default();
        let cache = discover(&feed, &[], &cancel, 1000).unwrap();
        assert_eq!(cache[1].releases[0].id, 2);
        let cache = discover(&feed, &cache, &cancel, 2000).unwrap();
        assert!(
            cache[1].releases.is_empty(),
            "the revoked later release disappears"
        );
        let error = discover(&feed, &cache, &cancel, 3000).unwrap_err();
        assert_eq!(error.retry_after_ms, Some(38_000));
        assert_eq!(cache[1].etag.as_deref(), Some("3"));
    }

    #[test]
    fn a_foreign_pagination_link_and_unsolicited_304_are_refused() {
        let link =
            r#"<https://evil.example/repos/SceneWorks/DevWorks/releases?page=2>; rel="next""#;
        assert!(next_page(&[link.into()], 1).is_err());
        let feed = Feed(Mutex::new(VecDeque::from([(
            "page=1",
            None,
            response(304, "", &[]),
        )])));
        assert!(discover(&feed, &[], &Cancellation::default(), 1000).is_err());
    }
}
