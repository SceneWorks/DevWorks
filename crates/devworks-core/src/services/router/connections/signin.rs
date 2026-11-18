//! Sign-in flows: device polling, preparation outside the unit of work, and completion.
use super::*;

/// What `prepare` did for one `router.configure` command.
pub(super) enum Prep {
    /// BeginSignIn: a flow staged (inactive) under this ID, or why it could not start.
    Begin(Result<Id, String>),
    /// CompleteSignIn: the exchanged credential staged under a handle, still pending, or why
    /// the exchange failed.
    Complete {
        flow_id: Id,
        /// The connection's handle when the exchange ran: deleted once the new one is published.
        retire: Option<Id>,
        outcome: Exchange,
    },
    /// Refresh / Inspect: the connection test run against the credential revision it saw.
    Check(Checked),
    /// The handles `settle` resolves: `retire` are the target connection's current handle
    /// (deleted once nothing references it), `staged` the submitted API key (unstaged once
    /// published).
    Handles { retire: Vec<Id>, staged: Option<Id> },
}

pub(super) enum Exchange {
    Staged {
        handle: Id,
        account_digest: Option<String>,
        expires_at_ms: i64,
    },
    Pending,
    Failed(DomainErrorCode, String),
}

/// Exchanged tokens to publish for a sign-in.
pub(super) struct PublishTokens {
    connection_id: Id,
    profile: String,
    endpoint: String,
    /// The credential revision the sign-in began at (compare-and-set).
    credential_revision: i64,
    handle: Id,
    account_digest: Option<String>,
    expires_at_ms: i64,
    restore_epoch: i64,
}

/// Publishes a sign-in's tokens: the connection still has the profile, endpoint and credential
/// revision the sign-in began with. Returns the saved connection and the handle it replaced,
/// or the refusal kind when the connection changed meanwhile.
pub(super) fn publish_tokens(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    t: &PublishTokens,
) -> Result<Result<(StoredConnection, Option<Id>), &'static str>, StorageError> {
    let Some(c) = tx.router().connection(&t.connection_id)? else {
        return Ok(Err(CONNECTION));
    };
    if c.profile != t.profile
        || c.endpoint != t.endpoint
        || c.credential_revision != t.credential_revision
    {
        return Ok(Err(CONNECTION));
    }
    let mut next = c.clone();
    let old = c.credential_ref.clone();
    if c.account_digest.is_none()
        || t.account_digest.is_none()
        || c.account_digest != t.account_digest
    {
        next.account_revision = next_account_revision(next.account_revision)?;
    }
    next.credential_ref = Some(t.handle.clone());
    next.credential_revision += 1;
    next.credential_epoch = Some(t.restore_epoch);
    next.lifecycle = "Connected".into();
    next.state_reason = None;
    next.access_expires_at_ms = Some(t.expires_at_ms);
    match (&c.account_digest, &t.account_digest) {
        // The first account binds a risk acceptance given before the first sign-in.
        (None, Some(new)) => {
            if let Some(r) = &mut next.risk
                && r.binding == profiles::risk_binding(&c.profile, &c.endpoint, None)
            {
                r.binding = profiles::risk_binding(&c.profile, &c.endpoint, Some(new));
            }
            next.account_digest = Some(new.clone());
        }
        // Another account: the acceptance belonged to the old one.
        (Some(old_account), Some(new)) if old_account != new => {
            next.risk = None;
            next.account_digest = Some(new.clone());
        }
        _ => {}
    }
    // A new sign-in is a new credential (perhaps another account or plan): list again.
    forget_credential_facts(&mut next);
    next.updated_at_ms = tx.now_ms();
    let saved = tx.router().put_connection(&next, Some(c.revision))?;
    // A new sign-in starts clean: earlier cooldowns of the profile's models belonged to the
    // previous account (reference `SaveLoginAsync`).
    clear_profile_cooldowns(tx, &saved)?;
    event(
        tx,
        actor,
        (CONNECTION_AGGREGATE, &saved.id, saved.revision),
        "ConnectionSignedIn",
        connection_payload(&saved),
    )?;
    Ok(Ok((saved, old)))
}

