//! The ConfigurationService use cases behind `settings.read_effective`, `settings.validate`,
//! `settings.save_revision` and `settings.explain_source`, plus the settings-owned proposal
//! lifecycle (stage → reconcile → approve/reject) and rollback.
//!
//! Agent drafting of settings and role instructions reaches users through the proposal commands
//! owned by the approvals and roles services (`approvals.propose`, `roles.propose`), and human
//! decisions through `approvals.record_human_decision` and `roles.rollback`. Those handlers call
//! the transaction-scoped functions here, so proposals have one store and one set of rules.
//!
//! Every surface reaches the four operations through the gateway with [`SettingsService`] as the
//! handler registered in [`crate::services::UseCases::with_settings`]; the proposal and rollback
//! functions are transaction-scoped so the owning decision operations
//! (`approvals.record_human_decision`, `roles.rollback`) run them inside their own gateway unit
//! of work. There is no second implementation of any rule.
//!
//! Principal invariants enforced here, on top of the gateway's authority port:
//! * a principal whose host-resolved role is read-only (code reviewers) can never write or
//!   propose settings, and an agent whose role cannot be resolved is refused the same way
//!   (fail closed);
//! * agents never apply settings: an agent `settings.save_revision` is a receipt-only
//!   `ApprovalRequired` refusal that writes and stages nothing; agents draft changes through the
//!   proposal command, which calls [`stage_proposal`];
//! * only a host-bound human principal applies, approves, rejects, reconciles or rolls back, and
//!   nobody approves content they authored: the proposal's author and every human who
//!   reconciled or edited it are recorded as content authors and none of them may approve it;
//! * harness-enforced policies are not values: every write to them is refused, and instruction
//!   text is typed text that cannot carry tool grants or policy;
//! * credential-typed settings accept only host-minted secret-store references
//!   ([`super::validate::is_secret_ref`]).
use super::catalog::{Adoption, AgentRole, catalog, descriptor};
use super::model::{
    self, AdoptedRevisions, Layer, NAMESPACE, Plan, body, chain, current_chain, current_layer,
    effective_values, layer_at, layers_at, plan, restore_assignments,
};
use super::validate::{Issue, codes};
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, settings as ops};
use crate::ports::storage::{
    ConfigRevision, ConfigScope, ConfigurationRead, NewEvent, NewProposal, ProposalChange,
    ProposalRecord, ProposalState, ReadSnapshot, StorageError, WriteTransaction,
};
use crate::ports::{
    CommandHandler, Decided, HandlerFailure, IdKind, IdSource, QueryAnswer, QueryHandler,
};
use crate::protocol::records::{
    SchemaValue, SettingAssignment, SettingEntry, ValidationIssue, ValidationReport,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, DynamicJson, Page};
use crate::protocol::{
    CorrelationId, DomainErrorCode, DomainFault, EventId, HoldId, Id, ProjectId, RetryAdvice,
    Revision, RevisionDimension,
};
use serde_json::{Value, json};
use std::sync::Arc;

/// History aggregate of a scope's configuration revisions (`aggregate_id` = scope key).
pub const REVISION_AGGREGATE: &str = "configuration_scope";
/// History aggregate of a settings proposal (`aggregate_id` = proposal ID). Approval inboxes and
/// notifications observe proposals through these events.
pub const PROPOSAL_AGGREGATE: &str = "settings_proposal";
/// Fault target kind of an agent `settings.save_revision`: the change must be drafted through
/// the proposal command instead, which stages it with [`stage_proposal`].
pub const PROPOSAL_COMMAND: &str = "approvals.propose";

/// The four operations [`SettingsService`] is registered for.
pub const OPERATIONS: [&str; 4] = [
    "settings.read_effective",
    "settings.validate",
    "settings.save_revision",
    "settings.explain_source",
];

/// Host-resolved role of an agent invocation (from its assignment/session records), never from
/// request fields. The production implementation is [`crate::authority::HostRoles`] (the
/// principal's current host binding); role enforcement for every operation happens earlier in
/// [`crate::authority::HostAuthority`]. Settings only needs to know whether the principal is a
/// read-only role; `None` means the role is unresolved and every settings write fails closed.
pub trait PrincipalRoles: Send + Sync {
    fn role(&self, ctx: &InvocationContext) -> Option<AgentRole>;
}

/// Runtime availability of a directory, kept separate from its configured value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathHealth {
    Available,
    Missing,
    NotADirectory,
    NotWritable,
    Unknown(String),
}
impl PathHealth {
    pub fn describe(&self) -> String {
        match self {
            Self::Available => "available".into(),
            Self::Missing => "does not exist or is not mounted".into(),
            Self::NotADirectory => "is not a directory".into(),
            Self::NotWritable => "is not writable".into(),
            Self::Unknown(e) => format!("could not be checked: {e}"),
        }
    }
}
/// Platform directory inspection (OS path adapter).
pub trait PathProbe: Send + Sync {
    fn probe(&self, path: &str) -> PathHealth;
}

