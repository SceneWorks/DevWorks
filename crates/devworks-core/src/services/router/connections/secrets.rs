//! Host intake and custody of staged secrets: API keys, MCP keys, settings secrets and the
//! reconciliation that deletes unpublished handles.
use super::*;

/// The prefix of settings secret references (`dwsecret-`).
pub(super) const SETTING_SECRET_PREFIX: &str = "dwsecret-";

/// The production [`SecretResolver`]: settings references resolve from the same OS secret
/// store, in this store's namespace (one secret-store abstraction).
impl SecretResolver for ConnectionService {
    fn resolve(&self, reference: &SecretRef) -> Result<Secret, SecretUnavailable> {
        let handle = reference_handle(reference).ok_or(SecretUnavailable::NotFound)?;
        let custody = self
            .read(|r| r.configuration().settings_secret(&handle))
            .map_err(|_| SecretUnavailable::Locked)?;
        if custody.is_some_and(|row| {
            matches!(
                row.state,
                SettingsSecretState::Writing
                    | SettingsSecretState::Deleting
                    | SettingsSecretState::Deleted
            ) || row.state == SettingsSecretState::Ready && row.expires_at_ms <= self.now()
        }) {
            return Err(SecretUnavailable::NotFound);
        }
        match self.noted(self.secrets.get(&self.key(&handle))) {
            Ok(Some(s)) => {
                crate::authority::redact::register_resolved_secret(s.expose());
                Ok(s)
            }
            Ok(None) => Err(SecretUnavailable::NotFound),
            Err(SecretError::Unavailable(_)) | Err(SecretError::Denied) => {
                Err(SecretUnavailable::Locked)
            }
            Err(_) => Err(SecretUnavailable::Locked),
        }
    }
}

impl ConnectionService {
    /// Writes an API key the user entered into the secret store under a new handle and returns
    /// the handle (`credentialRef`) the user's `router.configure` `Connection` action publishes.
    /// The handle is accepted only from `principal`; an unpublished one is deleted after
    /// [`STAGED_TTL_MS`].
    pub fn stage_api_key(&self, principal: &ActorId, key: Secret) -> Result<Id, SecretError> {
        let trimmed = key.expose().trim();
        if trimmed.is_empty() {
            return Err(SecretError::Failed("empty key".into()));
        }
        self.write_handle(principal, true, &Credential::ApiKey(Secret::new(trimmed)))
    }

    /// MCP bearer handles use a separate purpose marker inside this store's secret namespace.
    /// A remote MCP endpoint may never consume an ordinary provider connection handle.
    pub fn stage_mcp_api_key(&self, principal: &ActorId, key: Secret) -> Result<Id, SecretError> {
        let trimmed = key.expose().trim();
        if trimmed.is_empty() {
            return Err(SecretError::Failed("empty key".into()));
        }
        let minted = self.ids.next_id(IdKind::Credential);
        let handle = Id::new(format!("mcp-{}", minted.as_str()))
            .map_err(|_| SecretError::Failed("MCP credential handle unavailable".into()))?;
        self.write_handle_reserved(
            principal,
            true,
            &Credential::ApiKey(Secret::new(trimmed)),
            handle,
        )
    }

    /// Delete the stored secrets behind MCP bearer handles that a saved server referenced and no
    /// saved server references any more, such as a token a human removed or replaced. Intake
    /// leaves an MCP handle staged after it is saved, so the release clears that mark first;
    /// other handles are left alone.
    pub fn release_mcp_credentials(&self, handles: &[Id]) {
        let handles: Vec<Id> = handles
            .iter()
            .filter(|handle| handle.as_str().starts_with("mcp-"))
            .cloned()
            .collect();
        for handle in &handles {
            self.unstage(handle);
        }
        self.delete_unreferenced(&handles);
    }

