//! Match static HTTP call evidence to routes in the selected repository views.
use super::super::budget::Budget;
use crate::ports::storage::GraphNodeIdentity;
use serde_json::Value;
use std::collections::BTreeMap;
use url::Url;

pub(super) const METHODS: &[&str] = &[
    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "TRACE", "CONNECT",
];

pub(super) struct Route {
    pub node: GraphNodeIdentity,
    pub qualified_name: String,
    method: String,
    template: Vec<String>,
    origin: Option<String>,
}

impl Route {
    pub(super) fn from_node(
        node: GraphNodeIdentity,
        qualified_name: &str,
        properties: &Value,
    ) -> Option<Self> {
        let kind = string(properties, "route_kind")
            .unwrap_or("")
            .to_ascii_lowercase();
        let kind = kind.as_str();
        if qualified_name.to_ascii_lowercase().starts_with("ui-route:")
            || matches!(kind, "ui" | "frontend" | "page" | "screen")
            || boolean(properties, "registration_ambiguous")
        {
            return None;
        }
        let backend = matches!(kind, "api" | "http" | "backend" | "endpoint");
        let framework = string(properties, "framework")
            .unwrap_or("")
            .to_ascii_lowercase();
        if !backend
            && (nonempty(properties, "component").is_some()
                || nonempty(properties, "component_qn").is_some()
                || matches!(
                    framework.as_str(),
                    "angular" | "react" | "react-router" | "next" | "nextjs" | "next.js"
                ))
        {
            return None;
        }
        let method = string(properties, "http_method")?.to_ascii_uppercase();
        if !METHODS.contains(&method.as_str()) {
            return None;
        }
        let template = template(string(properties, "route_template")?);
        Some(Self {
            node,
            qualified_name: qualified_name.to_owned(),
            method,
            template,
            origin: origin(properties, ""),
        })
    }
}

/// Repository IDs stay authoritative even when display names collide by case.
pub(super) struct Names(BTreeMap<String, Option<crate::protocol::Id>>);
impl Names {
    pub(super) fn new(
        repositories: impl IntoIterator<Item = (String, crate::protocol::Id)>,
    ) -> Self {
        let mut names = BTreeMap::new();
        for (name, id) in repositories {
            names
                .entry(name.to_lowercase())
                .and_modify(|found: &mut Option<crate::protocol::Id>| *found = None)
                .or_insert(Some(id));
        }
        Self(names)
    }

    fn lookup(&self, name: &str) -> Option<&crate::protocol::Id> {
        self.0.get(&name.to_lowercase()).and_then(Option::as_ref)
    }

    pub(super) fn service(&self, name: &str) -> Option<&crate::protocol::Id> {
        let name = name.to_lowercase();
        match self.0.get(&name) {
            Some(id) => id.as_ref(),
            None => self.lookup(&format!("tc.{name}")),
        }
    }

    pub(super) fn nuget(&self, package: &str) -> Option<&crate::protocol::Id> {
        let package = package.to_lowercase();
        let name = [".models", ".contracts", ".client", ".shared"]
            .iter()
            .find_map(|suffix| package.strip_suffix(suffix))
            .unwrap_or(&package);
        self.lookup(name)
    }

    pub(super) fn dto(&self, dto: &str) -> Option<&crate::protocol::Id> {
        let mut namespace = dto;
        while let Some((prefix, _)) = namespace.rsplit_once('.') {
            if !prefix.contains('.') {
                break;
            }
            if let Some(id) = self.lookup(prefix) {
                return Some(id);
            }
            if let Some((name, suffix)) = prefix.rsplit_once('.')
                && ["Models", "Contracts", "Client", "Shared"]
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(suffix))
                && let Some(id) = self.lookup(name)
            {
                return Some(id);
            }
            namespace = prefix;
        }
        None
    }
}

pub(super) enum Match<'a> {
    Unique(&'a Route),
    Unresolved,
    Ambiguous,
    Incomplete,
}