impl ConnectionService {
    /// Polls one device flow (marked busy by the caller) and finishes an approved sign-in.
    pub(super) fn poll_device(&self, flow_id: &Id) {
        let snapshot = {
            let live = self.live();
            live.flows
                .get(flow_id.as_str())
                .and_then(|f| match &f.kind {
                    FlowKind::Device { code, .. } => Some((code.clone(), f.started_by.clone())),
                    FlowKind::Browser { .. } => None,
                })
        };
        let Some((code, actor)) = snapshot else {
            return;
        };
        let outcome = self.device_step(&actor, &code);
        match outcome {
            Exchange::Pending => {
                let mut live = self.live();
                if let Some(f) = live.flows.get_mut(flow_id.as_str()) {
                    f.busy = false;
                    if let FlowKind::Device { code, next_poll_ms } = &mut f.kind {
                        *next_poll_ms = self.now() + code.interval_ms;
                    }
                }
            }
            Exchange::Failed(_, why) => {
                let mut live = self.live();
                if let Some(f) = live.flows.remove(flow_id.as_str()) {
                    live.failures
                        .insert(f.connection_id.as_str().to_owned(), why);
                }
            }
            Exchange::Staged {
                handle,
                account_digest,
                expires_at_ms,
            } => {
                // The flow stays registered (busy) until its publication is decided, so a
                // cancellation meanwhile sees a pending sign-in and advances the credential
                // revision the publication compares. A flow cancelled while it was exchanging
                // never publishes.
                let publish = self
                    .live()
                    .flows
                    .get(flow_id.as_str())
                    .filter(|f| f.active)
                    .map(|f| PublishTokens {
                        connection_id: f.connection_id.clone(),
                        profile: f.profile.clone(),
                        endpoint: f.endpoint.clone(),
                        credential_revision: f.credential_revision,
                        handle: handle.clone(),
                        account_digest,
                        expires_at_ms,
                        restore_epoch: 0,
                    });
                let Some(mut publish) = publish else {
                    self.live().flows.remove(flow_id.as_str());
                    self.discard(&handle);
                    return;
                };
                publish.restore_epoch = self.restore_epoch();
                let connection = publish.connection_id.clone();
                let result = self.publication(move |tx| {
                    publish_tokens(tx, &actor, &publish).map(|r| r.map(|(c, old)| (c.id, old)))
                });
                self.live().flows.remove(flow_id.as_str());
                let flow_connection = connection;
                match result {
                    Ok(Ok((id, old))) => {
                        self.unstage(&handle);
                        self.live().failures.remove(id.as_str());
                        self.delete_unreferenced(&old.into_iter().collect::<Vec<_>>());
                        // Discover the models of the newly signed-in account.
                        if let Ok(Some(c)) = self.read(|s| s.router().connection(&id)) {
                            let _ = self.check_as_host(&c);
                        }
                    }
                    Ok(Err(_)) => {
                        self.live().failures.insert(
                            flow_connection.as_str().to_owned(),
                            "the connection changed during the sign-in; sign in again".into(),
                        );
                        self.discard(&handle);
                    }
                    Err(_) => self.discard(&handle),
                }
            }
        }
    }

    /// Polls a device code once; on approval exchanges it and stages the tokens.
    pub(super) fn device_step(&self, principal: &ActorId, code: &DeviceCode) -> Exchange {
        match auth::codex_poll(&*self.http, &self.endpoints, code) {
            Ok(None) => Exchange::Pending,
            // A transient network failure keeps polling until the code expires (reference).
            Err(AuthFailure::Transport(_)) => Exchange::Pending,
            Err(e) => Exchange::Failed(
                DomainErrorCode::CredentialsRequired,
                format!("the device sign-in was denied or expired: {}", e.reason()),
            ),
            Ok(Some((auth_code, verifier))) => {
                match auth::codex_exchange(
                    &*self.http,
                    &self.endpoints,
                    &auth_code,
                    &verifier,
                    self.now(),
                ) {
                    Ok(tokens) => self.stage_tokens(principal, tokens),
                    Err(e) => Exchange::Failed(DomainErrorCode::ExternalFailure, e.reason()),
                }
            }
        }
    }

    /// Writes exchanged tokens under a new handle (not yet published).
    pub(super) fn stage_tokens(&self, principal: &ActorId, tokens: Tokens) -> Exchange {
        let account_digest = tokens.account.as_deref().map(profiles::account_digest);
        let expires_at_ms = tokens.expires_at_ms;
        match self.write_handle(principal, false, &Credential::OAuth(tokens)) {
            Ok(handle) => Exchange::Staged {
                handle,
                account_digest,
                expires_at_ms,
            },
            Err(e) => Exchange::Failed(DomainErrorCode::StorageUnavailable, e.to_string()),
        }
    }

