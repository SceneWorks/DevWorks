//! Durable official-source refreshes. GET results are validated outside the write owner, then
//! the complete provider catalog is conditionally published under the captured refresh revision.
use super::PriceService;
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::ports::IdKind;
use crate::ports::storage::*;
use crate::ports::usage::*;
use crate::protocol::{DomainErrorCode, EventId, Id, OperationId};
use crate::services::sessions::{Failure, Refusal};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// Drains only admitted database units. No read, parser or external source fetch holds this
/// fence; after closure a late source result cannot publish or settle its journal.
#[derive(Default)]
pub struct RefreshWriteFence {
    stopping: std::sync::atomic::AtomicBool,
    units: std::sync::RwLock<()>,
}
impl RefreshWriteFence {
    pub fn stop(&self) {
        self.stopping
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    pub fn close(&self) {
        self.stop();
        drop(self.units.write().unwrap_or_else(|p| p.into_inner()));
    }
    pub fn write<T, F>(&self, store: &dyn ApplicationStore, work: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
    {
        let admitted = self.units.read().unwrap_or_else(|p| p.into_inner());
        if self.stopping.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StorageError::Cancelled);
        }
        let result = run_write(store, WriteOptions::default(), work);
        drop(admitted);
        result
    }
}

pub const CONSUMER: &str = "price_refresh";
pub const CAPABILITY: &str = "router.price_refresh";
pub const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const PAGE: u16 = 64;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderRequest {
    provider: String,
    status_revision: i64,
    binding: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshRequest {
    providers: Vec<ProviderRequest>,
}

pub(crate) fn conflict(kind: &'static str, id: &Id, expected: i64, current: i64) -> Failure {
    StorageError::conflict(
        crate::protocol::RevisionDimension::Configuration,
        kind,
        id.clone(),
        crate::services::sessions::rev(expected),
        crate::services::sessions::rev(current),
    )
    .into()
}
pub fn official(id: &str) -> Option<OfficialPriceProvider> {
    match id {
        "openai" => Some(OfficialPriceProvider::OpenAi),
        "anthropic" => Some(OfficialPriceProvider::Anthropic),
        _ => None,
    }
}

pub(crate) fn model(
    read: &dyn RouterRead,
    id: &Id,
) -> Result<Option<StoredRouteModel>, StorageError> {
    if let Some(m) = read.route_model(id)? {
        return Ok(Some(m));
    }
    let Some(m) = read.decision_model(id)? else {
        return read
            .embedding_price_subject(id)?
            .map(|subject| embedding_model(read, subject))
            .transpose();
    };
    let profile = read
        .connection(&m.connection_id)?
        .map_or_else(|| "SystemOne".into(), |c| c.profile);
    Ok(Some(StoredRouteModel {
        id: m.id,
        display_name: m.display_name,
        endpoint_profile: profile,
        upstream_model_id: m.upstream_model_id,
        enabled: m.enabled,
        efforts: vec![],
        image_input: m.accepts_images,
        tools: false,
        context_tokens: None,
        connection_id: Some(m.connection_id),
        position: 0,
        revision: m.revision,
        updated_at_ms: m.updated_at_ms,
    }))
}
// A billing subject is an internal price projection, never a conversational catalog row.
fn embedding_model(
    read: &dyn RouterRead,
    subject: StoredEmbeddingPriceSubject,
) -> Result<StoredRouteModel, StorageError> {
    let connection = read.connection(&subject.connection_id)?;
    Ok(StoredRouteModel {
        id: subject.model_id,
        display_name: subject.upstream_model_id.clone(),
        endpoint_profile: "Embedding".into(),
        upstream_model_id: subject.upstream_model_id,
        enabled: subject.current_candidate_id.is_some()
            && connection.as_ref().is_some_and(|c| c.enabled),
        efforts: vec![],
        image_input: false,
        tools: false,
        context_tokens: None,
        connection_id: Some(subject.connection_id),
        position: 0,
        revision: connection
            .as_ref()
            .map_or(subject.created_connection_revision, |c| c.revision),
        updated_at_ms: connection.map_or(subject.created_at_ms, |c| c.updated_at_ms),
    })
}
pub(crate) fn models(
    read: &dyn RouterRead,
    mut visit: impl FnMut(StoredRouteModel) -> Result<bool, StorageError>,
) -> Result<(), StorageError> {
    let mut after = None;
    loop {
        let rows = read.route_models(
            after
                .as_ref()
                .map(|(p, id): &(i64, String)| (*p, id.as_str())),
            PAGE,
        )?;
        if rows.is_empty() {
            break;
        }
        let n = rows.len();
        for row in rows {
            after = Some((row.position, row.id.as_str().into()));
            if !visit(row)? {
                return Ok(());
            }
        }
        if n < usize::from(PAGE) {
            break;
        }
    }
    let mut after = None;
    loop {
        let rows = read.decision_models(after.as_deref(), PAGE)?;
        if rows.is_empty() {
            break;
        }
        let n = rows.len();
        for row in rows {
            after = Some(row.id.as_str().to_owned());
            if let Some(m) = model(read, &row.id)?
                && !visit(m)?
            {
                return Ok(());
            }
        }
        if n < usize::from(PAGE) {
            break;
        }
    }
    let mut after = None;
    loop {
        let rows = read.embedding_price_subjects(after.as_ref(), PAGE)?;
        let n = rows.len();
        for subject in rows {
            after = Some(subject.model_id.clone());
            if !visit(embedding_model(read, subject)?)? {
                return Ok(());
            }
        }
        if n < usize::from(PAGE) {
            break;
        }
    }
    Ok(())
}
/// Official publishers required by currently enabled, explicit connections. Connection
/// revisions enter the binding; no credential or private endpoint content is retained.
pub fn configured(read: &dyn RouterRead) -> Result<BTreeMap<String, String>, StorageError> {
    let mut hashes: BTreeMap<String, Sha256> = BTreeMap::new();
    let mut after = None;
    loop {
        let rows = read.connections(after.as_deref(), PAGE)?;
        if rows.is_empty() {
            break;
        }
        let n = rows.len();
        for connection in rows {
            after = Some(connection.id.as_str().to_owned());
            if !connection.enabled || connection.local_free {
                continue;
            }
            let mut sources = BTreeSet::new();
            if let Some((p, _)) = super::official_price_source(&connection, "") {
                sources.insert(p.id());
            }
            if connection.profile == "OpenRouterChat" {
                models(read, |m| {
                    if m.enabled
                        && m.connection_id
                            .as_ref()
                            .is_none_or(|id| id == &connection.id)
                        && m.endpoint_profile == connection.profile
                        && let Some((p, _)) =
                            super::official_price_source(&connection, &m.upstream_model_id)
                    {
                        sources.insert(p.id());
                    }
                    Ok(true)
                })?;
            }
            for source in sources {
                let h = hashes.entry(source.into()).or_default();
                h.update(connection.id.as_str().as_bytes());
                h.update(connection.revision.to_be_bytes());
            }
        }
        if n < usize::from(PAGE) {
            break;
        }
    }
    Ok(hashes
        .into_iter()
        .map(|(p, h)| (p, h.finalize().iter().map(|b| format!("{b:02x}")).collect()))
        .collect())
}
pub fn validate_tariffs(rows: &[PublishedTariff]) -> Result<(), StorageError> {
    if rows.len() > 20_000 {
        return Err(StorageError::InvalidRecord(
            "price tariff population exceeds source bound".into(),
        ));
    }
    let mut keys = BTreeSet::new();
    for row in rows {
        super::inspection::tariff(row)?;
        let mut key = row.clone();
        key.amount_usd = "0".into();
        if !keys.insert(
            serde_json::to_string(&key).map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
        ) {
            return Err(StorageError::InvalidRecord(
                "duplicate tariff dimensions".into(),
            ));
        }
    }
    Ok(())
}
pub(crate) fn request(
    tx: &mut dyn WriteTransaction,
    ctx: Option<&InvocationContext>,
    expected: i64,
    providers: &[Id],
) -> Result<OperationId, Failure> {
    let current = tx.usage().price_revision()?;
    if current != expected {
        return Err(conflict(
            "price_catalog",
            &Id::new("price-catalog").expect("static"),
            expected,
            current,
        ));
    }
    let mut providers: Vec<_> = providers.iter().map(|p| p.as_str().to_owned()).collect();
    providers.sort();
    providers.dedup();
    let configured = configured(tx.router())?;
    let mut requested = Vec::new();
    for provider in providers {
        if official(&provider).is_none() {
            return Err(Refusal::new(
                DomainErrorCode::UnsupportedCapability,
                "official_price_provider",
                Id::new(provider).ok(),
            )
            .into());
        }
        let binding = configured
            .get(&provider)
            .cloned()
            .unwrap_or_else(|| sha256_hex(&[]));
        let status = tx.usage().price_status(&provider)?;
        if status.as_ref().is_some_and(|s| s.refresh_pending) {
            return Err(Refusal::new(
                DomainErrorCode::Conflict,
                "price_refresh_pending",
                Id::new(provider).ok(),
            )
            .into());
        }
        requested.push(ProviderRequest {
            provider,
            status_revision: status.map_or(1, |s| s.revision + 1),
            binding,
        });
    }
    if requested.is_empty() {
        return Err(Refusal::new(
            DomainErrorCode::UnsupportedCapability,
            "empty_price_refresh",
            None,
        )
        .into());
    }
    let operation = OperationId(tx.new_id(IdKind::Operation));
    let payload = serde_json::to_value(RefreshRequest {
        providers: requested.clone(),
    })
    .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
    tx.operations().record_intent(NewOperation {
        id: operation.clone(),
        root_id: None,
        assignment_id: None,
        capability: CAPABILITY.into(),
        schema_version: 1,
        args_digest: sha256_hex(payload.to_string().as_bytes()),
        payload,
    })?;
    let now = tx.now_ms();
    for p in requested {
        let old = tx.usage().price_status(&p.provider)?;
        let row = StoredPriceStatus {
            provider: p.provider,
            revision: p.status_revision,
            current_snapshot_id: old.as_ref().and_then(|s| s.current_snapshot_id.clone()),
            last_attempt_ms: Some(now),
            last_success_ms: old.as_ref().and_then(|s| s.last_success_ms),
            refresh_pending: true,
            refresh_operation_id: Some(operation.clone()),
            last_error: None,
            next_refresh_ms: Some(now.saturating_add(WEEK_MS)),
        };
        tx.usage().put_price_status(&row, old.map(|s| s.revision))?;
    }
    let actor = ctx.map_or_else(|| tx.store().host_actor.clone(), |c| c.principal().clone());
    let event = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id: event.clone(),
        actor,
        project_id: None,
        root_id: None,
        operation_id: Some(operation.clone()),
        aggregate_kind: "price_refresh".into(),
        aggregate_id: operation.0.as_str().into(),
        aggregate_revision: 1,
        kind: "PriceRefreshRequested".into(),
        payload: json!({"operationId":operation}),
    })?;
    let id = tx.new_id(IdKind::QueueEntry);
    tx.queue().enqueue(NewQueueEntry {
        id,
        consumer: CONSUMER.into(),
        event_id: event,
        operation_id: Some(operation.clone()),
        root_id: None,
        partition: None,
        priority: 0,
        ready_at_ms: now,
        payload: json!({"operationId":operation}),
    })?;
    Ok(operation)
}
impl PriceService {
    /// At most one due refresh per configured provider, anchored to now rather than replaying
    /// missed weeks. Existing pending operations retain their identity after restart.
    pub fn schedule_due(&self, tx: &mut dyn WriteTransaction) -> Result<usize, StorageError> {
        if !self.source_available {
            return Ok(0);
        }
        let sources = configured(tx.router())?;
        let now = tx.now_ms();
        let mut due = Vec::new();
        for provider in sources.keys() {
            let row = tx.usage().price_status(provider)?;
            if row
                .is_none_or(|s| !s.refresh_pending && s.next_refresh_ms.is_none_or(|at| at <= now))
            {
                due.push(Id::new(provider).map_err(|e| StorageError::InvalidRecord(e.into()))?);
            }
        }
        if due.is_empty() {
            return Ok(0);
        }
        let expected = tx.usage().price_revision()?;
        let count = due.len();
        request(tx, None, expected, &due).map_err(|e| match e {
            Failure::Storage(e) => e,
            Failure::Refused(e) => {
                StorageError::InvalidRecord(format!("price schedule refused: {:?}", e.code))
            }
        })?;
        Ok(count)
    }
}

