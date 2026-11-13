//! Egress policy of the owned browser: which requests a page may send. Every request the
//! browser intercepts (navigations, redirect hops, subresources, frames and workers) passes
//! [`decide`] before it is continued.
//!
//! * Only `http` and `https` reach the network; `data:`, `blob:` and `about:` never leave the
//!   browser and pass; every other scheme is refused.
//! * Addresses are classified by the same function `web.fetch` uses
//!   ([`egress::classify`]), after resolution and for every resolved address: one refused
//!   address refuses the request.
//! * Loopback (`localhost` names, `127.0.0.0/8`, `::1`) is always allowed: previewing local
//!   development servers is the browser's main use. Link-local and cloud metadata addresses and
//!   names are never allowed. The private ranges (RFC 1918, unique-local, shared CGNAT) follow
//!   the workspace's private-network setting of `web.fetch`. See
//!   [`AddressClass::browser_allowed`].
//!
//! The browser resolves names itself when it connects, so a name whose answer changes between
//! this check and that connection (DNS rebinding) is not caught here.
use super::cdp::RequestGate;
use crate::ports::storage::{ApplicationStore, ReadOptions, read_snapshot};
use crate::ports::web::Resolver;
use crate::services::web::egress::{self, AddressClass, Destination};
use crate::services::web::read_policy;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use url::Url;

/// Longest one resolution may take (the protocol connection waits meanwhile).
pub const RESOLVE_BOUND: Duration = Duration::from_secs(5);
/// How long a resolved answer is reused for the same name.
pub const ANSWER_TTL: Duration = Duration::from_secs(10);
/// Most names whose answers are kept.
const KEPT_ANSWERS: usize = 256;

/// Answers a domain's addresses (for a port).
pub type ResolveFn<'a> = dyn FnMut(&str, u16) -> Result<Vec<IpAddr>, String> + 'a;
/// Resolved answers by name and port, with when they were resolved.
type Answers = HashMap<(String, u16), (Instant, Vec<IpAddr>)>;

/// Why a request is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    /// The refused address class, when an address (or a name of a known class) was refused.
    pub class: Option<AddressClass>,
    pub detail: String,
}
fn refused(class: Option<AddressClass>, detail: impl Into<String>) -> Refused {
    Refused {
        class,
        detail: detail.into(),
    }
}

/// Decides one request URL. `allow_private` reads the private-network setting (only when a
/// private address is met); `resolve` answers a domain's addresses.
pub fn decide(
    url: &str,
    allow_private: &dyn Fn() -> bool,
    resolve: &mut ResolveFn<'_>,
) -> Result<(), Refused> {
    let scheme = url
        .split_once(':')
        .map(|(s, _)| s.to_ascii_lowercase())
        .unwrap_or_default();
    match scheme.as_str() {
        "http" | "https" => {}
        "data" | "blob" | "about" => return Ok(()),
        _ => {
            return Err(refused(
                None,
                format!("the {scheme:?} scheme is not allowed"),
            ));
        }
    }
    let parsed = Url::parse(url).map_err(|_| refused(None, "the URL is not valid"))?;
    if parsed.host().is_none() {
        return Err(refused(None, "the URL has no host"));
    }
    // The setting is read at most once, and only when a private address is met.
    let setting = std::cell::OnceCell::new();
    let check = |class: AddressClass, shown: &str| -> Result<(), Refused> {
        if class.browser_allowed(false)
            || (class.browser_allowed(true) && *setting.get_or_init(allow_private))
        {
            Ok(())
        } else {
            Err(refused(
                Some(class),
                format!("{shown} is a {} address", class.as_str()),
            ))
        }
    };
    let addrs = match egress::destination(&parsed) {
        Destination::Literal(ip) => vec![ip],
        Destination::Domain(domain) => {
            if let Some(class) = egress::name_class(&domain) {
                return check(class, &domain);
            }
            let addrs = resolve(&domain, egress::port(&parsed))
                .map_err(|e| refused(None, format!("{domain} could not be resolved: {e}")))?;
            if addrs.is_empty() {
                return Err(refused(None, format!("{domain} has no addresses")));
            }
            addrs
        }
    };
    // Every answer must be allowed, so a resolver cannot mix a public and a refused answer.
    for ip in addrs {
        check(egress::classify(ip), &ip.to_string())?;
    }
    Ok(())
}

/// The production gate: the workspace setting read from the store when needed, names resolved
/// through `resolver` (briefly reused per name). Without a resolver only literal addresses and
/// `localhost` names can be admitted.
pub struct BrowserEgress {
    store: Arc<dyn ApplicationStore>,
    resolver: Option<Arc<dyn Resolver>>,
    answers: Mutex<Answers>,
}

impl BrowserEgress {
    pub fn new(store: Arc<dyn ApplicationStore>, resolver: Option<Arc<dyn Resolver>>) -> Self {
        Self {
            store,
            resolver,
            answers: Mutex::new(HashMap::new()),
        }
    }

    /// Decides `url` under the current setting.
    pub fn check(&self, url: &str) -> Result<(), Refused> {
        let allow_private = || {
            read_snapshot(&*self.store, &ReadOptions::default(), |s| {
                read_policy(s.configuration())
            })
            // A setting that cannot be read refuses the private ranges.
            .is_ok_and(|p| p.allow_private)
        };
        decide(url, &allow_private, &mut |host, port| {
            self.resolve(host, port)
        })
    }

    fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, String> {
        let key = (host.to_owned(), port);
        let now = Instant::now();
        if let Some((at, addrs)) = self
            .answers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            && now.duration_since(*at) < ANSWER_TTL
        {
            return Ok(addrs.clone());
        }
        let Some(resolver) = &self.resolver else {
            return Err("no resolver is configured".into());
        };
        let addrs = resolver
            .resolve(host, port, now + RESOLVE_BOUND, &[])
            .map_err(|f| f.reason().to_owned())?;
        let mut answers = self.answers.lock().unwrap_or_else(|p| p.into_inner());
        if answers.len() >= KEPT_ANSWERS {
            answers.retain(|_, (at, _)| now.duration_since(*at) < ANSWER_TTL);
            if answers.len() >= KEPT_ANSWERS {
                answers.clear();
            }
        }
        answers.insert(key, (now, addrs.clone()));
        Ok(addrs)
    }
}

impl RequestGate for BrowserEgress {
    fn admit(&self, url: &str) -> Result<(), String> {
        self.check(url).map_err(|r| r.detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolves `public.example` to a global address, `rebind.example` to one global and one
    /// metadata address, `lan.example` to a private one and `ula.example` to a unique-local
    /// one; anything else fails. Records the names asked.
    fn resolver(asked: &mut Vec<String>) -> impl FnMut(&str, u16) -> Result<Vec<IpAddr>, String> {
        move |host: &str, _| {
            asked.push(host.to_owned());
            let ips: &[&str] = match host {
                "public.example" => &["93.184.216.34"],
                "rebind.example" => &["93.184.216.34", "169.254.169.254"],
                "lan.example" => &["192.168.1.10"],
                "ula.example" => &["fd12:3456::1"],
                "empty.example" => &[],
                _ => return Err("no such name".into()),
            };
            Ok(ips.iter().map(|s| s.parse().unwrap()).collect())
        }
    }

    fn verdict(url: &str, allow_private: bool) -> Result<(), Refused> {
        let mut asked = Vec::new();
        decide(url, &|| allow_private, &mut resolver(&mut asked))
    }

    fn class(url: &str, allow_private: bool) -> Option<AddressClass> {
        verdict(url, allow_private).expect_err(url).class
    }

    #[test]
    fn link_local_and_metadata_are_refused_whatever_the_setting() {
        for allow_private in [false, true] {
            for url in [
                "http://169.254.169.254/",
                "http://169.254.169.254/latest/meta-data/",
                "http://2852039166/",
                "http://[::ffff:169.254.169.254]/",
                "http://[fd00:ec2::254]/",
                "http://metadata.google.internal/computeMetadata/v1/",
                "http://METADATA.GOOGLE.INTERNAL./",
                "http://100.100.100.200/",
            ] {
                assert_eq!(
                    class(url, allow_private),
                    Some(AddressClass::Metadata),
                    "{url}"
                );
            }
            for url in ["http://169.254.1.1/", "http://[fe80::1]/"] {
                assert_eq!(
                    class(url, allow_private),
                    Some(AddressClass::LinkLocal),
                    "{url}"
                );
            }
            // One refused answer refuses the name, even beside a public one.
            assert_eq!(
                class("https://rebind.example/", allow_private),
                Some(AddressClass::Metadata)
            );
            assert_eq!(
                class("http://0.0.0.0:8080/", allow_private),
                Some(AddressClass::Unspecified)
            );
        }
    }

    #[test]
    fn loopback_is_always_allowed() {
        for allow_private in [false, true] {
            for url in [
                "http://localhost:3000/",
                "http://app.localhost:5173/",
                "http://127.0.0.1:8080/",
                "http://127.1.2.3/",
                "http://[::1]:4000/",
                "http://2130706433/",
            ] {
                assert_eq!(verdict(url, allow_private), Ok(()), "{url}");
            }
        }
        // `localhost` names are loopback without asking the resolver.
        let mut asked = Vec::new();
        decide("http://localhost/", &|| false, &mut resolver(&mut asked)).unwrap();
        assert!(asked.is_empty());
    }

    #[test]
    fn private_ranges_follow_the_private_network_setting() {
        for (url, c) in [
            ("http://192.168.1.1/", AddressClass::Private),
            ("http://10.0.0.5:8000/", AddressClass::Private),
            ("http://172.16.0.1/", AddressClass::Private),
            ("http://100.64.0.1/", AddressClass::Shared),
            ("http://[fd12:3456::1]/", AddressClass::UniqueLocal),
            ("https://lan.example/", AddressClass::Private),
            ("https://ula.example/", AddressClass::UniqueLocal),
        ] {
            assert_eq!(class(url, false), Some(c), "{url}");
            assert_eq!(verdict(url, true), Ok(()), "{url}");
        }
    }

    #[test]
    fn public_and_local_only_urls_pass_and_other_schemes_or_unresolved_names_do_not() {
        for url in [
            "https://public.example/a.js",
            "http://8.8.8.8/",
            "data:text/html,hi",
            "blob:https://public.example/1234",
            "about:blank",
        ] {
            assert_eq!(verdict(url, false), Ok(()), "{url}");
        }
        for url in [
            "file:///etc/passwd",
            "chrome://settings/",
            "ftp://public.example/",
            "https://unknown.example/",
            "https://empty.example/",
            "not a url",
        ] {
            let r = verdict(url, true).expect_err(url);
            assert_eq!(r.class, None, "{url}");
        }
        // The setting is read only when a private address is met.
        let mut asked = Vec::new();
        decide(
            "https://public.example/",
            &|| panic!("the setting was read"),
            &mut resolver(&mut asked),
        )
        .unwrap();
    }
}