    /// Cancels every open sign-in of `connection` (its credential, profile or endpoint changed,
    /// or it was removed).
    /// Whether a sign-in of `connection` is open or still finishing (exchanging or publishing).
    /// Cancelling one must advance the durable credential revision its publication compares.
    pub(super) fn sign_in_pending(&self, connection: &Id) -> bool {
        self.live()
            .flows
            .values()
            .any(|f| &f.connection_id == connection)
    }

    /// The first credential revision of a connection created as `connection`: past every
    /// sign-in still registered for that ID (one of a removed connection of the same ID may be
    /// finishing), so none of them can publish to the new one.
    pub(super) fn first_credential_revision(&self, connection: &Id) -> i64 {
        self.live()
            .flows
            .values()
            .filter(|f| &f.connection_id == connection)
            .map(|f| f.credential_revision + 1)
            .max()
            .unwrap_or(0)
    }

    pub(super) fn cancel_flows(&self, connection: &Id) {
        self.live()
            .flows
            .retain(|_, f| &f.connection_id != connection || f.busy);
        let mut live = self.live();
        for f in live.flows.values_mut() {
            if &f.connection_id == connection {
                // A running exchange finishes, but its publish fails the compare-and-set.
                f.active = false;
            }
        }
    }

    pub(super) fn sign_in(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        connection_id: &Id,
        action: &AccountAction,
        flow_id: Option<&Id>,
        prep: Option<&Prep>,
    ) -> Result<DomainDecision, Failure> {
        let c = tx
            .router()
            .connection(connection_id)?
            .ok_or_else(|| refuse(DomainErrorCode::NotFound, CONNECTION, Some(connection_id)))?;
        let p = profile(&c.profile).ok_or_else(|| invalid("profile"))?;
        let restore = tx.store().restore_epoch;
        match action {
            AccountAction::BeginSignIn => {
                if !p.oauth() {
                    return Err(refuse(
                        DomainErrorCode::UnsupportedCapability,
                        "sign_in",
                        Some(&c.id),
                    ));
                }
                if p.risk_gated && !risk_effective(&c, restore) {
                    return Err(refuse(
                        DomainErrorCode::PermissionDenied,
                        "subscription_risk",
                        Some(&c.id),
                    ));
                }
                match prep {
                    Some(Prep::Begin(Ok(flow))) => {
                        let mut live = self.live();
                        let f = live
                            .flows
                            .get_mut(flow.as_str())
                            .filter(|f| f.connection_id == c.id && !f.decided)
                            .ok_or_else(|| refuse(DomainErrorCode::Conflict, FLOW, Some(flow)))?;
                        if f.credential_revision != c.credential_revision
                            || f.profile != c.profile
                            || f.endpoint != c.endpoint
                        {
                            return Err(refuse(
                                DomainErrorCode::SourceChanged,
                                CONNECTION,
                                Some(&c.id),
                            ));
                        }
                        f.decided = true;
                        Ok(applied(&c.id, c.revision))
                    }
                    Some(Prep::Begin(Err(_))) => Err(Refusal::new(
                        DomainErrorCode::ExternalFailure,
                        "sign_in",
                        Some(c.id.clone()),
                    )
                    .retry(RetryAdvice::RefreshAndNewRequest)
                    .into()),
                    _ => Err(refuse(DomainErrorCode::Conflict, FLOW, None)),
                }
            }
            AccountAction::CompleteSignIn => {
                let flow = flow_id.ok_or_else(|| invalid("flow_id"))?;
                if p.risk_gated && !risk_effective(&c, restore) {
                    return Err(refuse(
                        DomainErrorCode::PermissionDenied,
                        "subscription_risk",
                        Some(&c.id),
                    ));
                }
                match prep {
                    Some(Prep::Complete {
                        flow_id: prepared,
                        outcome,
                        ..
                    }) if prepared == flow => match outcome {
                        Exchange::Pending => Ok(unchanged(&c.id, c.revision)),
                        Exchange::Failed(code, _) => {
                            Err(Refusal::new(*code, "sign_in", Some(c.id.clone()))
                                .retry(RetryAdvice::RefreshAndNewRequest)
                                .into())
                        }
                        Exchange::Staged {
                            handle,
                            account_digest,
                            expires_at_ms,
                        } => {
                            let (profile_name, endpoint, revision) = {
                                let live = self.live();
                                let f = live.flows.get(flow.as_str()).ok_or_else(|| {
                                    refuse(DomainErrorCode::NotFound, FLOW, Some(flow))
                                })?;
                                if !f.active {
                                    // Cancelled while its code was exchanged.
                                    return Err(refuse(
                                        DomainErrorCode::SourceChanged,
                                        CONNECTION,
                                        Some(&c.id),
                                    ));
                                }
                                (f.profile.clone(), f.endpoint.clone(), f.credential_revision)
                            };
                            let publish = PublishTokens {
                                connection_id: c.id.clone(),
                                profile: profile_name,
                                endpoint,
                                credential_revision: revision,
                                handle: handle.clone(),
                                account_digest: account_digest.clone(),
                                expires_at_ms: *expires_at_ms,
                                restore_epoch: restore,
                            };
                            match publish_tokens(tx, actor, &publish)? {
                                Ok((saved, _)) => Ok(applied(&saved.id, saved.revision)),
                                Err(kind) => {
                                    Err(refuse(DomainErrorCode::SourceChanged, kind, Some(&c.id)))
                                }
                            }
                        }
                    },
                    _ => Err(refuse(DomainErrorCode::NotFound, FLOW, Some(flow))),
                }
            }
            AccountAction::Refresh | AccountAction::Inspect
                if p.risk_gated && !risk_effective(&c, restore) =>
            {
                Err(refuse(
                    DomainErrorCode::PermissionDenied,
                    "subscription_risk",
                    Some(&c.id),
                ))
            }
            AccountAction::Refresh | AccountAction::Inspect => match prep {
                Some(Prep::Check(checked)) => {
                    // A recorded test (passed or failed: its result is new information) is
                    // applied; one whose credential or destination changed meanwhile records
                    // nothing.
                    let recorded = apply_check(tx, actor, &c.id, checked)?;
                    let after = tx
                        .router()
                        .connection(&c.id)?
                        .map_or(c.revision, |n| n.revision);
                    Ok(if recorded {
                        applied(&c.id, after)
                    } else {
                        unchanged(&c.id, after)
                    })
                }
                _ => Err(refuse(DomainErrorCode::Conflict, CONNECTION, Some(&c.id))),
            },
            AccountAction::Disconnect => {
                if c.credential_ref.is_none() {
                    if !self.sign_in_pending(&c.id) {
                        return Ok(unchanged(&c.id, c.revision));
                    }
                    // A first sign-in is cancelled: its publication (which may already be
                    // running) compares the credential revision, so it is advanced.
                    let mut next = c.clone();
                    next.credential_revision += 1;
                    next.account_revision = next_account_revision(next.account_revision)?;
                    next.updated_at_ms = tx.now_ms();
                    let saved = tx.router().put_connection(&next, Some(c.revision))?;
                    self.cancel_flows(&saved.id);
                    event(
                        tx,
                        actor,
                        (CONNECTION_AGGREGATE, &saved.id, saved.revision),
                        "SignInCancelled",
                        connection_payload(&saved),
                    )?;
                    return Ok(applied(&saved.id, saved.revision));
                }
                let mut next = c.clone();
                next.credential_ref = None;
                next.credential_revision += 1;
                next.account_revision = next_account_revision(next.account_revision)?;
                next.credential_epoch = None;
                next.lifecycle = without_credential(p).into();
                next.state_reason = None;
                next.access_expires_at_ms = None;
                forget_credential_facts(&mut next);
                next.updated_at_ms = tx.now_ms();
                let saved = tx.router().put_connection(&next, Some(c.revision))?;
                self.cancel_flows(&saved.id);
                clear_profile_cooldowns(tx, &saved)?;
                event(
                    tx,
                    actor,
                    (CONNECTION_AGGREGATE, &saved.id, saved.revision),
                    "ConnectionDisconnected",
                    connection_payload(&saved),
                )?;
                Ok(applied(&saved.id, saved.revision))
            }
        }
    }