/// A typed refusal the settings use cases return. Becomes a durable `Refused` decision (or a
/// query fault) with a correlation ID allocated by the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub code: DomainErrorCode,
    pub retry: RetryAdvice,
    pub target_kind: &'static str,
    pub target_id: Option<Id>,
    pub blockers: Vec<HoldId>,
}
impl Refusal {
    pub fn new(code: DomainErrorCode, target_kind: &'static str, target_id: Option<Id>) -> Self {
        Self {
            code,
            retry: RetryAdvice::Never,
            target_kind,
            target_id,
            blockers: Vec::new(),
        }
    }
    pub fn fault(self, correlation_id: CorrelationId) -> DomainFault {
        DomainFault {
            code: self.code,
            retry: self.retry,
            correlation_id,
            target_id: self.target_id,
            target_kind: Some(BoundedText(self.target_kind.to_owned())),
            revision_dimension: None,
            expected_revision: None,
            current_revision: None,
            blocker_ids: BoundedList(self.blockers),
            diagnostic_id: None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Failure {
    Refused(Refusal),
    /// A storage failure. `RevisionConflict` is the stale-revision outcome (`Configuration`
    /// dimension); the gateway records it as a durable refusal after undoing partial writes.
    Storage(StorageError),
}
impl From<StorageError> for Failure {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}
impl From<Refusal> for Failure {
    fn from(r: Refusal) -> Self {
        Self::Refused(r)
    }
}

fn denied(kind: &'static str) -> Failure {
    Refusal::new(DomainErrorCode::PermissionDenied, kind, None).into()
}

/// The fault for a plan's blocking issues. Credentials, harness policies and schema mismatches
/// keep distinct typed codes/targets so callers never have to parse messages.
fn plan_refusal(issues: &[&Issue], scope: &ConfigScope) -> Refusal {
    let has = |code: &str| issues.iter().any(|i| i.code == code);
    let (code, kind) = if has(codes::RAW_CREDENTIAL) {
        (DomainErrorCode::InvalidArgument, "credential_value")
    } else if has(codes::HARNESS_ENFORCED) {
        (DomainErrorCode::PermissionDenied, "harness_policy")
    } else if has(codes::SCHEMA_MISMATCH) || has(codes::STORED_UNKNOWN) {
        (DomainErrorCode::SchemaMismatch, "setting_schema")
    } else {
        (DomainErrorCode::InvalidArgument, "setting_value")
    };
    Refusal::new(code, kind, Some(scope.target_id()))
}

fn stale(scope: &ConfigScope, expected: i64, current: i64) -> Failure {
    Failure::Storage(StorageError::conflict(
        RevisionDimension::Configuration,
        "configuration_scope",
        scope.target_id(),
        revision(expected),
        revision(current),
    ))
}
fn revision(n: i64) -> Revision {
    Revision::new(n.max(1)).expect("positive")
}

fn require_scope(read: &dyn ConfigurationRead, scope: &ConfigScope) -> Result<(), Failure> {
    if read.scope_exists(scope)? {
        return Ok(());
    }
    Err(Refusal::new(
        DomainErrorCode::NotFound,
        "project",
        Some(scope.target_id()),
    )
    .into())
}

/// A host-bound human decision: the invocation is human and the store knows the principal as a
/// human. A model message or tool argument cannot produce either.
fn require_human(tx: &mut dyn WriteTransaction, ctx: &InvocationContext) -> Result<(), Failure> {
    if ctx.principal_kind() == PrincipalKind::Human
        && tx.actors().actor_kind(ctx.principal())? == Some(PrincipalKind::Human)
    {
        return Ok(());
    }
    Err(denied("human_decision"))
}

fn event(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    scope: &ConfigScope,
    aggregate: (&str, String, i64),
    kind: &str,
    payload: Value,
) -> Result<(), StorageError> {
    let id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id,
        actor: ctx.principal().clone(),
        project_id: scope.project_id().cloned(),
        root_id: None,
        operation_id: None,
        aggregate_kind: aggregate.0.to_owned(),
        aggregate_id: aggregate.1,
        aggregate_revision: aggregate.2,
        kind: kind.into(),
        payload,
    })?;
    Ok(())
}

/// Fresh writer-time credential checks supplement pure shape validation. A retired or
/// expired stage is a recorded validation refusal, never a retryable storage outage.
fn plan_for_write(
    tx: &mut dyn WriteTransaction,
    current: &Layer,
    entries: &[SettingAssignment],
) -> Result<Plan, StorageError> {
    let mut planned = plan(current, entries);
    for (key, value) in &planned.values {
        let Some(reference) = value
            .get("value")
            .and_then(|v| v.get("secretRef"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        if !super::validate::is_secret_ref(reference) {
            continue;
        }
        let handle = Id::new(reference).map_err(|_| StorageError::ScopeDenied)?;
        let unavailable = tx
            .configuration()
            .settings_secret(&handle)?
            .is_some_and(|row| {
                matches!(
                    row.state,
                    crate::ports::storage::SettingsSecretState::Writing
                        | crate::ports::storage::SettingsSecretState::Deleting
                        | crate::ports::storage::SettingsSecretState::Deleted
                ) || row.state == crate::ports::storage::SettingsSecretState::Ready
                    && row.expires_at_ms <= tx.now_ms()
            });
        if unavailable {
            planned.issues.push(Issue::blocking(key, "credential.stageUnavailable", "This credential stage expired or was removed. Enter the credential again before saving."));
        }
    }
    Ok(planned)
}

/// Appends the planned revision and its `SettingsRevisionSaved` history (with each changed
/// key's adoption boundary, which the engine consumes at safe steps).
fn commit(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    current: &Layer,
    plan: &Plan,
    origin: Value,
) -> Result<ConfigRevision, StorageError> {
    let scope = &current.scope;
    let doc = body(plan, ctx.principal(), tx.now_ms(), origin.clone());
    super::secrets::retain(tx, &doc)?;
    let saved = tx.configuration().append_revision(
        scope,
        NAMESPACE,
        current.revision,
        model::BODY_SCHEMA_VERSION,
        doc,
    )?;
    let changes: Vec<Value> = plan
        .changed
        .iter()
        .map(|d| json!({"key": d.key, "adoption": d.adoption.as_str()}))
        .collect();
    event(
        tx,
        ctx,
        scope,
        (REVISION_AGGREGATE, scope.key(), saved.revision),
        "SettingsRevisionSaved",
        json!({
            "scope": scope.key(),
            "revision": saved.revision,
            "previousRevision": current.revision,
            "origin": origin,
            "changes": changes,
        }),
    )?;
    Ok(saved)
}

/// Effective entries for `keys` at `scope` over its current chain.
fn effective_entries(
    read: &dyn ConfigurationRead,
    scope: &ConfigScope,
    keys: &[&str],
) -> Result<Vec<SettingEntry>, Failure> {
    let layers = current_chain(read, scope)?;
    keys.iter()
        .map(|k| {
            let d = descriptor(k)
                .ok_or_else(|| Refusal::new(DomainErrorCode::InvalidArgument, "setting", None))?;
            let mut c = chain(d, &layers).map_err(|m| {
                Refusal::new(
                    DomainErrorCode::SchemaMismatch,
                    "setting_schema",
                    Some(m.scope.target_id()),
                )
            })?;
            Ok(c.remove(0))
        })
        .collect()
}

fn page(items: Vec<SettingEntry>) -> Page<SettingEntry> {
    Page {
        items: BoundedList(items),
        next_cursor: None,
        has_more: false,
    }
}

fn digest(entries: &[SettingAssignment]) -> String {
    let v = serde_json::to_value(entries).expect("assignments");
    crate::fingerprint::canonical_digest(&v).expect("bounded JSON")
}

/// How a principal's save is handled.
enum Writer {
    Apply,
    Propose,
}

fn writer(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    roles: &dyn PrincipalRoles,
) -> Result<Writer, Failure> {
    match ctx.principal_kind() {
        PrincipalKind::Human => {
            require_human(tx, ctx)?;
            Ok(Writer::Apply)
        }
        PrincipalKind::Host => Ok(Writer::Apply),
        // A role that cannot be resolved fails closed: it may be a read-only role.
        PrincipalKind::Agent => match roles.role(ctx) {
            None => Err(denied("role_unresolved")),
            Some(role) if role.read_only() => Err(denied("read_only_role")),
            Some(_) => Ok(Writer::Propose),
        },
        PrincipalKind::Package | PrincipalKind::McpClient | PrincipalKind::Schedule => {
            Err(denied("settings_write"))
        }
    }
}

/// Saves `key = value` in the workspace scope for a human, exactly as `settings.save_revision`
/// would (same plan, validation, revision row and `SettingsRevisionSaved` history), at the
/// scope revision `expected`. This is the seam behind the router's `Tier` configuration
/// (`router.configure`), so tier membership has one home: the routing settings. `Ok(None)` is
/// an unchanged value; a stale revision is the `Configuration` revision conflict; a value the
/// settings validation refuses is the returned refusal.
pub(crate) fn save_workspace_value(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    key: &str,
    value: Value,
    expected: i64,
) -> Result<Result<Option<i64>, (DomainErrorCode, &'static str)>, StorageError> {
    let scope = ConfigScope::Workspace;
    if require_human(tx, ctx).is_err() {
        return Ok(Err((DomainErrorCode::PermissionDenied, "human_decision")));
    }
    let Some(d) = descriptor(key) else {
        return Ok(Err((DomainErrorCode::InvalidArgument, "setting")));
    };
    let current = current_layer(&*tx.configuration(), &scope)?;
    if current.revision != expected {
        return Err(StorageError::conflict(
            RevisionDimension::Configuration,
            "settings",
            scope.target_id(),
            revision(expected),
            revision(current.revision),
        ));
    }
    let entries = [SettingAssignment {
        key: BoundedText(d.key.to_owned()),
        value: SchemaValue {
            schema: d.schema_ref(),
            value: DynamicJson(value),
        },
    }];
    let planned = plan_for_write(tx, &current, &entries)?;
    if planned.blocking().next().is_some() {
        return Ok(Err((DomainErrorCode::InvalidArgument, "setting")));
    }
    if !planned.differs {
        return Ok(Ok(None));
    }
    let saved = commit(tx, ctx, &current, &planned, json!({"kind": "Saved"}))?;
    Ok(Ok(Some(saved.revision)))
}

/// `settings.save_revision`.
fn save(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    roles: &dyn PrincipalRoles,
    input: &ops::SaveRevisionInput,
) -> Result<DomainDecision, Failure> {
    let scope = crate::ports::storage::ConfigScope::from(&input.scope);
    require_scope(&*tx.configuration(), &scope)?;
    let writer = writer(tx, ctx, roles)?;
    let current = current_layer(&*tx.configuration(), &scope)?;
    let expected = input.expected_revision.get();
    if current.revision != expected {
        return Err(stale(&scope, expected, current.revision));
    }
    let planned = plan_for_write(tx, &current, &input.entries.0)?;
    let blocking: Vec<&Issue> = planned.blocking().collect();
    if !blocking.is_empty() {
        return Err(plan_refusal(&blocking, &scope).into());
    }
    let keys: Vec<&str> = input
        .entries
        .0
        .iter()
        .filter_map(|a| descriptor(&a.key.0).map(|d| d.key))
        .collect();
    if !planned.differs {
        let entries = effective_entries(&*tx.configuration(), &scope, &keys)?;
        return Ok(DomainDecision::NoChange {
            result: OperationResult::SettingsSaveRevision {
                value: page(entries),
            },
        });
    }
    match writer {
        Writer::Apply => {
            commit(tx, ctx, &current, &planned, json!({"kind": "Saved"}))?;
            let entries = effective_entries(&*tx.configuration(), &scope, &keys)?;
            Ok(DomainDecision::Applied {
                result: OperationResult::SettingsSaveRevision {
                    value: page(entries),
                },
            })
        }
        Writer::Propose => {
            // An agent never applies settings and this command never stages: it is a
            // receipt-only refusal that points the caller at the proposal command.
            let mut refusal = Refusal::new(
                DomainErrorCode::ApprovalRequired,
                PROPOSAL_COMMAND,
                Some(scope.target_id()),
            );
            refusal.retry = RetryAdvice::Never;
            Err(refusal.into())
        }
    }
}

/// Applies `entries` to `scope` as one new revision at its current revision, with the same
/// planning, validation and `SettingsRevisionSaved` history as a save. Human only (a host-bound
/// human decision). This is the seam a command of another family uses when its own input changes
/// a setting (`projects.edit` changing a project's access profile), so the setting has one
/// writer and one history. Returns whether anything changed.
pub fn apply_human_assignments(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    scope: &ConfigScope,
    entries: &[SettingAssignment],
    origin: Value,
) -> Result<bool, Failure> {
    require_human(tx, ctx)?;
    require_scope(&*tx.configuration(), scope)?;
    let current = current_layer(&*tx.configuration(), scope)?;
    let planned = plan_for_write(tx, &current, entries)?;
    let blocking: Vec<&Issue> = planned.blocking().collect();
    if !blocking.is_empty() {
        return Err(plan_refusal(&blocking, scope).into());
    }
    if !planned.differs {
        return Ok(false);
    }
    commit(tx, ctx, &current, &planned, origin)?;
    Ok(true)
}

/// Writes an activated role-instruction revision into the effective configuration of `scope`
/// as a new settings revision. The role instruction owner decides the activation (a human save,
/// or the replay of an exact human approval) in the same unit of work; this keeps the setting
/// the single effective source that running agents adopt at their next safe step. Returns the
/// new revision, or `None` when the scope already holds `text`.
pub fn mirror_role_instructions(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    scope: &ConfigScope,
    role: AgentRole,
    text: &str,
    origin: Value,
) -> Result<Option<i64>, Failure> {
    require_scope(&*tx.configuration(), scope)?;
    let d = descriptor(role.instructions_key())
        .ok_or_else(|| Refusal::new(DomainErrorCode::InvalidArgument, "setting", None))?;
    let current = current_layer(&*tx.configuration(), scope)?;
    let entries = [SettingAssignment {
        key: BoundedText(d.key.to_owned()),
        value: SchemaValue {
            schema: d.schema_ref(),
            value: DynamicJson(Value::String(text.to_owned())),
        },
    }];
    let planned = plan_for_write(tx, &current, &entries)?;
    let blocking: Vec<&Issue> = planned.blocking().collect();
    if !blocking.is_empty() {
        return Err(plan_refusal(&blocking, scope).into());
    }
    if !planned.differs {
        return Ok(None);
    }
    Ok(Some(commit(tx, ctx, &current, &planned, origin)?.revision))
}

/// Stages a proposal for human approval, bound to the scope revision the author read. This is
/// the transaction-scoped seam behind the approvals and roles proposal commands: it validates
/// exactly like a save, writes nothing to the effective configuration and records the proposal
/// plus its `SettingsProposalStaged` history. Read-only roles, packages, schedules and the host
/// cannot propose; humans save directly but may also propose.
pub fn stage_proposal(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    roles: &dyn PrincipalRoles,
    scope: &ConfigScope,
    expected_revision: i64,
    entries: &[SettingAssignment],
) -> Result<ProposalRecord, Failure> {
    require_scope(&*tx.configuration(), scope)?;
    match ctx.principal_kind() {
        PrincipalKind::Human => require_human(tx, ctx)?,
        PrincipalKind::Agent => match roles.role(ctx) {
            None => return Err(denied("role_unresolved")),
            Some(role) if role.read_only() => return Err(denied("read_only_role")),
            Some(_) => {}
        },
        PrincipalKind::Package
        | PrincipalKind::McpClient
        | PrincipalKind::Schedule
        | PrincipalKind::Host => {
            return Err(denied("settings_proposal"));
        }
    }
    let current = current_layer(&*tx.configuration(), scope)?;
    if current.revision != expected_revision {
        return Err(stale(scope, expected_revision, current.revision));
    }
    let planned = plan_for_write(tx, &current, entries)?;
    let blocking: Vec<&Issue> = planned.blocking().collect();
    if !blocking.is_empty() {
        return Err(plan_refusal(&blocking, scope).into());
    }
    if !planned.differs {
        return Err(Refusal::new(
            DomainErrorCode::InvalidArgument,
            "settings_proposal",
            Some(scope.target_id()),
        )
        .into());
    }
    Ok(stage(tx, ctx, &current, entries, &planned)?)
}

fn provenance(ctx: &InvocationContext) -> Value {
    json!({
        "invocationId": ctx.invocation_id().as_str(),
        "sessionId": ctx.session_id().map(Id::as_str),
        "projectId": ctx.project_id().map(|p| p.0.as_str()),
        "rootId": ctx.root_id().map(|r| r.0.as_str()),
        "assignmentId": ctx.assignment_id().map(|a| a.0.as_str()),
    })
}

fn proposal_body(
    entries: &[SettingAssignment],
    planned: &Plan,
    provenance: Value,
    content_authors: &[String],
) -> Value {
    json!({
        "entries": entries,
        "contentDigest": digest(entries),
        "keys": planned.changed.iter().map(|d| d.key).collect::<Vec<_>>(),
        "provenance": provenance,
        "contentAuthors": content_authors,
    })
}

/// Every principal who authored the proposal's current content: its author and each human who
/// reconciled or edited it. None of them may approve it.
pub fn content_authors(p: &ProposalRecord) -> Vec<String> {
    let mut authors = vec![p.author.0.as_str().to_owned()];
    if let Some(recorded) = p.body["contentAuthors"].as_array() {
        for a in recorded.iter().filter_map(Value::as_str) {
            if !authors.iter().any(|x| x == a) {
                authors.push(a.to_owned());
            }
        }
    }
    authors
}

fn stage(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    current: &Layer,
    entries: &[SettingAssignment],
    planned: &Plan,
) -> Result<ProposalRecord, StorageError> {
    let id = tx.new_id(IdKind::Operation);
    let doc = proposal_body(
        entries,
        planned,
        provenance(ctx),
        &[ctx.principal().0.as_str().to_owned()],
    );
    super::secrets::retain(tx, &doc)?;
    let record = tx.configuration().stage(NewProposal {
        id: id.clone(),
        scope: current.scope.clone(),
        namespace: NAMESPACE.into(),
        base_revision: current.revision,
        author: ctx.principal().clone(),
        body: doc,
    })?;
    event(
        tx,
        ctx,
        &current.scope,
        (PROPOSAL_AGGREGATE, id.as_str().to_owned(), record.revision),
        "SettingsProposalStaged",
        json!({
            "scope": current.scope.key(),
            "baseRevision": current.revision,
            "keys": record.body["keys"],
            "contentDigest": record.body["contentDigest"],
        }),
    )?;
    crate::services::notifications::notify_approval(
        tx,
        crate::ports::storage::ApprovalSource::Settings,
        &record.id,
        current.scope.project_id(),
        "Review settings changes",
        "A proposed settings revision requires your review.",
    )?;
    Ok(record)
}

/// Decoded entries of a stored proposal.
pub fn proposal_entries(p: &ProposalRecord) -> Result<Vec<SettingAssignment>, StorageError> {
    serde_json::from_value(p.body["entries"].clone()).map_err(|e| {
        StorageError::unavailable(
            crate::ports::storage::Unavailable::Corrupt,
            format!("stored proposal entries: {e}"),
        )
    })
}

fn pending(
    tx: &mut dyn WriteTransaction,
    id: &Id,
    expected_revision: i64,
) -> Result<ProposalRecord, Failure> {
    let p = tx.configuration().proposal(id)?.ok_or_else(|| {
        Refusal::new(
            DomainErrorCode::NotFound,
            "settings_proposal",
            Some(id.clone()),
        )
    })?;
    if p.revision != expected_revision {
        return Err(Failure::Storage(StorageError::conflict(
            RevisionDimension::Configuration,
            "settings_proposal",
            id.clone(),
            revision(expected_revision),
            revision(p.revision),
        )));
    }
    if p.state != ProposalState::Pending {
        return Err(Refusal::new(
            DomainErrorCode::Conflict,
            "settings_proposal",
            Some(id.clone()),
        )
        .into());
    }
    Ok(p)
}

/// Applies the exact reviewed proposal revision as a new configuration revision. A proposal
/// whose base revision is no longer current (a concurrent user edit or another approval) is a
/// `Configuration` revision conflict and needs [`reconcile_proposal`] first; approval never
/// applies content the human did not review.
pub fn approve_proposal(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    id: &Id,
    expected_revision: i64,
) -> Result<ProposalRecord, Failure> {
    require_human(tx, ctx)?;
    let p = pending(tx, id, expected_revision)?;
    let approver = ctx.principal().0.as_str();
    if content_authors(&p).iter().any(|a| a == approver) {
        return Err(denied("self_approval"));
    }
    let current = current_layer(&*tx.configuration(), &p.scope)?;
    if current.revision != p.base_revision {
        return Err(stale(&p.scope, p.base_revision, current.revision));
    }
    let entries = proposal_entries(&p)?;
    let planned = plan_for_write(tx, &current, &entries)?;
    let blocking: Vec<&Issue> = planned.blocking().collect();
    if !blocking.is_empty() {
        return Err(plan_refusal(&blocking, &p.scope).into());
    }
    let saved = commit(
        tx,
        ctx,
        &current,
        &planned,
        json!({"kind": "ApprovedProposal", "proposalId": id.as_str(), "proposalRevision": p.revision}),
    )?;
    let decided = tx.configuration().update_proposal(
        id,
        p.revision,
        ProposalChange::Decide {
            state: ProposalState::Approved,
            decided_by: ctx.principal().clone(),
            applied_revision: Some(saved.revision),
        },
    )?;
    event(
        tx,
        ctx,
        &p.scope,
        (PROPOSAL_AGGREGATE, id.as_str().to_owned(), decided.revision),
        "SettingsProposalApproved",
        json!({"scope": p.scope.key(), "appliedRevision": saved.revision, "contentDigest": p.body["contentDigest"]}),
    )?;
    Ok(decided)
}

/// Rejects a pending proposal; the current configuration stays effective.
pub fn reject_proposal(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    id: &Id,
    expected_revision: i64,
) -> Result<ProposalRecord, Failure> {
    require_human(tx, ctx)?;
    let p = pending(tx, id, expected_revision)?;
    let decided = tx.configuration().update_proposal(
        id,
        p.revision,
        ProposalChange::Decide {
            state: ProposalState::Rejected,
            decided_by: ctx.principal().clone(),
            applied_revision: None,
        },
    )?;
    event(
        tx,
        ctx,
        &p.scope,
        (PROPOSAL_AGGREGATE, id.as_str().to_owned(), decided.revision),
        "SettingsProposalRejected",
        json!({"scope": p.scope.key()}),
    )?;
    Ok(decided)
}

/// Re-binds a pending proposal to the scope's current revision after a concurrent edit,
/// optionally with the human's edited entries. The proposal's revision advances, so any approval
/// prepared against the earlier content is stale.
pub fn reconcile_proposal(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    id: &Id,
    expected_revision: i64,
    replacement: Option<Vec<SettingAssignment>>,
) -> Result<ProposalRecord, Failure> {
    require_human(tx, ctx)?;
    let p = pending(tx, id, expected_revision)?;
    let current = current_layer(&*tx.configuration(), &p.scope)?;
    let entries = match replacement {
        Some(e) => e,
        None => proposal_entries(&p)?,
    };
    let planned = plan_for_write(tx, &current, &entries)?;
    let blocking: Vec<&Issue> = planned.blocking().collect();
    if !blocking.is_empty() {
        return Err(plan_refusal(&blocking, &p.scope).into());
    }
    // The reconciler becomes a content author whether or not they edited the entries: the
    // content is now bound to a base they chose.
    let mut authors = content_authors(&p);
    let reconciler = ctx.principal().0.as_str().to_owned();
    if !authors.contains(&reconciler) {
        authors.push(reconciler);
    }
    let doc = proposal_body(&entries, &planned, p.body["provenance"].clone(), &authors);
    super::secrets::retain(tx, &doc)?;
    let reconciled = tx.configuration().update_proposal(
        id,
        p.revision,
        ProposalChange::Reconcile {
            base_revision: current.revision,
            body: doc,
        },
    )?;
    event(
        tx,
        ctx,
        &p.scope,
        (
            PROPOSAL_AGGREGATE,
            id.as_str().to_owned(),
            reconciled.revision,
        ),
        "SettingsProposalReconciled",
        json!({"scope": p.scope.key(), "baseRevision": current.revision, "contentDigest": reconciled.body["contentDigest"]}),
    )?;
    Ok(reconciled)
}

/// Assignments that restore `target_revision` of `scope` when saved at the current revision.
/// The native UI submits them through `settings.save_revision`, so rollback shares validation,
/// receipts and history with every other save.
pub fn rollback_assignments(
    read: &dyn ConfigurationRead,
    scope: &ConfigScope,
    target_revision: i64,
) -> Result<Vec<SettingAssignment>, Failure> {
    let current = current_layer(read, scope)?;
    let target = layer_at(read, scope, target_revision)?.ok_or_else(|| {
        Refusal::new(
            DomainErrorCode::NotFound,
            "configuration_revision",
            Some(scope.target_id()),
        )
    })?;
    Ok(restore_assignments(&current, &target))
}

/// Restores `target_revision` as a new revision (history is retained, never rewritten). Human
/// only. Returns `None` when the scope already equals the target.
pub fn rollback(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    scope: &ConfigScope,
    expected_revision: i64,
    target_revision: i64,
) -> Result<Option<ConfigRevision>, Failure> {
    require_human(tx, ctx)?;
    require_scope(&*tx.configuration(), scope)?;
    let current = current_layer(&*tx.configuration(), scope)?;
    if current.revision != expected_revision {
        return Err(stale(scope, expected_revision, current.revision));
    }
    let entries = rollback_assignments(&*tx.configuration(), scope, target_revision)?;
    let planned = plan_for_write(tx, &current, &entries)?;
    let blocking: Vec<&Issue> = planned.blocking().collect();
    if !blocking.is_empty() {
        return Err(plan_refusal(&blocking, scope).into());
    }
    if !planned.differs {
        return Ok(None);
    }
    Ok(Some(commit(
        tx,
        ctx,
        &current,
        &planned,
        json!({"kind": "Rollback", "targetRevision": target_revision}),
    )?))
}

/// A proposal as an approval surface presents it.
#[derive(Clone, Debug)]
pub struct ProposalView {
    pub record: ProposalRecord,
    pub entries: Vec<SettingAssignment>,
    /// The scope changed since the proposal's base revision: it must be reconciled before it
    /// can be approved.
    pub stale: bool,
}

/// Proposals of one scope (optionally only pending ones), with staleness against the current
/// revision.
pub fn proposals(
    read: &dyn ConfigurationRead,
    scope: &ConfigScope,
    state: Option<ProposalState>,
    after: Option<&Id>,
    limit: u16,
) -> Result<Vec<ProposalView>, StorageError> {
    let current = current_layer(read, scope)?.revision;
    read.proposals(scope, NAMESPACE, state, after, limit)?
        .into_iter()
        .map(|record| {
            Ok(ProposalView {
                entries: proposal_entries(&record)?,
                stale: record.state == ProposalState::Pending && record.base_revision != current,
                record,
            })
        })
        .collect()
}

/// One retained revision of a scope (version history).
#[derive(Clone, Debug, PartialEq)]
pub struct RevisionSummary {
    pub revision: i64,
    pub author: Option<String>,
    pub created_at_ms: Option<i64>,
    pub origin: Value,
    pub changed_keys: Vec<String>,
}
pub fn history(
    read: &dyn ConfigurationRead,
    scope: &ConfigScope,
    before: Option<i64>,
    limit: u16,
) -> Result<Vec<RevisionSummary>, StorageError> {
    Ok(read
        .history(scope, NAMESPACE, before, limit)?
        .into_iter()
        .map(|r| RevisionSummary {
            revision: r.revision,
            author: r.body["author"].as_str().map(str::to_owned),
            created_at_ms: r.body["createdAtMs"].as_i64(),
            origin: r.body["origin"].clone(),
            changed_keys: r.body["changedKeys"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|k| k.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect())
}

/// What a running agent must adopt at its next safe step (SET-04): the current chain revisions
/// and every effective change since the revisions it last adopted, with each key's boundary.
/// Unapproved proposals are never part of a revision, so they can never appear here.
#[derive(Clone, Debug, PartialEq)]
pub struct AdoptionNotice {
    pub current: AdoptedRevisions,
    pub changes: Vec<(&'static str, Adoption)>,
}
pub fn adoption_since(
    read: &dyn ConfigurationRead,
    project: Option<&ProjectId>,
    adopted: &AdoptedRevisions,
) -> Result<AdoptionNotice, Failure> {
    let current = current_revisions(read, project)?;
    let before = layers_at(read, project, adopted)?
        .ok_or_else(|| Refusal::new(DomainErrorCode::NotFound, "configuration_revision", None))?;
    let after = layers_at(read, project, &current)?.ok_or_else(|| {
        Failure::Storage(StorageError::unavailable(
            crate::ports::storage::Unavailable::Corrupt,
            "current configuration revision missing",
        ))
    })?;
    let (old, new) = (effective_values(&before), effective_values(&after));
    let changes = catalog()
        .iter()
        .filter(|d| old.get(d.key) != new.get(d.key))
        .map(|d| (d.key, d.adoption))
        .collect();
    Ok(AdoptionNotice { current, changes })
}

pub fn current_revisions(
    read: &dyn ConfigurationRead,
    project: Option<&ProjectId>,
) -> Result<AdoptedRevisions, StorageError> {
    Ok(AdoptedRevisions {
        workspace: current_layer(read, &ConfigScope::Workspace)?.revision,
        project: project
            .map(|p| current_layer(read, &ConfigScope::Project(p.clone())).map(|l| l.revision))
            .transpose()?,
    })
}

/// The effective instructions of a role for new or resumed work, with the exact revisions they
/// came from so completed work can record which instructions it used.
#[derive(Clone, Debug)]
pub struct EffectiveInstructions {
    pub role: AgentRole,
    pub text: String,
    pub revisions: AdoptedRevisions,
    pub source: SettingEntry,
}
pub fn role_instructions(
    read: &dyn ConfigurationRead,
    role: AgentRole,
    project: Option<&ProjectId>,
) -> Result<EffectiveInstructions, Failure> {
    let scope = project.map_or(ConfigScope::Workspace, |p| ConfigScope::Project(p.clone()));
    let source = effective_entries(read, &scope, &[role.instructions_key()])?.remove(0);
    Ok(EffectiveInstructions {
        role,
        text: source.value.value.0.as_str().unwrap_or_default().to_owned(),
        revisions: current_revisions(read, project)?,
        source,
    })
}

/// The settings domain service registered with the gateway for [`OPERATIONS`].
pub struct SettingsService {
    pub roles: Arc<dyn PrincipalRoles>,
    pub paths: Arc<dyn PathProbe>,
    /// Correlation IDs for query faults (commands allocate inside their transaction).
    pub ids: Arc<dyn IdSource>,
}

impl SettingsService {
    fn fault(&self, r: Refusal) -> DomainFault {
        r.fault(CorrelationId(self.ids.next_id(IdKind::Correlation)))
    }

    fn read_failure(&self, f: Failure) -> DomainFault {
        match f {
            Failure::Refused(r) => self.fault(r),
            Failure::Storage(e) => {
                let code = match e {
                    StorageError::CursorExpired => DomainErrorCode::CursorExpired,
                    StorageError::Cancelled => DomainErrorCode::Cancelled,
                    _ => DomainErrorCode::StorageUnavailable,
                };
                let mut r = Refusal::new(code, "configuration", None);
                if code == DomainErrorCode::StorageUnavailable {
                    r.retry = RetryAdvice::SameCommand;
                }
                self.fault(r)
            }
        }
    }

    fn read_effective(
        &self,
        read: &dyn ConfigurationRead,
        scope: &ConfigScope,
        input: &ops::ReadEffectiveInput,
    ) -> Result<(OperationResult, i64), Failure> {
        let keys: Vec<&str> = if input.keys.0.is_empty() {
            catalog().iter().map(|d| d.key).collect()
        } else {
            let mut seen = std::collections::HashSet::new();
            let mut keys = Vec::with_capacity(input.keys.0.len());
            for k in &input.keys.0 {
                let d = descriptor(&k.0).ok_or_else(|| {
                    Refusal::new(DomainErrorCode::InvalidArgument, "setting", None)
                })?;
                if !seen.insert(d.key) {
                    return Err(
                        Refusal::new(DomainErrorCode::InvalidArgument, "setting", None).into(),
                    );
                }
                keys.push(d.key);
            }
            keys
        };
        let revisions = current_revisions(read, scope.project_id())?;
        let binding = cursor_binding(scope, &keys, &revisions);
        let offset = match &input.page.cursor {
            None => 0,
            Some(c) => {
                c.0.strip_suffix(&format!(":{binding}"))
                    .and_then(|n| n.parse::<usize>().ok())
                    .filter(|n| *n <= keys.len())
                    .ok_or(Failure::Storage(StorageError::CursorExpired))?
            }
        };
        let limit = input.page.limit.get() as usize;
        let end = (offset + limit).min(keys.len());
        let items = effective_entries(read, scope, &keys[offset..end])?;
        let has_more = end < keys.len();
        let value = Page {
            items: BoundedList(items),
            next_cursor: has_more.then(|| BoundedText(format!("{end}:{binding}"))),
            has_more,
        };
        Ok((
            OperationResult::SettingsReadEffective { value },
            scope_revision(scope, &revisions),
        ))
    }

    fn validate(
        &self,
        read: &dyn ConfigurationRead,
        scope: &ConfigScope,
        input: &ops::ValidateInput,
    ) -> Result<(OperationResult, i64), Failure> {
        let current = current_layer(read, scope)?;
        let mut issues = Vec::new();
        if current.revision != input.expected_revision.get() {
            issues.push(Issue::blocking(
                "expectedRevision",
                codes::STALE_REVISION,
                format!(
                    "This scope changed; it is now at revision {}. Review the current values before saving.",
                    current.revision
                ),
            ));
        }
        let planned = plan(&current, &input.entries.0);
        issues.extend(planned.issues.iter().cloned());
        for a in &input.entries.0 {
            let Some(d) = descriptor(&a.key.0) else {
                continue;
            };
            if let (super::catalog::ValueKind::Directory, Some(path)) =
                (d.kind, a.value.value.0.as_str())
                && !issues.iter().any(|i| i.path == d.key && i.blocking)
            {
                let health = self.paths.probe(path);
                if health != PathHealth::Available {
                    issues.push(Issue::advisory(
                        d.key,
                        codes::PATH_UNAVAILABLE,
                        format!("{path} {}", health.describe()),
                    ));
                }
            }
        }
        let valid = !issues.iter().any(|i| i.blocking);
        let value = ValidationReport {
            valid,
            issues: BoundedList(
                issues
                    .into_iter()
                    .take(500)
                    .map(|i| ValidationIssue {
                        path: BoundedText(i.path),
                        code: BoundedText(i.code.to_owned()),
                        message: BoundedText(i.message),
                    })
                    .collect(),
            ),
        };
        Ok((
            OperationResult::SettingsValidate { value },
            current.revision,
        ))
    }

    fn explain(
        &self,
        read: &dyn ConfigurationRead,
        scope: &ConfigScope,
        input: &ops::ExplainSourceInput,
    ) -> Result<(OperationResult, i64), Failure> {
        let d = descriptor(&input.key.0)
            .ok_or_else(|| Refusal::new(DomainErrorCode::InvalidArgument, "setting", None))?;
        let layers = current_chain(read, scope)?;
        let items = chain(d, &layers).map_err(|m| {
            Refusal::new(
                DomainErrorCode::SchemaMismatch,
                "setting_schema",
                Some(m.scope.target_id()),
            )
        })?;
        let revisions = current_revisions(read, scope.project_id())?;
        Ok((
            OperationResult::SettingsExplainSource { value: page(items) },
            scope_revision(scope, &revisions),
        ))
    }
}

fn scope_revision(scope: &ConfigScope, r: &AdoptedRevisions) -> i64 {
    match scope {
        ConfigScope::Workspace => r.workspace,
        ConfigScope::Project(_) => r.project.unwrap_or(model::INITIAL_REVISION),
    }
}

/// Binds a read cursor to the exact scope, key list and chain revisions; any change expires it.
fn cursor_binding(scope: &ConfigScope, keys: &[&str], r: &AdoptedRevisions) -> String {
    let doc = json!([scope.key(), keys, r.workspace, r.project]);
    sha256_hex(doc.to_string().as_bytes())[..32].to_owned()
}

impl QueryHandler for SettingsService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let OperationInput::Settings(input) = query.input() else {
            return Err(self.fault(Refusal::new(
                DomainErrorCode::UnsupportedCapability,
                "operation",
                None,
            )));
        };
        let read = snapshot.configuration();
        let scope = match input {
            ops::Input::ReadEffective(i) => crate::ports::storage::ConfigScope::from(&i.scope),
            ops::Input::Validate(i) => crate::ports::storage::ConfigScope::from(&i.scope),
            ops::Input::ExplainSource(i) => crate::ports::storage::ConfigScope::from(&i.scope),
            ops::Input::SaveRevision(_) => {
                return Err(self.fault(Refusal::new(
                    DomainErrorCode::UnsupportedCapability,
                    "operation",
                    None,
                )));
            }
        };
        let answered = if ctx.principal_kind() == PrincipalKind::Agent
            && scope.project_id() != ctx.project_id()
        {
            Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                "configuration_scope",
                Some(scope.target_id()),
            )
            .into())
        } else {
            require_scope(read, &scope)
        }
        .and_then(|()| match input {
            ops::Input::ReadEffective(i) => self.read_effective(read, &scope, i),
            ops::Input::Validate(i) => self.validate(read, &scope, i),
            ops::Input::ExplainSource(i) => self.explain(read, &scope, i),
            ops::Input::SaveRevision(_) => unreachable!("refused above"),
        });
        let (result, scope_revision) = answered.map_err(|f| self.read_failure(f))?;
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), revision(scope_revision)),
            result,
        })
    }
}