    /// Writes `credential` under a new handle, registered as staged first so a concurrent
    /// reconcile never deletes it before it is published.
    pub(super) fn write_handle(
        &self,
        principal: &ActorId,
        api_key: bool,
        credential: &Credential,
    ) -> Result<Id, SecretError> {
        let handle = self.ids.next_id(IdKind::Credential);
        self.write_handle_reserved(principal, api_key, credential, handle)
    }

    pub(super) fn write_handle_reserved(
        &self,
        principal: &ActorId,
        api_key: bool,
        credential: &Credential,
        handle: Id,
    ) -> Result<Id, SecretError> {
        self.register_stage(&handle, principal, api_key)?;
        if let Err(e) = self.noted(
            self.secrets
                .put(&self.key(&handle), &credential.to_secret()),
        ) {
            self.live().staged.remove(handle.as_str());
            return Err(e);
        }
        if self.is_closed() {
            self.discard(&handle);
            return Err(SecretError::Failed(
                "provider connection owner closed after external secret write".into(),
            ));
        }
        Ok(handle)
    }

    pub(super) fn unstage(&self, handle: &Id) {
        self.live().staged.remove(handle.as_str());
    }

    /// Unstages and deletes a handle that will never be published.
    pub(super) fn discard(&self, handle: &Id) {
        self.unstage(handle);
        self.delete_unreferenced(std::slice::from_ref(handle));
    }

    /// Stages a host-entered settings credential for STAGED_TTL_MS. Retained revisions
    /// and proposals keep it alive; values remain only in this store's SecretStore namespace.
    pub fn stage_setting_secret(&self, value: Secret) -> Result<SecretRef, SecretError> {
        let trimmed = value.expose().trim();
        if trimmed.is_empty() {
            return Err(SecretError::Failed("empty value".into()));
        }
        let mut bytes = [0u8; 16];
        self.entropy.fill(&mut bytes);
        let reference = SecretRef::mint(u128::from_be_bytes(bytes));
        bytes.iter_mut().for_each(|b| *b = 0);
        let handle = reference_handle(&reference)
            .ok_or_else(|| SecretError::Failed("unrepresentable reference".into()))?;
        let reserved = handle.clone();
        let expires = self
            .now()
            .checked_add(STAGED_TTL_MS)
            .ok_or_else(|| SecretError::Failed("settings stage expiry overflow".into()))?;
        self.publication(move |tx| {
            tx.configuration()
                .reserve_settings_secret(&reserved, expires)
        })
        .map_err(|_| SecretError::Failed("settings credential intake unavailable".into()))?;
        if let Err(error) = self.noted(self.secrets.put(&self.key(&handle), &Secret::new(trimmed)))
        {
            let _ = self.collect_setting_handle(&handle, true);
            // Cleanup success does not make the failed intake available. Settings reads
            // this state to offer its session-only fallback after the completion.
            return self.noted(Err(error));
        }
        let published = handle.clone();
        if matches!(
            self.publication(move |tx| tx.configuration().publish_settings_secret(&published)),
            Ok(true)
        ) {
            return Ok(reference);
        }
        // Expiry, shutdown or an uncertain writer acknowledgement won. The reference
        // was never returned; retire it before cleaning a possibly completed external put.
        let _ = self.collect_setting_handle(&handle, true);
        Err(SecretError::Failed(
            "settings credential stage expired or closed".into(),
        ))
    }