    pub(super) fn prepare_action(
        &self,
        ctx: &InvocationContext,
        action: &RouterConfigAction,
    ) -> Result<Option<Prep>, StorageError> {
        match action {
            RouterConfigAction::Connection {
                connection_id,
                credential_ref,
                ..
            } => {
                let retire = match connection_id {
                    Some(id) => self
                        .read(|s| s.router().connection(id))?
                        .and_then(|c| c.credential_ref)
                        .into_iter()
                        .collect(),
                    None => Vec::new(),
                };
                Ok(Some(Prep::Handles {
                    retire,
                    staged: credential_ref.clone(),
                }))
            }
            RouterConfigAction::RemoveConnection { connection_id, .. } => {
                let retire = self
                    .read(|s| s.router().connection(connection_id))?
                    .and_then(|c| c.credential_ref)
                    .into_iter()
                    .collect();
                Ok(Some(Prep::Handles {
                    retire,
                    staged: None,
                }))
            }
            RouterConfigAction::SignIn {
                connection_id,
                action,
                flow_id,
            } => {
                let Some(c) = self.read(|s| s.router().connection(connection_id))? else {
                    return Ok(None);
                };
                match action {
                    AccountAction::Disconnect => Ok(Some(Prep::Handles {
                        retire: c.credential_ref.into_iter().collect(),
                        staged: None,
                    })),
                    AccountAction::BeginSignIn => Ok(self.begin_flow(ctx, &c).map(Prep::Begin)),
                    AccountAction::CompleteSignIn => Ok(flow_id
                        .as_ref()
                        .and_then(|f| self.complete_flow(ctx, &c, f))),
                    AccountAction::Refresh | AccountAction::Inspect
                        if profile(&c.profile).is_some_and(|p| p.risk_gated)
                            && !risk_effective(&c, self.restore_epoch()) =>
                    {
                        // Refused by `decide`; nothing is sent for a disabled subscription.
                        Ok(None)
                    }
                    AccountAction::Refresh | AccountAction::Inspect => {
                        let force = matches!(action, AccountAction::Refresh)
                            && profile(&c.profile).is_some_and(Profile::oauth);
                        // Labelled with the credential the request actually carried.
                        Ok(Some(Prep::Check(self.check(&c, force))))
                    }
                }
            }
            _ => Ok(None),
        }
    }

