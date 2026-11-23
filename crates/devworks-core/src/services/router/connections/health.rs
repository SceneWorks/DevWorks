//! Connection tests, model discovery and the host tick.
use super::*;

/// What a connection test was sent with: the credential revision, profile and endpoint of the
/// one read its request was built from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CheckBasis {
    connection_revision: i64,
    credential_revision: i64,
    profile: String,
    endpoint: String,
}

impl CheckBasis {
    fn of(c: &StoredConnection) -> Self {
        Self {
            connection_revision: c.revision,
            credential_revision: c.credential_revision,
            profile: c.profile.clone(),
            endpoint: c.endpoint.clone(),
        }
    }
    pub(super) fn from_access(a: &Access, connection_revision: i64) -> Self {
        Self {
            connection_revision,
            credential_revision: a.credential_revision,
            profile: a.profile.clone(),
            endpoint: a.endpoint.clone(),
        }
    }
    pub(super) fn matches(&self, c: &StoredConnection) -> bool {
        self.connection_revision == c.revision
            && self.credential_revision == c.credential_revision
            && self.profile == c.profile
            && self.endpoint == c.endpoint
    }
}

/// A connection test's outcome, labelled with what it was sent with.
pub(super) struct Checked {
    basis: CheckBasis,
    outcome: Result<Discovery, ListFailure>,
}

/// Ends the cooldowns of the catalog models `c` may serve (a credential or endpoint change).
pub(super) fn clear_profile_cooldowns(
    tx: &mut dyn WriteTransaction,
    c: &StoredConnection,
) -> Result<(), StorageError> {
    if c.profile == "SystemOne" {
        for model in super::all_decision_models(tx.router())?
            .into_iter()
            .filter(|model| model.connection_id == c.id)
        {
            tx.router().clear_cooldown(&model.id)?;
        }
        return Ok(());
    }
    let models = all_models(tx.router())?;
    for m in models.iter().filter(|m| {
        m.endpoint_profile == c.profile && m.connection_id.as_ref().is_none_or(|b| b == &c.id)
    }) {
        tx.router().clear_cooldown(&m.id)?;
    }
    Ok(())
}

/// Records a connection test against the credential revision it ran with; a check whose
/// credential changed meanwhile records nothing. Returns whether it was recorded.
pub(super) fn apply_check(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    id: &Id,
    checked: &Checked,
) -> Result<bool, StorageError> {
    let Some(c) = tx.router().connection(id)? else {
        return Ok(false);
    };
    // Recorded only for the exact credential and destination the request was sent with.
    if !checked.basis.matches(&c) {
        return Ok(false);
    }
    let outcome = &checked.outcome;
    let now = tx.now_ms();
    let mut next = c.clone();
    next.checked_at_ms = Some(now);
    match outcome {
        Ok(d) => {
            // The connection's constraints apply to discovery: an excluded model is not
            // listed as offered.
            let filter = status_filter(&c);
            let mut models: Vec<DiscoveredModel> = d
                .models
                .iter()
                .filter(|m| filter.permits(&m.id))
                .cloned()
                .collect();
            models.sort_by(|a, b| a.id.cmp(&b.id));
            models.dedup_by(|a, b| a.id == b.id);
            next.discovered = Some(models);
            next.discovered_at_ms = Some(now);
            next.healthy = Some(true);
            next.state_reason = None;
            if next.credential_ref.is_some()
                || profile(&c.profile).is_some_and(|p| !p.needs_credential())
            {
                next.lifecycle = "Connected".into();
            }
        }
        Err(ListFailure::Unauthorized(status)) => {
            // Refused authorization makes the connection unusable for routing whether or not a
            // key is stored: a keyless endpoint that now demands one needs its key.
            next.healthy = Some(false);
            next.lifecycle = "ReauthenticationRequired".into();
            next.state_reason = Some(if next.credential_ref.is_some() {
                ListFailure::Unauthorized(*status).reason()
            } else {
                format!(
                    "the endpoint refused a request without a key (HTTP {status}); add its API key"
                )
            });
        }
        Err(e @ ListFailure::Malformed(_))
            if next.profile == "SystemOne" && next.discovered.is_some() =>
        {
            // A response arrived but was insufficient to replace the last complete inventory.
            next.state_reason = Some(e.reason());
        }
        Err(e) => {
            // An availability event, not a revocation: the last good list is kept.
            next.healthy = Some(false);
            next.state_reason = Some(e.reason());
            if next.lifecycle == "Connected" {
                next.lifecycle = "Unavailable".into();
            }
        }
    }
    next.updated_at_ms = now;
    let saved = tx.router().put_connection(&next, Some(c.revision))?;
    if let Ok(d) = outcome {
        apply_discovery(tx, &saved, d)?;
    }
    event(
        tx,
        actor,
        (CONNECTION_AGGREGATE, &saved.id, saved.revision),
        "ConnectionChecked",
        json!({
            "connectionId": saved.id.as_str(),
            "healthy": saved.healthy,
            "lifecycle": saved.lifecycle,
            "discoveredModelCount": saved.discovered.as_ref().map(Vec::len),
        }),
    )?;
    Ok(true)
}

