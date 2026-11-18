//! Credentials for provider transports: effective access, per-connection refresh and rotation.
use super::*;

/// Registers a provider credential read from the secret store with the process-wide redactor,
/// so a provider error body or process output that echoes it is redacted.
pub(super) fn register_credential(credential: &Credential) {
    use crate::authority::redact::register_resolved_secret;
    match credential {
        Credential::ApiKey(k) => register_resolved_secret(k.expose()),
        Credential::OAuth(t) => {
            register_resolved_secret(t.access.expose());
            register_resolved_secret(t.refresh.expose());
        }
    }
}

impl ConnectionService {
    pub(super) fn refresh_lock(&self, connection: &Id) -> Arc<Mutex<()>> {
        self.refresh_locks
            .locked()
            .entry(connection.as_str().to_owned())
            .or_default()
            .clone()
    }

    /// The credential for one request on `connection_id`. An OAuth access token near expiry, or
    /// equal to `rejected` (the provider just refused it), is refreshed first: one refresh per
    /// connection at a time, shared by concurrent callers, persisted whatever happens to the
    /// caller. Network failure is an availability error, never a revocation.
    pub fn access(
        &self,
        connection_id: &Id,
        rejected: Option<&Secret>,
    ) -> Result<Access, AccessError> {
        self.access_with(connection_id, rejected, false, false)
    }

    /// Forces a token refresh (the `Refresh` action); API-key connections only re-read.
    pub fn refresh(&self, connection_id: &Id) -> Result<Access, AccessError> {
        self.access_with(connection_id, None, true, false)
    }

    /// `testing`: the user's own connection test or refresh of a connection they turned off
    /// (never a routed request). A credential replaced while it was being read (a rotation or
    /// a new key retired the handle just read) is read again from the new row, a bounded
    /// number of times: only a credential that is still referenced is reported as needing a
    /// new sign-in.
    ///
    /// Reading a credential may wait (the secret store, the per-connection refresh lock, a token
    /// exchange), and the user may withdraw the risk acceptance, turn the connection off or
    /// remove it meanwhile without changing its credential revision. So an [`Access`] is only
    /// returned once the [`ConnectionService::usable`] gate holds on a row read after all of
    /// that, for the same credential revision, profile and endpoint; a rotated token is
    /// persisted either way.
    pub(super) fn access_with(
        &self,
        connection_id: &Id,
        rejected: Option<&Secret>,
        force: bool,
        testing: bool,
    ) -> Result<Access, AccessError> {
        for _ in 0..CREDENTIAL_READ_ATTEMPTS {
            if self.is_closed() {
                return Err(AccessError::Unavailable(
                    "the provider connection owner is closed".into(),
                ));
            }

            let Some(a) = self.access_once(connection_id, rejected, force, testing)? else {
                continue;
            };
            let now = self
                .read(|s| s.router().connection(connection_id))
                .map_err(|e| AccessError::Unavailable(format!("{e:?}")))?
                .ok_or(AccessError::NotFound)?;
            self.usable(&now, testing)?;
            if CheckBasis::from_access(&a, now.revision).matches(&now) {
                return Ok(a);
            }
        }
        Err(AccessError::Unavailable(
            "the connection's credential kept changing while it was read".into(),
        ))
    }