    /// Starts a sign-in flow (staged, inactive until its command applies).
    pub(super) fn begin_flow(
        &self,
        ctx: &InvocationContext,
        c: &StoredConnection,
    ) -> Option<Result<Id, String>> {
        let p = profile(&c.profile)?;
        let now = self.now();
        let kind = match p.auth {
            profiles::Auth::DeviceCode => match auth::codex_start(&*self.http, &self.endpoints) {
                Ok(code) => FlowKind::Device {
                    next_poll_ms: now + code.interval_ms,
                    code,
                },
                Err(e) => return Some(Err(e.reason())),
            },
            profiles::Auth::AuthorizationCode => {
                let verifier = self.random();
                let state = self.random();
                let authorize_url = auth::claude_authorize_url(&self.endpoints, &verifier, &state);
                FlowKind::Browser {
                    verifier,
                    state,
                    authorize_url,
                    pasted: None,
                }
            }
            profiles::Auth::ApiKey { .. } => return None,
        };
        let id = self.ids.next_id(IdKind::SignInFlow);
        let lifetime = match kind {
            FlowKind::Device { .. } => auth::DEVICE_LIFETIME_MS,
            FlowKind::Browser { .. } => auth::BROWSER_LIFETIME_MS,
        };
        let mut live = self.live();
        live.failures.remove(c.id.as_str());
        live.flows.insert(
            id.as_str().to_owned(),
            Flow {
                id: id.clone(),
                connection_id: c.id.clone(),
                profile: c.profile.clone(),
                endpoint: c.endpoint.clone(),
                credential_revision: c.credential_revision,
                started_by: ctx.principal().clone(),
                kind,
                expires_at_ms: now + lifetime,
                decided: false,
                active: false,
                busy: false,
            },
        );
        Some(Ok(id))
    }

