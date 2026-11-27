//! Retained VitalsAnalyzer scoring and classification.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Complexity {
    pub nesting: usize,
    pub deep_lines: usize,
    pub functions: usize,
    pub longest_function: usize,
    pub branches: usize,
    pub lines: usize,
}

impl Complexity {
    pub(super) fn score(&self) -> u32 {
        if self.nesting <= 2 {
            return 0;
        }
        let nesting = (self.nesting as f64 / 8.0).min(1.0) * 35.0;
        let deep_ratio = ratio(self.deep_lines, self.lines);
        let deep = (deep_ratio / 0.3).min(1.0) * 25.0;
        let functions = (self.longest_function as f64 / 200.0).min(1.0) * 20.0;
        let length = (self.lines as f64 / 1000.0).min(1.0) * 10.0;
        let branches = (ratio(self.branches, self.functions) / 5.0).min(1.0) * 10.0;
        (nesting + deep + functions + length + branches).round_ties_even() as u32
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct Lint {
    pub errors: usize,
    pub warnings: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Churn {
    pub changes: usize,
    pub lines_added: u64,
    pub lines_removed: u64,
    pub authors: usize,
    pub last_change_ms: Option<i64>,
    pub weighted_30: f64,
    pub weighted_90: f64,
    pub weighted_365: f64,
    pub bug_fixes_90: f64,
    pub bug_fixes_365: f64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Coupling {
    pub strength: f64,
    pub partners: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Knowledge {
    pub truck_factor: usize,
    pub top_authors: Vec<(String, usize)>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::services::graph::repository_analysis) struct Scores {
    pub health: f64,
    pub trust: f64,
    pub risk: f64,
    pub concern: f64,
    pub recurring_churn: f64,
    pub bug_fix_ratio: f64,
}

pub(super) fn is_test(path: &str) -> bool {
    path.split(['/', '\\']).any(|part| {
        [
            "test", "tests", "spec", "specs", "testing", ".tests", ".test",
        ]
        .iter()
        .any(|test| test.eq_ignore_ascii_case(part))
    })
}

pub(super) struct FileClassification {
    pub untrusted: bool,
    pub test: bool,
}

pub(super) fn scores(
    complexity: &Complexity,
    churn: &Churn,
    coupling: &Coupling,
    knowledge: &Knowledge,
    lint: Option<&Lint>,
    classification: FileClassification,
    now_ms: i64,
) -> Scores {
    let FileClassification { untrusted, test } = classification;
    let complexity_score = (10.0 - f64::from(complexity.score()) * 0.09).max(1.0);
    let churn_score = (10.0 - churn.changes as f64 * 0.18).max(1.0);
    let coupling_score =
        (10.0 - coupling.strength * 5.0 - (coupling.partners as f64 / 10.0).min(4.0)).max(1.0);
    let authors = match knowledge.truck_factor {
        0 => 7.0,
        1 => 2.0,
        2 => 5.0,
        3 => 8.0,
        count => (8.0 + (count - 3) as f64 * 0.5).min(10.0),
    };
    let weighted = match lint.filter(|lint| lint.errors > 0 || lint.warnings > 0) {
        Some(lint) => {
            let lint_score = (10.0
                - (lint.errors as f64 * 1.5).min(9.0)
                - (lint.warnings as f64 * 0.4).min(3.0))
            .max(1.0);
            0.25 * complexity_score
                + 0.25 * churn_score
                + 0.20 * coupling_score
                + 0.15 * authors
                + 0.15 * lint_score
        }
        None => {
            0.30 * complexity_score + 0.30 * churn_score + 0.20 * coupling_score + 0.20 * authors
        }
    };
    let health = round(weighted.clamp(1.0, 10.0), 10.0);
    let trust = if untrusted {
        0.1
    } else {
        let age_days = churn
            .last_change_ms
            .map_or(90.0, |at| now_ms.saturating_sub(at) as f64 / 86_400_000.0);
        let stable = if age_days >= 90.0 {
            0.15
        } else if age_days >= 30.0 {
            0.05
        } else {
            0.0
        };
        let recent = if age_days <= 7.0 { 0.05 } else { 0.0 };
        let lint = lint.map_or(0.0, |lint| {
            (lint.errors as f64 * 0.05).min(0.3) + (lint.warnings as f64 * 0.02).min(0.1)
        });
        let trust = 0.7 + stable - (churn.changes as f64 / 50.0).min(0.4) - recent - lint;
        round(trust.clamp(0.0, 1.0), 100.0)
    };
    let risk = round(
        (10.0 - health)
            * churn.changes as f64
            * if test { 0.3 } else { 1.0 }
            * (1.0 + coupling.partners as f64 * 0.2),
        10.0,
    );
    let recurring_churn = round(
        0.2 * (churn.weighted_30 / 10.0).min(1.0)
            + 0.3 * (churn.weighted_90 / 25.0).min(1.0)
            + 0.5 * (churn.weighted_365 / 100.0).min(1.0),
        100.0,
    );
    let bug_fix_ratio = if churn.weighted_365 <= 0.0 {
        0.0
    } else {
        round(churn.bug_fixes_365 / churn.weighted_365, 1_000.0)
    };
    let concern = round(
        (risk.max((10.0 - health) * 2.0)
            + recurring_churn * 8.0
            + (bug_fix_ratio * 12.0).min(8.0)
            + churn.bug_fixes_365.min(4.0))
            * if test { 0.5 } else { 1.0 },
        10.0,
    );
    Scores {
        health,
        trust,
        risk,
        concern,
        recurring_churn,
        bug_fix_ratio,
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::services::graph::repository_analysis) struct Health {
    pub base: f64,
    pub files: usize,
    pub hotspots: usize,
    pub alerts: usize,
}

pub(super) fn aggregate(health: impl IntoIterator<Item = f64>) -> Health {
    let (mut weight, mut weighted) = (0.0, 0.0);
    let (mut files, mut hotspots, mut alerts) = (0, 0, 0);
    for score in health {
        weight += 11.0 - score;
        weighted += score * (11.0 - score);
        files += 1;
        hotspots += usize::from(score < 4.0);
        alerts += usize::from(score < 2.5);
    }
    let base = if weight > 0.0 {
        round(weighted / weight, 10.0)
    } else {
        5.0
    };
    Health {
        base,
        files,
        hotspots,
        alerts,
    }
}

pub(super) fn round(value: f64, power: f64) -> f64 {
    (value * power).round_ties_even() / power
}

pub(super) fn ratio(value: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        value as f64 / total as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_weights_unhealthy_files_and_thresholds_are_strict() {
        let health = aggregate([2.4, 2.5, 4.0, 10.0]);
        assert_eq!((health.alerts, health.hotspots, health.files), (1, 2, 4));
        assert!((health.base - 3.2).abs() < 1e-9);
        assert_eq!(aggregate([]).base, 5.0);
    }

    #[test]
    fn lint_weight_and_test_penalties_preserve_independent_scores() {
        let churn = Churn {
            changes: 10,
            weighted_30: 8.0,
            weighted_90: 10.0,
            weighted_365: 20.0,
            bug_fixes_365: 4.0,
            ..Default::default()
        };
        let lint = Lint {
            errors: 2,
            warnings: 3,
        };
        let complexity = Complexity::default();
        let coupling = Coupling {
            strength: 0.4,
            partners: 3,
        };
        let knowledge = Knowledge {
            truck_factor: 2,
            top_authors: Vec::new(),
        };
        let core = scores(
            &complexity,
            &churn,
            &coupling,
            &knowledge,
            Some(&lint),
            FileClassification {
                untrusted: false,
                test: false,
            },
            0,
        );
        let test = scores(
            &complexity,
            &churn,
            &coupling,
            &knowledge,
            Some(&lint),
            FileClassification {
                untrusted: true,
                test: true,
            },
            0,
        );
        assert_eq!(core.health, 7.7);
        assert_eq!(core.trust, 0.49);
        assert_eq!(test.trust, 0.1);
        assert_eq!((core.risk, test.risk), (36.8, 11.0));
        assert_eq!(core.recurring_churn, 0.38);
        assert_eq!(core.bug_fix_ratio, 0.2);
        assert!(test.concern < core.concern);
        assert!(is_test("Tests/Feature.cs"));
        assert!(!is_test("src/Contest.cs"));
    }
}
