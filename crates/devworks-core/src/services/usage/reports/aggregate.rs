//! Retained AdminReports arithmetic. Missing measurements omit that metric and mark coverage;
//! an actually empty bucket is a measured zero, not an unknown field coerced to zero.
use super::*;
use std::cmp::Ordering;
// Store real metrics sparsely; absent buckets are measured zero. Serialized spans bind
// the cursor to all metrics without expanding millions of identical synthetic rows.
#[derive(Default, Serialize)]
pub(super) struct ReportRows(Vec<Span>);
#[derive(Serialize)]
enum Span {
    Row(UsageReportRow),
    Series {
        key: String,
        label: String,
        first: i64,
        interval: Interval,
        count: usize,
        values: BTreeMap<usize, Option<i64>>,
    },
}
impl ReportRows {
    fn push(&mut self, row: UsageReportRow) {
        self.0.push(Span::Row(row));
    }
    fn series(
        &mut self,
        q: &Query,
        key: &str,
        label: &str,
        values: BTreeMap<i64, Option<i64>>,
        gaps: &mut Gaps,
    ) -> SResult<()> {
        let first = q.interval.floor(q.start)?;
        let count = q.interval.count(first, q.end)?;
        let mut sparse = BTreeMap::new();
        for (bucket, value) in values {
            if value.is_none() {
                gaps.add("report-metric-unavailable");
            }
            if value != Some(0) {
                sparse.insert(q.interval.index(first, bucket)?, value);
            }
        }
        self.0.push(Span::Series {
            key: key.into(),
            label: label.into(),
            first,
            interval: q.interval,
            count,
            values: sparse,
        });
        Ok(())
    }
}
impl RowSource<UsageReportRow> for ReportRows {
    fn len(&self) -> usize {
        self.0
            .iter()
            .map(|span| match span {
                Span::Row(_) => 1,
                Span::Series { count, values, .. } => {
                    count - values.values().filter(|v| v.is_none()).count()
                }
            })
            .sum()
    }
    fn row(&self, mut index: usize) -> SResult<UsageReportRow> {
        for span in &self.0 {
            match span {
                Span::Row(row) => {
                    if index == 0 {
                        return Ok(row.clone());
                    }
                    index -= 1;
                }
                Span::Series {
                    key,
                    label,
                    first,
                    interval,
                    count,
                    values,
                } => {
                    let length = count - values.values().filter(|v| v.is_none()).count();
                    if index >= length {
                        index -= length;
                        continue;
                    }
                    let mut physical = index;
                    for (&at, value) in values {
                        if at > physical {
                            break;
                        }
                        if value.is_none() {
                            physical += 1;
                        }
                    }
                    return Ok(UsageReportRow::SeriesPoint {
                        key: BoundedText(key.clone()),
                        label: BoundedText(label.clone()),
                        bucket_start_ms: crate::protocol::support::SignedCounter(
                            interval.at(*first, physical)?.to_string(),
                        ),
                        value: count_value(values.get(&physical).copied().flatten().unwrap_or(0))?,
                    });
                }
            }
        }
        Err(StorageError::CursorExpired.into())
    }
}
fn count_value(value: i64) -> SResult<Counter> {
    count(value)
}
fn ordinal(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}
fn normalized_key(value: Option<&str>) -> String {
    normalized(value).unwrap_or_else(|| "(unknown)".into())
}
fn sum(rows: &[&Fact], value: impl Fn(&Fact) -> Option<i64>) -> SResult<Option<i64>> {
    let mut out = 0;
    for row in rows {
        let Some(v) = value(row) else { return Ok(None) };
        out = plus(out, v)?;
    }
    Ok(Some(out))
}
fn unique(rows: &[&Fact], chat: bool) -> Option<i64> {
    let mut values = BTreeSet::new();
    for row in rows {
        let user = row.user.as_deref()?;
        if chat {
            values.insert(json!([user, row.chat.as_deref()?]).to_string());
        } else if !user.trim().is_empty() {
            values.insert(user.into());
        }
    }
    Some(values.len() as i64)
}
fn round_even(sum: i64, n: i64) -> SResult<i64> {
    if n == 0 {
        return Ok(0);
    }
    let whole = sum / n;
    let remain = sum % n;
    plus(
        whole,
        i64::from(
            i128::from(remain) * 2 > i128::from(n)
                || i128::from(remain) * 2 == i128::from(n) && whole % 2 != 0,
        ),
    )
}
fn average(rows: &[&Fact]) -> SResult<Option<i64>> {
    sum(rows, |f| f.duration)?
        .map(|v| round_even(v, rows.len() as i64))
        .transpose()
}
fn status(f: &Fact, status: &str) -> Option<i64> {
    f.status
        .as_ref()
        .map(|s| i64::from(s.eq_ignore_ascii_case(status)))
}
fn total_tokens(f: &Fact) -> Option<i64> {
    f.input?.checked_add(f.output?)
}
fn metric(rows: &[&Fact], key: &str) -> SResult<Option<i64>> {
    match key {
        "totalTokens" => sum(rows, total_tokens),
        "inputTokens" => sum(rows, |f| f.input),
        "outputTokens" => sum(rows, |f| f.output),
        "runCount" | "requestCount" | "callCount" => Ok(Some(rows.len() as i64)),
        "uniqueChats" => Ok(unique(rows, true)),
        "uniqueUsers" => Ok(unique(rows, false)),
        "completedRuns" => sum(rows, |f| status(f, "completed")),
        "failedRuns" => sum(rows, |f| status(f, "failed")),
        "cancelledRuns" => sum(rows, |f| status(f, "cancelled")),
        "successfulCalls" => sum(rows, |f| status(f, "success")),
        "failedCalls" => sum(rows, |f| status(f, "failure")),
        "averageDurationMs" => average(rows),
        _ => Err(invalid("usage_metric")),
    }
}
fn series(
    out: &mut ReportRows,
    q: &Query,
    rows: &[&Fact],
    key: &str,
    label: &str,
    gaps: &mut Gaps,
) -> SResult<()> {
    let mut groups: BTreeMap<i64, Vec<&Fact>> = BTreeMap::new();
    for row in rows {
        groups
            .entry(q.interval.floor(row.time)?)
            .or_default()
            .push(row);
    }
    let values = groups
        .into_iter()
        .map(|(bucket, rows)| Ok((bucket, metric(&rows, key)?)))
        .collect::<SResult<BTreeMap<_, _>>>()?;
    out.series(q, key, label, values, gaps)?;
    Ok(())
}
fn breakdown(
    out: &mut ReportRows,
    q: &Query,
    rows: &[&Fact],
    shape: (&str, bool, bool),
    gaps: &mut Gaps,
) -> SResult<()> {
    let (dimension, tokens, include_total) = shape;
    if include_total {
        // Retained totals group raw values before normalizing labels; series group normalized
        // labels. Whitespace variants can therefore intentionally retain two total rows.
        let raw = matches!(
            q.kind,
            UsageReportKind::AssistantUsage
                | UsageReportKind::CodeReviewUsage
                | UsageReportKind::RepositoryAnalysisUsage
        ) || matches!(q.kind, UsageReportKind::AssistantActivity)
            && matches!(dimension, "user" | "status")
            || matches!(q.kind, UsageReportKind::McpUsage) && dimension == "tool";
        let mut totals: BTreeMap<Option<String>, Vec<&Fact>> = BTreeMap::new();
        for row in rows {
            let key = if raw {
                row.dimension(dimension).map(str::to_owned)
            } else {
                Some(normalized_key(row.dimension(dimension)))
            };
            totals.entry(key).or_default().push(row);
        }
        let mut ranked = Vec::new();
        for (key, rows) in totals {
            let value = if tokens {
                sum(&rows, total_tokens)?
            } else {
                Some(rows.len() as i64)
            };
            if let Some(value) = value {
                if value > 0 {
                    ranked.push((normalized_key(key.as_deref()), value));
                }
            } else {
                gaps.add("report-metric-unavailable");
            }
        }
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| ordinal(&a.0, &b.0)));
        for (key, value) in ranked {
            out.push(UsageReportRow::Breakdown {
                dimension: BoundedText(dimension.into()),
                key: BoundedText(key.clone()),
                label: BoundedText(key),
                value: count(value)?,
            });
        }
    }
    let mut groups: BTreeMap<String, Vec<&Fact>> = BTreeMap::new();
    for row in rows {
        groups
            .entry(normalized_key(row.dimension(dimension)))
            .or_default()
            .push(row);
    }
    let mut ranked = Vec::new();
    for (key, rows) in groups {
        let value = if tokens {
            sum(&rows, total_tokens)?
        } else {
            Some(rows.len() as i64)
        };
        if let Some(value) = value {
            if value > 0 {
                ranked.push((key, value, rows));
            }
        } else {
            gaps.add("report-metric-unavailable");
        }
    }
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| ordinal(&a.0, &b.0)));
    for (key, _value, rows) in ranked {
        let series_key = format!(
            "breakdown:{dimension}:{}",
            &crate::fingerprint::sha256_hex(key.as_bytes())[..40]
        );
        let mut groups: BTreeMap<i64, Vec<&Fact>> = BTreeMap::new();
        for row in rows {
            groups
                .entry(q.interval.floor(row.time)?)
                .or_default()
                .push(row);
        }
        let values = groups
            .into_iter()
            .map(|(bucket, rows)| {
                Ok((
                    bucket,
                    if tokens {
                        sum(&rows, total_tokens)?
                    } else {
                        Some(rows.len() as i64)
                    },
                ))
            })
            .collect::<SResult<BTreeMap<_, _>>>()?;
        out.series(q, &series_key, &key, values, gaps)?;
    }
    Ok(())
}
pub(super) fn activity(facts: &[&Fact], q: &Query) -> Vec<Fact> {
    let mut requests: BTreeMap<&str, &Fact> = BTreeMap::new();
    for f in facts.iter().copied().filter(|f| f.activity) {
        if let Some(key) = f.request.as_deref() {
            let replace = requests.get(key).is_none_or(|old| {
                (f.attempt_number, f.time, &f.key) > (old.attempt_number, old.time, &old.key)
            });
            if replace {
                requests.insert(key, f);
            }
        }
    }
    requests
        .into_values()
        .filter(|f| q.matches(f) && f.activity_time.is_some_and(|at| q.in_range(at)))
        .map(|f| {
            let mut f = f.clone();
            f.time = f.activity_time.expect("filtered");
            f
        })
        .collect()
}
pub(super) fn report(q: &Query, facts: &[&Fact], gaps: &mut Gaps) -> SResult<ReportRows> {
    let mcp = matches!(q.kind, UsageReportKind::McpUsage);
    let required = match q.kind {
        UsageReportKind::CodeReviewUsage => Path::Review,
        UsageReportKind::RepositoryAnalysisUsage => Path::Analysis,
        _ => Path::Assistant,
    };
    let eligible = facts
        .iter()
        .copied()
        .filter(|f| q.in_range(f.time) && q.matches(f))
        .collect::<Vec<_>>();
    let unknown_path = eligible.iter().any(|f| {
        f.path == Path::Unknown
            && f.legacy
                .as_ref()
                .is_none_or(|o| o.source_kind != "ToolInvocation")
    });
    if !mcp && unknown_path {
        gaps.add("report-path-unrecorded");
    }
    let usage = eligible
        .iter()
        .copied()
        .filter(|f| f.path == required && f.record.is_some())
        .collect::<Vec<_>>();
    let run_source = facts
        .iter()
        .copied()
        .filter(|f| f.path == required)
        .collect::<Vec<_>>();
    let run_values = activity(&run_source, q);
    let runs = run_values.iter().collect::<Vec<_>>();
    let mut mcp_rows = Vec::new();
    if mcp {
        for f in eligible.iter().copied().filter(|f| f.mcp) {
            let mut f = f.clone();
            f.status = match f.status.as_deref() {
                Some("Completed") | Some("success") => Some("success".into()),
                Some("Failed") | Some("failure") => Some("failure".into()),
                _ => {
                    gaps.add("mcp-success-unrecorded");
                    None
                }
            };
            mcp_rows.push(f);
        }
    }
    let mcp_refs = mcp_rows.iter().collect::<Vec<_>>();
    let (rows, metrics): (&[&Fact], &[(&str, &str)]) = match q.kind {
        UsageReportKind::AssistantUsage => (
            &usage,
            &[
                ("totalTokens", "Total Tokens"),
                ("inputTokens", "Input Tokens"),
                ("outputTokens", "Output Tokens"),
            ],
        ),
        UsageReportKind::AssistantActivity => (
            &runs,
            &[
                ("runCount", "Run Count"),
                ("uniqueChats", "Unique Chats"),
                ("completedRuns", "Completed Runs"),
                ("failedRuns", "Failed Runs"),
                ("cancelledRuns", "Cancelled Runs"),
            ],
        ),
        UsageReportKind::McpUsage => (
            &mcp_refs,
            &[
                ("callCount", "Call Count"),
                ("successfulCalls", "Successful Calls"),
                ("failedCalls", "Failed Calls"),
                ("uniqueUsers", "Unique Users"),
                ("averageDurationMs", "Avg Duration (ms)"),
            ],
        ),
        _ => (
            &usage,
            &[
                ("totalTokens", "Total Tokens"),
                ("inputTokens", "Input Tokens"),
                ("outputTokens", "Output Tokens"),
                ("requestCount", "Request Count"),
                ("uniqueUsers", "Unique Users"),
            ],
        ),
    };
    let coverage_rows = match q.kind {
        UsageReportKind::AssistantUsage => usage
            .iter()
            .copied()
            .chain(runs.iter().copied())
            .collect::<Vec<_>>(),
        UsageReportKind::AssistantActivity => runs.clone(),
        UsageReportKind::McpUsage => mcp_refs.clone(),
        _ => usage.clone(),
    };
    for fact in coverage_rows {
        if fact.balance.as_ref().is_some_and(|b| b.unknown_tail)
            || fact.legacy.as_ref().is_some_and(|o| o.unknown_tail)
        {
            gaps.add("remote-outcome-unknown");
        }
        if fact.balance.as_ref().is_some_and(|b| b.incomplete)
            || fact.legacy.as_ref().is_some_and(|o| o.incomplete)
        {
            gaps.add("usage-observation-incomplete");
        }
    }
    // Unknown categorization cannot establish that a source population is empty.
    if rows.is_empty()
        && (gaps.0.contains("filter-identity-unrecorded")
            || (!mcp && unknown_path)
            || (mcp && gaps.0.contains("tool-transport-unrecorded")))
    {
        return Ok(ReportRows::default());
    }
    let mut out = ReportRows::default();
    for (key, label) in metrics {
        if let Some(value) = metric(rows, key)? {
            out.push(UsageReportRow::Total {
                key: BoundedText((*key).into()),
                label: BoundedText((*label).into()),
                value: count(value)?,
            });
        } else {
            gaps.add("report-metric-unavailable");
        }
    }
    if matches!(q.kind, UsageReportKind::AssistantUsage) {
        for (key, label) in [("runCount", "Run Count"), ("uniqueChats", "Unique Chats")] {
            if let Some(value) = metric(&runs, key)? {
                out.push(UsageReportRow::Total {
                    key: BoundedText(key.into()),
                    label: BoundedText(label.into()),
                    value: count(value)?,
                });
            } else {
                gaps.add("report-metric-unavailable");
            }
        }
    }
    for (key, label) in metrics {
        if *key != "uniqueUsers" {
            series(&mut out, q, rows, key, label, gaps)?;
        }
    }
    match q.kind {
        UsageReportKind::AssistantUsage => {
            for (key, label) in [("runCount", "Run Count"), ("uniqueChats", "Unique Chats")] {
                series(&mut out, q, &runs, key, label, gaps)?;
            }
            for dim in ["user", "provider", "model"] {
                breakdown(&mut out, q, &usage, (dim, true, true), gaps)?;
            }
            breakdown(&mut out, q, &runs, ("status", false, false), gaps)?;
        }
        UsageReportKind::AssistantActivity => {
            for dim in ["status", "user", "provider", "model"] {
                breakdown(&mut out, q, &runs, (dim, false, true), gaps)?;
            }
        }
        UsageReportKind::McpUsage => {
            for dim in ["tool", "user", "status"] {
                breakdown(&mut out, q, &mcp_refs, (dim, false, true), gaps)?;
            }
        }
        _ => {
            for dim in ["user", "provider", "model"] {
                breakdown(&mut out, q, &usage, (dim, true, true), gaps)?;
            }
        }
    }
    Ok(out)
}
