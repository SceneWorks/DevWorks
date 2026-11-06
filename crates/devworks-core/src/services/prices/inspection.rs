//! A view-bound page spans models (including unpriced models), provider state, immutable
//! snapshot provenance and exact tariff dimensions. Provider IDs here identify PRICE SOURCES;
//! usage attribution continues to identify the actual billing transport.
use super::{PriceService, refresh};
use crate::context::InvocationContext;
use crate::operations::router::PriceStatusInput;
use crate::ports::storage::*;
use crate::ports::usage::PublishedTariff;
use crate::protocol::records::*;
use crate::protocol::support::{BoundedList, Check, Page};
use crate::protocol::{DomainErrorCode, Id};
use crate::services::cursor::CursorBinding;
use crate::services::sessions::{Failure, Refusal};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(crate) fn wire<T: DeserializeOwned + Check>(v: Value) -> Result<T, StorageError> {
    let value: T =
        serde_json::from_value(v).map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
    value.check().map_err(StorageError::InvalidRecord)?;
    Ok(value)
}
pub(crate) fn tariff(t: &PublishedTariff) -> Result<PriceTariff, StorageError> {
    wire(
        json!({"upstreamModelId":t.upstream_model_id,"serviceTier":t.service_tier,"contextLowerExclusive":t.context_lower_exclusive.map(|n|n.to_string()),"contextUpperInclusive":t.context_upper_inclusive.map(|n|n.to_string()),"modality":t.modality,"chargeKind":t.charge_kind,"cacheTtlSeconds":t.cache_ttl_seconds.map(|n|n.to_string()),"unit":t.unit,"units":t.units.to_string(),"amountUsd":t.amount_usd,"region":t.region,"endpointProfile":t.endpoint_profile}),
    )
}
pub(crate) fn published(t: &PriceTariff) -> Result<PublishedTariff, StorageError> {
    Ok(PublishedTariff {
        upstream_model_id: t.upstream_model_id.0.clone(),
        service_tier: t.service_tier.0.clone(),
        context_lower_exclusive: t.context_lower_exclusive.as_ref().map(|n| n.get()),
        context_upper_inclusive: t.context_upper_inclusive.as_ref().map(|n| n.get()),
        modality: serde_json::to_value(&t.modality)
            .expect("enum")
            .as_str()
            .expect("enum")
            .into(),
        charge_kind: serde_json::to_value(&t.charge_kind)
            .expect("enum")
            .as_str()
            .expect("enum")
            .into(),
        cache_ttl_seconds: t
            .cache_ttl_seconds
            .as_ref()
            .map(|n| n.0.parse::<i64>().expect("checked counter")),
        unit: serde_json::to_value(&t.unit)
            .expect("enum")
            .as_str()
            .expect("enum")
            .into(),
        units: t.units.0.parse::<i64>().expect("checked counter"),
        amount_usd: t.amount_usd.0.as_str().into(),
        region: t.region.as_ref().map(|s| s.0.clone()),
        endpoint_profile: t.endpoint_profile.as_ref().map(|s| s.0.clone()),
    })
}
pub(crate) fn legacy_tariffs(m: &StoredRouteModel, p: &PriceInput) -> Vec<PublishedTariff> {
    [
        ("Input", Some(&p.input_usd_per_million)),
        ("Output", Some(&p.output_usd_per_million)),
        ("CacheRead", p.cache_read_usd_per_million.as_ref()),
        ("CacheWrite", p.cache_write_usd_per_million.as_ref()),
    ]
    .into_iter()
    .filter_map(|(kind, amount)| {
        amount.map(|amount| PublishedTariff {
            upstream_model_id: m.upstream_model_id.clone(),
            service_tier: "Standard".into(),
            context_lower_exclusive: None,
            context_upper_inclusive: None,
            modality: "Text".into(),
            charge_kind: kind.into(),
            cache_ttl_seconds: None,
            unit: "Token".into(),
            units: 1_000_000,
            amount_usd: amount.0.as_str().into(),
            region: None,
            endpoint_profile: Some(m.endpoint_profile.clone()),
        })
    })
    .collect()
}
pub(crate) fn legacy_price(
    model: &StoredRouteModel,
    rows: &[PublishedTariff],
    revision: i64,
    origin: &str,
    at: i64,
    url: Option<&str>,
) -> Result<PriceRecord, Failure> {
    let unsupported = || {
        Refusal::new(
            DomainErrorCode::UnsupportedCapability,
            "legacy_tariff_dimensions",
            Some(model.id.clone()),
        )
    };
    if rows.iter().any(|r| {
        r.unit != "Token"
            || r.units != 1_000_000
            || r.service_tier != "Standard"
            || r.modality != "Text"
            || r.context_lower_exclusive.is_some()
            || r.context_upper_inclusive.is_some()
            || r.cache_ttl_seconds.is_some()
            || r.region.as_ref().is_some_and(|v| v != "global")
            || r.endpoint_profile
                .as_ref()
                .is_some_and(|v| v != &model.endpoint_profile)
            || !["Input", "Output", "CacheRead", "CacheWrite"].contains(&r.charge_kind.as_str())
    }) {
        return Err(unsupported().into());
    }
    let rate = |kind: &str| -> Result<Option<&str>, Failure> {
        let mut matching = rows.iter().filter(|r| r.charge_kind == kind);
        let first = matching.next().map(|r| r.amount_usd.as_str());
        if matching.next().is_some() {
            return Err(unsupported().into());
        }
        Ok(first)
    };
    let input = rate("Input")?.ok_or_else(unsupported)?;
    let output = rate("Output")?.ok_or_else(unsupported)?;
    Ok(wire(
        json!({"modelId":model.id,"revision":revision.to_string(),"inputUsdPerMillion":input,"outputUsdPerMillion":output,"cacheReadUsdPerMillion":rate("CacheRead")?,"cacheWriteUsdPerMillion":rate("CacheWrite")?,"origin":origin,"observedAtMs":at.to_string(),"sourceUrl":url}),
    )?)
}
fn connection(
    read: &dyn RouterRead,
    m: &StoredRouteModel,
) -> Result<Option<StoredConnection>, StorageError> {
    if let Some(id) = &m.connection_id {
        return read.connection(id);
    }
    let mut after: Option<String> = None;
    let mut found = None;
    loop {
        let rows = read.connections(after.as_deref(), 64)?;
        let n = rows.len();
        for c in rows {
            after = Some(c.id.as_str().into());
            if c.enabled && c.profile == m.endpoint_profile {
                if found.is_some() {
                    return Ok(None);
                }
                found = Some(c);
            }
        }
        if n < 64 {
            break;
        }
    }
    Ok(found)
}
fn source(m: &StoredRouteModel, c: Option<&StoredConnection>) -> String {
    if c.is_some_and(|c| c.local_free) {
        return c.expect("present").id.as_str().into();
    }
    c.and_then(|c| super::official_price_source(c, &m.upstream_model_id))
        .map(|(provider, _)| provider.id().to_owned())
        .unwrap_or_else(|| {
            c.map_or_else(
                || {
                    m.connection_id
                        .as_ref()
                        .map_or("unconfigured", Id::as_str)
                        .into()
                },
                |c| c.id.as_str().into(),
            )
        })
}
fn upstream(m: &StoredRouteModel) -> &str {
    if m.endpoint_profile == "OpenRouterChat" {
        m.upstream_model_id
            .split_once('/')
            .map_or(&m.upstream_model_id, |(_, v)| v)
    } else {
        &m.upstream_model_id
    }
}
struct ModelFacts {
    source: String,
    kind: &'static str,
    reason: Option<&'static str>,
    snapshot: Option<StoredPriceSnapshot>,
    override_: Option<StoredPriceOverride>,
    tariffs: Vec<PublishedTariff>,
}
fn facts(s: &dyn ReadSnapshot, m: &StoredRouteModel) -> Result<ModelFacts, StorageError> {
    let c = connection(s.router(), m)?;
    let source = source(m, c.as_ref());
    let override_ = s.usage().price_override(&m.id)?.filter(|v| v.mode == "Set");
    let snapshot = if override_.is_none() && refresh::official(&source).is_some() {
        s.usage().current_price_snapshot(&source)?
    } else {
        None
    };
    let tariffs = override_.as_ref().map_or_else(
        || {
            snapshot.as_ref().map_or_else(Vec::new, |v| {
                v.tariffs
                    .iter()
                    .filter(|r| r.upstream_model_id == upstream(m))
                    .cloned()
                    .collect()
            })
        },
        |v| v.tariffs.clone(),
    );
    let (kind, reason) = if c.as_ref().is_some_and(|c| c.local_free) {
        ("Free", None)
    } else if c.is_none() {
        ("Untracked", Some("connection_unavailable_or_ambiguous"))
    } else if tariffs.is_empty() {
        (
            "Untracked",
            Some(if m.endpoint_profile == "OpenRouterChat" {
                "awaiting_reported_or_configured_cost"
            } else {
                "price_not_configured_or_published"
            }),
        )
    } else {
        ("Estimated", None)
    };
    Ok(ModelFacts {
        source,
        kind,
        reason,
        snapshot,
        override_,
        tariffs,
    })
}
fn snapshot_value(s: &StoredPriceSnapshot) -> Value {
    json!({"id":s.id,"providerId":s.provider,"revision":s.revision.to_string(),"sourceUrl":s.source_url,"sourceDigest":s.source_digest,"parserVersion":s.parser_version,"fetchedAtMs":s.fetched_at_ms.to_string(),"verifiedAtMs":s.verified_at_ms.to_string(),"effectiveDate":s.effective_date})
}
fn provider_value(
    service: &PriceService,
    s: &dyn ReadSnapshot,
    provider: &str,
) -> Result<Value, StorageError> {
    let status = s.usage().price_status(provider)?;
    let now = service.clock.now_ms().get();
    Ok(
        json!({"providerId":provider,"revision":status.as_ref().map_or(1,|v|v.revision).to_string(),"snapshotId":status.as_ref().and_then(|v|v.current_snapshot_id.clone()),"stale":refresh::official(provider).is_some()&&status.as_ref().is_none_or(|v|v.last_success_ms.is_none_or(|at|at.saturating_add(refresh::WEEK_MS)<=now)),"lastCheckedAtMs":status.as_ref().and_then(|v|v.last_attempt_ms).map(|v|v.to_string()),"nextCheckAtMs":status.as_ref().and_then(|v|v.next_refresh_ms).map(|v|v.to_string()),"refreshOperationId":status.as_ref().and_then(|v|v.refresh_operation_id.clone()),"error":status.as_ref().and_then(|v|v.last_error.clone()).or_else(||(!service.source_available && refresh::official(provider).is_some()).then(||"official price source adapter unavailable".into()))}),
    )
}

