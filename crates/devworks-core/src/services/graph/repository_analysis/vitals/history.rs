//! Retained Git-derived metrics over one exact, bounded history.
use super::super::super::budget::Budget;
use super::scores::{Churn, Coupling, Knowledge, ratio, round};
use crate::services::git::client::{GitHistory, HistoryCommit, HistoryTouch};
use jiff::{Timestamp, ToSpan, civil::Date, tz::TimeZone};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

const DAY: i64 = 86_400;
const MONTHS: usize = 12;
const MAX_DAILY_COUPLING_PATHS: usize = 50;
const EXTENSIONS: &[&str] = &[
    "cs", "ts", "tsx", "js", "jsx", "cfm", "cfc", "sql", "vue", "py", "java", "swift", "rs", "c",
    "h", "cpp", "cc", "cxx", "hpp", "hxx", "hh", "go", "rb", "php", "sh", "bash", "zsh",
];

static BUG_FIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(fix|bug|broken|defect|patch)\b").expect("bug-fix keywords")
});
static FIREFIGHT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(hotfix|incident|urgent|emergency|rollback|revert)\b")
        .expect("firefighting keywords")
});

pub(super) fn extension(path: &str) -> Option<String> {
    let (_, ext) = path.rsplit_once('.')?;
    let ext = ext.to_ascii_lowercase();
    EXTENSIONS.contains(&ext.as_str()).then_some(ext)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct MonthlyPoint {
    pub month: String,
    pub commits: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(in crate::services::graph::repository_analysis) struct Vitality {
    pub total_commits: usize,
    pub first_commit_at_ms: Option<i64>,
    pub maturity: Option<String>,
    pub activity: Option<String>,
    pub sufficient_trends: bool,
    monthly: Vec<MonthlyPoint>,
    pub last_six_months: usize,
    pub prior_six_months: usize,
    pub velocity_change_percent: f64,
    pub dormant_months: usize,
    pub inactive_streak: usize,
    pub source_commits_90: usize,
    pub source_commits_365: usize,
    pub firefighting_commits_90: usize,
    pub firefighting_commits_365: usize,
    pub firefighting_rate_90: f64,
    pub firefighting_rate_365: f64,
    pub firefighting: String,
}

#[derive(Default)]
pub(super) struct HistoryMetrics {
    pub churn: BTreeMap<String, Churn>,
    pub knowledge: BTreeMap<String, Knowledge>,
    pub coupling: BTreeMap<String, Coupling>,
    pub vitality: Option<Vitality>,
    pub gaps: Vec<String>,
}

#[derive(Default)]
struct Changes {
    churn: Churn,
    authors: BTreeSet<String>,
}

#[derive(Default)]
struct Daily {
    distinct: BTreeSet<String>,
    first_paths: Vec<String>,
}
impl Daily {
    fn add(&mut self, path: String) {
        if self.distinct.insert(path.clone()) && self.first_paths.len() < MAX_DAILY_COUPLING_PATHS {
            self.first_paths.push(path);
        }
    }
}

#[derive(Default)]
struct VitalityInput {
    monthly: BTreeMap<(i16, i8), usize>,
    source_90: usize,
    source_365: usize,
    fire_90: usize,
    fire_365: usize,
}

impl HistoryMetrics {
    pub(super) fn build(history: &GitHistory, now: Timestamp, budget: &mut Budget) -> Self {
        let mut out = Self::default();
        if history.shallow {
            out.gaps.push("ShallowHistory".into());
        }
        if history.clipped {
            out.gaps.push("CommitLimit".into());
        }
        if history.refused_paths > 0 {
            out.gaps.push("UnrepresentableHistoryPath".into());
        }

        let mut files = BTreeMap::<String, Changes>::new();
        let mut authors = BTreeMap::<String, BTreeMap<String, (String, usize)>>::new();
        let mut days = BTreeMap::<Date, Daily>::new();
        let mut vitality = VitalityInput::default();
        'commits: for commit in &history.commits {
            if !budget.visit() {
                break;
            }
            let author = commit.author.to_lowercase();
            let coupling_age = now.as_second().saturating_sub(commit.committed_secs);
            let mut source_touches = BTreeMap::<String, &HistoryTouch>::new();
            for touch in &commit.touches {
                if !budget.visit() {
                    break 'commits;
                }
                let key = touch.path.to_lowercase();
                if !budget.retain(key.len() + author.len() + 96) {
                    break 'commits;
                }
                let per_file = authors.entry(key.clone()).or_default();
                let count = per_file
                    .entry(author.clone())
                    .or_insert_with(|| (commit.author.clone(), 0));
                count.1 += 1;

                if extension(&touch.path).is_none() {
                    continue;
                }
                if coupling_age <= 180 * DAY {
                    days.entry(commit.author_day).or_default().add(key.clone());
                }
                if touch.lines.is_some() {
                    source_touches.insert(key, touch);
                }
            }
            record_commit(now, commit, &source_touches, &mut files, &mut vitality);
        }
        if let Some(exhausted) = budget.exhausted {
            out.gaps.push(exhausted.into());
            return out;
        }
        out.churn = files
            .into_iter()
            .map(|(path, mut changes)| {
                changes.churn.authors = changes.authors.len();
                (path, changes.churn)
            })
            .collect();
        out.knowledge = authors
            .into_iter()
            .map(|(path, counts)| (path, knowledge(counts)))
            .collect();
        if let Some(coupling) = calculate_coupling(days, budget) {
            out.coupling = coupling;
        } else if let Some(exhausted) = budget.exhausted {
            out.gaps.push(exhausted.into());
        }
        out.vitality = vitality.finish(history, now);
        out
    }
}

fn record_commit(
    now: Timestamp,
    commit: &HistoryCommit,
    touches: &BTreeMap<String, &HistoryTouch>,
    files: &mut BTreeMap<String, Changes>,
    vitality: &mut VitalityInput,
) {
    // Git's retained vitals query restricts the committer timestamp before the
    // score windows apply to the author timestamp.
    if touches.is_empty() || now.as_second().saturating_sub(commit.committed_secs) > 400 * DAY {
        return;
    }
    let age = now
        .as_millisecond()
        .saturating_sub(commit.author_time.as_millisecond()) as f64
        / (1_000.0 * DAY as f64);
    let bug = BUG_FIX.is_match(&commit.subject);
    let fight = FIREFIGHT.is_match(&commit.subject);
    let changed_lines: u64 = touches
        .values()
        .filter_map(|touch| touch.lines)
        .map(|(a, b)| u64::from(a) + u64::from(b))
        .sum();
    let author = commit.author.to_lowercase();

    for (path, touch) in touches {
        let Some((added, removed)) = touch.lines else {
            continue;
        };
        let weight = if changed_lines > 0 {
            (u64::from(added) + u64::from(removed)) as f64 / changed_lines as f64
        } else {
            1.0 / touches.len() as f64
        };
        let changes = files.entry(path.clone()).or_default();
        let churn = &mut changes.churn;
        if age <= 365.0 {
            churn.weighted_365 += weight;
            if bug {
                churn.bug_fixes_365 += weight;
            }
        }
        if age <= 90.0 {
            churn.changes += 1;
            churn.lines_added += u64::from(added);
            churn.lines_removed += u64::from(removed);
            changes.authors.insert(author.clone());
            let time = commit.author_time.as_millisecond();
            churn.last_change_ms = Some(
                churn
                    .last_change_ms
                    .map_or(time, |previous| previous.max(time)),
            );
            churn.weighted_90 += weight;
            if bug {
                churn.bug_fixes_90 += weight;
            }
        }
        if age <= 30.0 {
            churn.weighted_30 += weight;
        }
    }

    let author = commit.author_time.to_zoned(TimeZone::UTC);
    *vitality
        .monthly
        .entry((author.year(), author.month()))
        .or_default() += 1;
    if age <= 365.0 {
        vitality.source_365 += 1;
        vitality.fire_365 += usize::from(fight);
    }
    if age <= 90.0 {
        vitality.source_90 += 1;
        vitality.fire_90 += usize::from(fight);
    }
}

fn knowledge(authors: BTreeMap<String, (String, usize)>) -> Knowledge {
    let mut authors: Vec<_> = authors.into_values().collect();
    authors.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let total = authors.iter().map(|author| author.1).sum::<usize>();
    let (mut covered, mut truck_factor) = (0, 0);
    for (_, count) in &authors {
        covered += count;
        truck_factor += 1;
        if covered > total / 2 {
            break;
        }
    }
    authors.truncate(5);
    Knowledge {
        truck_factor,
        top_authors: authors,
    }
}

fn calculate_coupling(
    days: BTreeMap<Date, Daily>,
    budget: &mut Budget,
) -> Option<BTreeMap<String, Coupling>> {
    let mut pair_counts = BTreeMap::<(String, String), usize>::new();
    let mut day_counts = BTreeMap::<String, usize>::new();
    for (_, mut day) in days {
        for path in day.distinct {
            if !budget.visit() {
                return None;
            }
            *day_counts.entry(path).or_default() += 1;
        }
        day.first_paths.sort();
        for (i, a) in day.first_paths.iter().enumerate() {
            for b in &day.first_paths[i + 1..] {
                if !budget.visit() || !budget.retain(a.len() + b.len() + 64) {
                    return None;
                }
                *pair_counts.entry((a.clone(), b.clone())).or_default() += 1;
            }
        }
    }
    let mut coupling = BTreeMap::<String, Coupling>::new();
    for ((a, b), count) in pair_counts {
        for path in [a, b] {
            let strength = ratio(count, day_counts[&path]).min(1.0);
            let data = coupling.entry(path).or_default();
            data.strength = data.strength.max(strength);
            data.partners += 1;
        }
    }
    Some(coupling)
}

impl VitalityInput {
    fn finish(self, history: &GitHistory, now: Timestamp) -> Option<Vitality> {
        let now_utc = now.to_zoned(TimeZone::UTC);
        let month = Date::new(now_utc.year(), now_utc.month(), 1).ok()?;
        let monthly: Vec<_> = (0..MONTHS)
            .rev()
            .map(|offset| {
                let current = month.checked_sub((offset as i64).months()).ok()?;
                Some(MonthlyPoint {
                    month: format!("{:04}-{:02}", current.year(), current.month()),
                    commits: self
                        .monthly
                        .get(&(current.year(), current.month()))
                        .copied()
                        .unwrap_or_default(),
                })
            })
            .collect::<Option<_>>()?;
        let recent = monthly[6..].iter().map(|point| point.commits).sum();
        let prior = monthly[..6].iter().map(|point| point.commits).sum();
        let maturity = history.first_commit_time.map(|first| {
            let days = now.as_second().saturating_sub(first.as_second()) / DAY;
            if days < 180 || history.commit_count < 100 {
                "Young"
            } else if days < 365 || history.commit_count < 300 {
                "Growing"
            } else {
                "Mature"
            }
        });
        let sufficient_trends = maturity != Some("Young") && !history.shallow && !history.clipped;
        let dormant = monthly.iter().filter(|point| point.commits == 0).count();
        let activity = sufficient_trends.then(|| activity(&monthly, recent, prior, dormant).into());
        let (mut current, mut streak) = (0, 0);
        for point in &monthly {
            current = if point.commits == 0 { current + 1 } else { 0 };
            streak = streak.max(current);
        }
        let change_percent = if prior > 0 {
            round((recent as f64 - prior as f64) / prior as f64 * 100.0, 10.0)
        } else if recent > 0 {
            100.0
        } else {
            0.0
        };
        let rate_90 = round(ratio(self.fire_90, self.source_90), 1000.0);
        let rate_365 = round(ratio(self.fire_365, self.source_365), 1000.0);
        let firefighting = if rate_365 < 0.10 {
            "Low"
        } else if rate_365 < 0.25 {
            "Moderate"
        } else if rate_365 < 0.40 {
            "High"
        } else {
            "Critical"
        };
        Some(Vitality {
            total_commits: history.commit_count,
            first_commit_at_ms: history.first_commit_time.map(|time| time.as_millisecond()),
            maturity: maturity.map(str::to_owned),
            activity,
            sufficient_trends,
            monthly,
            last_six_months: recent,
            prior_six_months: prior,
            velocity_change_percent: change_percent,
            dormant_months: dormant,
            inactive_streak: streak,
            source_commits_90: self.source_90,
            source_commits_365: self.source_365,
            firefighting_commits_90: self.fire_90,
            firefighting_commits_365: self.fire_365,
            firefighting_rate_90: rate_90,
            firefighting_rate_365: rate_365,
            firefighting: firefighting.into(),
        })
    }
}

fn activity(monthly: &[MonthlyPoint], recent: usize, prior: usize, dormant: usize) -> &'static str {
    let recent_zero = monthly
        .iter()
        .rev()
        .take_while(|point| point.commits == 0)
        .count();
    if recent_zero >= 3 && recent <= (prior / 4).max(1) {
        "PossiblyAbandoned"
    } else if recent_zero >= 2 {
        "Dormant"
    } else if dormant >= 2 && prior > 0 && recent as f64 >= (prior as f64 * 1.3).ceil() {
        "Revived"
    } else if prior > 0 && recent as f64 <= (prior as f64 * 0.7).floor() {
        "Slowing"
    } else if prior == 0 || ((recent as f64 - prior as f64) / prior as f64 * 100.0).abs() <= 20.0 {
        "Stable"
    } else {
        "Active"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::data::QueryLimits;

    fn sample(date: &str, subject: &str, author: &str, paths: &[(&str, u32)]) -> HistoryCommit {
        let author_time = date.parse::<Timestamp>().unwrap();
        HistoryCommit {
            oid: "a".repeat(40),
            committed_secs: author_time.as_second(),
            author_time,
            author_day: date[..10].parse().unwrap(),
            author: author.into(),
            subject: subject.into(),
            touches: paths
                .iter()
                .map(|(path, lines)| HistoryTouch {
                    path: (*path).into(),
                    lines: Some((*lines, 0)),
                })
                .collect(),
        }
    }

    #[test]
    fn weighted_bugfix_daily_coupling_and_majority_authorship_are_separate() {
        let commits = vec![
            sample(
                "2026-11-01T17:00:00Z",
                "Fix regression",
                "Ada",
                &[("src/A.rs", 9), ("src/B.rs", 1)],
            ),
            sample(
                "2026-11-01T12:00:00Z",
                "Hotfix for incident",
                "BOB",
                &[("src/B.rs", 2)],
            ),
            sample(
                "2026-10-30T12:00:00Z",
                "Add flow",
                "Bob",
                &[("src/A.rs", 2)],
            ),
        ];
        let history = GitHistory {
            first_commit_time: Some(commits[0].author_time),
            commits,
            commit_count: 3,
            shallow: false,
            clipped: false,
            refused_paths: 0,
        };
        let now = "2026-11-19T00:00:00Z".parse().unwrap();
        let metrics = HistoryMetrics::build(
            &history,
            now,
            &mut Budget::new(&QueryLimits {
                max_results: 10,
                max_visited: 100,
                deadline_ms: 30_000,
            }),
        );
        assert_eq!(metrics.churn["src/a.rs"].changes, 2);
        assert_eq!(metrics.churn["src/a.rs"].weighted_365, 1.9);
        assert_eq!(metrics.churn["src/a.rs"].bug_fixes_365, 0.9);
        assert_eq!(metrics.coupling["src/a.rs"].strength, 0.5);
        assert_eq!(metrics.coupling["src/b.rs"].strength, 1.0);
        assert_eq!(metrics.knowledge["src/a.rs"].truck_factor, 2);
        assert_eq!(metrics.knowledge["src/b.rs"].truck_factor, 2);
        let vitality = metrics.vitality.unwrap();
        assert_eq!(vitality.firefighting_rate_90, 0.333);
        assert_eq!(vitality.maturity.as_deref(), Some("Young"));
        assert!(vitality.activity.is_none());
    }
}
