//! Egress policy of the shared web tools: which URLs may be requested, which addresses may be
//! connected to, and how a URL appears in records and results. The owned browser classifies
//! addresses with the same [`classify`] (see [`AddressClass::browser_allowed`]).
//!
//! * **URLs** are parsed with the WHATWG parser (`url`), so host spellings are canonical before
//!   any decision: decimal, octal, hexadecimal and shortened IPv4 forms (`2130706433`,
//!   `0177.0.0.1`, `0x7f.1`, `127.1`), percent-encoded hosts and IDNA all become the address or
//!   ASCII domain the network would use. Only `http` and `https` with a host are accepted; a URL
//!   carrying a user name or password is refused (and never recorded with it); fragments are
//!   dropped.
//! * **Addresses** are classified after resolution, never from the host text alone. IPv6 forms
//!   that embed an IPv4 address (IPv4-mapped, IPv4-compatible, NAT64 `64:ff9b::/96`, 6to4) are
//!   classified by the embedded address. Only global unicast is allowed by default; loopback,
//!   RFC 1918, shared (CGNAT) and unique-local ranges are allowed only when private networks are
//!   explicitly allowed; link-local, cloud metadata, unspecified (`0.0.0.0`, `::`), multicast,
//!   broadcast, documentation, benchmarking, Teredo and other reserved ranges never are. Metadata
//!   addresses are checked before the ranges that contain them.
//! * **Records** never hold credentials: user info and fragments are dropped, the values of
//!   secret-named query parameters and credential-shaped path segments are replaced, and the
//!   result is redacted again for known secret shapes.
use crate::authority::redact::{REDACTED, Redactor, sensitive_name};
use crate::ports::web::{Scheme, ServerName};
use crate::settings::validate::looks_like_credential;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use url::{Host, Url};

/// Longest URL accepted, requested or recorded (the contract's `Url` bound).
pub const URL_LIMIT: usize = 4096;

/// Why a URL is not requested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UrlRefusal {
    Invalid,
    /// Not `http` or `https`.
    Scheme,
    /// It carries a user name or password.
    Credentials,
    /// No host, or port 0.
    Host,
    TooLong,
}
impl UrlRefusal {
    /// The fault target kind.
    pub fn target(self) -> &'static str {
        match self {
            Self::Invalid => "web_url_invalid",
            Self::Scheme => "web_url_scheme",
            Self::Credentials => "web_url_credentials",
            Self::Host => "web_url_host",
            Self::TooLong => "web_url_length",
        }
    }
}

/// Parses a URL to request.
pub fn parse(raw: &str) -> Result<Url, UrlRefusal> {
    if raw.len() > URL_LIMIT {
        return Err(UrlRefusal::TooLong);
    }
    let mut url = Url::parse(raw).map_err(|_| UrlRefusal::Invalid)?;
    admit(&mut url)?;
    Ok(url)
}

/// Resolves a redirect `Location` against the URL that answered with it.
pub fn join(base: &Url, location: &str) -> Result<Url, UrlRefusal> {
    if location.len() > URL_LIMIT {
        return Err(UrlRefusal::TooLong);
    }
    let mut url = base
        .join(location.trim())
        .map_err(|_| UrlRefusal::Invalid)?;
    admit(&mut url)?;
    Ok(url)
}

fn admit(url: &mut Url) -> Result<(), UrlRefusal> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(UrlRefusal::Scheme);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(UrlRefusal::Credentials);
    }
    if url.host().is_none() || url.port() == Some(0) {
        return Err(UrlRefusal::Host);
    }
    url.set_fragment(None);
    if url.as_str().len() > URL_LIMIT {
        return Err(UrlRefusal::TooLong);
    }
    Ok(())
}

/// Where a URL's host points before resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Destination {
    /// An IP literal (after WHATWG canonicalization of every numeric spelling).
    Literal(IpAddr),
    /// An ASCII domain without a trailing dot.
    Domain(String),
}