/// Canonical discovery ID for one exact upstream model served by one connection. Snapshot
/// suffixes are identity, never stripped. The digest input avoids delimiter collisions.
/// Discovery uses a stable free suffix if user configuration has repurposed this ID.
pub fn discovered_model_id(profile: &str, connection: &Id, upstream: &str) -> Id {
    let identity =
        serde_json::to_vec(&(profile, connection.as_str(), upstream)).expect("strings serialize");
    let digest = crate::fingerprint::sha256_hex(&identity);
    Id::new(format!("drmodel_{}", &digest[..40])).expect("bounded digest identifier")
}

/// Applies provider-reported facts to the catalog models a connection serves: an unknown
/// context window from the model list; for OpenRouter, the reported tools and image support
/// and, when none is declared, the supported efforts (reference `UpdateOpenRouterModelsAsync`).
pub(super) fn apply_discovery(
    tx: &mut dyn WriteTransaction,
    c: &StoredConnection,
    d: &Discovery,
) -> Result<(), StorageError> {
    let listed: BTreeSet<String> = d.models.iter().map(|m| m.id.clone()).collect();
    let mut models = all_models(tx.router())?;
    // Discovery adds exact models to the same catalog. A configured global or bound row,
    // including a user-disabled row, already owns that identity and must never be bypassed.
    if !matches!(c.profile.as_str(), "SystemOne" | "Embedding") {
        let mut position = models.iter().map(|m| m.position).max().unwrap_or(0);
        let permitted = status_filter(c);
        for found in d.models.iter().filter(|m| {
            permitted.permits(&m.id)
                && (matches!(
                    c.profile.as_str(),
                    "CodexSubscriptionResponses"
                        | "ClaudeSubscriptionMessages"
                        | "AnthropicMessages"
                ) || crate::services::router::latest::parse(&m.id).is_some()
                    || d.conversational.contains(&m.id))
        }) {
            if (c.profile == select::OPEN_ROUTER_PROFILE
                && crate::ports::router::is_open_router_auto(&found.id))
                || models.iter().any(|m| {
                    m.endpoint_profile == c.profile
                        && m.upstream_model_id == found.id
                        && m.connection_id.as_ref().is_none_or(|id| id == &c.id)
                })
            {
                continue;
            }
            // A discovery row remains editable. Its original generated ID may now belong to
            // another upstream or connection; retain it and reserve the next free stable ID.
            // A disabled edited row keeps the replacement disabled, never bypassing that choice.
            let canonical_id = discovered_model_id(&c.profile, &c.id, &found.id);
            let mut id = canonical_id.clone();
            let mut enabled = true;
            let mut suffix = 0_u64;
            loop {
                if let Some(occupied) = models.iter().find(|m| m.id == id) {
                    enabled &= occupied.enabled;
                } else if tx.router().decision_model(&id)?.is_none() {
                    break;
                }
                suffix += 1;
                id = Id::new(format!("{}_{}", canonical_id.as_str(), suffix))
                    .expect("bounded digest and numeric suffix");
            }
            let reported = d
                .reported
                .iter()
                .find(|(id, _)| id == &found.id)
                .map(|(_, r)| r);
            position = position.saturating_add(1);
            let model = StoredRouteModel {
                id,
                display_name: label(&found.id, "model").0,
                endpoint_profile: c.profile.clone(),
                upstream_model_id: found.id.clone(),
                enabled,
                efforts: reported.map(|r| r.efforts.clone()).unwrap_or_default(),
                image_input: reported.and_then(|r| r.vision).unwrap_or(false),
                tools: reported.and_then(|r| r.tools).unwrap_or(false),
                context_tokens: found.context_tokens,
                connection_id: Some(c.id.clone()),
                position,
                revision: 1,
                updated_at_ms: tx.now_ms(),
            };
            models.push(tx.router().put_route_model(&model, None)?);
        }
    }
    for m in models.iter().filter(|m| {
        m.endpoint_profile == c.profile && m.connection_id.as_ref().is_none_or(|b| b == &c.id)
    }) {
        if (c.profile == select::OPEN_ROUTER_PROFILE
            && crate::ports::router::is_open_router_auto(&m.upstream_model_id))
            || !select::lists(&c.profile, &listed, &m.upstream_model_id)
        {
            continue;
        }
        let found = d.models.iter().find(|x| x.id == m.upstream_model_id);
        let mut next = m.clone();
        if next.context_tokens.is_none() {
            next.context_tokens = found.and_then(|x| x.context_tokens);
        }
        if let Some((_, r)) = d.reported.iter().find(|(id, _)| id == &m.upstream_model_id) {
            if let Some(t) = r.tools {
                next.tools = t;
            }
            if let Some(v) = r.vision {
                next.image_input = v;
            }
            if next.efforts.is_empty() && !r.efforts.is_empty() {
                next.efforts = r.efforts.clone();
            }
        }
        if next != *m {
            next.updated_at_ms = tx.now_ms();
            tx.router().put_route_model(&next, Some(m.revision))?;
        }
    }
    Ok(())
}

