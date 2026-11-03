//! [`HostAuthority`], the production [`AuthorityPort`]: current authority resolved from host
//! records (principal bindings, grants, effective settings), never from request fields; plus the
//! transaction-scoped seams through which the host creates and revokes authority.
use super::policy::{
    self, AccessProfile, DenialReason, EffectivePolicy, Provenance, Verdict, resolve_policy,
};
use crate::context::{InvocationContext, PrincipalKind};
use crate::ports::storage::{
    ApplicationStore, AuthorityGrant, AuthorityRead, ConfigurationRead, GrantKind, GrantState,
    NewAuthorityGrant, NewEvent, NewPrincipalBinding, PrincipalBinding, ReadOptions, ReadSnapshot,
    StorageError, WriteTransaction, read_snapshot,
};
use crate::ports::{
    AuthorityDenial, AuthorityFence, AuthorityPort, AuthorityRequest, Authorization, Clock,
    GrantUse, IdKind,
};
use crate::protocol::{ActorId, EventId, Id, ProjectId, RootId};
use crate::registry::{self, Mode};
use crate::settings::catalog::AgentRole;
use crate::settings::service::PrincipalRoles;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

/// History aggregate of a principal's authority binding (`aggregate_id` = actor ID).
pub const PRINCIPAL_AGGREGATE: &str = "principal_authority";
/// History aggregate of a host-minted grant (`aggregate_id` = grant ID).
pub const GRANT_AGGREGATE: &str = "authority_grant";
/// History aggregate of a denied command (`aggregate_id` = the rejection's correlation ID).
pub const DENIAL_AGGREGATE: &str = "authority_denial";
/// Longest lifetime of an operation or native grant (engineering default).
pub const MAX_GRANT_LIFETIME_MS: i64 = 24 * 60 * 60 * 1000;
/// Most uses one grant may carry (engineering default).
pub const MAX_GRANT_USES: i64 = 1000;

/// Explicit, bounded lifetime and use count of a grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GrantBounds {
    pub expires_at_ms: i64,
    pub uses: i64,
}

/// What an `Operation` or `NativeExecution` grant covers. Every present member must match the
/// request; `request_digest` binds one exact normalized request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrantBinding {
    pub operation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_id: Option<RootId>,
    /// Digest of the effective access policy the grant was reviewed under; a changed policy
    /// invalidates it. Required for native grants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_digest: Option<String>,
}
impl GrantBinding {
    fn covers(
        &self,
        ctx: &InvocationContext,
        request: &AuthorityRequest<'_>,
        exact: bool,
        policy_digest: &str,
    ) -> bool {
        if exact && self.request_digest.is_none() {
            return false;
        }
        if self
            .policy_digest
            .as_deref()
            .is_some_and(|d| d != policy_digest)
        {
            return false;
        }
        self.operation == request.descriptor.name
            && self
                .action
                .as_deref()
                .is_none_or(|a| Some(a) == request.action)
            && self
                .request_digest
                .as_deref()
                .is_none_or(|d| d == request.fingerprint.as_str())
            && self
                .project_id
                .as_ref()
                .is_none_or(|p| Some(p) == ctx.project_id())
            && self
                .root_id
                .as_ref()
                .is_none_or(|r| Some(r) == ctx.root_id())
    }
}

/// A refusal with the reason the UI and history show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Denied {
    pub denial: AuthorityDenial,
    pub reason: DenialReason,
}
fn deny(reason: DenialReason) -> Denied {
    Denied {
        denial: AuthorityDenial::Policy(reason),
        reason,
    }
}

/// Effective permissions of one principal, as shown in Settings and with blocked requests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectivePermissions {
    pub principal: ActorId,
    pub kind: PrincipalKind,
    pub role: Option<AgentRole>,
    /// Whether the host has bound and not revoked this principal (always true for the user and
    /// the host).
    pub active: bool,
    pub project_id: Option<ProjectId>,
    pub tool_families: Option<Vec<String>>,
    /// `None` when the configured profile is unsupported (every agent command is refused).
    pub profile: Option<AccessProfile>,
    pub profile_source: Option<Provenance>,
    /// The enforcement boundary actually provided.
    pub guarantee: &'static str,
    pub shell_tools: bool,
    pub web_tools: bool,
    /// Restrictions that hold under every profile.
    pub mandatory: Vec<&'static str>,
}

/// Current host authority over the storage ports.
pub struct HostAuthority<'a> {
    pub store: &'a dyn ApplicationStore,
    pub clock: &'a dyn Clock,
    /// Branch names protected in addition to `main`/`master`, for example remote default
    /// branches the Git adapter knows.
    pub protected_branches: &'a [String],
}