pub fn destination(url: &Url) -> Destination {
    match url.host() {
        Some(Host::Ipv4(a)) => Destination::Literal(IpAddr::V4(a)),
        Some(Host::Ipv6(a)) => Destination::Literal(IpAddr::V6(a)),
        Some(Host::Domain(d)) => Destination::Domain(d.trim_end_matches('.').to_owned()),
        None => Destination::Domain(String::new()),
    }
}

pub fn scheme(url: &Url) -> Scheme {
    if url.scheme() == "https" {
        Scheme::Https
    } else {
        Scheme::Http
    }
}

pub fn port(url: &Url) -> u16 {
    url.port_or_known_default().unwrap_or(80)
}

/// The `Host` header: the host as written canonically, with a non-default port.
pub fn host_header(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default().trim_end_matches('.');
    match url.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_owned(),
    }
}

/// What a TLS certificate is verified against.
pub fn server_name(url: &Url) -> ServerName {
    match destination(url) {
        Destination::Literal(ip) => ServerName::Ip(ip),
        Destination::Domain(d) => ServerName::Dns(d),
    }
}

/// The origin-form request target (`/path?query`), without the fragment.
pub fn request_target(url: &Url) -> String {
    match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_owned(),
    }
}

// ---------------------------------------------------------------------------------------------
// Addresses

/// The class of an address, as far as egress is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressClass {
    Global,
    Loopback,
    /// RFC 1918 and deprecated IPv6 site-local.
    Private,
    /// RFC 6598 shared address space (carrier-grade NAT).
    Shared,
    /// IPv6 unique local `fc00::/7`.
    UniqueLocal,
    LinkLocal,
    /// Cloud instance metadata and similar host-internal services.
    Metadata,
    Unspecified,
    Multicast,
    Broadcast,
    Documentation,
    Benchmarking,
    Reserved,
}
impl AddressClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "Global",
            Self::Loopback => "Loopback",
            Self::Private => "Private",
            Self::Shared => "Shared",
            Self::UniqueLocal => "UniqueLocal",
            Self::LinkLocal => "LinkLocal",
            Self::Metadata => "Metadata",
            Self::Unspecified => "Unspecified",
            Self::Multicast => "Multicast",
            Self::Broadcast => "Broadcast",
            Self::Documentation => "Documentation",
            Self::Benchmarking => "Benchmarking",
            Self::Reserved => "Reserved",
        }
    }
    /// Whether a fetch may connect to an address of this class. Private networks are an explicit
    /// workspace setting; link-local and metadata addresses are never reachable.
    pub fn allowed(self, allow_private: bool) -> bool {
        match self {
            Self::Global => true,
            Self::Loopback | Self::Private | Self::Shared | Self::UniqueLocal => allow_private,
            _ => false,
        }
    }
    /// Whether the owned browser may connect to an address of this class. Loopback is always
    /// allowed (previewing local development servers is the browser's main use); every other
    /// class follows [`Self::allowed`], so link-local and metadata addresses are never reachable
    /// and the private ranges follow the same private-network setting as a fetch.
    pub fn browser_allowed(self, allow_private: bool) -> bool {
        self == Self::Loopback || self.allowed(allow_private)
    }
}

/// Instance metadata and host-internal service addresses of the common clouds (AWS, GCP and
/// Azure IMDS, AWS ECS task metadata and Time Sync, Alibaba, Azure WireServer).
const METADATA_V4: [Ipv4Addr; 5] = [
    Ipv4Addr::new(169, 254, 169, 254),
    Ipv4Addr::new(169, 254, 170, 2),
    Ipv4Addr::new(169, 254, 169, 123),
    Ipv4Addr::new(100, 100, 100, 200),
    Ipv4Addr::new(168, 63, 129, 16),
];
/// AWS IMDS over IPv6 (inside `fc00::/7`, so checked first).
const METADATA_V6: [Ipv6Addr; 1] = [Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254)];
/// Host names of metadata services that resolve to them inside a cloud.
const METADATA_NAMES: [&str; 4] = [
    "metadata.google.internal",
    "metadata.goog",
    "instance-data",
    "instance-data.ec2.internal",
];