impl ConnectionService {
    /// Tests `c` with an authenticated model list (refreshing an OAuth token when needed).
    /// Every request is built from one [`Access`] snapshot: its profile, endpoint and secret
    /// come from the same read, never the caller's `c`. The result is labelled with the
    /// [`CheckBasis`] of the credential actually sent (including a refresh or the 401 retry),
    /// so [`apply_check`] records it only for that exact credential and destination.
    pub(super) fn check(&self, c: &StoredConnection, force_refresh: bool) -> Checked {
        // No credential was sent: the basis is the configuration the refusal was decided on.
        let unsent = |outcome| Checked {
            basis: CheckBasis::of(c),
            outcome,
        };
        let access = self.access_with(&c.id, None, force_refresh, true);
        let access = match access {
            Ok(a) => a,
            Err(AccessError::Unavailable(why)) => {
                return unsent(Err(ListFailure::Unreachable(why)));
            }
            Err(AccessError::NotFound) => return unsent(Err(ListFailure::Malformed("connection"))),
            Err(AccessError::Disabled) => {
                return unsent(Err(ListFailure::Unreachable(
                    "the subscription risk notice is not accepted".into(),
                )));
            }
            Err(AccessError::Disconnected) | Err(AccessError::ReauthenticationRequired(_)) => {
                return unsent(Err(ListFailure::Unauthorized(401)));
            }
        };
        let run = |a: &Access| {
            // Refresh may have rotated the credential. Read its current connection revision
            // after access; a newer check or any configuration change invalidates this result.
            let current = self
                .read(|s| s.router().connection(&a.connection_id))
                .ok()
                .flatten();
            let basis = CheckBasis::from_access(a, current.as_ref().map_or(-1, |c| c.revision));
            if current.as_ref().is_none_or(|c| !basis.matches(c)) {
                return Checked {
                    basis,
                    outcome: Err(ListFailure::Unreachable(
                        "the connection changed before discovery".into(),
                    )),
                };
            }
            let auth = match (&a.credential, a.profile.as_str()) {
                (AccessCredential::None, _) => discovery::Auth::None,
                (AccessCredential::ApiKey(k), "AnthropicMessages") => {
                    discovery::Auth::AnthropicKey(k)
                }
                (AccessCredential::ApiKey(k), _) => discovery::Auth::Bearer(k),
                (AccessCredential::Bearer { token, account }, "CodexSubscriptionResponses") => {
                    discovery::Auth::Codex(token, account.as_deref().unwrap_or(""))
                }
                (AccessCredential::Bearer { token, .. }, _) => {
                    discovery::Auth::ClaudeSubscription(token)
                }
            };
            Checked {
                basis,
                outcome: discovery::list(
                    &*self.http,
                    &self.endpoints,
                    &a.profile,
                    &a.endpoint,
                    &auth,
                ),
            }
        };
        let first = run(&access);
        match &first.outcome {
            // The access token may have been revoked before its expiry; one forced refresh
            // decides (reference `CodexModelList`).
            Err(ListFailure::Unauthorized(_))
                if matches!(access.credential, AccessCredential::Bearer { .. }) =>
            {
                let rejected = match &access.credential {
                    AccessCredential::Bearer { token, .. } => token.clone(),
                    _ => unreachable!("matched a bearer credential"),
                };
                match self.access_with(&c.id, Some(&rejected), false, true) {
                    Ok(again) => run(&again),
                    Err(AccessError::Unavailable(why)) => Checked {
                        basis: first.basis,
                        outcome: Err(ListFailure::Unreachable(why)),
                    },
                    Err(_) => first,
                }
            }
            _ => first,
        }
    }