impl CommandHandler for SettingsService {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let OperationInput::Settings(ops::Input::SaveRevision(input)) = command.input() else {
            let correlation = CorrelationId(tx.new_id(IdKind::Correlation));
            return Ok(Decided {
                decision: DomainDecision::Refused {
                    fault: Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None)
                        .fault(correlation),
                },
                operation_id: None,
            });
        };
        let decision = match save(tx, ctx, &*self.roles, input) {
            Ok(d) => d,
            Err(Failure::Refused(r)) => DomainDecision::Refused {
                fault: r.fault(CorrelationId(tx.new_id(IdKind::Correlation))),
            },
            Err(Failure::Storage(e)) => return Err(HandlerFailure::Storage(e)),
        };
        Ok(Decided {
            decision,
            operation_id: None,
        })
    }
}

/// The current inherited allowance for one feature/issue's children. Host schedulers share the
/// same descriptor, defaults and revisioned configuration chain as Settings UI and tools.
pub fn max_subagents(
    read: &dyn ConfigurationRead,
    project: &ProjectId,
) -> Result<usize, StorageError> {
    let source = effective_entries(
        read,
        &ConfigScope::Project(project.clone()),
        &["agents.maxSubagents"],
    )
    .map_err(|error| match error {
        Failure::Storage(error) => error,
        _ => StorageError::InvalidRecord("effective subagent allowance".into()),
    })?
    .remove(0);
    source
        .value
        .value
        .0
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            StorageError::InvalidRecord("effective subagent allowance is a positive integer".into())
        })
}
