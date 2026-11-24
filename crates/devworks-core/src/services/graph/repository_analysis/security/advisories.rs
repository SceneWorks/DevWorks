//! Exact-version NuGet advisory checks through the shared public HTTPS egress.
use super::{Category, Finding, Severity};
use crate::ports::{
    storage::Cancellation,
    web::{HopTransport, Resolver},
};
use crate::services::web::egress;
use regex::Regex;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};

const HOST: &str = "api.nuget.org";
const ROOT: &str = "/v3/registration5-semver2/";
const TTL_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_CACHE_ENTRIES: usize = 4096;
const MAX_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_FEED_RESPONSES: usize = 64;
const MAX_ADVISORIES_PER_VERSION: usize = 128;

static EXACT_VERSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[0-9]+(?:\.[0-9]+){1,3}(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$")
        .expect("exact NuGet version")
});

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(in crate::services::graph::repository_analysis) struct Package {
    pub name: String,
    pub version: String,
}
impl Package {
    pub(in crate::services::graph::repository_analysis) fn exact(
        name: &str,
        version: &str,
    ) -> Option<Self> {
        if !(1..=100).contains(&name.len())
            || !name.as_bytes()[0].is_ascii_alphanumeric()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
            || name.contains("..")
            || version.len() > 128
            || !EXACT_VERSION.is_match(version)
        {
            return None;
        }
        Some(Self {
            name: name.to_ascii_lowercase(),
            version: version.to_ascii_lowercase(),
        })
    }
}

#[derive(Clone, Debug)]
struct Advisory {
    severity: Severity,
    url: Option<String>,
}
#[derive(Clone)]
struct Cached {
    observed_at: i64,
    advisories: Vec<Advisory>,
}

pub(in crate::services::graph::repository_analysis) enum Coverage {
    Current {
        checked_at: i64,
        findings: Vec<Finding>,
    },
    Stale {
        checked_at: i64,
        findings: Vec<Finding>,
    },
    Unknown {
        cause: &'static str,
    },
}

pub(in crate::services::graph::repository_analysis) struct Feed {
    resolver: Arc<dyn Resolver>,
    transport: Arc<dyn HopTransport>,
    cache: Mutex<BTreeMap<Package, Cached>>,
}
pub(in crate::services::graph::repository_analysis) struct RequestBudget {
    remaining: usize,
    deadline: Instant,
    cancel: Cancellation,
}
impl RequestBudget {
    pub(in crate::services::graph::repository_analysis) fn new(
        cancel: Cancellation,
        deadline: Instant,
    ) -> Self {
        Self {
            remaining: MAX_FEED_RESPONSES,
            deadline,
            cancel,
        }
    }
}

impl Feed {
    pub(in crate::services::graph::repository_analysis) fn new(
        resolver: Arc<dyn Resolver>,
        transport: Arc<dyn HopTransport>,
    ) -> Self {
        Self {
            resolver,
            transport,
            cache: Mutex::new(BTreeMap::new()),
        }
    }