    /// Tests a connection and records the outcome as the host (periodic checks).
    pub(super) fn check_as_host(&self, c: &StoredConnection) -> Result<(), StorageError> {
        let checked = self.check(c, false);
        let (id, host) = (c.id.clone(), self.store.info().host_actor);
        self.publication(move |tx| apply_check(tx, &host, &id, &checked).map(|_| ()))
    }

    /// One bounded step of background connection work, driven by the host timer: expires
    /// sign-in flows, polls due device codes (finishing an approved sign-in), retests
    /// unavailable connections, rediscovers stale model lists and reconciles orphaned secret
    /// handles hourly. A tick that is still running makes the next one a no-op.
    pub fn tick(&self) {
        if self.is_closed() {
            return;
        }
        {
            let mut live = self.live();
            if live.ticking {
                return;
            }
            live.ticking = true;
        }
        struct Ticking<'a>(&'a ConnectionService);
        impl Drop for Ticking<'_> {
            fn drop(&mut self) {
                self.0.live().ticking = false;
            }
        }
        let _guard = Ticking(self);
        let now = self.now();
        let due: Vec<Id> = {
            let mut live = self.live();
            live.flows.retain(|_, f| f.expires_at_ms > now || f.busy);
            live.flows
                .values_mut()
                .filter(|f| f.active && !f.busy)
                .filter_map(|f| match &f.kind {
                    FlowKind::Device { next_poll_ms, .. } if *next_poll_ms <= now => {
                        f.busy = true;
                        Some(f.id.clone())
                    }
                    _ => None,
                })
                .collect()
        };
        for flow in due {
            self.poll_device(&flow);
        }
        if let Ok(connections) = self.read(|s| all_connections(s.router())) {
            let restore = self.restore_epoch();
            for c in connections {
                let (state, _) = {
                    let live = self.live();
                    self.effective(&c, &live, restore, now)
                };
                let stale = match state {
                    ConnectionState::Connected => {
                        c.discovered_at_ms
                            .is_none_or(|t| now - t >= DISCOVERY_INTERVAL_MS)
                            && c.checked_at_ms
                                .is_none_or(|t| now - t >= UNAVAILABLE_RECHECK_MS)
                    }
                    ConnectionState::Unavailable => c
                        .checked_at_ms
                        .is_none_or(|t| now - t >= UNAVAILABLE_RECHECK_MS),
                    _ => false,
                };
                if stale {
                    let _ = self.check_as_host(&c);
                }
            }
        }
        let reconcile_due = self
            .live()
            .last_reconcile_ms
            .is_none_or(|t| now - t >= RECONCILE_INTERVAL_MS);
        if reconcile_due {
            let _ = self.reconcile();
        }
    }
}