/// Evaluate explicit eligibility before specificity. As in the source, exact
/// templates win over numeric/UUID normalization, then wildcard matching.
pub(super) fn resolve<'a>(
    routes: &'a [Route],
    names: &Names,
    properties: &Value,
    target_qualified_name: Option<&str>,
    budget: &mut Budget,
) -> Match<'a> {
    if boolean(properties, "binding_ambiguous") {
        return Match::Ambiguous;
    }
    let Some(method) = string(properties, "http_method").map(str::to_ascii_uppercase) else {
        return Match::Unresolved;
    };
    let Some(url) = nonempty(properties, "url_pattern") else {
        return Match::Unresolved;
    };
    let raw_segments = path(url).trim_matches('/');
    if !METHODS.contains(&method.as_str())
        || has_non_http_scheme(url)
        || (!raw_segments.is_empty()
            && !raw_segments.split('/').any(|part| {
                !matches!(part, "" | "*" | "unknown" | "undefined") && !part.starts_with('{')
            }))
    {
        return Match::Unresolved;
    }
    let hint = if let Some(service) = nonempty(properties, "service_name") {
        let Some(repo) = names.service(service) else {
            return Match::Unresolved;
        };
        Some(repo)
    } else {
        target_qualified_name
            .and_then(route_repository)
            .and_then(|repo| names.lookup(repo))
            .or_else(|| nonempty(properties, "request_dto").and_then(|dto| names.dto(dto)))
    };
    let call_origin = origin(properties, url);
    let origin_repo = call_origin
        .as_deref()
        .and_then(|value| Url::parse(value).ok())
        .and_then(|parsed| parsed.host_str().and_then(|host| names.service(host)));
    if matches!((hint, origin_repo), (Some(hint), Some(origin_repo)) if hint != origin_repo) {
        return Match::Unresolved;
    }

    let literal = template(url);
    let normalized = path(url).to_lowercase();
    let normalized: Vec<_> = normalized
        .trim_start_matches('/')
        .split('/')
        .map(|part| {
            if likely_id(part) {
                "{*}".to_owned()
            } else {
                part.to_owned()
            }
        })
        .collect();
    let mut best = None::<(&Route, u8)>;
    let mut ambiguous = false;
    for route in routes {
        if !budget.visit() {
            return Match::Incomplete;
        }
        if route.method != method || hint.is_some_and(|repo| route.node.repository_id != *repo) {
            continue;
        }
        if let Some(origin) = &call_origin {
            match route.origin.as_ref() {
                Some(route_origin) if !route_origin.eq_ignore_ascii_case(origin) => continue,
                None if hint.is_none()
                    && origin_repo.is_none_or(|repo| route.node.repository_id != *repo) =>
                {
                    continue;
                }
                _ => {}
            }
        }
        let rank = if route.template == literal {
            3
        } else if route.template == normalized {
            2
        } else if wildcard(&route.template, &literal) {
            1
        } else {
            continue;
        };
        match best {
            Some((current, old)) if rank == old => {
                ambiguous |= route.node.repository_id != current.node.repository_id
                    || route.qualified_name != current.qualified_name;
            }
            Some((_, old)) if rank < old => {}
            _ => {
                best = Some((route, rank));
                ambiguous = false;
            }
        }
    }
    match (best, ambiguous) {
        (Some((route, _)), false) => Match::Unique(route),
        (_, true) => Match::Ambiguous,
        _ => Match::Unresolved,
    }
}

fn wildcard(template: &[String], actual: &[String]) -> bool {
    for (index, part) in template.iter().enumerate() {
        if part == "{**}" {
            return true;
        }
        if !actual
            .get(index)
            .is_some_and(|value| part == "{*}" || part == value)
        {
            return false;
        }
    }
    template.len() == actual.len()
}

fn template(value: &str) -> Vec<String> {
    let normalized = path(value).to_lowercase();
    let mut remainder = normalized.trim_start_matches('/');
    let mut result = String::with_capacity(remainder.len());
    while let Some((before, open)) = remainder.split_once('{') {
        result.push_str(before);
        let Some((parameter, after)) = open.split_once('}') else {
            result.push('{');
            remainder = open;
            break;
        };
        if parameter.is_empty() {
            result.push_str("{}");
        } else if parameter.starts_with('*') && parameter.len() > 1 {
            result.push_str("{**}");
        } else {
            result.push_str("{*}");
        }
        remainder = after;
    }
    result.push_str(remainder);
    result
        .split('/')
        .map(|part| {
            let normalized = match part.as_bytes().first() {
                Some(b':')
                    if parameter_name(part[1..].split('(').next().unwrap_or(""))
                        && (!part.contains('(') || part.ends_with(')')) =>
                {
                    "{*}"
                }
                Some(b'*') if parameter_name(&part[1..]) => "{**}",
                _ => part,
            };
            normalized.to_owned()
        })
        .collect()
}

fn parameter_name(part: &str) -> bool {
    let mut bytes = part.bytes();
    bytes
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == b'_')
        && bytes.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}

fn likely_id(value: &str) -> bool {
    value.parse::<i64>().is_ok()
        || (value.starts_with('{') && value.ends_with('}'))
        || valid_guid(value)
}

fn valid_guid(value: &str) -> bool {
    let value = value
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .or_else(|| value.strip_prefix('(').and_then(|s| s.strip_suffix(')')))
        .unwrap_or(value);
    if value.len() == 32 {
        return value.bytes().all(|c| c.is_ascii_hexdigit());
    }
    value.len() == 36
        && value.bytes().enumerate().all(|(index, c)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

fn origin(properties: &Value, url: &str) -> Option<String> {
    let candidate = ["origin", "http_origin", "url_origin"]
        .iter()
        .find_map(|key| string(properties, key))
        .filter(|value| !value.trim().is_empty())
        .or_else(|| absolute_origin(url))?;
    if let Ok(url) = Url::parse(candidate)
        && matches!(url.scheme(), "http" | "https")
    {
        return Some(url.origin().ascii_serialization());
    }
    Some(candidate.trim_end_matches('/').to_owned())
}

fn absolute_origin(url: &str) -> Option<&str> {
    let (scheme, rest) = url.trim().split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let host_len = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    (host_len > 0).then(|| &url.trim()[..scheme.len() + 3 + host_len])
}

fn path(url: &str) -> &str {
    let url = url.trim();
    let path = absolute_origin(url).map_or(url, |origin| &url[origin.len()..]);
    let mut braces: i32 = 0;
    for (index, c) in path.char_indices() {
        match c {
            '{' => braces += 1,
            '}' => braces -= 1,
            '?' | '#' if braces == 0 => return &path[..index],
            _ => {}
        }
    }
    path
}

fn has_non_http_scheme(url: &str) -> bool {
    let Some((scheme, _)) = url.split_once(':') else {
        return false;
    };
    let mut bytes = scheme.bytes();
    bytes.next().is_some_and(|c| c.is_ascii_alphabetic())
        && bytes.all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'.' | b'-'))
        && absolute_origin(url).is_none()
}