#[derive(Debug)]
pub enum RefreshOutcome {
    Completed(Value),
    Failed(String),
    NotApplied(String),
    Unknown(String),
}
impl PriceService {
    /// Safe reconciliation repeats only credential-free GETs. Publication itself is atomic and
    /// fenced by provider revision and operation identity, so a lost journal ACK cannot publish
    /// another generation or alter historical quotes.
    pub fn execute_refresh(
        &self,
        store: &dyn ApplicationStore,
        source: &dyn PriceSource,
        operation: &OperationRecord,
        stop: &dyn Fn() -> bool,
        writes: &RefreshWriteFence,
    ) -> RefreshOutcome {
        if operation.capability != CAPABILITY
            || operation.schema_version != 1
            || sha256_hex(operation.payload.to_string().as_bytes()) != operation.args_digest
        {
            return RefreshOutcome::Failed("price refresh journal binding changed".into());
        }
        let request: RefreshRequest = match serde_json::from_value(operation.payload.clone()) {
            Ok(v) => v,
            Err(_) => return RefreshOutcome::Failed("invalid saved price refresh".into()),
        };
        let unique: BTreeSet<_> = request
            .providers
            .iter()
            .map(|p| p.provider.as_str())
            .collect();
        if request.providers.is_empty()
            || request.providers.len() > 2
            || unique.len() != request.providers.len()
            || request.providers.iter().any(|p| {
                official(&p.provider).is_none() || p.status_revision < 1 || p.binding.len() != 64
            })
        {
            return RefreshOutcome::Failed("invalid price provider refresh set".into());
        }
        let Some(attempt) = operation.latest_attempt() else {
            return RefreshOutcome::Failed("price refresh has no journaled dispatch".into());
        };
        let attempt_no = attempt.attempt_no;
        let mut failures = Vec::new();
        for p in request.providers {
            if stop() {
                return RefreshOutcome::Unknown("price refresh stopped before publication".into());
            }
            let (current, proof) = match read_snapshot(store, &ReadOptions::default(), |s| {
                let journal = s.operations().operation(&operation.id)?.ok_or_else(|| {
                    StorageError::InvalidRecord("price journal disappeared".into())
                })?;
                let proof = journal
                    .attempts
                    .iter()
                    .flat_map(|a| a.observations.iter())
                    .find(|v| {
                        v["kind"] == "PriceProviderPublication" && v["provider"] == p.provider
                    })
                    .cloned();
                Ok((s.usage().price_status(&p.provider)?, proof))
            }) {
                Ok(value) => value,
                Err(e) => return RefreshOutcome::Unknown(format!("{e:?}")),
            };
            if let Some(proof) = proof {
                if proof["outcome"] == "Failed" {
                    failures.push(
                        proof["reason"]
                            .as_str()
                            .unwrap_or("price publication failed")
                            .to_owned(),
                    );
                }
                continue;
            }
            let Some(current) = current else {
                return RefreshOutcome::Failed("price refresh status missing".into());
            };
            if current.refresh_operation_id.as_ref() != Some(&operation.id) {
                return RefreshOutcome::Failed("price refresh superseded".into());
            }
            if !current.refresh_pending {
                if let Some(e) = current.last_error {
                    failures.push(e);
                }
                continue;
            }
            if current.revision != p.status_revision {
                return RefreshOutcome::Failed("price refresh revision changed".into());
            }
            let Some(provider) = official(&p.provider) else {
                return RefreshOutcome::Failed("unknown official price provider".into());
            };
            let now = self.clock.now_ms().get();
            let fetched = source.fetch(provider, now).and_then(|r| {
                if r.provider != provider
                    || r.source_url != provider.source_url()
                    || r.tariffs.is_empty()
                    || r.fetched_at_ms < 0
                    || r.verified_at_ms < r.fetched_at_ms
                    || r.parser_version.is_empty()
                    || r.parser_version.len() > 128
                    || !crate::fingerprint::is_sha256_hex(&r.source_digest)
                    || r.effective_date.as_ref().is_some_and(|v| v.len() > 128)
                    || r.verified_at_ms > now
                {
                    return Err(PriceFetchError::SourceChanged(
                        "official catalog identity/timestamp changed".into(),
                    ));
                }
                validate_tariffs(&r.tariffs)
                    .map_err(|e| PriceFetchError::InvalidRate(format!("{e:?}")))?;
                Ok(r)
            });
            if stop() {
                return RefreshOutcome::Unknown("price refresh stopped before publication".into());
            }
            let operation_id = operation.id.clone();
            let requested = p.clone();
            let result = writes.write(store, move |tx| {
                let old = tx
                    .usage()
                    .price_status(&requested.provider)?
                    .ok_or_else(|| {
                        StorageError::InvalidRecord("price status disappeared".into())
                    })?;
                if old.revision != requested.status_revision
                    || !old.refresh_pending
                    || old.refresh_operation_id.as_ref() != Some(&operation_id)
                {
                    return Err(StorageError::Busy);
                }
                let binding = configured(tx.router())?
                    .remove(&requested.provider)
                    .unwrap_or_else(|| sha256_hex(&[]));
                let fetched = if binding != requested.binding {
                    Err(PriceFetchError::SourceChanged(
                        "configured provider binding changed".into(),
                    ))
                } else {
                    fetched
                };
                let provider_key = requested.provider.clone();
                match fetched {
                    Ok(fetched) => {
                        let revision = tx
                            .usage()
                            .current_price_snapshot(&requested.provider)?
                            .map_or(1, |s| s.revision + 1);
                        let row = StoredPriceSnapshot {
                            id: tx.new_id(IdKind::View),
                            provider: requested.provider,
                            revision,
                            source_url: fetched.source_url,
                            source_digest: fetched.source_digest,
                            parser_version: fetched.parser_version,
                            fetched_at_ms: fetched.fetched_at_ms,
                            verified_at_ms: fetched.verified_at_ms,
                            effective_date: fetched.effective_date,
                            tariffs: fetched.tariffs,
                        };
                        let mut status = tx.usage().publish_price_snapshot(&row, old.revision)?;
                        if status.last_attempt_ms != Some(now)
                            || status.next_refresh_ms != Some(now.saturating_add(WEEK_MS))
                        {
                            let before = status.revision;
                            status.revision += 1;
                            status.last_attempt_ms = Some(now);
                            status.next_refresh_ms = Some(now.saturating_add(WEEK_MS));
                            tx.usage().put_price_status(&status, Some(before))?;
                        }
                        tx.operations().observe(&operation_id,attempt_no,json!({"kind":"PriceProviderPublication","provider":provider_key,"outcome":"Completed","snapshotId":row.id,"snapshotRevision":row.revision.to_string()}))?;
                        Ok(None)
                    }
                    Err(error) => {
                        let reason = format!("{error:?}");
                        let reason: String = reason.chars().take(1024).collect();
                        let mut row = old.clone();
                        row.revision += 1;
                        row.refresh_pending = false;
                        row.last_error = Some(reason.clone());
                        row.last_attempt_ms = Some(now);
                        row.next_refresh_ms = Some(now.saturating_add(WEEK_MS));
                        tx.usage().put_price_status(&row, Some(old.revision))?;
                        tx.operations().observe(&operation_id,attempt_no,json!({"kind":"PriceProviderPublication","provider":provider_key,"outcome":"Failed","reason":reason}))?;
                        Ok(Some(reason))
                    }
                }
            });
            match result {
                Ok(Some(e)) => failures.push(e),
                Ok(None) => {}
                Err(e) => return RefreshOutcome::Unknown(format!("price publication: {e:?}")),
            }
        }
        if failures.is_empty() {
            RefreshOutcome::Completed(json!({"kind":"PriceRefreshComplete"}))
        } else {
            RefreshOutcome::Failed(failures.join("; "))
        }
    }
}