    pub(in crate::services::graph::repository_analysis) fn check(
        &self,
        package: &Package,
        now: i64,
        allowed: &mut impl FnMut() -> bool,
        budget: &mut RequestBudget,
    ) -> Coverage {
        if !allowed() {
            return Coverage::Unknown {
                cause: "AdvisoryEgressDenied",
            };
        }
        let cached = self
            .cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(package).cloned());
        if let Some(cached) = &cached
            && now >= cached.observed_at
            && now - cached.observed_at < TTL_MS
        {
            return Coverage::Current {
                checked_at: cached.observed_at,
                findings: findings(package, &cached.advisories),
            };
        }
        match self.load(package, allowed, budget) {
            Ok(advisories) => {
                let reported = findings(package, &advisories);
                if let Ok(mut cache) = self.cache.lock() {
                    if cache.len() >= MAX_CACHE_ENTRIES
                        && let Some(oldest) = cache
                            .iter()
                            .min_by_key(|(_, entry)| entry.observed_at)
                            .map(|(key, _)| key.clone())
                    {
                        cache.remove(&oldest);
                    }
                    cache.insert(
                        package.clone(),
                        Cached {
                            observed_at: now,
                            advisories,
                        },
                    );
                }
                Coverage::Current {
                    checked_at: now,
                    findings: reported,
                }
            }
            Err(cause) => match cached.filter(|entry| now >= entry.observed_at && allowed()) {
                Some(cached) => Coverage::Stale {
                    checked_at: cached.observed_at,
                    findings: findings(package, &cached.advisories),
                },
                None => Coverage::Unknown { cause },
            },
        }
    }

    fn load(
        &self,
        package: &Package,
        allowed: &mut impl FnMut() -> bool,
        budget: &mut RequestBudget,
    ) -> Result<Vec<Advisory>, &'static str> {
        let prefix = format!("{ROOT}{}/", package.name);
        let root = format!("https://{HOST}{prefix}index.json");
        let index = self.get(&root, &prefix, allowed, budget)?;
        let pages = index["items"].as_array().ok_or("IncompleteAdvisoryIndex")?;
        let mut visited = BTreeSet::new();
        for page in pages {
            let owned;
            let entries = if let Some(items) = page["items"].as_array() {
                items
            } else {
                let url = page["@id"].as_str().ok_or("IncompleteAdvisoryPage")?;
                if !visited.insert(url) {
                    return Err("RepeatedAdvisoryPage");
                }
                owned = self.get(url, &prefix, allowed, budget)?;
                owned["items"].as_array().ok_or("IncompleteAdvisoryPage")?
            };
            for item in entries {
                let entry = item["catalogEntry"]
                    .as_object()
                    .ok_or("IncompleteAdvisoryEntry")?;
                let version = entry
                    .get("version")
                    .and_then(Value::as_str)
                    .ok_or("IncompleteAdvisoryVersion")?;
                if version.eq_ignore_ascii_case(&package.version) {
                    return parse_advisories(entry.get("vulnerabilities"));
                }
            }
        }
        Err("ExactAdvisoryVersionUnavailable")
    }

    fn get(
        &self,
        url: &str,
        prefix: &str,
        allowed: &mut impl FnMut() -> bool,
        budget: &mut RequestBudget,
    ) -> Result<Value, &'static str> {
        if !allowed() {
            return Err("AdvisoryEgressDenied");
        }
        if budget.remaining == 0 {
            return Err("AdvisoryResponseLimit");
        }
        if budget.cancel.is_cancelled() || Instant::now() >= budget.deadline {
            return Err("AdvisoryDeadline");
        }
        let url = fixed_page(url, prefix)?;
        budget.remaining -= 1;
        let deadline = budget.deadline.min(Instant::now() + Duration::from_secs(5));
        let cancel = vec![budget.cancel.clone()];
        let port = egress::port(&url);
        let egress::Admission::Allowed { addrs, .. } =
            egress::admit_destination(self.resolver.as_ref(), &url, port, false, deadline, &cancel)
        else {
            return Err("AdvisoryDestinationUnavailable");
        };
        if !allowed() {
            return Err("AdvisoryEgressDenied");
        }
        let response = egress::send(
            self.transport.as_ref(),
            &url,
            port,
            &addrs,
            &egress::Outgoing {
                method: "GET",
                body: &[],
                deadline,
                cancel,
                user_agent: crate::services::web::USER_AGENT,
                accept: "application/json",
                headers: &[],
            },
            &mut |_, _| {},
        )
        .map_err(|_| "AdvisoryTransportUnavailable")?;
        if response.status != 200 {
            return Err("AdvisoryFeedUnavailable");
        }
        let mut bytes = Vec::new();
        response
            .body
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "AdvisoryResponseUnavailable")?;
        if bytes.len() as u64 > MAX_RESPONSE_BYTES {
            return Err("AdvisoryResponseByteLimit");
        }
        serde_json::from_slice(&bytes).map_err(|_| "InvalidAdvisoryResponse")
    }
}

fn fixed_page(value: &str, prefix: &str) -> Result<egress::Url, &'static str> {
    let parsed = url::Url::parse(value).map_err(|_| "InvalidAdvisoryPageUrl")?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some(HOST)
        || parsed.port().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !parsed.path().starts_with(prefix)
        || parsed.path().contains('%')
        || !parsed.path().ends_with(".json")
    {
        return Err("AdvisoryPageOutsideFeed");
    }
    egress::parse(parsed.as_str()).map_err(|_| "InvalidAdvisoryPageUrl")
}

fn parse_advisories(value: Option<&Value>) -> Result<Vec<Advisory>, &'static str> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let entries = value.as_array().ok_or("InvalidAdvisoryEntries")?;
    if entries.len() > MAX_ADVISORIES_PER_VERSION {
        return Err("AdvisoryEntryLimit");
    }
    entries
        .iter()
        .map(|entry| {
            let entry = entry.as_object().ok_or("InvalidAdvisoryEntry")?;
            let label = entry.get("severity").map(|severity| {
                severity
                    .as_str()
                    .map(str::to_ascii_lowercase)
                    .unwrap_or_else(|| severity.to_string())
            });
            let severity = match label.as_deref() {
                Some("0" | "low") => Severity::Low,
                Some("2" | "high") => Severity::High,
                Some("3" | "critical") => Severity::Critical,
                _ => Severity::Medium,
            };
            let url = entry
                .get("advisoryUrl")
                .and_then(Value::as_str)
                .filter(|url| url.len() <= 2048)
                .and_then(|value| url::Url::parse(value).ok())
                .filter(|url| {
                    url.scheme() == "https"
                        && url.username().is_empty()
                        && url.password().is_none()
                        && url.query().is_none()
                        && url.fragment().is_none()
                })
                .map(|url| url.to_string());
            Ok(Advisory { severity, url })
        })
        .collect()
}