pub fn classify(ip: IpAddr) -> AddressClass {
    match ip {
        IpAddr::V4(a) => classify_v4(a),
        IpAddr::V6(a) => classify_v6(a),
    }
}

fn classify_v4(a: Ipv4Addr) -> AddressClass {
    if METADATA_V4.contains(&a) {
        return AddressClass::Metadata;
    }
    match a.octets() {
        [0, ..] => AddressClass::Unspecified,
        [127, ..] => AddressClass::Loopback,
        [10, ..] => AddressClass::Private,
        [172, b, ..] if (16..=31).contains(&b) => AddressClass::Private,
        [192, 168, ..] => AddressClass::Private,
        [100, b, ..] if (64..=127).contains(&b) => AddressClass::Shared,
        [169, 254, ..] => AddressClass::LinkLocal,
        [192, 0, 0, _] | [192, 88, 99, _] => AddressClass::Reserved,
        [192, 0, 2, _] | [198, 51, 100, _] | [203, 0, 113, _] => AddressClass::Documentation,
        [198, 18 | 19, ..] => AddressClass::Benchmarking,
        [255, 255, 255, 255] => AddressClass::Broadcast,
        [224..=239, ..] => AddressClass::Multicast,
        [240..=255, ..] => AddressClass::Reserved,
        _ => AddressClass::Global,
    }
}

fn embedded(hi: u16, lo: u16) -> Ipv4Addr {
    Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8)
}

fn classify_v6(a: Ipv6Addr) -> AddressClass {
    if METADATA_V6.contains(&a) {
        return AddressClass::Metadata;
    }
    let s = a.segments();
    if a.is_unspecified() {
        return AddressClass::Unspecified;
    }
    if a.is_loopback() {
        return AddressClass::Loopback;
    }
    // IPv4-mapped `::ffff:a.b.c.d`.
    if let Some(v4) = a.to_ipv4_mapped() {
        return classify_v4(v4);
    }
    // Deprecated IPv4-compatible `::a.b.c.d` (`::` and `::1` are handled above).
    if s[..6] == [0; 6] {
        return classify_v4(embedded(s[6], s[7]));
    }
    // NAT64 well-known prefix `64:ff9b::/96` reaches the embedded IPv4 address.
    if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0; 4] {
        return classify_v4(embedded(s[6], s[7]));
    }
    // Local-use NAT64 `64:ff9b:1::/48`.
    if s[0] == 0x64 && s[1] == 0xff9b && s[2] == 1 {
        return AddressClass::Reserved;
    }
    // 6to4 `2002::/16` tunnels to the IPv4 address in its next 32 bits.
    if s[0] == 0x2002 {
        return classify_v4(embedded(s[1], s[2]));
    }
    // Teredo `2001::/32` carries an obfuscated client address: never reachable.
    if s[0] == 0x2001 && s[1] == 0 {
        return AddressClass::Reserved;
    }
    if s[0] == 0x2001 && s[1] == 0x0db8 {
        return AddressClass::Documentation;
    }
    // ORCHID (`2001:10::/28`, `2001:20::/28`) and benchmarking (`2001:2::/48`).
    if s[0] == 0x2001 && matches!(s[1] & 0xfff0, 0x0010 | 0x0020) {
        return AddressClass::Reserved;
    }
    if s[0] == 0x2001 && s[1] == 0x0002 && s[2] == 0 {
        return AddressClass::Benchmarking;
    }
    // Discard-only `100::/64`.
    if s[0] == 0x0100 && s[1..4] == [0; 3] {
        return AddressClass::Reserved;
    }
    match s[0] {
        x if x & 0xfe00 == 0xfc00 => AddressClass::UniqueLocal,
        x if x & 0xffc0 == 0xfe80 => AddressClass::LinkLocal,
        x if x & 0xffc0 == 0xfec0 => AddressClass::Private,
        x if x & 0xff00 == 0xff00 => AddressClass::Multicast,
        // Global unicast is `2000::/3`; everything else is unassigned.
        x if x & 0xe000 == 0x2000 => AddressClass::Global,
        _ => AddressClass::Reserved,
    }
}