impl PriceService {
    /// Only this maintenance capability is safe to reconcile automatically: its external action
    /// is a credential-free GET, and snapshot publication is fenced. Other effect consumers keep
    /// their existing operator reconciliation rules.
    pub fn recover_refreshes(
        &self,
        store: &dyn ApplicationStore,
        source: &dyn PriceSource,
        stop: &dyn Fn() -> bool,
        writes: &RefreshWriteFence,
    ) -> Result<usize, StorageError> {
        let mut seen = BTreeSet::new();
        let mut resolved = 0;
        for provider in ["openai", "anthropic"] {
            if stop() {
                break;
            }
            let operation = read_snapshot(store, &ReadOptions::default(), |s| {
                let Some(id) = s
                    .usage()
                    .price_status(provider)?
                    .and_then(|v| v.refresh_operation_id)
                else {
                    return Ok(None);
                };
                s.operations().operation(&id)
            })?;
            let Some(operation) = operation else {
                continue;
            };
            if operation.capability != CAPABILITY
                || !seen.insert(operation.id.0.as_str().to_owned())
            {
                continue;
            }
            if is_terminal(operation.state) {
                resolved += writes.write(store, move |tx| {
                    let mut changed = 0;
                    for source in ["openai", "anthropic"] {
                        if let Some(mut row) = tx.usage().price_status(source)?
                            && row.refresh_pending
                            && row.refresh_operation_id.as_ref() == Some(&operation.id)
                        {
                            let before = row.revision;
                            row.revision += 1;
                            row.refresh_pending = false;
                            row.last_error = Some(format!(
                                "refresh ended before publication: {:?}",
                                operation.state
                            ));
                            tx.usage().put_price_status(&row, Some(before))?;
                            changed += 1;
                        }
                    }
                    Ok(changed)
                })?;
                continue;
            }
            if operation.state != crate::protocol::OperationState::OutcomeUnknown {
                continue;
            }
            let outcome = self.execute_refresh(store, source, &operation, stop, writes);
            if stop() {
                break;
            }
            let (state, note) = match outcome {
                RefreshOutcome::Completed(value) => (
                    crate::protocol::OperationState::ObservedComplete,
                    json!({"outcome":"completed","evidence":value}),
                ),
                RefreshOutcome::Failed(reason) => (
                    crate::protocol::OperationState::Failed,
                    json!({"outcome":"failed","cause":reason}),
                ),
                RefreshOutcome::NotApplied(_) | RefreshOutcome::Unknown(_) => continue,
            };
            let changed = writes.write(store, move |tx| {
                let Some(current) = tx.operations().operation(&operation.id)? else {
                    return Ok(false);
                };
                if current.revision != operation.revision
                    || current.state != crate::protocol::OperationState::OutcomeUnknown
                {
                    return Ok(false);
                }
                tx.operations().transition(
                    &current.id,
                    current.revision,
                    None,
                    state,
                    Some(json!({"reconciliation":note})),
                )?;
                let filter = DeliveryFilter {
                    consumer: Some(CONSUMER.into()),
                    operation_id: Some(current.id),
                };
                let mut after = None;
                loop {
                    let page = tx.queue().deliveries(&filter, after.as_ref(), PAGE)?;
                    for entry in page.entries {
                        if entry.state == QueueState::Reconciling {
                            tx.queue().requeue(&entry.id, 0)?;
                        }
                    }
                    match page.next {
                        Some(next) => after = Some(next),
                        None => break,
                    }
                }
                Ok(true)
            })?;
            resolved += usize::from(changed);
        }
        Ok(resolved)
    }
}
