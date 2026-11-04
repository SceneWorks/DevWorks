//! Brave Search API web results as a [`SearchProvider`] (`web.searchProvider = "brave"`).
//!
//! `GET {endpoint}?q=&count=&offset=` with the API key in `X-Subscription-Token`, sent only to
//! the configured endpoint over TLS (the same native transport as fetches; no redirect is
//! followed, so the key never reaches another origin). The API pages by page index (`offset`
//! 0..=9) of up to 20 results; result offsets that do not fall on a page boundary are served
//! from the page that contains them. The answer body is bounded to [`BODY_LIMIT`] bytes.
use devworks_core::ports::web::{
    HopRequest, HttpTransport, NetFault, Resolver, SearchFault, SearchHit, SearchPage,
    SearchProvider, SearchRequest,
};
use devworks_core::services::web::egress;
use serde_json::Value;
use std::io::{ErrorKind, Read};
use std::net::SocketAddr;
use std::sync::Arc;

/// The provider ID.
pub const BRAVE: &str = "brave";
/// The public web search endpoint.
pub const ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
/// Results per API page.
const PAGE: u32 = 20;
/// Highest API page index.
const LAST_PAGE: u32 = 9;
/// Largest answer read.
pub const BODY_LIMIT: usize = 2 * 1024 * 1024;

/// The Brave Search provider.
pub struct BraveSearch {
    resolver: Arc<dyn Resolver>,
    transport: Arc<dyn HttpTransport>,
    endpoint: url::Url,
}

impl BraveSearch {
    pub fn new(resolver: Arc<dyn Resolver>, transport: Arc<dyn HttpTransport>) -> Self {
        Self::with_endpoint(resolver, transport, ENDPOINT).expect("the built-in endpoint parses")
    }
    /// The provider against another endpoint (a test server).
    pub fn with_endpoint(
        resolver: Arc<dyn Resolver>,
        transport: Arc<dyn HttpTransport>,
        endpoint: &str,
    ) -> Result<Self, String> {
        let endpoint = egress::parse(endpoint).map_err(|r| format!("{r:?}"))?;
        Ok(Self {
            resolver,
            transport,
            endpoint,
        })
    }
}

impl SearchProvider for BraveSearch {
    fn id(&self) -> &'static str {
        BRAVE
    }
    fn requires_credential(&self) -> bool {
        true
    }
    fn page_max(&self) -> u32 {
        PAGE
    }
    fn offset_max(&self) -> u32 {
        LAST_PAGE * PAGE + PAGE - 1
    }

    fn search(&self, request: &SearchRequest<'_>) -> Result<SearchPage, SearchFault> {
        let Some(key) = request.credential else {
            return Err(SearchFault::CredentialRejected);
        };
        let page = request.offset / PAGE;
        let skip = (request.offset % PAGE) as usize;
        if page > LAST_PAGE {
            return Ok(SearchPage {
                hits: Vec::new(),
                more: false,
            });
        }
        let mut url = self.endpoint.clone();
        url.query_pairs_mut()
            .append_pair("q", request.query)
            .append_pair("count", &PAGE.to_string())
            .append_pair("offset", &page.to_string());
        let port = egress::port(&url);
        let addrs = match egress::destination(&url) {
            egress::Destination::Literal(ip) => vec![ip],
            egress::Destination::Domain(d) => self
                .resolver
                .resolve(&d, port, request.deadline, &request.cancel)
                .map_err(SearchFault::Net)?,
        };
        let (host_header, server_name, target) = (
            egress::host_header(&url),
            egress::server_name(&url),
            egress::request_target(&url),
        );
        let headers = [("X-Subscription-Token", key.expose())];
        let mut last = NetFault::NotSent {
            reason: "no address".into(),
            timed_out: false,
        };
        let mut response = None;
        for ip in addrs {
            match self.transport.get(&HopRequest {
                scheme: egress::scheme(&url),
                host_header: &host_header,
                server_name: &server_name,
                target: &target,
                addr: SocketAddr::new(ip, port),
                deadline: request.deadline,
                cancel: request.cancel.clone(),
                user_agent: devworks_core::services::web::USER_AGENT,
                accept: "application/json",
                headers: &headers,
            }) {
                Ok(r) => {
                    response = Some(r);
                    break;
                }
                Err(f @ NetFault::NotSent { .. }) if !f.timed_out() => last = f,
                Err(f) => return Err(SearchFault::Net(f)),
            }
        }
        let Some(mut response) = response else {
            return Err(SearchFault::Net(last));
        };
        match response.status {
            200 => {}
            401 | 403 => return Err(SearchFault::CredentialRejected),
            429 => return Err(SearchFault::RateLimited),
            status => return Err(SearchFault::Rejected { status }),
        }
        let mut body = Vec::new();
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match response.body.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    body.extend_from_slice(&chunk[..n]);
                    if body.len() > BODY_LIMIT {
                        return Err(SearchFault::Malformed("the answer is too large".into()));
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {
                    return Err(SearchFault::Net(NetFault::Cancelled { sent: true }));
                }
                Err(e) => {
                    return Err(SearchFault::Net(NetFault::AfterSend {
                        reason: format!("the answer could not be read: {e}"),
                        timed_out: e.kind() == ErrorKind::TimedOut,
                    }));
                }
            }
        }
        parse(&body, skip, request.count as usize)
    }
}

/// Parses an answer: `web.results[]` (absent when there are none) and
/// `query.more_results_available`.
fn parse(body: &[u8], skip: usize, count: usize) -> Result<SearchPage, SearchFault> {
    let v: Value = serde_json::from_slice(body)
        .map_err(|e| SearchFault::Malformed(format!("not JSON: {e}")))?;
    if !v.is_object() {
        return Err(SearchFault::Malformed("not a JSON object".into()));
    }
    let results = match v.pointer("/web/results") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.clone(),
        Some(_) => return Err(SearchFault::Malformed("web.results is not a list".into())),
    };
    let text = |r: &Value, k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
    let available = results.len();
    let hits: Vec<SearchHit> = results
        .iter()
        .skip(skip)
        .take(count)
        .map(|r| SearchHit {
            title: text(r, "title"),
            url: text(r, "url"),
            snippet: text(r, "description"),
        })
        .collect();
    let more_on_page = available > skip + count;
    let more = more_on_page
        || v.pointer("/query/more_results_available")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    Ok(SearchPage { hits, more })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_parse_with_paging_and_absent_results() {
        let body = br#"{"query":{"more_results_available":true},"web":{"results":[
            {"title":"A","url":"https://a.example/","description":"<strong>a</strong>"},
            {"title":"B","url":"https://b.example/","description":"b"},
            {"title":"C","url":"https://c.example/","description":"c"}]}}"#;
        let p = parse(body, 1, 1).unwrap();
        assert_eq!(p.hits.len(), 1);
        assert_eq!(p.hits[0].title, "B");
        assert!(p.more);
        let none = parse(br#"{"query":{"more_results_available":false}}"#, 0, 5).unwrap();
        assert!(none.hits.is_empty() && !none.more);
        assert!(matches!(parse(b"[]", 0, 1), Err(SearchFault::Malformed(_))));
        assert!(matches!(
            parse(br#"{"web":{"results":7}}"#, 0, 1),
            Err(SearchFault::Malformed(_))
        ));
    }
}
