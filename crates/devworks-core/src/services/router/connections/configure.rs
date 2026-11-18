//! `router.configure` decisions for connections.
use super::*;

/// The fields of a `Connection` action.
pub(super) struct ConnectionEdit<'a> {
    id: Option<&'a Id>,
    expected: Option<i64>,
    name: &'a str,
    profile: &'a str,
    endpoint: &'a str,
    credential_ref: Option<&'a Id>,
    /// `None` keeps the stored value.
    enabled: Option<bool>,
    /// Explicit user billing policy; omission preserves the stored declaration.
    local_free: Option<bool>,
    /// `None` keeps the stored constraints.
    open_router: Option<&'a OpenRouterConstraints>,
}

impl ConnectionService {
    pub(super) fn decide_action(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        action: &RouterConfigAction,
        prep: Option<&Prep>,
    ) -> Result<DomainDecision, Failure> {
        // `router.configure` is human-only (HumanConfiguration); the binding of a risk decision
        // to a known human principal is rechecked here, never taken from the request.
        if ctx.principal_kind() != PrincipalKind::Human
            || tx.actors().actor_kind(ctx.principal())? != Some(PrincipalKind::Human)
        {
            return Err(refuse(
                DomainErrorCode::PermissionDenied,
                "human_configuration",
                None,
            ));
        }
        let actor = ctx.principal().clone();
        match action {
            RouterConfigAction::Connection {
                connection_id,
                expected_revision,
                name,
                profile: kind,
                endpoint,
                credential_ref,
                enabled,
                open_router,
                local_free,
            } => self.configure_connection(
                tx,
                &actor,
                ConnectionEdit {
                    id: connection_id.as_ref(),
                    expected: expected_revision.as_ref().map(|r| r.get()),
                    name: &name.0,
                    profile: &wire(kind),
                    endpoint: &endpoint.0,
                    credential_ref: credential_ref.as_ref(),
                    enabled: *enabled,
                    local_free: *local_free,
                    open_router: open_router.as_ref(),
                },
            ),
            RouterConfigAction::Model {
                model,
                expected_revision,
            } => configure_model(
                tx,
                &actor,
                model,
                expected_revision.as_ref().map(|r| r.get()),
            ),
            RouterConfigAction::Tier {
                tier,
                expected_revision,
                member_ids,
            } => configure_tier(tx, ctx, tier, expected_revision.get(), &member_ids.0),
            RouterConfigAction::DecisionModel {
                model,
                expected_revision,
            } => configure_decision_model(
                tx,
                &actor,
                model,
                expected_revision.as_ref().map(|r| r.get()),
            ),
            RouterConfigAction::SignIn {
                connection_id,
                action,
                flow_id,
            } => self.sign_in(tx, &actor, connection_id, action, flow_id.as_ref(), prep),
            RouterConfigAction::RemoveConnection {
                connection_id,
                expected_revision,
            } => self.remove_connection(tx, &actor, connection_id, expected_revision.get()),
            RouterConfigAction::SubscriptionRisk {
                connection_id,
                expected_revision,
                notice_sha256,
                accepted,
            } => self.subscription_risk(
                tx,
                &actor,
                connection_id,
                expected_revision.get(),
                &String::from(notice_sha256.clone()),
                *accepted,
            ),
            _ => Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                "embedding_configuration",
                None,
            )),
        }
    }

    pub(super) fn configure_connection(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        e: ConnectionEdit<'_>,
    ) -> Result<DomainDecision, Failure> {
        let p = profile(e.profile).ok_or_else(|| invalid("profile"))?;
        profiles::check_endpoint(p, e.endpoint).map_err(invalid)?;
        // OpenRouter constraints belong to an OpenRouter connection only; an empty set clears
        // them. They are stored in canonical form so an equal resend changes nothing.
        let constraints = match e.open_router {
            Some(_) if p.name != "OpenRouterChat" => return Err(invalid("open_router")),
            Some(o) => {
                check_constraints(o).map_err(invalid)?;
                let empty = o.allowed_models.0.is_empty()
                    && o.excluded_models.0.is_empty()
                    && o.cost_tier.is_none()
                    && o.provider.is_none();
                Some((!empty).then(|| canonical_constraints(o)).transpose()?)
            }
            None => None,
        };
        let now = tx.now_ms();
        let restore = tx.store().restore_epoch;
        let current = match (e.id, e.expected) {
            (None, Some(_)) => return Err(invalid("expected_revision")),
            (Some(id), Some(expected)) => {
                let c = tx
                    .router()
                    .connection(id)?
                    .ok_or_else(|| refuse(DomainErrorCode::NotFound, CONNECTION, Some(id)))?;
                if c.revision != expected {
                    return Err(Failure::Storage(StorageError::conflict(
                        RevisionDimension::Configuration,
                        CONNECTION,
                        id.clone(),
                        rev(expected),
                        rev(c.revision),
                    )));
                }
                Some(c)
            }
            (Some(id), None) => {
                if tx.router().connection(id)?.is_some() {
                    return Err(refuse(DomainErrorCode::Conflict, CONNECTION, Some(id)));
                }
                None
            }
            (None, None) => None,
        };
        let id = match (&current, e.id) {
            (Some(c), _) => c.id.clone(),
            (None, Some(id)) => id.clone(),
            (None, None) => tx.new_id(IdKind::Connection),
        };
        // A staged API key, only from the principal who staged it, only for an API-key profile.
        let staged = match e.credential_ref {
            Some(r) if current.as_ref().and_then(|c| c.credential_ref.as_ref()) == Some(r) => None,
            Some(r) => {
                if p.oauth() {
                    // Subscription credentials come only from their own sign-in.
                    return Err(refuse(
                        DomainErrorCode::InvalidArgument,
                        CREDENTIAL,
                        Some(r),
                    ));
                }
                let ok = self
                    .live()
                    .staged
                    .get(r.as_str())
                    .is_some_and(|s| s.api_key && &s.principal == actor);
                if !ok {
                    return Err(refuse(DomainErrorCode::NotFound, CREDENTIAL, Some(r)));
                }
                Some(r.clone())
            }
            None => None,
        };
        let mut next = match &current {
            Some(c) => c.clone(),
            None => StoredConnection {
                id: id.clone(),
                name: e.name.to_owned(),
                profile: p.name.to_owned(),
                endpoint: e.endpoint.to_owned(),
                enabled: true,
                local_free: false,
                open_router: None,
                credential_ref: None,
                credential_revision: self.first_credential_revision(&id),
                account_revision: next_account_revision(
                    tx.router().connection_account_revision(&id)?.unwrap_or(0),
                )?,
                refresh_operation_id: None,
                credential_epoch: None,
                lifecycle: without_credential(p).into(),
                state_reason: None,
                account_digest: None,
                access_expires_at_ms: None,
                risk: None,
                discovered: None,
                discovered_at_ms: None,
                healthy: None,
                checked_at_ms: None,
                revision: 1,
                created_at_ms: now,
                updated_at_ms: now,
            },
        };
        let profile_changed = current.as_ref().is_some_and(|c| c.profile != p.name);
        let endpoint_changed = current.as_ref().is_some_and(|c| c.endpoint != e.endpoint);
        let origin_changed = current
            .as_ref()
            .is_some_and(|c| profiles::origin(&c.endpoint) != profiles::origin(e.endpoint));
        if profile_changed && bound_models(tx.router(), &id)? {
            return Err(refuse(
                DomainErrorCode::Conflict,
                "connection_in_use",
                Some(&id),
            ));
        }
        next.name = e.name.to_owned();
        next.profile = p.name.to_owned();
        next.endpoint = e.endpoint.to_owned();
        if profile_changed || endpoint_changed {
            // Another endpoint lists other models and answers for itself.
            next.discovered = None;
            next.discovered_at_ms = None;
            next.healthy = None;
            next.checked_at_ms = None;
            next.risk = None;
        }
        if profile_changed {
            next.account_digest = None;
        }
        if let Some(enabled) = e.enabled {
            next.enabled = enabled;
        }
        if let Some(local_free) = e.local_free {
            next.local_free = local_free;
        }
        if let Some(o) = constraints {
            next.open_router = o;
        } else if p.name != "OpenRouterChat" {
            next.open_router = None;
        }
        let constraints_changed = current
            .as_ref()
            .is_some_and(|c| c.open_router != next.open_router);
        if constraints_changed {
            // Discovery is filtered by the constraints it was made under; list again.
            next.discovered = None;
            next.discovered_at_ms = None;
        }
        let mut credential_changed = false;
        if let Some(r) = &staged {
            next.credential_ref = Some(r.clone());
            next.credential_revision += 1;
            next.credential_epoch = Some(restore);
            next.lifecycle = "Connected".into();
            next.state_reason = None;
            next.access_expires_at_ms = None;
            forget_credential_facts(&mut next);
            credential_changed = true;
        } else if (profile_changed || origin_changed) && next.credential_ref.is_some() {
            // Never forward one profile's or origin's credential to another.
            next.credential_ref = None;
            next.credential_revision += 1;
            next.credential_epoch = None;
            next.lifecycle = without_credential(p).into();
            next.state_reason = None;
            next.access_expires_at_ms = None;
            forget_credential_facts(&mut next);
            credential_changed = true;
        } else if profile_changed || (endpoint_changed && next.credential_ref.is_none()) {
            // Another endpoint answers for itself.
            next.lifecycle = without_credential(p).into();
            next.state_reason = None;
        }
        if let Some(c) = &current {
            let same = c.name == next.name
                && c.profile == next.profile
                && c.endpoint == next.endpoint
                && c.credential_ref == next.credential_ref
                && c.enabled == next.enabled
                && c.local_free == next.local_free
                && c.open_router == next.open_router;
            if same {
                return Ok(unchanged(&c.id, c.revision));
            }
        }
        if (profile_changed || endpoint_changed) && !credential_changed && self.sign_in_pending(&id)
        {
            // The open sign-in is cancelled below; its publication must fail even if the
            // profile or endpoint changes back before it runs.
            next.credential_revision += 1;
        }
        if current.is_some() && (credential_changed || profile_changed || endpoint_changed) {
            next.account_revision = next_account_revision(next.account_revision)?;
        }
        next.updated_at_ms = now;
        let saved = tx
            .router()
            .put_connection(&next, current.as_ref().map(|c| c.revision))?;
        if credential_changed || profile_changed || endpoint_changed {
            self.cancel_flows(&saved.id);
            clear_profile_cooldowns(tx, &saved)?;
        }
        event(
            tx,
            actor,
            (CONNECTION_AGGREGATE, &saved.id, saved.revision),
            if current.is_some() {
                "ConnectionConfigured"
            } else {
                "ConnectionCreated"
            },
            connection_payload(&saved),
        )?;
        Ok(applied(&saved.id, saved.revision))
    }

    pub(super) fn remove_connection(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        id: &Id,
        expected: i64,
    ) -> Result<DomainDecision, Failure> {
        let c = tx
            .router()
            .connection(id)?
            .ok_or_else(|| refuse(DomainErrorCode::NotFound, CONNECTION, Some(id)))?;
        if c.revision != expected {
            return Err(Failure::Storage(StorageError::conflict(
                RevisionDimension::Configuration,
                CONNECTION,
                id.clone(),
                rev(expected),
                rev(c.revision),
            )));
        }
        if bound_models(tx.router(), id)? {
            return Err(refuse(
                DomainErrorCode::Conflict,
                "connection_in_use",
                Some(id),
            ));
        }
        tx.router().delete_connection(id, expected)?;
        self.cancel_flows(id);
        event(
            tx,
            actor,
            (CONNECTION_AGGREGATE, id, expected + 1),
            "ConnectionRemoved",
            connection_payload(&c),
        )?;
        Ok(applied(id, expected + 1))
    }

    pub(super) fn subscription_risk(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        id: &Id,
        expected: i64,
        notice: &str,
        accepted: bool,
    ) -> Result<DomainDecision, Failure> {
        let c = tx
            .router()
            .connection(id)?
            .ok_or_else(|| refuse(DomainErrorCode::NotFound, CONNECTION, Some(id)))?;
        let p = profile(&c.profile).ok_or_else(|| invalid("profile"))?;
        if !p.risk_gated {
            return Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                "subscription_risk",
                Some(id),
            ));
        }
        if c.revision != expected {
            return Err(Failure::Storage(StorageError::conflict(
                RevisionDimension::Configuration,
                CONNECTION,
                id.clone(),
                rev(expected),
                rev(c.revision),
            )));
        }
        // Only the exact current notice can be accepted.
        if accepted && notice != profiles::risk_notice_sha256() {
            return Err(Refusal::new(
                DomainErrorCode::SourceChanged,
                "risk_notice",
                Some(id.clone()),
            )
            .retry(RetryAdvice::RefreshAndNewRequest)
            .into());
        }
        let restore = tx.store().restore_epoch;
        let effective = risk_effective(&c, restore);
        if accepted == effective && (accepted || c.risk.is_none()) {
            return Ok(unchanged(id, c.revision));
        }
        let now = tx.now_ms();
        let mut next = c.clone();
        next.risk = accepted.then(|| StoredRiskAcceptance {
            notice_sha256: profiles::risk_notice_sha256(),
            binding: profiles::risk_binding(&c.profile, &c.endpoint, c.account_digest.as_deref()),
            restore_epoch: restore,
            accepted_by: actor.0.as_str().to_owned(),
            accepted_at_ms: now,
        });
        if !accepted && self.sign_in_pending(id) {
            // The open sign-in is cancelled below: refuse its publication durably.
            next.credential_revision += 1;
        }
        next.updated_at_ms = now;
        let saved = tx.router().put_connection(&next, Some(c.revision))?;
        if !accepted {
            self.cancel_flows(id);
        }
        event(
            tx,
            actor,
            (CONNECTION_AGGREGATE, id, saved.revision),
            if accepted {
                "SubscriptionRiskAccepted"
            } else {
                "SubscriptionRiskRevoked"
            },
            json!({
                "connectionId": id.as_str(),
                "noticeSha256": profiles::risk_notice_sha256(),
                "accepted": accepted,
            }),
        )?;
        Ok(applied(id, saved.revision))
    }
}