fn findings(package: &Package, advisories: &[Advisory]) -> Vec<Finding> {
    advisories
        .iter()
        .map(|advisory| Finding {
            category: Category::VulnerablePackage,
            severity: advisory.severity,
            title: "Vulnerable NuGet package".into(),
            path: None,
            line: None,
            package: Some(package.name.clone()),
            version: Some(package.version.clone()),
            advisory: advisory.url.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::ports::web::{HopRequest, HopResponse, NetFault};
    use std::{collections::VecDeque, io::Cursor, net::IpAddr};

    struct ResolverFixture;
    impl Resolver for ResolverFixture {
        fn resolve(
            &self,
            host: &str,
            port: u16,
            _: Instant,
            _: &[Cancellation],
        ) -> Result<Vec<IpAddr>, NetFault> {
            assert_eq!((host, port), (HOST, 443));
            Ok(vec!["1.1.1.1".parse().unwrap()])
        }
    }

    #[derive(Default)]
    struct TransportFixture(Mutex<VecDeque<(&'static str, Result<Value, NetFault>)>>);
    impl HopTransport for TransportFixture {
        fn get(&self, request: &HopRequest<'_>) -> Result<HopResponse, NetFault> {
            let (target, answer) = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .expect("only the expected advisory requests");
            assert_eq!(request.target, target);
            assert!(request.headers.is_empty());
            Ok(HopResponse {
                status: 200,
                headers: Vec::new(),
                body: Box::new(Cursor::new(serde_json::to_vec(&answer?).unwrap())),
            })
        }
    }

    #[test]
    fn only_exact_package_version_is_current_across_cache_failure_and_denial() {
        const INDEX: &str = "/v3/registration5-semver2/example.library/index.json";
        const PAGE: &str = "/v3/registration5-semver2/example.library/page/1.0.0/2.0.0.json";
        let response = Arc::new(TransportFixture(Mutex::new(VecDeque::from([
            (
                INDEX,
                Ok(json!({"items": [{"@id": format!("https://{HOST}{PAGE}")}]})),
            ),
            (
                PAGE,
                Ok(json!({"items": [
                    {"catalogEntry":{"version":"1.0.0","vulnerabilities":[{"severity":3}]}},
                    {"catalogEntry":{"version":"2.0.0","vulnerabilities":[{"severity":2}]}}
                ]})),
            ),
            (
                INDEX,
                Err(NetFault::NotSent {
                    timed_out: true,
                    reason: "fixture loss".into(),
                }),
            ),
        ]))));
        let feed = Feed::new(Arc::new(ResolverFixture), response.clone());
        let package = Package::exact("Example.Library", "2.0.0").unwrap();
        let mut requests = RequestBudget::new(
            Cancellation::new(),
            Instant::now() + Duration::from_secs(10),
        );
        assert!(matches!(
            feed.check(&package, 1, &mut || true, &mut requests),
            Coverage::Current { checked_at: 1, findings }
                if findings.len() == 1 && findings[0].severity == Severity::High
        ));
        assert!(matches!(
            feed.check(&package, 2, &mut || true, &mut requests),
            Coverage::Current { checked_at: 1, .. }
        ));
        assert!(matches!(
            feed.check(&package, TTL_MS + 2, &mut || true, &mut requests),
            Coverage::Stale { checked_at: 1, .. }
        ));
        assert!(matches!(
            feed.check(&package, TTL_MS + 3, &mut || false, &mut requests),
            Coverage::Unknown {
                cause: "AdvisoryEgressDenied"
            }
        ));
        assert!(response.0.lock().unwrap().is_empty());
    }

    #[test]
    fn validates_exact_version_and_same_origin_pages() {
        assert!(Package::exact("Example.Library", "2.4.0-preview.1").is_some());
        assert!(Package::exact("Example.Library", "[2.4.0,3.0)").is_none());
        assert!(Package::exact("../Library", "2.4.0").is_none());
        let prefix = "/v3/registration5-semver2/example.library/";
        for url in [
            "https://other.test.invalid/v3/registration5-semver2/example.library/index.json",
            "https://api.nuget.org/v3/registration5-semver2/other/index.json",
            "https://api.nuget.org/v3/registration5-semver2/example.library/%2f.json",
            "https://api.nuget.org/v3/registration5-semver2/example.library/data.json?q=test",
        ] {
            assert!(fixed_page(url, prefix).is_err());
        }
        assert!(fixed_page("https://api.nuget.org/v3/registration5-semver2/example.library/page/2.0.0/3.0.0.json", prefix).is_ok());
    }

    #[test]
    fn preserves_only_safe_advisory_urls_and_known_severity() {
        let value = json!([
            {"severity": "Moderate", "advisoryUrl": "https://advisory.test.invalid/issue/7"},
            {"severity": "3", "advisoryUrl": "http://advisory.test.invalid/?token=secret"},
        ]);
        let advisories = parse_advisories(Some(&value)).unwrap();
        assert_eq!(advisories[0].severity, Severity::Medium);
        assert!(advisories[0].url.is_some());
        assert_eq!(advisories[1].severity, Severity::Critical);
        assert!(advisories[1].url.is_none());
        assert!(parse_advisories(Some(&json!("malformed"))).is_err());
    }
}