    /// Takes an open flow of the caller for this connection and finishes it if it can.
    pub(super) fn complete_flow(
        &self,
        ctx: &InvocationContext,
        c: &StoredConnection,
        flow_id: &Id,
    ) -> Option<Prep> {
        let now = self.now();
        let taken = {
            let mut live = self.live();
            let f = live.flows.get_mut(flow_id.as_str()).filter(|f| {
                f.active
                    && !f.busy
                    && f.connection_id == c.id
                    && &f.started_by == ctx.principal()
                    && f.expires_at_ms > now
            })?;
            f.busy = true;
            match &mut f.kind {
                FlowKind::Device { code, .. } => Ok(code.clone()),
                FlowKind::Browser {
                    verifier,
                    state,
                    pasted,
                    ..
                } => Err((verifier.clone(), state.clone(), pasted.take())),
            }
        };
        let device = taken.is_ok();
        let outcome = match taken {
            Ok(code) => self.device_step(ctx.principal(), &code),
            Err((_, _, None)) => Exchange::Failed(
                DomainErrorCode::InvalidArgument,
                "no authorization code was entered".into(),
            ),
            Err((verifier, state, Some(pasted))) => {
                match auth::claude_pasted_code(&pasted, &state) {
                    Err(why) => Exchange::Failed(DomainErrorCode::InvalidArgument, why.into()),
                    Ok(code) => match auth::claude_exchange(
                        &*self.http,
                        &self.endpoints,
                        &code,
                        &state,
                        &verifier,
                        now,
                    ) {
                        Ok(tokens) => self.stage_tokens(ctx.principal(), tokens),
                        Err(e @ AuthFailure::Rejected(_)) => {
                            Exchange::Failed(DomainErrorCode::CredentialsRequired, e.reason())
                        }
                        Err(e) => Exchange::Failed(DomainErrorCode::ExternalFailure, e.reason()),
                    },
                }
            }
        };
        let mut live = self.live();
        if let Exchange::Failed(_, why) = &outcome {
            live.failures.insert(c.id.as_str().to_owned(), why.clone());
        }
        if device && matches!(outcome, Exchange::Failed(..)) {
            // A denied or expired device code is finished; a wrong browser paste may be retried.
            live.flows.remove(flow_id.as_str());
        } else if let Some(f) = live.flows.get_mut(flow_id.as_str()) {
            f.busy = false;
            if let FlowKind::Device { code, next_poll_ms } = &mut f.kind {
                *next_poll_ms = now + code.interval_ms;
            }
        }
        Some(Prep::Complete {
            flow_id: flow_id.clone(),
            retire: c.credential_ref.clone(),
            outcome,
        })
    }

    /// Resolves what the unit of work did not commit. Idempotent and decided from durable
    /// state (what a connection references now), so a refusal, a replay, a rollback or a race
    /// with recovery never deletes a published credential.
    pub(super) fn settle_prep(&self, prep: &Prep, reply: Option<&DomainCommandReply>) {
        let applied_now = matches!(
            reply,
            Some(CommandReply::Recorded { receipt, .. })
                if matches!(receipt.decision, DomainDecision::Applied { .. })
        );
        let published = |h: &Id| {
            self.read(|s| referenced(s.router()))
                .is_ok_and(|r| r.contains(h.as_str()))
        };
        match prep {
            Prep::Begin(Ok(flow)) => {
                let mut live = self.live();
                let keep = applied_now && live.flows.get(flow.as_str()).is_some_and(|f| f.decided);
                if keep {
                    // The new sign-in replaces any earlier one of the same connection.
                    let connection = live
                        .flows
                        .get(flow.as_str())
                        .map(|f| f.connection_id.clone());
                    live.flows.retain(|k, f| {
                        k == flow.as_str()
                            || f.busy
                            || Some(&f.connection_id) != connection.as_ref()
                    });
                    if let Some(f) = live.flows.get_mut(flow.as_str()) {
                        f.active = true;
                    }
                } else {
                    live.flows.remove(flow.as_str());
                }
            }
            Prep::Begin(Err(_)) | Prep::Check { .. } => {}
            Prep::Complete {
                flow_id,
                retire,
                outcome,
            } => {
                if let Exchange::Staged { handle, .. } = outcome {
                    if published(handle) {
                        self.unstage(handle);
                        {
                            let mut live = self.live();
                            if let Some(f) = live.flows.remove(flow_id.as_str()) {
                                live.failures.remove(f.connection_id.as_str());
                            }
                        }
                        self.delete_unreferenced(&retire.iter().cloned().collect::<Vec<_>>());
                    } else {
                        // An exchanged code is single-use: unpublished tokens are discarded.
                        self.discard(handle);
                    }
                }
            }
            Prep::Handles { retire, staged } => {
                if let Some(s) = staged
                    && published(s)
                {
                    self.unstage(s);
                }
                self.delete_unreferenced(retire);
            }
        }
    }
}