struct Slice {
    skip: usize,
    limit: usize,
    seen: usize,
    more: bool,
    models: Vec<Value>,
    providers: Vec<Value>,
    snapshots: Vec<Value>,
    items: Vec<Value>,
    gaps: Vec<Id>,
    gaps_truncated: bool,
}
impl Slice {
    fn push(&mut self, kind: u8, value: Value) -> bool {
        let index = self.seen;
        self.seen += 1;
        if index < self.skip {
            return true;
        }
        if self.models.len() + self.providers.len() + self.snapshots.len() + self.items.len()
            >= self.limit
        {
            self.more = true;
            return false;
        }
        match kind {
            0 => self.models.push(value),
            1 => self.providers.push(value),
            2 => self.snapshots.push(value),
            _ => self.items.push(value),
        }
        true
    }
}
pub(crate) fn query(
    service: &PriceService,
    s: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    input: &PriceStatusInput,
    schema: u32,
) -> Result<PriceCatalogResult, Failure> {
    let revision = s.usage().price_revision()?;
    let model_filter: BTreeSet<_> = input
        .model_ids
        .0
        .iter()
        .map(|v| v.as_str().to_owned())
        .collect();
    let provider_filter: BTreeSet<_> = input
        .provider_ids
        .as_ref()
        .map_or_else(Vec::new, |v| {
            v.0.iter().map(|v| v.as_str().to_owned()).collect()
        })
        .into_iter()
        .collect();
    let snapshot_filter: BTreeSet<_> = input
        .snapshot_ids
        .as_ref()
        .map_or_else(Vec::new, |v| {
            v.0.iter().map(|v| v.as_str().to_owned()).collect()
        })
        .into_iter()
        .collect();
    let normalized = json!({"models":model_filter,"providers":provider_filter,"snapshots":snapshot_filter,"history":input.include_history.unwrap_or(false),"pageLimit":input.page.limit,"schema":schema,"viewSequence":s.stamp().committed_sequence});
    let serialized = normalized.to_string();
    let binding = CursorBinding::new(ctx, "router.price_status", &[&serialized], revision);
    let offset = input
        .page
        .cursor
        .as_ref()
        .map(|c| {
            binding
                .decode(&c.0)
                .and_then(|v| v.parse::<usize>().map_err(|_| StorageError::CursorExpired))
        })
        .transpose()?
        .unwrap_or(0);
    let mut page = Slice {
        skip: offset,
        limit: input.page.limit.get() as usize,
        seen: 0,
        more: false,
        models: vec![],
        providers: vec![],
        snapshots: vec![],
        items: vec![],
        gaps: vec![],
        gaps_truncated: false,
    };
    let mut relevant = BTreeSet::new();
    if model_filter.is_empty() {
        for provider in ["openai", "anthropic"] {
            if s.usage().price_status(provider)?.is_some() {
                relevant.insert(provider.to_owned());
            }
        }
    }
    for provider in &provider_filter {
        if refresh::official(provider).is_some() {
            relevant.insert(provider.clone());
        }
    }
    for id in &snapshot_filter {
        if let Some(row) = s
            .usage()
            .price_snapshot(&Id::new(id).map_err(|e| StorageError::InvalidRecord(e.into()))?)?
            && (provider_filter.is_empty() || provider_filter.contains(&row.provider))
        {
            relevant.insert(row.provider);
        }
    }
    let mut upstreams = BTreeSet::new();
    let mut legacy = 0usize;
    let mut legacy_unsupported = false;
    refresh::models(s.router(), |m| {
        if !model_filter.is_empty() && !model_filter.contains(m.id.as_str()) {
            return Ok(true);
        }
        let f = facts(s, &m)?;
        if !provider_filter.is_empty() && !provider_filter.iter().any(|id| id == &f.source) {
            return Ok(true);
        }
        if snapshot_filter.is_empty()
            && (!model_filter.is_empty() || refresh::official(&f.source).is_some())
        {
            relevant.insert(f.source.clone());
        }
        if !model_filter.is_empty() {
            upstreams.insert((f.source.clone(), upstream(&m).to_owned()));
        }
        if !snapshot_filter.is_empty()
            && f.snapshot
                .as_ref()
                .is_none_or(|v| !snapshot_filter.contains(v.id.as_str()))
        {
            return Ok(true);
        }
        if schema < 3 {
            let price = if f.kind == "Free" || f.tariffs.is_empty() {
                None
            } else {
                match legacy_price(
                    &m,
                    &f.tariffs,
                    s.usage().price_override_revision(&m.id)?,
                    if f.override_.is_some() {
                        "UserConfigured"
                    } else {
                        "OfficialSnapshot"
                    },
                    f.override_.as_ref().map_or_else(
                        || f.snapshot.as_ref().map_or(0, |v| v.verified_at_ms),
                        |v| v.updated_at_ms,
                    ),
                    f.snapshot.as_ref().map(|v| v.source_url.as_str()),
                ) {
                    Ok(price) => Some(price),
                    Err(Failure::Storage(error)) => return Err(error),
                    Err(Failure::Refused(_)) => {
                        legacy_unsupported = true;
                        return Ok(false);
                    }
                }
            };
            let status = s.usage().price_status(&f.source)?;
            let row = wire::<ModelPriceStatus>(
                json!({"modelId":m.id,"priceKind":f.kind,"price":price,"stale":f.override_.is_none()&&refresh::official(&f.source).is_some()&&status.as_ref().is_none_or(|v|v.last_success_ms.is_none_or(|at|at.saturating_add(refresh::WEEK_MS)<=service.clock.now_ms().get())),"nextCheckAtMs":status.and_then(|v|v.next_refresh_ms).map(|v|v.to_string())}),
            )?;
            if legacy >= page.skip && legacy < page.skip + page.limit + 1 {
                page.models
                    .push(serde_json::to_value(&row).expect("record"));
            }
            legacy += 1;
            return Ok(legacy <= page.skip + page.limit);
        }
        let view = json!({"modelId":m.id,"revision":s.usage().price_override_revision(&m.id)?.to_string(),"priceKind":f.kind,"snapshotId":f.snapshot.as_ref().map(|s|s.id.clone()),"overrideRevision":s.usage().price_override_revision(&m.id)?.to_string(),"untrackedReason":f.reason});
        if f.kind == "Untracked" {
            if page.gaps.len() < 128 {
                page.gaps.push(m.id.clone());
            } else {
                page.gaps_truncated = true;
            }
        }
        let keep = page.push(0, view);
        if !keep {
            return Ok(false);
        }
        if let Some(o) = f.override_ {
            for row in o.tariffs {
                if !page.push(3,json!({"providerId":f.source,"modelId":m.id,"upstreamModelId":m.upstream_model_id,"origin":"UserConfigured","overrideRevision":o.revision.to_string(),"tariff":tariff(&row)?})){return Ok(false);}
            }
        }
        Ok(true)
    })?;
    if legacy_unsupported {
        return Err(Refusal::new(
            DomainErrorCode::UnsupportedCapability,
            "legacy_tariff_dimensions",
            None,
        )
        .into());
    }
    if schema < 3 {
        let more = page.models.len() > page.limit;
        page.models.truncate(page.limit);
        return Ok(PriceCatalogResult::Legacy(Page {
            items: BoundedList(
                page.models
                    .into_iter()
                    .map(wire)
                    .collect::<Result<Vec<ModelPriceStatus>, _>>()?,
            ),
            next_cursor: more.then(|| {
                crate::protocol::support::BoundedText(
                    binding.encode(&(offset + page.limit).to_string()),
                )
            }),
            has_more: more,
        }));
    }
    if !page.more {
        let configured = refresh::configured(s.router())?;
        if model_filter.is_empty() && snapshot_filter.is_empty() {
            relevant.extend(configured.into_keys());
        }
        for provider in &relevant {
            if !provider_filter.is_empty() && !provider_filter.iter().any(|id| id == provider) {
                continue;
            }
            let row = provider_value(service, s, provider)?;
            if !page.push(1, row) {
                break;
            }
        }
    }
    if !page.more && model_filter.is_empty() && snapshot_filter.is_empty() {
        // Custom/local publishers stream from the configured connection keyset. Keeping them
        // out of `relevant` bounds query memory independently of the number of models/sources.
        let mut after = None;
        loop {
            let rows = s.router().connections(after.as_deref(), 64)?;
            let n = rows.len();
            for connection in rows {
                after = Some(connection.id.as_str().to_owned());
                if !connection.local_free && super::official_price_source(&connection, "").is_some()
                {
                    continue;
                }
                let provider = connection.id.as_str();
                if !provider_filter.is_empty() && !provider_filter.contains(provider) {
                    continue;
                }
                if !page.push(1, provider_value(service, s, provider)?) {
                    break;
                }
            }
            if page.more || n < 64 {
                break;
            }
        }
    }
    if !page.more {
        let mut after = None;
        loop {
            let rows = s.usage().price_snapshots(None, after.as_ref(), 64)?;
            let n = rows.len();
            for snapshot in rows {
                after = Some(snapshot.id.clone());
                if !relevant.contains(&snapshot.provider)
                    || (!provider_filter.is_empty()
                        && !provider_filter.iter().any(|id| id == &snapshot.provider))
                    || (!snapshot_filter.is_empty()
                        && !snapshot_filter.contains(snapshot.id.as_str()))
                {
                    continue;
                }
                if snapshot_filter.is_empty()
                    && !input.include_history.unwrap_or(false)
                    && s.usage()
                        .price_status(&snapshot.provider)?
                        .and_then(|v| v.current_snapshot_id)
                        .as_ref()
                        != Some(&snapshot.id)
                {
                    continue;
                }
                if !page.push(2, snapshot_value(&snapshot)) {
                    break;
                }
                for row in snapshot.tariffs {
                    if !model_filter.is_empty()
                        && !upstreams
                            .contains(&(snapshot.provider.clone(), row.upstream_model_id.clone()))
                    {
                        continue;
                    }
                    if !page.push(3,json!({"providerId":snapshot.provider,"upstreamModelId":row.upstream_model_id,"origin":"OfficialSnapshot","snapshotId":snapshot.id,"tariff":tariff(&row)?})){break;}
                }
                if page.more {
                    break;
                }
            }
            if page.more || n < 64 {
                break;
            }
        }
    }
    let next = page
        .more
        .then(|| binding.encode(&(offset + page.limit).to_string()));
    let partial = page.more || !page.gaps.is_empty();
    if page.more && page.gaps.len() < 128 {
        page.gaps
            .push(Id::new("additional-price-pages").expect("static"));
    }
    let value = wire(
        json!({"catalogRevision":revision.to_string(),"items":page.items,"snapshots":page.snapshots,"providers":page.providers,"models":page.models,"nextCursor":next,"hasMore":page.more,"coverage":{"state":if partial{"Partial"}else{"Complete"},"gaps":page.gaps,"moreGaps":page.more||page.gaps_truncated}}),
    )?;
    Ok(PriceCatalogResult::Current(value))
}