    /// The gate every use of `c`'s credential passes, on the latest row read: a known profile,
    /// an effective risk acceptance for a risk-gated profile and, except for the user's own
    /// test (`testing`), the connection turned on.
    pub(super) fn usable(
        &self,
        c: &StoredConnection,
        testing: bool,
    ) -> Result<&'static Profile, AccessError> {
        let p = profile(&c.profile).ok_or(AccessError::NotFound)?;
        if p.risk_gated && !risk_effective(c, self.restore_epoch()) {
            return Err(AccessError::Disabled);
        }
        if !c.enabled && !testing {
            return Err(AccessError::Disabled);
        }
        Ok(p)
    }

    /// One read of the connection and its credential; `None` when the credential was replaced
    /// meanwhile.
    pub(super) fn access_once(
        &self,
        connection_id: &Id,
        rejected: Option<&Secret>,
        force: bool,
        testing: bool,
    ) -> Result<Option<Access>, AccessError> {
        let restore = self.restore_epoch();
        let unavailable = |e: StorageError| AccessError::Unavailable(format!("{e:?}"));
        let c = self
            .read(|s| s.router().connection(connection_id))
            .map_err(unavailable)?
            .ok_or(AccessError::NotFound)?;
        let p = self.usable(&c, testing)?;
        self.check_refresh_owner(&c)?;
        // Profile, destination and credential all come from this one read.
        let base = |credential| Access {
            connection_id: c.id.clone(),
            profile: c.profile.clone(),
            endpoint: c.endpoint.clone(),
            credential_revision: c.credential_revision,
            credential,
            open_router: constraints_of(&c),
        };
        let Some(handle) = c.credential_ref.clone() else {
            return if p.needs_credential() {
                Err(AccessError::Disconnected)
            } else if c.lifecycle == "ReauthenticationRequired" && !testing {
                // A keyless endpoint that refused authorization is not routed through until
                // it accepts a test again or is given a key.
                Err(AccessError::ReauthenticationRequired(
                    c.state_reason
                        .clone()
                        .unwrap_or_else(|| "the endpoint requires a key".into()),
                ))
            } else {
                Ok(Some(base(AccessCredential::None)))
            };
        };
        if c.credential_epoch != Some(restore) {
            return Err(AccessError::ReauthenticationRequired(
                "the workspace was restored; sign in again".into(),
            ));
        }
        let refused = c.lifecycle == "ReauthenticationRequired";
        let reauthenticate = || {
            AccessError::ReauthenticationRequired(
                c.state_reason
                    .clone()
                    .unwrap_or_else(|| "sign in again".into()),
            )
        };
        if refused && !testing {
            return Err(reauthenticate());
        }
        let Some(credential) = self.load(&c, &handle)? else {
            return Ok(None);
        };
        // The user's own test or refresh retries a refused API key (it may be valid again;
        // a passing test returns the connection to Connected). Routing never does, and a
        // refused subscription token needs a new sign-in.
        if refused && !matches!(credential, Credential::ApiKey(_)) {
            return Err(reauthenticate());
        }
        match credential {
            Credential::ApiKey(k) => Ok(Some(base(AccessCredential::ApiKey(k)))),
            Credential::OAuth(t) => {
                let fresh = t.expires_at_ms > self.now() + auth::REFRESH_SKEW_MS
                    && rejected.is_none_or(|r| r != &t.access);
                if fresh && !force {
                    return Ok(Some(base(AccessCredential::Bearer {
                        token: t.access,
                        account: t.account,
                    })));
                }
                drop(t);
                self.refresh_tokens(&c, rejected, force, testing)
            }
        }
    }

    /// Reads and parses a handle. `None` when the connection no longer references it (the
    /// credential was replaced while it was read: read the row again); a missing or unreadable
    /// secret that is still referenced requires reauthentication.
    pub(super) fn load(
        &self,
        c: &StoredConnection,
        handle: &Id,
    ) -> Result<Option<Credential>, AccessError> {
        let why = match self.noted(self.secrets.get(&self.key(handle))) {
            Ok(Some(s)) => match Credential::parse(&s) {
                Some(credential) => {
                    register_credential(&credential);
                    return Ok(Some(credential));
                }
                None => "the stored credential is unreadable",
            },
            Ok(None) => "the stored credential is missing; sign in or enter the key again",
            Err(e) => return Err(AccessError::Unavailable(e.to_string())),
        };
        let current = self
            .read(|s| s.router().connection(&c.id))
            .map_err(|e| AccessError::Unavailable(format!("{e:?}")))?;
        let still = current.as_ref().is_some_and(|n| {
            n.credential_ref.as_ref() == Some(handle)
                && n.credential_revision == c.credential_revision
        });
        if !still {
            return Ok(None);
        }
        self.mark(c, "ReauthenticationRequired", why);
        Err(AccessError::ReauthenticationRequired(why.into()))
    }

    /// Records a lifecycle for `c` if its credential is still the one it was decided for.
    pub(super) fn mark(&self, c: &StoredConnection, lifecycle: &'static str, reason: &str) {
        let (id, seen, reason) = (c.id.clone(), c.credential_revision, reason.to_owned());
        let host = self.store.info().host_actor;
        let _ = self.publication(move |tx| {
            let Some(mut cur) = tx.router().connection(&id)? else {
                return Ok(());
            };
            if cur.credential_revision != seen || cur.lifecycle == lifecycle {
                return Ok(());
            }
            let expected = cur.revision;
            cur.lifecycle = lifecycle.into();
            cur.state_reason = Some(reason);
            cur.updated_at_ms = tx.now_ms();
            let saved = tx.router().put_connection(&cur, Some(expected))?;
            event(
                tx,
                &host,
                (CONNECTION_AGGREGATE, &saved.id, saved.revision),
                "ConnectionStateChanged",
                connection_payload(&saved),
            )?;
            if saved.lifecycle == "ReauthenticationRequired" {
                crate::services::notifications::credentials_rejected(tx, &saved)?;
            }
            Ok(())
        });
    }

    /// `None` when the credential was replaced while it was read (the caller reads again).
    pub(super) fn refresh_tokens(
        &self,
        seen: &StoredConnection,
        rejected: Option<&Secret>,
        force: bool,
        testing: bool,
    ) -> Result<Option<Access>, AccessError> {
        let lock = self.refresh_lock(&seen.id);
        let _serialized = lock.locked();
        // Another caller may have refreshed while this one waited, and the connection may have
        // been disabled meanwhile: nothing is sent for a connection that no longer passes.
        let c = self
            .read(|s| s.router().connection(&seen.id))
            .map_err(|e| AccessError::Unavailable(format!("{e:?}")))?
            .ok_or(AccessError::NotFound)?;
        self.usable(&c, testing)?;
        let Some(handle) = c.credential_ref.clone() else {
            return Err(AccessError::Disconnected);
        };
        let tokens = match self.load(&c, &handle)? {
            None => return Ok(None),
            Some(Credential::OAuth(tokens)) => tokens,
            Some(Credential::ApiKey(_)) => {
                return Err(AccessError::ReauthenticationRequired(
                    "the stored credential is not a subscription token".into(),
                ));
            }
        };
        // Destination and credential come from this one read (`c`): a refreshed token is never
        // sent to an endpoint read elsewhere.
        let base = |token: Secret, account: Option<String>, revision: i64| Access {
            connection_id: c.id.clone(),
            profile: c.profile.clone(),
            endpoint: c.endpoint.clone(),
            credential_revision: revision,
            credential: AccessCredential::Bearer { token, account },
            open_router: constraints_of(&c),
        };
        if c.credential_revision != seen.credential_revision
            && !force
            && tokens.expires_at_ms > self.now() + auth::REFRESH_SKEW_MS
            && rejected.is_none_or(|r| r != &tokens.access)
        {
            return Ok(Some(base(
                tokens.access,
                tokens.account,
                c.credential_revision,
            )));
        }
        struct Refreshing<'a>(&'a ConnectionService, String);
        impl Drop for Refreshing<'_> {
            fn drop(&mut self) {
                self.0.live().refreshing.remove(&self.1);
            }
        }
        self.live().refreshing.insert(c.id.as_str().to_owned());
        let _guard = Refreshing(self, c.id.as_str().to_owned());
        self.check_refresh_owner(&c)?;
        let owner = self
            .begin_refresh(&c)
            .map_err(|e| AccessError::Unavailable(format!("refresh custody: {e:?}")))?;
        let now = self.now();
        let outcome = match c.profile.as_str() {
            "CodexSubscriptionResponses" => {
                auth::codex_refresh(&*self.http, &self.endpoints, &tokens, now)
            }
            _ => auth::claude_refresh(&*self.http, &self.endpoints, &tokens, now),
        };
        match outcome {
            Ok(rotated) => {
                register_credential(&Credential::OAuth(rotated.clone()));
                self.live().ambiguous.remove(c.id.as_str());
                let account = rotated.account.clone();
                let access = rotated.access.clone();
                let expires = rotated.expires_at_ms;
                let digest = rotated.account.as_deref().map(profiles::account_digest);
                let host = self.store.info().host_actor;
                let new_handle = match self.write_handle_reserved(
                    &host,
                    false,
                    &Credential::OAuth(rotated),
                    owner.handle.clone(),
                ) {
                    Ok(handle) => handle,
                    Err(error) => {
                        let _=self.finish_refresh(&owner,crate::protocol::OperationState::OutcomeUnknown,json!({"credentialPublication":"NotPublished","externalSecretWrite":"Unknown","allocatedHandle":owner.handle}));
                        self.mark(&c,"ReauthenticationRequired","the refresh succeeded but its credential could not be safely published");
                        return Err(AccessError::Unavailable(error.to_string()));
                    }
                };
                let published = self.publish_rotation(&c, &new_handle, digest, expires, &owner);
                match published {
                    Ok(Some(revision)) => {
                        self.refresh_published(&owner);
                        self.unstage(&new_handle);
                        self.delete_unreferenced(&[handle]);
                        Ok(Some(base(access, account, revision)))
                    }
                    Ok(None) => {
                        // The credential changed meanwhile (a sign-out or a new sign-in): the
                        // rotation is not published and its tokens are not used; the caller
                        // reads the new credential instead.
                        self.discard(&new_handle);
                        let _ = self.finish_refresh(
                            &owner,
                            crate::protocol::OperationState::Failed,
                            json!({"credentialPublication":"Superseded"}),
                        );
                        Ok(None)
                    }
                    Err(e) => {
                        // Not published: the old refresh token may already be spent, so the
                        // connection needs a new sign-in rather than a repeat of it.
                        self.discard(&new_handle);
                        let _ = self.finish_refresh(&owner, crate::protocol::OperationState::OutcomeUnknown, json!({"credentialPublication":"NotPublished","allocatedHandle":new_handle}));
                        self.mark(
                            &c,
                            "ReauthenticationRequired",
                            "the refreshed token could not be saved",
                        );
                        Err(AccessError::Unavailable(format!("{e:?}")))
                    }
                }
            }
            Err(AuthFailure::Rejected(status)) => {
                let _ = self.finish_refresh(
                    &owner,
                    crate::protocol::OperationState::Failed,
                    json!({"httpStatus":status,"credentialPublication":"NotPublished"}),
                );
                let why = AuthFailure::Rejected(status).reason();
                self.mark(&c, "ReauthenticationRequired", &why);
                Err(AccessError::ReauthenticationRequired(why))
            }
            Err(e @ AuthFailure::Transport(_)) | Err(e @ AuthFailure::Malformed(_)) => {
                let _ = self.finish_refresh(
                    &owner,
                    crate::protocol::OperationState::OutcomeUnknown,
                    json!({"credentialPublication":"NotPublished","refreshOutcome":"Unknown"}),
                );
                // The request may have reached the issuer and rotated the refresh token: allow
                // a bounded number of such outcomes, then require a new sign-in.
                let n = {
                    let mut live = self.live();
                    let seen = live
                        .ambiguous
                        .entry(c.id.as_str().to_owned())
                        .or_insert((c.credential_revision, 0));
                    if seen.0 != c.credential_revision {
                        *seen = (c.credential_revision, 0);
                    }
                    seen.1 += 1;
                    seen.1
                };
                if n >= AMBIGUOUS_REFRESH_LIMIT {
                    let why = "the token refresh outcome is unknown; sign in again";
                    self.mark(&c, "ReauthenticationRequired", why);
                    return Err(AccessError::ReauthenticationRequired(why.into()));
                }
                Err(AccessError::Unavailable(e.reason()))
            }
            Err(e @ AuthFailure::Status(_)) => {
                let _ = self.finish_refresh(
                    &owner,
                    crate::protocol::OperationState::Failed,
                    json!({"credentialPublication":"NotPublished","refreshOutcome":"Rejected"}),
                );
                Err(AccessError::Unavailable(e.reason()))
            }
        }
    }

    /// Publishes a rotated token (compare-and-set on the credential revision it replaced); the
    /// new revision, or `None` when the credential changed meanwhile.
    pub(super) fn publish_rotation(
        &self,
        seen: &StoredConnection,
        handle: &Id,
        account_digest: Option<String>,
        expires_at_ms: i64,
        owner: &lifecycle::RefreshOwner,
    ) -> Result<Option<i64>, StorageError> {
        let (id, old_ref, old_rev) = (
            seen.id.clone(),
            seen.credential_ref.clone(),
            seen.credential_revision,
        );
        let (handle, host, restore) = (
            handle.clone(),
            self.store.info().host_actor,
            self.restore_epoch(),
        );
        let owner = owner.clone();
        self.publication(move |tx| {
            let Some(mut c) = tx.router().connection(&id)? else {
                return Ok(None);
            };
            if c.credential_ref != old_ref || c.credential_revision != old_rev || c.refresh_operation_id.as_ref() != Some(&owner.operation) || c.account_revision != owner.account_revision {

                return Ok(None);
            }
            let expected = c.revision;
            c.credential_ref = Some(handle);
            c.credential_revision += 1;
            c.credential_epoch = Some(restore);
            c.lifecycle = "Connected".into();
            c.state_reason = None;
            c.access_expires_at_ms = Some(expires_at_ms);
            let account_changed = account_digest.is_some() && account_digest != c.account_digest;
            if account_changed {
                c.account_revision = next_account_revision(c.account_revision)?;
                // A refresh never changes the account; if the provider says otherwise, the risk
                // acceptance and what was learned with the old account no longer cover it.
                c.risk = None;
                c.account_digest = account_digest;
                forget_credential_facts(&mut c);
            }
            c.updated_at_ms = tx.now_ms();
            let saved = tx.router().put_connection(&c, Some(expected))?;
            if account_changed {
                clear_profile_cooldowns(tx, &saved)?;
            }
            event(
                tx,
                &host,
                (CONNECTION_AGGREGATE, &saved.id, saved.revision),
                "CredentialRotated",
                connection_payload(&saved),
            )?;
            lifecycle::finish_in(tx, &owner, crate::protocol::OperationState::ObservedComplete, json!({"credentialPublication":"Published","allocatedHandle":saved.credential_ref,"credentialRevision":saved.credential_revision}))?;
            Ok(Some(saved.credential_revision))
        })
    }
}