fn route_repository(qn: &str) -> Option<&str> {
    let mut parts = qn.splitn(4, ':');
    let (kind, repo, _, _) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    (kind.eq_ignore_ascii_case("route") && !matches!(repo, "" | "*")).then_some(repo)
}

pub(super) fn boolean(value: &Value, property: &str) -> bool {
    value[property].as_bool().unwrap_or_else(|| {
        string(value, property).is_some_and(|value| value.eq_ignore_ascii_case("true"))
    })
}

pub(super) fn string<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str)
}

pub(super) fn nonempty<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    string(value, field).filter(|text| !text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Id, data::QueryLimits};
    use serde_json::json;

    fn route(repository: &str, qn: &str, pattern: &str, origin: Option<&str>) -> Route {
        Route::from_node(
            GraphNodeIdentity {
                repository_id: Id::new(repository.replace('.', "_")).unwrap(),
                unit_key: "api.rs".into(),
                node_id: format!("node_{repository}"),
            },
            qn,
            &json!({"http_method": "GET", "route_kind": "endpoint", "route_template": pattern,
                    "origin": origin}),
        )
        .unwrap()
    }

    fn budget(limit: u32) -> Budget {
        Budget::new(&QueryLimits {
            max_results: 10,
            max_visited: limit,
            deadline_ms: 1000,
        })
    }

    fn names() -> Names {
        Names::new(
            ["Orders", "TC.Inventory"]
                .into_iter()
                .map(|name| (name.into(), Id::new(name.replace('.', "_")).unwrap())),
        )
    }

    #[test]
    fn exact_static_route_wins_and_numeric_call_matches_a_parameter() {
        let routes = [
            route("Orders", "list", "/api/orders/123", None),
            route("Orders", "by_id", "/api/orders/{id:int}", None),
        ];
        for (path, expected) in [
            ("/api/orders/123?view=full", "list"),
            ("/api/orders/456", "by_id"),
        ] {
            let call = json!({"http_method": "get", "url_pattern": path});
            assert!(
                matches!(resolve(&routes, &names(), &call, None, &mut budget(10)),
                Match::Unique(route) if route.qualified_name == expected)
            );
        }
    }

    #[test]
    fn explicit_service_constrains_candidate_routes_and_origin_must_agree() {
        let routes = [
            route("Orders", "exact", "/api/orders/123", Some("https://orders")),
            route("TC.Inventory", "parameter", "/api/orders/:id", None),
        ];
        let call = json!({"http_method": "GET", "url_pattern": "/api/orders/123",
                          "service_name": "Inventory"});
        assert!(
            matches!(resolve(&routes, &names(), &call, None, &mut budget(10)),
            Match::Unique(route) if route.qualified_name == "parameter")
        );
        let call = json!({"http_method": "GET", "url_pattern": "https://orders/api/orders/123",
                          "service_name": "Inventory"});
        assert!(matches!(
            resolve(&routes, &names(), &call, None, &mut budget(10)),
            Match::Unresolved
        ));
    }

    #[test]
    fn equal_identities_are_deduplicated_while_distinct_routes_and_clipped_inputs_are_not_unique() {
        let routes = [
            route("Orders", "one", "/api/orders/:id", None),
            route("Orders", "one", "/api/orders/:id", None),
            route("Orders", "two", "/api/orders/{id}", None),
        ];
        let call = json!({"http_method": "GET", "url_pattern": "/api/orders/20"});
        assert!(matches!(
            resolve(&routes, &names(), &call, None, &mut budget(10)),
            Match::Ambiguous
        ));
        assert!(matches!(
            resolve(&routes, &names(), &call, None, &mut budget(2)),
            Match::Incomplete
        ));
    }

    #[test]
    fn catch_all_respects_prefix_and_one_parameter_does_not_accept_extra_segments() {
        assert!(wildcard(&template("/api/{*rest}"), &template("/api/a/b")));
        assert!(!wildcard(&template("/api/{rest}"), &template("/api/a/b")));
        assert!(!wildcard(
            &template("/api/{*rest}"),
            &template("/other/a/b")
        ));
        assert_eq!(template("https://host/a/{id?}?q=value"), vec!["a", "{*}"]);
    }
}
