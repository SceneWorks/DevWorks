//! Retained reports over immutable accounting facts. Complete authorized populations are read
//! in bounded source pages before output paging; missing identity/meters are never invented.
mod aggregate;
mod calendar;
use super::{accounting, meter::InputSemantics};
use crate::context::InvocationContext;
use crate::operations::router::UsageInput;
use crate::ports::storage::*;
use crate::protocol::data::{Coverage, CoverageState, DataScope};
use crate::protocol::records::*;
use crate::protocol::support::{BoundedList, BoundedText, Check, Cursor};
use crate::protocol::{Counter, DomainErrorCode, Id, Revision, RootId};
use crate::services::{
    cursor::CursorBinding,
    sessions::{Failure, Refusal},
};
use calendar::{DAY, Interval};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
type SResult<T> = Result<T, Failure>;
fn invalid(name: &'static str) -> Failure {
    Refusal::new(DomainErrorCode::InvalidArgument, name, None).into()
}
fn corrupt(name: &str) -> StorageError {
    StorageError::InvalidRecord(name.into())
}
fn wire<T: DeserializeOwned + Check>(v: Value) -> Result<T, StorageError> {
    let t: T = serde_json::from_value(v).map_err(|e| corrupt(&e.to_string()))?;
    t.check().map_err(StorageError::InvalidRecord)?;
    Ok(t)
}
fn count(v: i64) -> SResult<Counter> {
    Counter::new(v).map_err(|_| invalid("usage_counter"))
}
fn optional(v: Option<i64>) -> Option<String> {
    v.map(|n| n.to_string())
}
fn plus(a: i64, b: i64) -> SResult<i64> {
    a.checked_add(b).ok_or_else(|| invalid("usage_overflow"))
}
fn normalized(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
#[derive(Default)]
struct Gaps(BTreeSet<String>);
impl Gaps {
    fn add(&mut self, gap: &str) {
        self.0.insert(gap.into());
    }
    fn coverage(&self) -> Coverage {
        Coverage {
            state: if self.0.is_empty() {
                CoverageState::Complete
            } else {
                CoverageState::Partial
            },
            gaps: self
                .0
                .iter()
                .take(128)
                .map(|s| Id::new(s).expect("static gap identifier"))
                .collect(),
            more_gaps: self.0.len() > 128,
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Path {
    Assistant,
    Review,
    Analysis,
    Other,
    Unknown,
}
#[derive(Clone)]
struct Fact {
    key: String,
    time: i64,
    activity_time: Option<i64>,
    request: Option<String>,
    chat: Option<String>,
    user: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    tool: Option<String>,
    purpose: Option<String>,
    path: Path,
    status: Option<String>,
    duration: Option<i64>,
    mcp: bool,
    activity: bool,
    attempt_number: i64,
    input: Option<i64>,
    output: Option<i64>,
    record: Option<UsageAttemptRecord>,
    balance: Option<StoredAttemptBalance>,
    legacy: Option<StoredUsageOrigin>,
}
impl Fact {
    fn dimension(&self, dimension: &str) -> Option<&str> {
        match dimension {
            "user" => self.user.as_deref(),
            "provider" => self.provider.as_deref(),
            "model" => self.model.as_deref(),
            "tool" => self.tool.as_deref(),
            "status" => self.status.as_deref(),
            _ => None,
        }
    }
}
struct Query {
    context: UsageQueryContext,
    legacy_all_dates: bool,
    start: i64,
    end: i64,
    interval: Interval,
    project: Option<crate::protocol::ProjectId>,
    root: Option<Id>,
    root_is_feature: bool,
    session: Option<Id>,
    job: Option<Id>,
    kind: UsageReportKind,
}
impl Query {
    fn new(
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &UsageInput,
        now: i64,
        resolved: Option<(i64, i64)>,
        legacy_all_dates: bool,
    ) -> SResult<Self> {
        if ctx.store_id() != &read.stamp().store_id
            || ctx.restore_epoch().get() != read.stamp().restore_epoch
        {
            return Err(StorageError::ScopeDenied.into());
        }
        let kind = if legacy_all_dates {
            UsageReportKind::Attempts
        } else {
            i.report_kind.clone().unwrap_or(UsageReportKind::Attempts)
        };
        let (start, end, interval) = if legacy_all_dates {
            (0, i64::MAX, Interval::Day)
        } else {
            let parse_time = |v: &crate::protocol::support::SignedCounter| {
                v.0.parse::<i64>().map_err(|_| invalid("usage_range"))
            };
            let end = i
                .end_ms
                .as_ref()
                .map(parse_time)
                .transpose()?
                .or(resolved.map(|r| r.1))
                .unwrap_or(now);
            calendar::valid_source_date(end)?;
            let default_start = end
                .checked_sub(30 * DAY)
                .ok_or_else(|| invalid("usage_range"))?;
            let start = i
                .start_ms
                .as_ref()
                .map(parse_time)
                .transpose()?
                .or(resolved.map(|r| r.0))
                .unwrap_or(default_start);
            calendar::valid_source_date(start)?;
            if start >= end || end.checked_sub(start).is_none() {
                return Err(invalid("usage_range"));
            }
            let interval = Interval::parse(i.interval.as_ref().map(|s| s.0.as_str()), start, end)?;
            (start, end, interval)
        };
        let mut project = ctx.project_id().cloned();
        fn constrain(
            project: &mut Option<crate::protocol::ProjectId>,
            p: &crate::protocol::ProjectId,
        ) -> SResult<()> {
            if project.as_ref().is_some_and(|v| v != p) {
                return Err(StorageError::ScopeDenied.into());
            }
            *project = Some(p.clone());
            Ok(())
        }
        if let Some(scope) = &i.scope {
            match scope {
                DataScope::Workspace if ctx.project_id().is_some() => {
                    return Err(StorageError::ScopeDenied.into());
                }
                DataScope::Project { project_id } => constrain(&mut project, project_id)?,
                _ => {}
            }
        }
        if let Some(p) = &i.project_id {
            constrain(&mut project, p)?;
        }
        if let Some(root) = &i.root_id {
            let w = read
                .sessions()
                .work_root(root)?
                .ok_or_else(|| invalid("usage_root"))?;
            constrain(&mut project, &w.project_id)?;
        }
        if let Some(session) = &i.session_id {
            let row = read
                .sessions()
                .session(session)?
                .ok_or_else(|| invalid("usage_session"))?;
            if let Some(p) = row.project_id.as_ref() {
                constrain(&mut project, p)?;
            } else if project.is_some() {
                return Err(StorageError::ScopeDenied.into());
            }
        }
        if let Some(job) = &i.job_run_id {
            let run = read
                .jobs()
                .job_run(job)?
                .ok_or_else(|| invalid("usage_job_run"))?;
            let job = read
                .jobs()
                .job(&run.job_id)?
                .ok_or_else(|| invalid("usage_job"))?;
            if let Some(p) = job.project_id.as_ref() {
                constrain(&mut project, p)?;
            } else if project.is_some() {
                return Err(StorageError::ScopeDenied.into());
            }
        }
        if let Some(p) = &project
            && read.projects().project(p)?.is_none()
        {
            return Err(invalid("usage_project"));
        }
        let filters = matches!(kind, UsageReportKind::Filters);
        let mcp = matches!(kind, UsageReportKind::McpUsage);
        let context = UsageQueryContext {
            report_kind: kind.clone(),
            range: UsageReportRange {
                start_ms: crate::protocol::support::SignedCounter(start.to_string()),
                end_ms: crate::protocol::support::SignedCounter(end.to_string()),
            },
            interval: BoundedText(interval.name().into()),
            read_revision: Revision::new(read.stamp().committed_sequence.max(1))
                .map_err(|_| invalid("usage_revision"))?,
            applied_filters: UsageAppliedFilters {
                scope: i.scope.clone(),
                project_id: project.clone(),
                root_id: i.root_id.clone(),
                session_id: i.session_id.clone(),
                job_run_id: i.job_run_id.clone(),
                user: if filters {
                    None
                } else {
                    normalized(i.user.as_ref().map(|s| s.0.as_str())).map(BoundedText)
                },
                provider: if filters || mcp {
                    None
                } else {
                    normalized(i.provider.as_ref().map(|s| s.0.as_str())).map(BoundedText)
                },
                model: if filters || mcp {
                    None
                } else {
                    normalized(i.model.as_ref().map(|s| s.0.as_str())).map(BoundedText)
                },
                tool: if filters || !mcp {
                    None
                } else {
                    normalized(i.tool.as_ref().map(|s| s.0.as_str())).map(BoundedText)
                },
                purpose: if filters { None } else { i.purpose.clone() },
            },
        };
        Ok(Self {
            context,
            legacy_all_dates,
            start,
            end,
            interval,
            project,
            root: i.root_id.as_ref().map(|r| r.0.clone()),
            root_is_feature: i
                .root_id
                .as_ref()
                .map(|r| read.work().work(&r.0))
                .transpose()?
                .flatten()
                .is_some_and(|w| w.is_feature()),
            session: i.session_id.clone(),
            job: i.job_run_id.clone(),
            kind,
        })
    }
    fn in_range(&self, time: i64) -> bool {
        self.legacy_all_dates || time >= self.start && time < self.end
    }
    fn matches(&self, f: &Fact) -> bool {
        let a = &self.context.applied_filters;
        [
            (a.user.as_ref().map(|s| s.0.as_str()), f.user.as_deref()),
            (
                a.provider.as_ref().map(|s| s.0.as_str()),
                f.provider.as_deref(),
            ),
            (a.model.as_ref().map(|s| s.0.as_str()), f.model.as_deref()),
            (a.tool.as_ref().map(|s| s.0.as_str()), f.tool.as_deref()),
            (
                a.purpose.as_ref().map(|s| s.0.as_str()),
                f.purpose.as_deref(),
            ),
        ]
        .into_iter()
        .all(|(asked, actual)| asked.is_none_or(|s| actual == Some(s)))
    }
    fn relevant(&self, f: &Fact) -> bool {
        match self.kind {
            UsageReportKind::Attempts | UsageReportKind::Filters => true,
            UsageReportKind::AssistantUsage | UsageReportKind::AssistantActivity => {
                matches!(f.path, Path::Assistant | Path::Unknown)
            }
            UsageReportKind::CodeReviewUsage => matches!(f.path, Path::Review | Path::Unknown),
            UsageReportKind::RepositoryAnalysisUsage => {
                matches!(f.path, Path::Analysis | Path::Unknown)
            }
            UsageReportKind::McpUsage => f.mcp,
        }
    }
    /// Missing immutable identity cannot prove that a filtered population is empty. Known
    /// contradictory dimensions still exclude the fact; this never guesses a matching owner.
    fn uncertain_filter(&self, f: &Fact) -> bool {
        if !self.relevant(f) {
            return false;
        }
        let a = &self.context.applied_filters;
        let model_unknown = f
            .record
            .as_ref()
            .is_some_and(|r| r.resolved_model_id.is_none())
            || f.activity && f.record.is_none();
        let mut unknown = false;
        for (asked, actual, uncertain) in [
            (
                a.user.as_ref().map(|s| s.0.as_str()),
                f.user.as_deref(),
                false,
            ),
            (
                a.provider.as_ref().map(|s| s.0.as_str()),
                f.provider.as_deref(),
                false,
            ),
            (
                a.model.as_ref().map(|s| s.0.as_str()),
                f.model.as_deref(),
                model_unknown,
            ),
            (
                a.tool.as_ref().map(|s| s.0.as_str()),
                f.tool.as_deref(),
                false,
            ),
            (
                a.purpose.as_ref().map(|s| s.0.as_str()),
                f.purpose.as_deref(),
                false,
            ),
        ] {
            if let Some(asked) = asked {
                match actual {
                    Some(actual) if !uncertain && actual != asked => return false,
                    None => unknown = true,
                    _ if uncertain => unknown = true,
                    _ => {}
                }
            }
        }
        unknown
    }
    fn ownership(
        &self,
        project: Option<&crate::protocol::ProjectId>,
        feature: Option<&Id>,
        work: Option<&Id>,
        execution: Option<&RootId>,
        session: Option<&Id>,
        job: Option<&Id>,
    ) -> bool {
        self.project.as_ref().is_none_or(|p| Some(p) == project)
            && self.root.as_ref().is_none_or(|r| {
                Some(r) == feature || Some(r) == work || Some(r) == execution.map(|x| &x.0)
            })
            && self.session.as_ref().is_none_or(|s| Some(s) == session)
            && self.job.as_ref().is_none_or(|j| Some(j) == job)
    }
}
/// Provider identity is the captured billing transport, never today's catalog or a price source.
fn provider(profile: &str, connection: &Id) -> String {
    match profile {
        "OpenAiResponses" | "CodexSubscriptionResponses" => "openai".into(),
        "AnthropicMessages" | "ClaudeSubscriptionMessages" => "anthropic".into(),
        "OpenRouterChat" => "openrouter".into(),
        _ => connection.as_str().into(),
    }
}
fn path(value: Option<&str>) -> Path {
    match value {
        Some("Assistant") => Path::Assistant,
        Some("CodeReview") => Path::Review,
        Some("RepositoryAnalysis") => Path::Analysis,
        Some("Other" | "Mcp") => Path::Other,
        _ => Path::Unknown,
    }
}
fn recorded_path(
    read: &dyn ReadSnapshot,
    operation: &crate::protocol::OperationId,
) -> Result<Path, StorageError> {
    Ok(read
        .operations()
        .operation(operation)?
        .map_or(Path::Unknown, |op| {
            path(op.payload["reportCategory"].as_str())
        }))
}

fn tariff_json(t: &crate::ports::usage::PublishedTariff) -> Value {
    json!({"upstreamModelId":t.upstream_model_id,"serviceTier":t.service_tier,"contextLowerExclusive":optional(t.context_lower_exclusive),"contextUpperInclusive":optional(t.context_upper_inclusive),"modality":t.modality,"chargeKind":t.charge_kind,"cacheTtlSeconds":optional(t.cache_ttl_seconds),"unit":t.unit,"units":t.units.to_string(),"amountUsd":t.amount_usd,"region":t.region,"endpointProfile":t.endpoint_profile})
}
fn cost(
    classification: &str,
    amount: Option<i64>,
    complete: bool,
    incomplete: bool,
    quote: Option<&crate::ports::usage::PriceQuote>,
    current: Option<&accounting::BalanceSummary>,
) -> Value {
    let reported = current.and_then(|s| s.reported_cost.as_ref());
    let reason = if complete {
        None
    } else {
        current
            .filter(|s| !s.unknown_components.is_empty())
            .map(|s| s.unknown_components.join(", "))
            .or_else(|| quote.and_then(|q| q.untracked_reason.clone()))
            .or_else(|| Some("Usage observation is not available or complete".into()))
    };
    let (kind, origin) = match classification {
        "ReportedCharge" => ("Reported", "ProviderReported"),
        "ConfiguredEstimate" => ("Estimated", "UserConfigured"),
        "OfficialEstimate" | "SubscriptionApiEquivalent" => ("Estimated", "OfficialSnapshot"),
        "LocalFree" => ("Free", "LocalFree"),
        "Untracked" => ("Untracked", "Untracked"),
        _ => ("Unknown", "Untracked"),
    };
    json!({"priceKind":kind,"priceOrigin":origin,"amountMicroUsd":optional(amount),"currency":reported.map(|r|r.currency.as_str()).or(quote.map(|q|q.currency.as_str())),"originalAmount":reported.map(|r|r.amount.as_str()),"classification":classification,"priceSnapshotId":quote.and_then(|q|q.snapshot_id.as_ref()),"priceRevision":optional(quote.and_then(|q|q.price_revision)),"overrideRevision":optional(quote.and_then(|q|q.override_revision)),"subscriptionEquivalent":classification=="SubscriptionApiEquivalent","incomplete":incomplete,"untrackedReason":reason,"tariffs":quote.map(|q|q.tariffs.iter().map(tariff_json).collect::<Vec<_>>()).unwrap_or_default(),"monetaryComplete":complete})
}
fn attempt(read: &dyn ReadSnapshot, a: StoredInferenceAttempt, gaps: &mut Gaps) -> SResult<Fact> {
    let admitted = accounting::snapshot(&a)?;
    let balance = read.usage().attempt_balance(&a.attempt_id)?;
    let summary = balance
        .as_ref()
        .map(accounting::summary)
        .transpose()?
        .unwrap_or_default();
    let q = &summary.meter.quantities;
    let (cache_read_cost, cache_write_cost) =
        super::pricing::cache_estimates(&admitted.quote, q, &admitted.pricing)
            .map_err(|_| invalid("usage_cache_cost"))?;
    let mut normalized_quantities = q.clone();
    if !admitted.pricing.cache_read_possible && normalized_quantities.cached_read_tokens.is_none() {
        normalized_quantities.cached_read_tokens = Some(0);
    }
    if !admitted.pricing.cache_write_possible && normalized_quantities.cache_write_tokens.is_none()
    {
        normalized_quantities.cache_write_tokens = Some(0);
    }
    let input = normalized_quantities
        .total_input(admitted.pricing.input_semantics)
        .map_err(|_| invalid("usage_meter"))?;
    if input
        .zip(q.output_tokens)
        .is_some_and(|(a, b)| a.checked_add(b).is_none())
    {
        return Err(invalid("usage_overflow"));
    }
    let cache_conflict = (!admitted.pricing.cache_read_possible
        && q.cached_read_tokens.is_some_and(|n| n > 0))
        || (!admitted.pricing.cache_write_possible && q.cache_write_tokens.is_some_and(|n| n > 0));
    let uncached = if cache_conflict {
        None
    } else {
        normalized_quantities
            .uncached_input(admitted.pricing.input_semantics)
            .map_err(|_| invalid("usage_meter"))?
    };
    let provider = provider(&a.endpoint_profile, &a.connection_id);
    if balance.as_ref().is_none_or(|b| !b.monetary_complete) {
        gaps.add("monetary-coverage-incomplete");
    }
    if balance.as_ref().is_none_or(|b| b.incomplete) {
        gaps.add("usage-observation-incomplete");
    }
    if balance.as_ref().is_some_and(|b| b.unknown_tail) {
        gaps.add("remote-outcome-unknown");
    }
    let b = balance.as_ref();
    let classification = b.map_or("Unknown", |b| b.classification.as_str());
    let mut row = json!({"id":a.attempt_id,"requestId":a.request_id,"attemptId":a.attempt_id,"attemptNumber":a.attempt_number.to_string(),"sourceKind":"InferenceAttempt","sourceId":a.attempt_id,"actorId":a.actor_id,"projectId":a.project_id,"featureId":a.feature_id,"workId":a.work_id,"assignmentId":a.assignment_id,"sessionId":a.session_id,"jobRunId":a.job_run_id,"operationId":a.operation_id});
    row.as_object_mut().expect("object").extend(json!({"purpose":a.purpose,"role":a.role,"providerId":provider,"connectionId":a.connection_id,"modelId":a.catalog_model_id,"resolvedModelId":a.provider_model_id,"startedAtMs":a.created_at_ms.to_string(),"finishedAtMs":optional(summary.finished_at_ms),"state":a.state}).as_object().expect("object").clone());
    row.as_object_mut().expect("object").extend(json!({"inputTokens":optional(input),"uncachedInputTokens":optional(uncached),"outputTokens":optional(q.output_tokens),"cacheReadTokens":optional(q.cached_read_tokens),"cacheWriteTokens":optional(q.cache_write_tokens),"cacheReadMicroUsd":optional(cache_read_cost),"cacheWriteMicroUsd":optional(cache_write_cost),"inputSemantics":if admitted.pricing.input_semantics==InputSemantics::Inclusive{"Inclusive"}else{"Disjoint"},"unknownTail":b.is_some_and(|b|b.unknown_tail),"reasoningTokens":optional(q.reasoning_tokens),"connectionRevision":optional(admitted.quote.connection_revision)}).as_object().expect("object").clone());
    row["cost"] = cost(
        classification,
        b.and_then(|b| b.priced_micro_usd),
        b.is_some_and(|b| b.monetary_complete),
        b.is_none_or(|b| b.incomplete),
        Some(&admitted.quote),
        Some(&summary),
    );
    let mut diagnostics = admitted.cache_diagnostics.clone();
    if cache_read_cost.is_some() || cache_write_cost.is_some() {
        diagnostics.push(UsageCacheDiagnostic{kind:BoundedText("CacheCostBasis".into()),inferred:true,detail:Some(BoundedText("Cache component costs are estimates from the admitted tariff; provider-reported total remains independent".into())),source_revision:admitted.quote.price_revision.map(Revision::new).transpose().map_err(|_|invalid("usage_price_revision"))?});
    }
    if cache_conflict {
        diagnostics.push(UsageCacheDiagnostic {
            kind: BoundedText("CacheMeasurementConflict".into()),
            inferred: false,
            detail: Some(BoundedText(
                "Positive cache quantities contradict the host-declared cache capability".into(),
            )),
            source_revision: None,
        });
    }
    row["cacheDiagnostics"] =
        serde_json::to_value(&diagnostics).map_err(|e| corrupt(&e.to_string()))?;
    row["unitQuantities"]=json!(q.additional.iter().map(|u|json!({"modality":u.modality,"chargeKind":u.charge_kind,"unit":u.unit,"cacheWriteTtlMs":optional(u.cache_write_ttl_ms),"quantity":u.quantity.to_string()})).collect::<Vec<_>>());
    let record = wire(row)?;
    Ok(Fact {
        key: format!("Attempt:{}", a.attempt_id.as_str()),
        time: a.created_at_ms,
        activity_time: read
            .router()
            .route_request(&a.request_id)?
            .map(|r| r.created_at_ms),
        request: Some(a.request_id.as_str().into()),
        chat: a.session_id.map(|s| s.as_str().into()),
        user: Some(a.actor_id.0.as_str().into()),
        provider: Some(provider),
        model: a
            .provider_model_id
            .clone()
            .or_else(|| Some(a.catalog_model_id.as_str().into())),
        tool: None,
        purpose: Some(a.purpose.clone()),
        path: recorded_path(read, &a.operation_id)?,
        status: read
            .operations()
            .operation(&a.operation_id)?
            .map(|op| operation_status(op.state))
            .or(summary.outcome)
            .or(Some(a.state)),
        duration: None,
        mcp: false,
        activity: true,
        attempt_number: a.attempt_number,
        input,
        output: q.output_tokens,
        record: Some(record),
        balance,
        legacy: None,
    })
}
fn operation_status(state: crate::protocol::OperationState) -> String {
    use crate::protocol::OperationState;
    match state {
        OperationState::ObservedComplete => "completed".into(),
        OperationState::Failed => "failed".into(),
        OperationState::CancelledBeforeDispatch => "cancelled".into(),
        other => serde_json::to_value(other)
            .expect("enum")
            .as_str()
            .expect("enum")
            .into(),
    }
}
fn string(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}
fn number(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(|n| {
        n.as_i64()
            .or_else(|| n.as_str().and_then(|s| s.parse().ok()))
    })
}
fn source_id(kind: &str, source: &str) -> Id {
    Id::new(format!(
        "usage_{}",
        &crate::fingerprint::sha256_hex(json!([kind, source]).to_string().as_bytes())[..40]
    ))
    .expect("fixed source reference")
}
fn origin(read: &dyn ReadSnapshot, o: StoredUsageOrigin, gaps: &mut Gaps) -> SResult<Fact> {
    let key = format!("Origin:{}:{}", o.source_kind, o.source_id);
    let v = if o.source_kind == "SessionUsage" {
        &o.payload["item"]
    } else {
        &o.payload
    };
    let purpose = string(v, "purpose");
    let model = string(v, "modelId");
    let user = string(&o.payload, "actorId");
    let input = number(v, "inputTokens");
    let output = number(v, "outputTokens");
    if input
        .zip(output)
        .is_some_and(|(a, b)| a.checked_add(b).is_none())
    {
        return Err(invalid("usage_overflow"));
    }
    let mut state = string(&o.payload, "state");
    let mut report_path = path(o.payload["reportCategory"].as_str());
    if let Some(id) = string(&o.payload, "operationId") {
        let op = crate::protocol::OperationId(
            Id::new(id).map_err(|_| invalid("usage_source_operation"))?,
        );
        if let Some(operation) = read.operations().operation(&op)? {
            state = Some(operation_status(operation.state));
            report_path = path(operation.payload["reportCategory"].as_str());
        }
    }
    let tool = string(&o.payload, "capabilityId");
    let transport = string(&o.payload, "transportOrigin");
    let mcp = transport.as_deref() == Some("Mcp") || transport.as_deref() == Some("MCP");
    if o.source_kind == "ToolInvocation" && !mcp {
        gaps.add("tool-transport-unrecorded");
    }
    let record = if matches!(o.source_kind.as_str(), "SessionUsage" | "JobUsage")
        && o.attempt_id.is_none()
    {
        let reference = source_id(&o.source_kind, &o.source_id);
        let request = o.request_id.as_ref();
        if request.is_none() {
            gaps.add("legacy-request-identity-unrecorded");
        }
        let (kind, origin) = match o.classification.as_str() {
            "ReportedCharge" => ("Reported", "ProviderReported"),
            "ConfiguredEstimate" => ("Estimated", "UserConfigured"),
            "OfficialEstimate" | "SubscriptionApiEquivalent" => ("Estimated", "OfficialSnapshot"),
            "LocalFree" => ("Free", "LocalFree"),
            _ => ("Untracked", "Untracked"),
        };
        let c = json!({"priceKind":kind,"priceOrigin":origin,"amountMicroUsd":optional(o.priced_micro_usd),"currency":o.priced_micro_usd.map(|_|"USD"),"originalAmount":v.get("amountUsd"),"priceRevision":v.get("priceRevision"),"classification":o.classification,"subscriptionEquivalent":o.classification=="SubscriptionApiEquivalent","incomplete":o.incomplete,"untrackedReason":"Historical observation has no proven attempt identity","tariffs":[],"monetaryComplete":false});
        Some(wire(
            json!({"id":reference,"requestId":request,"sourceKind":o.source_kind,"sourceId":reference,"actorId":user,"projectId":o.project_id,"featureId":o.feature_id,"workId":o.work_id,"sessionId":o.session_id,"jobRunId":o.job_run_id,"purpose":purpose.as_deref().unwrap_or("Unknown"),"modelId":model,"startedAtMs":o.recorded_at_ms.to_string(),"state":"HistoricalObservation","inputTokens":optional(input),"outputTokens":optional(output),"cacheReadTokens":optional(number(v,"cacheReadTokens")),"cacheWriteTokens":optional(number(v,"cacheWriteTokens")),"cost":c,"cacheDiagnostics":[],"unknownTail":o.unknown_tail,"unitQuantities":[]}),
        )?)
    } else {
        None
    };
    let activity = o.source_kind == "RouterRequest";
    let status = if o.source_kind == "ToolInvocation" {
        string(&o.payload, "outcome")
    } else {
        state
    };
    Ok(Fact {
        key,
        time: o.recorded_at_ms,
        activity_time: activity.then_some(o.recorded_at_ms),
        request: o.request_id.as_ref().map(|s| s.as_str().into()),
        chat: o.session_id.as_ref().map(|s| s.as_str().into()),
        user,
        provider: string(&o.payload, "providerId"),
        model,
        tool,
        purpose,
        path: report_path,
        status,
        duration: number(&o.payload, "durationMs"),
        mcp,
        activity,
        attempt_number: 0,
        input,
        output,
        record,
        balance: None,
        legacy: Some(o),
    })
}
fn collect(read: &dyn ReadSnapshot, q: &Query, gaps: &mut Gaps) -> SResult<Vec<Fact>> {
    let mut facts = Vec::new();
    if q.end <= 0 {
        return Ok(facts);
    }
    let mut after = None;
    let activity_view = matches!(
        q.kind,
        UsageReportKind::AssistantUsage
            | UsageReportKind::AssistantActivity
            | UsageReportKind::Filters
    );
    let filter = UsageAttemptFilter {
        project_id: q.project.clone(),
        start_ms: (!activity_view && !q.legacy_all_dates).then(|| q.start.max(0)),
        end_ms: (!activity_view && !q.legacy_all_dates).then_some(q.end),
        ..Default::default()
    };
    loop {
        let rows = read
            .usage()
            .inference_attempts(&filter, after.as_ref(), 500)?;
        let full = rows.len() == 500;
        for a in rows {
            after = Some(a.attempt_id.clone());
            if q.ownership(
                a.project_id.as_ref(),
                a.feature_id.as_ref(),
                a.work_id.as_ref(),
                a.execution_root_id.as_ref(),
                a.session_id.as_ref(),
                a.job_run_id.as_ref(),
            ) {
                let logical_time = if activity_view {
                    read.router()
                        .route_request(&a.request_id)?
                        .map(|r| r.created_at_ms)
                } else {
                    None
                };
                if q.in_range(a.created_at_ms) || logical_time.is_some_and(|at| q.in_range(at)) {
                    facts.push(attempt(read, a, &mut Gaps::default())?);
                }
            }
        }
        if !full {
            break;
        }
    }
    let mut after = None;
    let filter = UsageOriginFilter {
        project_id: q.project.clone(),
        start_ms: (!q.legacy_all_dates).then(|| q.start.max(0)),
        end_ms: (!q.legacy_all_dates).then_some(q.end),
        ..Default::default()
    };
    loop {
        let rows = read.usage().usage_origins(&filter, after.as_deref(), 500)?;
        let full = rows.len() == 500;
        for o in rows {
            after = Some(o.cursor());
            if o.attempt_id.is_some()
                && matches!(o.source_kind.as_str(), "SessionUsage" | "JobUsage")
            {
                continue;
            }
            if q.root_is_feature
                && o.feature_id.is_none()
                && o.work_id.as_ref() != q.root.as_ref()
                && matches!(o.source_kind.as_str(), "SessionUsage" | "JobUsage")
            {
                gaps.add("historical-feature-ownership-unrecorded");
            }
            if q.ownership(
                o.project_id.as_ref(),
                o.feature_id.as_ref(),
                o.work_id.as_ref(),
                None,
                o.session_id.as_ref(),
                o.job_run_id.as_ref(),
            ) {
                let mut source_gaps = Gaps::default();
                let fact = origin(read, o, &mut source_gaps)?;
                for gap in source_gaps.0 {
                    if gap == "tool-transport-unrecorded" {
                        if matches!(q.kind, UsageReportKind::McpUsage | UsageReportKind::Filters) {
                            gaps.add(&gap);
                        }
                    } else if matches!(q.kind, UsageReportKind::Attempts) {
                        gaps.add(&gap);
                    }
                }
                facts.push(fact);
            }
        }
        if !full {
            break;
        }
    }
    if read.usage().usage_bootstrap()?.phase != "Complete" {
        gaps.add("historical-indexing-incomplete");
    }
    Ok(facts)
}
fn totals(facts: &[&Fact], gaps: &mut Gaps) -> SResult<UsageTotals> {
    let mut values = [0i64; 14];
    for f in facts {
        if let Some(b) = &f.balance {
            values[0] = plus(values[0], 1)?;
            values[1] = plus(values[1], b.priced_micro_usd.unwrap_or(0))?;
            if b.classification == "ReportedCharge" {
                values[2] = plus(values[2], b.priced_micro_usd.unwrap_or(0))?;
            } else if matches!(
                b.classification.as_str(),
                "OfficialEstimate" | "ConfiguredEstimate" | "SubscriptionApiEquivalent"
            ) {
                values[3] = plus(values[3], b.priced_micro_usd.unwrap_or(0))?;
            }
            values[4] = plus(values[4], b.reserved_micro_usd.unwrap_or(0))?;
            for (index, test) in [
                (5, b.classification == "LocalFree"),
                (6, b.classification == "Untracked"),
                (7, b.unknown_tail),
                (8, b.incomplete),
                (9, !b.monetary_complete),
            ] {
                values[index] = plus(values[index], i64::from(test))?;
            }
            if !b.monetary_complete {
                gaps.add("monetary-coverage-incomplete");
            }
            if b.incomplete {
                gaps.add("usage-observation-incomplete");
            }
            if b.unknown_tail {
                gaps.add("remote-outcome-unknown");
            }
        } else if f.record.as_ref().is_some_and(|r| r.attempt_id.is_some()) {
            values[0] = plus(values[0], 1)?;
            values[9] = plus(values[9], 1)?;
            values[8] = plus(values[8], 1)?;
            gaps.add("attempt-balance-unavailable");
        } else if let Some(o) = &f.legacy
            && matches!(o.source_kind.as_str(), "SessionUsage" | "JobUsage")
        {
            values[10] = plus(values[10], 1)?;
            values[11] = plus(values[11], i64::from(o.priced_micro_usd.is_none()))?;
            values[12] = plus(values[12], o.priced_micro_usd.unwrap_or(0))?;
            gaps.add("legacy-observations-not-incurred-spend");
        }
    }
    wire(json!({"attempts":values[0].to_string(),"trackedMicroUsd":values[1].to_string(),"reportedMicroUsd":values[2].to_string(),"estimatedMicroUsd":values[3].to_string(),"reservedMicroUsd":values[4].to_string(),"localFreeAttempts":values[5].to_string(),"untrackedAttempts":values[6].to_string(),"unknownTailAttempts":values[7].to_string(),"incompleteAttempts":values[8].to_string(),"monetaryIncompleteAttempts":values[9].to_string(),"legacyUsageFacts":values[10].to_string(),"legacyUntrackedFacts":values[11].to_string(),"legacyObservationMicroUsd":values[12].to_string()})).map_err(Into::into)
}
fn cursor_range(cursor: &str) -> Result<(i64, i64, &str), StorageError> {
    let mut parts = cursor.splitn(4, '.');
    if parts.next() != Some("usage1") {
        return Err(StorageError::CursorExpired);
    }
    let start = parts
        .next()
        .and_then(|v| v.parse::<i64>().ok())
        .ok_or(StorageError::CursorExpired)?;
    let end = parts
        .next()
        .and_then(|v| v.parse::<i64>().ok())
        .ok_or(StorageError::CursorExpired)?;
    let inner = parts.next().ok_or(StorageError::CursorExpired)?;
    Ok((start, end, inner))
}
fn page<T: Clone + Serialize>(
    ctx: &InvocationContext,
    i: &UsageInput,
    q: &Query,
    rows: Vec<T>,
) -> SResult<(Vec<T>, Option<Cursor>, bool)> {
    let context = serde_json::to_string(&q.context).map_err(|e| corrupt(&e.to_string()))?;
    let source = crate::fingerprint::sha256_hex(
        serde_json::to_string(&rows)
            .map_err(|e| corrupt(&e.to_string()))?
            .as_bytes(),
    );
    let binding = CursorBinding::new(
        ctx,
        "router.usage",
        &[
            &context,
            &source,
            &i.page.limit.get().to_string(),
            if q.legacy_all_dates {
                "legacy-all-dates"
            } else {
                "current-date-range"
            },
        ],
        q.context.read_revision.get(),
    );
    let start = i
        .page
        .cursor
        .as_ref()
        .map(|c| {
            binding
                .decode(cursor_range(&c.0)?.2)
                .and_then(|s| s.parse::<usize>().map_err(|_| StorageError::CursorExpired))
        })
        .transpose()?
        .unwrap_or(0);
    if start > rows.len() {
        return Err(StorageError::CursorExpired.into());
    }
    let mut end = start;
    let mut bytes = context
        .len()
        .checked_add(16 * 1024)
        .ok_or_else(|| invalid("usage_response_size"))?;
    if bytes >= crate::framing::MAX_FRAME_BYTES {
        return Err(invalid("usage_context_exceeds_response_bound"));
    }
    while end < rows.len() && end - start < (i.page.limit.get() as usize) {
        let size = serde_json::to_vec(&rows[end])
            .map_err(|e| corrupt(&e.to_string()))?
            .len()
            + 1;
        if bytes.saturating_add(size) > crate::framing::MAX_FRAME_BYTES {
            if end == start {
                return Err(invalid("usage_row_exceeds_response_bound"));
            }
            break;
        }
        bytes += size;
        end += 1;
    }
    let more = end < rows.len();
    Ok((
        rows[start..end].to_vec(),
        more.then(|| {
            BoundedText(format!(
                "usage1.{}.{}.{}",
                q.start,
                q.end,
                binding.encode(&end.to_string())
            ))
        }),
        more,
    ))
}
/// Query handler projection; the owner registers this through the ordinary authenticated gateway.
fn query_with_mode(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    input: &UsageInput,
    now_ms: i64,
    legacy_all_dates: bool,
) -> SResult<UsageQueryResult> {
    let resolved = input
        .page
        .cursor
        .as_ref()
        .map(|c| cursor_range(&c.0).map(|(start, end, _)| (start, end)))
        .transpose()?;
    let q = Query::new(read, ctx, input, now_ms, resolved, legacy_all_dates)?;
    let mut gaps = Gaps::default();
    let facts = collect(read, &q, &mut gaps)?;
    if !matches!(q.kind, UsageReportKind::McpUsage)
        && facts.iter().any(|f| {
            (q.in_range(f.time) || f.activity_time.is_some_and(|t| q.in_range(t)))
                && q.matches(f)
                && f.record
                    .as_ref()
                    .is_some_and(|r| r.resolved_model_id.is_none())
        })
    {
        gaps.add("actual-served-model-unrecorded");
    }
    if facts.iter().any(|f| {
        q.in_range(f.time)
            && q.matches(f)
            && f.record.as_ref().is_some_and(|r| {
                r.cache_diagnostics
                    .0
                    .iter()
                    .any(|d| d.kind.0 == "CacheMeasurementConflict")
            })
    }) {
        gaps.add("cache-profile-observation-conflict");
    }
    if facts.iter().any(|f| {
        (q.in_range(f.time) || f.activity_time.is_some_and(|t| q.in_range(t)))
            && q.uncertain_filter(f)
    }) {
        gaps.add("filter-identity-unrecorded");
    }
    let selected = facts
        .iter()
        .filter(|f| q.in_range(f.time) && q.matches(f))
        .collect::<Vec<_>>();
    match q.kind {
        UsageReportKind::Attempts => {
            let totals = totals(&selected, &mut gaps)?;
            let mut rows = selected
                .iter()
                .filter_map(|f| f.record.clone().map(|r| (&f.key, r)))
                .collect::<Vec<_>>();
            rows.sort_by(|a, b| a.0.cmp(b.0));
            let (items, next_cursor, has_more) =
                page(ctx, input, &q, rows.into_iter().map(|(_, r)| r).collect())?;
            Ok(UsageQueryResult::Attempts {
                page: UsageAttemptPage {
                    context: q.context,
                    items: BoundedList(items),
                    totals,
                    next_cursor,
                    has_more,
                    coverage: gaps.coverage(),
                },
            })
        }
        UsageReportKind::Filters => {
            let mut options = Vec::new();
            let activity_source = facts
                .iter()
                .filter(|f| f.path == Path::Assistant)
                .collect::<Vec<_>>();
            let activity = aggregate::activity(&activity_source, &q);
            let filter_facts = facts
                .iter()
                .filter(|f| q.in_range(f.time))
                .chain(activity.iter())
                .collect::<Vec<_>>();
            for dimension in ["user", "provider", "model", "tool"] {
                let mut values = filter_facts
                    .iter()
                    .filter(|f| dimension != "tool" || f.mcp)
                    .filter_map(|f| normalized(f.dimension(dimension)))
                    .collect::<Vec<_>>();
                values.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
                values.dedup();
                for value in values {
                    options.push(UsageFilterOption {
                        dimension: BoundedText(dimension.into()),
                        value: BoundedText(value),
                    });
                }
            }
            let (items, next_cursor, has_more) = page(ctx, input, &q, options)?;
            Ok(UsageQueryResult::Filters {
                filters: UsageFilterOptions {
                    context: q.context,
                    items: BoundedList(items),
                    next_cursor,
                    has_more,
                    coverage: gaps.coverage(),
                },
            })
        }
        _ => {
            let rows = aggregate::report(&q, &facts.iter().collect::<Vec<_>>(), &mut gaps)?;
            let (items, next_cursor, has_more) = page(ctx, input, &q, rows)?;
            Ok(UsageQueryResult::Report {
                report: UsageAggregatePage {
                    context: q.context,
                    items: BoundedList(items),
                    next_cursor,
                    has_more,
                    coverage: gaps.coverage(),
                },
            })
        }
    }
}

/// Current retained AdminReports query, including signed DateTime-compatible ranges.
pub fn query(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    input: &UsageInput,
    now_ms: i64,
) -> SResult<UsageQueryResult> {
    query_with_mode(read, ctx, input, now_ms, false)
}
/// The legacy native shape had no date range. This private service path selects every stored
/// timestamp (including i64::MAX), while sharing scope, meters, totals, cursor and byte bounds.
/// It forces Attempts, so no public calendar range or aggregate bucket walk is bypassed.
pub(crate) fn query_legacy_attempts(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    input: &UsageInput,
    now_ms: i64,
) -> SResult<UsageQueryResult> {
    query_with_mode(read, ctx, input, now_ms, true)
}