impl HostAuthority<'_> {
    /// Resolves current authority for one request, with the reason when refused. Any failure to
    /// read current records refuses (fail closed).
    pub fn decide(
        &self,
        ctx: &InvocationContext,
        request: &AuthorityRequest<'_>,
    ) -> Result<Authorization, Denied> {
        let now = self.clock.now_ms().get();
        read_snapshot(self.store, &ReadOptions::default(), |snapshot| {
            Ok(decide_in(
                snapshot,
                ctx,
                request,
                now,
                self.protected_branches,
            ))
        })
        .unwrap_or(Err(deny(DenialReason::AuthorityUnavailable)))
    }

    /// The principal's effective permissions in its invocation scope.
    pub fn effective_permissions(
        &self,
        ctx: &InvocationContext,
    ) -> Result<EffectivePermissions, StorageError> {
        read_snapshot(self.store, &ReadOptions::default(), |s| {
            let binding = bound(s, ctx)?;
            let policy = resolve_policy(s.configuration(), ctx.project_id())?;
            let host_or_human = matches!(
                ctx.principal_kind(),
                PrincipalKind::Human | PrincipalKind::Host
            );
            Ok(EffectivePermissions {
                principal: ctx.principal().clone(),
                kind: ctx.principal_kind(),
                role: binding.as_ref().and_then(|b| b.role),
                active: host_or_human || binding.as_ref().is_some_and(|b| b.active),
                project_id: binding.as_ref().and_then(|b| b.project_id.clone()),
                tool_families: binding.as_ref().and_then(|b| b.tool_families.clone()),
                guarantee: policy.profile.map_or(
                    "The configured access profile is unsupported; agent actions are refused.",
                    AccessProfile::guarantee,
                ),
                profile: policy.profile,
                profile_source: policy.profile_source,
                shell_tools: policy.shell_tools,
                web_tools: policy.web_tools,
                mandatory: vec![
                    "Code review agents are read-only",
                    "Explicit approvals are required where configured",
                    "Main is never force-pushed",
                    "Destructive operations are checked before dispatch",
                    "Primary repositories are deleted only through your dedicated confirmation",
                ],
            })
        })
    }
}

impl AuthorityPort for HostAuthority<'_> {
    fn authorize(
        &self,
        ctx: &InvocationContext,
        request: &AuthorityRequest<'_>,
    ) -> Result<Authorization, AuthorityDenial> {
        self.decide(ctx, request).map_err(|d| d.denial)
    }
}

fn bound(
    s: &dyn ReadSnapshot,
    ctx: &InvocationContext,
) -> Result<Option<PrincipalBinding>, StorageError> {
    match ctx.principal_kind() {
        PrincipalKind::Human | PrincipalKind::Host => Ok(None),
        _ => s.authority().principal(ctx.principal()),
    }
}

fn decide_in(
    s: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    request: &AuthorityRequest<'_>,
    now: i64,
    protected_branches: &[String],
) -> Result<Authorization, Denied> {
    let unavailable = |_: StorageError| deny(DenialReason::AuthorityUnavailable);
    let stamp = s.stamp();
    if &stamp.store_id != ctx.store_id() || stamp.restore_epoch != ctx.restore_epoch().get() {
        return Err(deny(DenialReason::StaleInvocation));
    }
    if s.actors()
        .actor_kind(ctx.principal())
        .map_err(unavailable)?
        != Some(ctx.principal_kind())
    {
        return Err(deny(DenialReason::UnregisteredPrincipal));
    }
    let binding = bound(s, ctx).map_err(unavailable)?;
    let policy = resolve_policy(s.configuration(), ctx.project_id()).map_err(unavailable)?;
    let verdict = policy::evaluate(ctx, binding.as_ref(), &policy, request, protected_branches);
    let digest = policy.digest();
    let fences = fences_for(ctx, binding.as_ref(), &policy);
    match verdict {
        Verdict::Allow => Ok(Authorization::direct().fenced_by(fences)),
        Verdict::Deny(reason) => Err(deny(reason)),
        Verdict::NeedsGrant {
            kind,
            exact,
            reason,
        } => {
            let mut replayable = false;
            for source in ctx.authority_sources() {
                let Some(grant) = s.authority().authority_grant(source).map_err(unavailable)?
                else {
                    continue;
                };
                if grant.kind != kind || &grant.actor != ctx.principal() {
                    continue;
                }
                let Ok(binding) = serde_json::from_value::<GrantBinding>(grant.binding.clone())
                else {
                    continue;
                };
                if !binding.covers(ctx, request, exact, &digest) {
                    continue;
                }
                if grant.usable_by(ctx.principal(), now) {
                    return Ok(Authorization::direct()
                        .spending(vec![GrantUse {
                            grant_id: grant.id,
                            expected_revision: grant.revision,
                        }])
                        .fenced_by(fences));
                }
                // A spent or expired (never a revoked) grant still lets its holder resolve the
                // receipt of a command it already authorized, e.g. after a lost acknowledgment.
                replayable |= grant.state != GrantState::Revoked;
            }
            if replayable {
                return Ok(Authorization::direct().fenced_by(fences).replay_only());
            }
            Err(deny(reason))
        }
    }
}

