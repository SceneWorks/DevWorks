//! Numeric family identities for the eight stable latest selectors. Provider inventory order,
//! lexical ordering and snapshot dates never determine a family version.

pub const FAMILIES: [&str; 8] = [
    "astra", "sol", "terra", "luna", "fable", "opus", "sonnet", "haiku",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedVersion {
    pub family: &'static str,
    pub version: Vec<u32>,
    /// Provider namespace, retained to avoid equating e.g. two OpenRouter publishers.
    pub namespace: String,
    /// A dated snapshot identifies a concrete upstream, but is not a family version.
    pub snapshot: Option<String>,
}

pub fn alias_id(family: &str) -> String {
    format!("{family}-latest")
}

pub fn alias_family(id: &str) -> Option<&'static str> {
    let family = id.strip_suffix("-latest")?;
    FAMILIES.iter().copied().find(|f| *f == family)
}

/// Accept only the model family's full upstream grammar. In particular `latest`, preview
/// suffixes, extra family names and date-only versions cannot masquerade as numeric releases.
pub fn parse(upstream: &str) -> Option<ParsedVersion> {
    let lower = upstream.to_ascii_lowercase();
    let (namespace, model) = lower.rsplit_once('/').unwrap_or(("", lower.as_str()));
    if model.is_empty() || namespace.split('/').any(|p| p.is_empty()) && !namespace.is_empty() {
        return None;
    }
    let (model, snapshot) = model
        .rsplit_once('-')
        .map_or((model, None), |(head, tail)| {
            if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) {
                (head, Some(tail.to_owned()))
            } else {
                (model, None)
            }
        });
    let (family, digits, separators): (&str, &str, &[char]) =
        if let Some(rest) = model.strip_prefix("gpt-") {
            let (digits, family) = rest.rsplit_once('-')?;
            if !FAMILIES[..4].contains(&family) {
                return None;
            }
            (family, digits, &['.'])
        } else if let Some(rest) = model.strip_prefix("claude-") {
            let (family, digits) = rest.split_once('-')?;
            if !FAMILIES[4..].contains(&family) {
                return None;
            }
            (family, digits, &['.', '-'])
        } else {
            return None;
        };
    let mut version = Vec::new();
    for part in digits.split(separators) {
        // Eight digits are a snapshot date, never a version component; leading zeroes and
        // zero versions are rejected rather than normalizing malformed provider identities.
        if part.is_empty()
            || part.len() >= 8
            || !part.bytes().all(|b| b.is_ascii_digit())
            || part.len() > 1 && part.starts_with('0')
        {
            return None;
        }
        version.push(part.parse::<u32>().ok()?);
    }
    if version.first().copied()? == 0 {
        return None;
    }
    while version.len() > 1 && version.last() == Some(&0) {
        version.pop();
    }
    Some(ParsedVersion {
        family: FAMILIES.iter().copied().find(|f| *f == family)?,
        version,
        namespace: namespace.to_owned(),
        snapshot,
    })
}

/// Current targets of a family, independently in each configured connection/provider scope.
/// Inventory and policy establish identity first; credentials, cooldowns and request capability
/// constraints are assessed afterwards and cannot relabel an older release as latest.
pub fn targets(
    family: &str,
    models: &std::collections::BTreeMap<String, crate::ports::storage::StoredRouteModel>,
    connections: &[crate::ports::router::ConnectionStatus],
) -> Vec<crate::protocol::Id> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut best: BTreeMap<
        (String, String, String),
        (ParsedVersion, &crate::ports::storage::StoredRouteModel),
    > = BTreeMap::new();
    for model in models.values().filter(|m| m.enabled) {
        let Some(parsed) = parse(&model.upstream_model_id).filter(|p| p.family == family) else {
            continue;
        };
        for connection in connections
            .iter()
            .filter(|c| c.profile == model.endpoint_profile)
            .filter(|c| model.connection_id.as_ref().is_none_or(|id| id == &c.id))
            .filter(|c| c.permits(&model.upstream_model_id))
            .filter(|c| {
                c.discovered.as_ref().is_some_and(|d| {
                    super::select::lists(&model.endpoint_profile, d, &model.upstream_model_id)
                })
            })
        {
            let key = (
                model.endpoint_profile.clone(),
                connection.id.as_str().to_owned(),
                parsed.namespace.clone(),
            );
            let replace = best.get(&key).is_none_or(|(old, previous)| {
                parsed.version > old.version
                    || parsed.version == old.version
                        && (
                            // Equal numeric versions do not rank by snapshot date. Prefer a declared
                            // undated identity, then a connection-bound row, then a stable catalog ID.
                            (parsed.snapshot.is_none(), model.connection_id.is_some())
                                > (old.snapshot.is_none(), previous.connection_id.is_some())
                                || (parsed.snapshot.is_none(), model.connection_id.is_some())
                                    == (old.snapshot.is_none(), previous.connection_id.is_some())
                                    && model.id.as_str() < previous.id.as_str()
                        )
            });
            if replace {
                best.insert(key, (parsed.clone(), model));
            }
        }
    }
    let mut seen = BTreeSet::new();
    best.into_values()
        .filter_map(|(_, m)| seen.insert(m.id.as_str().to_owned()).then(|| m.id.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_versions_do_not_compare_lexically_or_by_snapshot() {
        assert!(parse("gpt-6.10-sol").unwrap().version > parse("gpt-6.9-sol").unwrap().version);
        assert!(
            parse("claude-opus-6-10").unwrap().version > parse("claude-opus-6-9").unwrap().version
        );
        let dated = parse("anthropic/claude-sonnet-5-5-20261105").unwrap();
        assert_eq!(dated.version, vec![5, 5]);
        assert_eq!(dated.snapshot.as_deref(), Some("20261105"));
        assert_eq!(dated.namespace, "anthropic");
        assert_eq!(parse("claude-sonnet-5.5").unwrap().version, dated.version);
        assert_eq!(parse("gpt-6.0-sol").unwrap().version, vec![6]);
    }

    #[test]
    fn families_have_exact_boundaries_and_numeric_versions() {
        for family in FAMILIES {
            assert_eq!(alias_family(&alias_id(family)), Some(family));
            let upstream = if FAMILIES[..4].contains(&family) {
                format!("gpt-7.2-{family}")
            } else {
                format!("claude-{family}-7-2")
            };
            assert_eq!(parse(&upstream).unwrap().family, family);
        }
        for invalid in [
            "gpt-6-sol-preview",
            "gpt-latest-sol",
            "gpt-6-solar",
            "gpt-6..1-sol",
            "claude-sonnet-latest",
            "claude-sonnet-20261105",
            "claude-sonnet-5-20261105-extra",
            "claude-sonnet-5-01",
            "gpt-0-sol",
            "claude-sonnet-5-",
            "gpt-6-sol/",
            "gpt-6-solitude",
        ] {
            assert!(parse(invalid).is_none(), "{invalid}");
        }
    }
}