/// The class a host name has whatever its DNS answer says: `localhost` names are loopback
/// (RFC 6761) and are never resolved for a policy that refuses loopback; metadata service
/// names are metadata.
pub fn name_class(domain: &str) -> Option<AddressClass> {
    let d = domain.trim_end_matches('.').to_ascii_lowercase();
    if d == "localhost" || d.ends_with(".localhost") {
        return Some(AddressClass::Loopback);
    }
    METADATA_NAMES
        .contains(&d.as_str())
        .then_some(AddressClass::Metadata)
}

// ---------------------------------------------------------------------------------------------
// Records

/// Query parameter names whose values are credentials in common APIs and signed URLs, besides
/// the general secret-name rule.
const SECRET_PARAMS: [&str; 18] = [
    "sig",
    "signature",
    "x-amz-signature",
    "x-amz-credential",
    "x-amz-security-token",
    "x-goog-signature",
    "x-goog-credential",
    "code",
    "key",
    "auth",
    "jwt",
    "session",
    "sessionid",
    "sid",
    "pass",
    "pwd",
    "client_secret",
    "access_token",
];

fn secret_param(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    sensitive_name(name) || SECRET_PARAMS.contains(&lower.as_str())
}

/// The URL as it may appear in records, logs and results: no user info or fragment, secret
/// query values and credential-shaped path segments replaced, known secret shapes redacted,
/// whitespace-free and at most [`URL_LIMIT`] bytes.
pub fn redacted(url: &Url, redactor: &Redactor) -> String {
    let mut u = url.clone();
    let _ = u.set_username("");
    let _ = u.set_password(None);
    u.set_fragment(None);
    let segments: Option<Vec<String>> = u.path_segments().map(|s| {
        s.map(|seg| {
            if looks_like_credential(seg) {
                REDACTED.to_owned()
            } else {
                seg.to_owned()
            }
        })
        .collect()
    });
    if let Some(segments) = segments
        && segments.iter().any(|s| s == REDACTED)
    {
        u.set_path(&format!("/{}", segments.join("/")));
    }
    if let Some(query) = u.query().map(str::to_owned) {
        let kept: Vec<String> = query
            .split('&')
            .map(|pair| {
                let (raw_key, value) = pair.split_once('=').unwrap_or((pair, ""));
                let key = url::form_urlencoded::parse(raw_key.as_bytes())
                    .next()
                    .map(|(k, _)| k.into_owned())
                    .unwrap_or_default();
                if secret_param(&key) && !value.is_empty() {
                    format!("{raw_key}={REDACTED}")
                } else {
                    pair.to_owned()
                }
            })
            .collect();
        u.set_query(Some(&kept.join("&")));
    }
    let mut out: String = redactor
        .redact_text(u.as_str())
        .chars()
        .map(|c| if c.is_whitespace() { '+' } else { c })
        .collect();
    if out.len() > URL_LIMIT {
        let mut end = URL_LIMIT;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class_of(raw: &str) -> AddressClass {
        match destination(&parse(raw).expect("parses")) {
            Destination::Literal(ip) => classify(ip),
            Destination::Domain(d) => panic!("{raw} parsed as domain {d}"),
        }
    }

    #[test]
    fn numeric_spellings_of_loopback_are_canonical_addresses() {
        for raw in [
            "http://127.0.0.1/",
            "http://2130706433/",
            "http://0177.0.0.1/",
            "http://0x7f.0.0.1/",
            "http://0x7f000001/",
            "http://127.1/",
            "http://%31%32%37.0.0.1/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[::ffff:7f00:1]/",
            "http://[0:0:0:0:0:ffff:127.0.0.1]/",
            "http://[64:ff9b::7f00:1]/",
            "http://[2002:7f00:1::]/",
        ] {
            assert_eq!(class_of(raw), AddressClass::Loopback, "{raw}");
        }
    }

    #[test]
    fn non_global_ranges_are_classified() {
        let cases = [
            ("http://0.0.0.0/", AddressClass::Unspecified),
            ("http://0/", AddressClass::Unspecified),
            ("http://[::]/", AddressClass::Unspecified),
            ("http://10.1.2.3/", AddressClass::Private),
            ("http://172.16.0.1/", AddressClass::Private),
            ("http://172.31.255.255/", AddressClass::Private),
            ("http://192.168.1.1/", AddressClass::Private),
            ("http://100.64.0.1/", AddressClass::Shared),
            ("http://169.254.169.254/", AddressClass::Metadata),
            ("http://[::ffff:169.254.169.254]/", AddressClass::Metadata),
            ("http://[::ffff:a9fe:a9fe]/", AddressClass::Metadata),
            ("http://2852039166/", AddressClass::Metadata),
            ("http://0251.0376.0251.0376/", AddressClass::Metadata),
            ("http://100.100.100.200/", AddressClass::Metadata),
            ("http://[fd00:ec2::254]/", AddressClass::Metadata),
            ("http://169.254.1.1/", AddressClass::LinkLocal),
            ("http://[fe80::1]/", AddressClass::LinkLocal),
            ("http://[fc00::1]/", AddressClass::UniqueLocal),
            ("http://[fd12:3456::1]/", AddressClass::UniqueLocal),
            ("http://224.0.0.1/", AddressClass::Multicast),
            ("http://[ff02::1]/", AddressClass::Multicast),
            ("http://255.255.255.255/", AddressClass::Broadcast),
            ("http://240.0.0.1/", AddressClass::Reserved),
            ("http://192.0.2.1/", AddressClass::Documentation),
            ("http://[2001:db8::1]/", AddressClass::Documentation),
            (
                "http://[2001:0:4136:e378:8000:63bf:3fff:fdd2]/",
                AddressClass::Reserved,
            ),
            ("http://198.18.0.1/", AddressClass::Benchmarking),
            ("http://[::10.0.0.1]/", AddressClass::Private),
            ("http://[64:ff9b::a00:1]/", AddressClass::Private),
            ("http://[2002:c0a8:101::]/", AddressClass::Private),
            ("http://8.8.8.8/", AddressClass::Global),
            ("http://[2606:4700:4700::1111]/", AddressClass::Global),
            ("http://172.32.0.1/", AddressClass::Global),
            ("http://100.128.0.1/", AddressClass::Global),
        ];
        for (raw, class) in cases {
            assert_eq!(class_of(raw), class, "{raw}");
        }
    }

    #[test]
    fn only_global_is_allowed_by_default_and_metadata_never() {
        assert!(AddressClass::Global.allowed(false));
        for c in [
            AddressClass::Loopback,
            AddressClass::Private,
            AddressClass::Shared,
            AddressClass::UniqueLocal,
        ] {
            assert!(!c.allowed(false), "{c:?}");
            assert!(c.allowed(true), "{c:?}");
        }
        for c in [
            AddressClass::LinkLocal,
            AddressClass::Metadata,
            AddressClass::Unspecified,
            AddressClass::Multicast,
            AddressClass::Broadcast,
            AddressClass::Documentation,
            AddressClass::Benchmarking,
            AddressClass::Reserved,
        ] {
            assert!(!c.allowed(true), "{c:?}");
        }
    }

    #[test]
    fn the_browser_always_allows_loopback_and_never_link_local_or_metadata() {
        for allow_private in [false, true] {
            assert!(AddressClass::Loopback.browser_allowed(allow_private));
            assert!(AddressClass::Global.browser_allowed(allow_private));
            for c in [
                AddressClass::LinkLocal,
                AddressClass::Metadata,
                AddressClass::Unspecified,
                AddressClass::Multicast,
                AddressClass::Broadcast,
                AddressClass::Documentation,
                AddressClass::Benchmarking,
                AddressClass::Reserved,
            ] {
                assert!(!c.browser_allowed(allow_private), "{c:?}");
            }
            for c in [
                AddressClass::Private,
                AddressClass::Shared,
                AddressClass::UniqueLocal,
            ] {
                assert_eq!(c.browser_allowed(allow_private), allow_private, "{c:?}");
            }
        }
    }

    #[test]
    fn urls_with_credentials_or_other_schemes_are_refused() {
        assert_eq!(
            parse("https://user:pw@example.com/"),
            Err(UrlRefusal::Credentials)
        );
        assert_eq!(
            parse("https://user@example.com/"),
            Err(UrlRefusal::Credentials)
        );
        assert_eq!(parse("ftp://example.com/"), Err(UrlRefusal::Scheme));
        assert_eq!(parse("file:///etc/passwd"), Err(UrlRefusal::Scheme));
        assert_eq!(parse("http://example.com:0/"), Err(UrlRefusal::Host));
        assert_eq!(parse("not a url"), Err(UrlRefusal::Invalid));
        let base = parse("https://example.com/a/b").unwrap();
        assert_eq!(
            join(&base, "//evil:pw@example.org/"),
            Err(UrlRefusal::Credentials)
        );
        assert_eq!(
            join(&base, "gopher://example.org/"),
            Err(UrlRefusal::Scheme)
        );
        assert_eq!(
            join(&base, "../c?x=1#frag").unwrap().as_str(),
            "https://example.com/c?x=1"
        );
        assert_eq!(
            parse(&format!("https://example.com/{}", "a".repeat(URL_LIMIT))),
            Err(UrlRefusal::TooLong)
        );
    }

    #[test]
    fn request_parts_keep_the_canonical_host() {
        let u = parse("https://Example.COM.:8443/p%20q?a=1#f").unwrap();
        assert_eq!(destination(&u), Destination::Domain("example.com".into()));
        assert_eq!(host_header(&u), "example.com:8443");
        assert_eq!(request_target(&u), "/p%20q?a=1");
        assert_eq!(port(&u), 8443);
        let v6 = parse("http://[2001:db8::1]/").unwrap();
        assert_eq!(host_header(&v6), "[2001:db8::1]");
        assert_eq!(
            server_name(&v6),
            ServerName::Ip("2001:db8::1".parse().unwrap())
        );
        assert_eq!(port(&parse("https://example.com").unwrap()), 443);
    }

    #[test]
    fn localhost_and_metadata_names_are_classified_without_dns() {
        assert_eq!(name_class("localhost"), Some(AddressClass::Loopback));
        assert_eq!(name_class("LOCALHOST."), Some(AddressClass::Loopback));
        assert_eq!(name_class("api.localhost"), Some(AddressClass::Loopback));
        assert_eq!(
            name_class("metadata.google.internal"),
            Some(AddressClass::Metadata)
        );
        assert_eq!(name_class("example.com"), None);
    }

    #[test]
    fn records_never_hold_secrets() {
        let r = Redactor::new();
        let u = parse(
            "https://example.com/a?api_key=k1&page=2&X-Amz-Signature=s1&token=t1&q=rust#frag",
        )
        .unwrap();
        let out = redacted(&u, &r);
        assert_eq!(
            out,
            "https://example.com/a?api_key=[REDACTED]&page=2&X-Amz-Signature=[REDACTED]&token=[REDACTED]&q=rust"
        );
        let token = format!("ghp_{}", "A1b2C3d4E5f6G7h8I9j0K1l2");
        let u = parse(&format!("https://example.com/hooks/{token}/x")).unwrap();
        assert_eq!(redacted(&u, &r), "https://example.com/hooks/[REDACTED]/x");
        let mut known = Redactor::new();
        known.register("supersecretvalue");
        let u = parse("https://example.com/p/supersecretvalue").unwrap();
        assert!(!redacted(&u, &known).contains("supersecretvalue"));
    }
}