fn fences_for(
    ctx: &InvocationContext,
    binding: Option<&PrincipalBinding>,
    policy: &EffectivePolicy,
) -> Vec<AuthorityFence> {
    let mut fences = Vec::new();
    if let Some(b) = binding {
        fences.push(AuthorityFence::Principal {
            actor: b.actor.clone(),
            revision: b.revision,
        });
    }
    if !matches!(
        ctx.principal_kind(),
        PrincipalKind::Human | PrincipalKind::Host
    ) {
        fences.push(AuthorityFence::Policy {
            project_id: ctx.project_id().cloned(),
            digest: policy.digest(),
        });
    }
    fences
}

fn principal_holds(
    read: &dyn AuthorityRead,
    actor: &ActorId,
    revision: i64,
) -> Result<bool, StorageError> {
    Ok(read
        .principal(actor)?
        .is_some_and(|b| b.active && b.revision == revision))
}
fn policy_holds(
    read: &dyn ConfigurationRead,
    project: Option<&ProjectId>,
    digest: &str,
) -> Result<bool, StorageError> {
    Ok(resolve_policy(read, project)?.digest() == digest)
}

/// Re-checks every fence inside the command's write transaction (the commit linearization
/// point). Called by the gateway before the receipt lookup.
pub fn fences_hold(
    tx: &mut dyn WriteTransaction,
    fences: &[AuthorityFence],
) -> Result<bool, StorageError> {
    for fence in fences {
        let holds = match fence {
            AuthorityFence::Principal { actor, revision } => {
                principal_holds(tx.authority(), actor, *revision)?
            }
            AuthorityFence::Policy { project_id, digest } => {
                policy_holds(tx.configuration(), project_id.as_ref(), digest)?
            }
        };
        if !holds {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Re-checks fences and the grants an authorization relies on against a fresh snapshot (the
/// publication point of a read). A grant revoked or changed since authorization denies output.
pub fn authorization_holds(
    s: &dyn ReadSnapshot,
    authorization: &Authorization,
) -> Result<bool, StorageError> {
    for fence in authorization.fences() {
        let holds = match fence {
            AuthorityFence::Principal { actor, revision } => {
                principal_holds(s.authority(), actor, *revision)?
            }
            AuthorityFence::Policy { project_id, digest } => {
                policy_holds(s.configuration(), project_id.as_ref(), digest)?
            }
        };
        if !holds {
            return Ok(false);
        }
    }
    for g in authorization.authority_grants() {
        let current = s.authority().authority_grant(&g.grant_id)?;
        if !current
            .is_some_and(|c| c.revision == g.expected_revision && c.state == GrantState::Active)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

// ---------------------------------------------------------------------------------------------
// Host seams that create and revoke authority

/// Why an authority administration request was refused.
#[derive(Clone, Debug)]
pub enum AdminError {
    Denied(DenialReason),
    Invalid(&'static str),
    Storage(StorageError),
}
impl From<StorageError> for AdminError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}

fn require_kind(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    allowed: &[PrincipalKind],
) -> Result<(), AdminError> {
    let kind = ctx.principal_kind();
    if !allowed.contains(&kind) {
        return Err(AdminError::Denied(DenialReason::HumanOnly));
    }
    if tx.actors().actor_kind(ctx.principal())? != Some(kind) {
        return Err(AdminError::Denied(DenialReason::UnregisteredPrincipal));
    }
    Ok(())
}

/// Appends one attributed authority history event (a principal, grant or repository-deletion
/// record, and nothing else) through the capability-gated history the gateway's handler view
/// allows only for the ingress principal. An agent invocation stays attributed to the agent;
/// linked human authorization sources are recorded beside it.
pub(crate) fn record(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    aggregate_kind: &str,
    aggregate_id: &str,
    aggregate_revision: i64,
    kind: &str,
    mut payload: Value,
) -> Result<i64, StorageError> {
    if ![
        PRINCIPAL_AGGREGATE,
        GRANT_AGGREGATE,
        super::deletion::DELETION_AGGREGATE,
    ]
    .contains(&aggregate_kind)
    {
        return Err(StorageError::InvalidRecord(format!(
            "{aggregate_kind} is not an authority history aggregate"
        )));
    }
    if let Some(map) = payload.as_object_mut() {
        map.insert(
            "principalKind".into(),
            json!(format!("{:?}", ctx.principal_kind())),
        );
        map.insert("invocationId".into(), json!(ctx.invocation_id().as_str()));
        let sources: Vec<&str> = ctx.authority_sources().iter().map(Id::as_str).collect();
        if !sources.is_empty() {
            map.insert("authoritySources".into(), json!(sources));
        }
    }
    let id = EventId(tx.new_id(IdKind::Event));
    tx.authority_history(&super::CAPABILITY, ctx.principal())?
        .append(NewEvent {
            id,
            actor: ctx.principal().clone(),
            project_id: None,
            root_id: None,
            operation_id: None,
            aggregate_kind: aggregate_kind.into(),
            aggregate_id: aggregate_id.into(),
            aggregate_revision,
            kind: kind.into(),
            payload,
        })
}

/// Binds (or re-binds at `expected_revision`) a non-human principal's authority. Only the user
/// or the host creates authority; `bound_by` is always the caller, whatever the input says.
pub fn bind_principal(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    mut binding: NewPrincipalBinding,
    expected_revision: Option<i64>,
) -> Result<PrincipalBinding, AdminError> {
    require_kind(tx, ctx, &[PrincipalKind::Human, PrincipalKind::Host])?;
    match (binding.kind, binding.role) {
        (PrincipalKind::Human | PrincipalKind::Host, _) => {
            return Err(AdminError::Invalid("the user and the host are not bound"));
        }
        (PrincipalKind::Agent, None) => return Err(AdminError::Invalid("an agent needs a role")),
        (PrincipalKind::Agent, Some(_)) | (_, None) => {}
        (_, Some(_)) => return Err(AdminError::Invalid("only agents have roles")),
    }
    if let Some(families) = &binding.tool_families
        && families
            .iter()
            .any(|f| !registry::families().contains(&f.as_str()))
    {
        return Err(AdminError::Invalid("unknown tool family"));
    }
    binding.bound_by = ctx.principal().clone();
    let actor = binding.actor.clone();
    let binding = tx
        .authority_writes(&super::CAPABILITY, ctx.principal())?
        .bind_principal(binding, expected_revision)?;
    record(
        tx,
        ctx,
        PRINCIPAL_AGGREGATE,
        actor.0.as_str(),
        binding.revision,
        "PrincipalAuthorityBound",
        json!({
            "role": binding.role.map(AgentRole::as_str),
            "projectId": binding.project_id.as_ref().map(|p| p.0.as_str()),
            "toolFamilies": binding.tool_families,
        }),
    )?;
    Ok(binding)
}

/// Revokes a principal's authority at `expected_revision`; every authorization resolved earlier
/// is fenced at its commit or publication point.
pub fn revoke_principal(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    actor: &ActorId,
    expected_revision: i64,
) -> Result<PrincipalBinding, AdminError> {
    require_kind(tx, ctx, &[PrincipalKind::Human, PrincipalKind::Host])?;
    let binding = tx
        .authority_writes(&super::CAPABILITY, ctx.principal())?
        .revoke_principal(actor, expected_revision)?;
    record(
        tx,
        ctx,
        PRINCIPAL_AGGREGATE,
        actor.0.as_str(),
        binding.revision,
        "PrincipalAuthorityRevoked",
        json!({}),
    )?;
    Ok(binding)
}

/// Mints an `Operation` or `NativeExecution` grant from the user's trusted local action. The
/// holder may be an agent: it can exercise the grant only within its binding. Every grant has
/// an explicit expiry within [`MAX_GRANT_LIFETIME_MS`] and 1..=[`MAX_GRANT_USES`] uses. Native
/// grants bind one exact request, a project and the effective policy digest of that project.
/// Primary repository deletion has its own confirmation seam.
pub fn mint_grant(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    holder: ActorId,
    kind: GrantKind,
    binding: GrantBinding,
    bounds: GrantBounds,
) -> Result<AuthorityGrant, AdminError> {
    let command = registry::find(&binding.operation).and_then(|d| d.binding(Mode::Command));
    if command.is_none() {
        return Err(AdminError::Invalid("grants bind a registered command"));
    }
    match kind {
        GrantKind::Operation => {}
        GrantKind::NativeExecution => {
            if binding.request_digest.is_none()
                || !policy::NATIVE_EXECUTION.contains(&binding.operation.as_str())
            {
                return Err(AdminError::Invalid(
                    "a native grant binds one exact native execution request",
                ));
            }
            let Some(project) = binding.project_id.as_ref() else {
                return Err(AdminError::Invalid("a native grant binds its project"));
            };
            let current = resolve_policy(tx.configuration(), Some(project))?.digest();
            if binding.policy_digest.as_deref() != Some(current.as_str()) {
                return Err(AdminError::Invalid(
                    "a native grant binds the current effective policy of its project",
                ));
            }
        }
        GrantKind::PrimaryRepositoryDeletion => {
            return Err(AdminError::Invalid(
                "primary repository deletion uses its dedicated confirmation",
            ));
        }
    }
    let binding = serde_json::to_value(&binding).expect("grant binding");
    issue(
        tx,
        ctx,
        holder,
        kind,
        binding,
        bounds,
        MAX_GRANT_LIFETIME_MS,
    )
}

/// Shared minting: user only, registered holder, bounded lifetime and uses, attributed history.
pub(crate) fn issue(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    holder: ActorId,
    kind: GrantKind,
    binding: Value,
    bounds: GrantBounds,
    max_lifetime_ms: i64,
) -> Result<AuthorityGrant, AdminError> {
    require_kind(tx, ctx, &[PrincipalKind::Human])?;
    if tx.actors().actor_kind(&holder)?.is_none() {
        return Err(AdminError::Invalid("the grant holder is not registered"));
    }
    let now = tx.now_ms();
    if !(1..=MAX_GRANT_USES).contains(&bounds.uses)
        || bounds.expires_at_ms <= now
        || bounds.expires_at_ms > now.saturating_add(max_lifetime_ms)
    {
        return Err(AdminError::Invalid(
            "a grant needs an explicit, bounded expiry and use count",
        ));
    }
    let (expires_at_ms, uses) = (Some(bounds.expires_at_ms), Some(bounds.uses));
    let id = tx.new_id(IdKind::Grant);
    let grant = tx
        .authority_writes(&super::CAPABILITY, ctx.principal())?
        .issue_grant(NewAuthorityGrant {
            id: id.clone(),
            actor: holder.clone(),
            kind,
            issued_by: ctx.principal().clone(),
            binding: binding.clone(),
            expires_at_ms,
            uses,
        })?;
    record(
        tx,
        ctx,
        GRANT_AGGREGATE,
        id.as_str(),
        grant.revision,
        "AuthorityGrantIssued",
        json!({
            "kind": kind.as_str(),
            "holder": holder.0.as_str(),
            "binding": binding,
            "expiresAtMs": expires_at_ms,
            "uses": uses,
        }),
    )?;
    Ok(grant)
}

/// Revokes a grant at `expected_revision`. A command that resolved it earlier is denied at its
/// commit (the consumption compare-and-set fails) and a read is denied before publication.
pub fn revoke_grant(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    id: &Id,
    expected_revision: i64,
) -> Result<AuthorityGrant, AdminError> {
    require_kind(tx, ctx, &[PrincipalKind::Human, PrincipalKind::Host])?;
    let grant = tx
        .authority_writes(&super::CAPABILITY, ctx.principal())?
        .revoke_grant(id, expected_revision, ctx.principal())?;
    record(
        tx,
        ctx,
        GRANT_AGGREGATE,
        id.as_str(),
        grant.revision,
        "AuthorityGrantRevoked",
        json!({"kind": grant.kind.as_str()}),
    )?;
    Ok(grant)
}

/// The production [`PrincipalRoles`]: an agent's role is its current, active host binding. Every
/// other case (not an agent, no binding, a revoked or mis-kinded binding, a binding without a
/// role, or records that cannot be read) is unresolved (`None`), which the settings use cases
/// refuse as `role_unresolved`; a missing record can never widen a settings write.
pub struct HostRoles {
    pub store: Arc<dyn ApplicationStore>,
}
impl PrincipalRoles for HostRoles {
    fn role(&self, ctx: &InvocationContext) -> Option<AgentRole> {
        if ctx.principal_kind() != PrincipalKind::Agent {
            return None;
        }
        let bound = read_snapshot(&*self.store, &ReadOptions::default(), |s| {
            s.authority().principal(ctx.principal())
        })
        .ok()
        .flatten()?;
        (bound.active && bound.kind == PrincipalKind::Agent)
            .then_some(bound.role)
            .flatten()
    }
}
