//! Exact and redacted repository security evidence.
pub(super) mod scanner;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Category {
    Secret,
    AttackSurface,
    VulnerablePackage,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    fn weight(self) -> f64 {
        match self {
            Self::Low => 0.3,
            Self::Medium => 0.7,
            Self::High => 1.5,
            Self::Critical => 2.5,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Finding {
    pub category: Category,
    pub severity: Severity,
    pub title: String,
    pub path: Option<String>,
    pub line: Option<usize>,
    pub package: Option<String>,
    pub version: Option<String>,
    pub advisory: Option<String>,
}

impl Finding {
    fn source(
        category: Category,
        severity: Severity,
        title: &str,
        path: &str,
        line: usize,
    ) -> Self {
        Self {
            category,
            severity,
            title: title.into(),
            path: Some(path.into()),
            line: Some(line),
            package: None,
            version: None,
            advisory: None,
        }
    }
}

pub(super) fn score(findings: &[Finding]) -> f64 {
    let mut scopes = BTreeMap::<(Category, String), Vec<Severity>>::new();
    for finding in findings {
        let key = match finding.category {
            Category::Secret | Category::AttackSurface => {
                finding.path.clone().unwrap_or_else(|| {
                    format!("{}:{}", finding.title, finding.line.unwrap_or_default())
                })
            }
            Category::VulnerablePackage => format!(
                "{}@{}",
                finding.package.as_deref().unwrap_or(&finding.title),
                finding.version.as_deref().unwrap_or("")
            ),
        };
        scopes
            .entry((finding.category, key))
            .or_default()
            .push(finding.severity);
    }
    let mut totals = BTreeMap::<Category, f64>::new();
    for ((category, _), mut severities) in scopes {
        severities.sort_unstable_by(|a, b| b.cmp(a));
        let weight = severities[0].weight()
            + severities[1..]
                .iter()
                .map(|severity| severity.weight() * 0.6)
                .sum::<f64>();
        *totals.entry(category).or_default() += weight;
    }
    let grade = 10.0
        - totals
            .get(&Category::Secret)
            .copied()
            .unwrap_or_default()
            .min(6.5)
        - totals
            .get(&Category::AttackSurface)
            .copied()
            .unwrap_or_default()
            .min(4.0)
        - totals
            .get(&Category::VulnerablePackage)
            .copied()
            .unwrap_or_default()
            .min(4.5);
    (grade.clamp(1.0, 10.0) * 10.0).round_ties_even() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dampens_repeated_findings_in_one_file_and_caps_categories_independently() {
        let a = Finding::source(
            Category::Secret,
            Severity::Critical,
            "Embedded credential",
            "a",
            1,
        );
        let b = Finding::source(
            Category::Secret,
            Severity::High,
            "Embedded credential",
            "a",
            2,
        );
        let c = Finding::source(
            Category::AttackSurface,
            Severity::Medium,
            "Source finding",
            "b",
            4,
        );
        assert_eq!(score(&[a.clone(), b.clone(), c.clone()]), 5.9);
        assert_eq!(score(&[a.clone(), a.clone(), a.clone(), a, b, c]), 2.8);
        assert_eq!(score(&[]), 10.0);
    }
}

mod advisories;
pub(super) use advisories::{Feed, RequestBudget};

mod report;
pub(super) use report::*;