    pub(super) fn collect_setting_handle(
        &self,
        handle: &Id,
        abandon: bool,
    ) -> Result<bool, SecretError> {
        let retired = handle.clone();
        let expires = self
            .now()
            .checked_add(STAGED_TTL_MS)
            .ok_or_else(|| SecretError::Failed("settings stage expiry overflow".into()))?;
        let ready = run_write(
            self.store.as_ref(),
            WriteOptions::housekeeping(),
            move |tx| {
                if referenced(tx.router())?.contains(retired.as_str()) {
                    return Ok(false);
                }
                tx.configuration()
                    .retire_settings_secret(&retired, expires, abandon)
            },
        )
        .map_err(|_| SecretError::Failed("settings credential collection unavailable".into()))?;
        if !ready {
            return Ok(false);
        }
        let deleted = self.noted(self.secrets.delete(&self.key(handle)))?;
        let finished = handle.clone();
        run_write(
            self.store.as_ref(),
            WriteOptions::housekeeping(),
            move |tx| {
                tx.configuration()
                    .finish_settings_secret_deletion(&finished)
            },
        )
        .map_err(|_| {
            SecretError::Failed("settings credential collection acknowledgement unavailable".into())
        })?;
        Ok(deleted)
    }

    /// Attaches the `code#state` value Claude showed to `principal`'s open browser sign-in, for
    /// the `CompleteSignIn` command that follows. It stays in memory only.
    pub fn submit_authorization_code(
        &self,
        principal: &ActorId,
        flow_id: &Id,
        pasted: Secret,
    ) -> Result<(), IntakeError> {
        let mut live = self.live();
        let now = self.now();
        let flow = live
            .flows
            .get_mut(flow_id.as_str())
            .filter(|f| f.active && &f.started_by == principal && f.expires_at_ms > now)
            .ok_or(IntakeError::UnknownFlow)?;
        match &mut flow.kind {
            FlowKind::Browser { pasted: slot, .. } => {
                *slot = Some(pasted);
                Ok(())
            }
            FlowKind::Device { .. } => Err(IntakeError::NotBrowserFlow),
        }
    }

    /// Deletes each handle that no connection references and no intake has staged.
    pub(super) fn delete_unreferenced(&self, handles: &[Id]) {
        if handles.is_empty() {
            return;
        }
        let Ok(refs) = self.read(|s| {
            let mut refs = referenced(s.router())?;
            refs.extend(crate::services::mcp::credential_references(s)?);
            Ok(refs)
        }) else {
            return;
        };
        for h in handles {
            let staged = self.live().staged.contains_key(h.as_str());
            if !refs.contains(h.as_str()) && !staged {
                // Best effort: a leftover is an orphan the next reconcile removes.
                let _ = self.noted(self.secrets.delete(&self.key(h)));
            }
        }
    }

    /// Deletes every handle of this store's namespace that no connection references and no
    /// intake has staged (expiring staged keys older than [`STAGED_TTL_MS`]). Never reads a value.
    pub fn reconcile(&self) -> Result<usize, SecretError> {
        let now = self.now();
        let listed = self.noted(self.secrets.list(&self.namespace()))?;
        let refs = self
            .read(|s| {
                let mut refs = referenced(s.router())?;
                refs.extend(crate::services::mcp::credential_references(s)?);
                Ok(refs)
            })
            .map_err(|e| SecretError::Failed(format!("{e:?}")))?;
        let mut removed = 0;
        for h in listed {
            if refs.contains(h.as_str()) {
                continue;
            }
            if h.as_str().starts_with(SETTING_SECRET_PREFIX) {
                if self.collect_setting_handle(&h, false)? {
                    removed += 1;
                }
                continue;
            }
            {
                let mut live = self.live();
                if let Some(s) = live.staged.get(h.as_str()) {
                    if now - s.created_at_ms < STAGED_TTL_MS {
                        continue;
                    }
                    live.staged.remove(h.as_str());
                }
            }
            // Re-read the references: a publish may have committed meanwhile.
            let still = self
                .read(|s| {
                    let mut refs = referenced(s.router())?;
                    refs.extend(crate::services::mcp::credential_references(s)?);
                    Ok(refs)
                })
                .map_err(|e| SecretError::Failed(format!("{e:?}")))?;
            if still.contains(h.as_str()) {
                continue;
            }
            if self.noted(self.secrets.delete(&self.key(&h)))? {
                removed += 1;
            }
        }
        self.live().last_reconcile_ms = Some(now);
        Ok(removed)
    }
}
