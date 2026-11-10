//! Skills and role instructions (DW-034): `skills.*` (SkillService) and `roles.*`
//! (InstructionService) share one owner, registered through
//! [`crate::services::UseCases::with_instructions`], so the native UI, agent tools, packages and
//! MCP reach the same rules through the gateway.
//!
//! # Model
//!
//! An instruction item is one skill or role in one scope. Its revisions are immutable and their
//! contents are managed artifacts; only an `Approved` revision can be the item's active
//! revision, which is what agents adopt at their next safe step. Revisions are approved by
//!
//! * observation of a user-owned file (shared `~/.agents/skills`, or a harness skill the user
//!   edited on disk) or of the effective role setting, recorded by the host [`sync`];
//! * a human save (`skills.draft` / `roles.draft` by a human principal), effective at once;
//! * a human decision over an agent proposal: `skills.propose` stages a saved
//!   `skills.rollback` request (activation of the exact proposed revision) through the shared
//!   approvals path; only after the exact human approval can the replayed request activate it.
//!
//! `skills.rollback` / `roles.rollback` are the one activation command. They are
//! approval-required for every non-human principal ([`crate::authority::policy`]), and a
//! non-human activation must name the approved proposal bound to its target revision.
//!
//! # Files
//!
//! Shared skills are read in place and never written: an approved agent improvement to a
//! shared skill becomes a same-name harness override, which takes precedence. Harness skills
//! live in the managed skill root; their approved revisions are materialized there after the
//! deciding unit of work commits, and [`sync`] repairs an interrupted materialization. Skill
//! resources (documents, scripts, assets) resolve relative to their owning skill directory;
//! scripts gain no authority from being part of a skill.
//!
//! Role revisions keep the `roles.<role>.instructions` setting as their single effective source:
//! activation writes the setting in the same unit of work, and a setting saved elsewhere is
//! observed as a new approved revision.
pub mod adoption;
mod approval;
mod commands;
mod queries;
pub mod scan;
mod sync;

pub use adoption::{EffectiveSkill, EffectiveSkills, SkillConflict, effective_skills};
pub use sync::SyncReport;

use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, roles as role_ops, skills as skill_ops};
use crate::ports::indexing::SourceTree;
use crate::ports::skill_files::SkillFiles;
use crate::ports::storage::{
    ApplicationStore, ConfigScope, InstructionKind, InstructionOrigin, InstructionRevisionState,
    ReadSnapshot, StorageError, StoredInstructionItem, StoredInstructionRevision, WriteTransaction,
};
use crate::ports::{
    CommandHandler, Decided, HandlerFailure, IdKind, IdSource, Preparation, Prepared, QueryAnswer,
    QueryHandler,
};
use crate::protocol::data::DataScope;
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{InstructionRecord, RegisteredPath};
use crate::protocol::support::{BoundedList, RelativePath};
use crate::protocol::{CorrelationId, DomainErrorCode, DomainFault, Id};
use crate::services::artifacts::ArtifactService;
use crate::services::sessions::{Failure, Refusal, decided, label, not_found, query_fault, rev};
use crate::settings::catalog::AgentRole;
use crate::settings::service::PrincipalRoles;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The twelve operations this owner is registered for.
pub const OPERATIONS: [&str; 12] = [
    "skills.discover",
    "skills.read",
    "skills.resources",
    "skills.draft",
    "skills.propose",
    "skills.history",
    "skills.rollback",
    "roles.read",
    "roles.draft",
    "roles.propose",
    "roles.history",
    "roles.rollback",
];

/// Registered root of the user-owned shared skills (`~/.agents/skills`).
pub const SHARED_ROOT_ID: &str = "skills-shared";
/// Registered root of the harness-owned skills (`<data-root>/skills`).
pub const HARNESS_ROOT_ID: &str = "skills-harness";
/// History aggregate of an instruction item (`aggregate_id` = item ID).
pub const AGGREGATE: &str = "instruction";
/// Maximum instruction text a draft may carry.
pub const MAX_TEXT_BYTES: u64 = scan::MAX_INSTRUCTION_BYTES;

/// Where file-backed skills are found.
#[derive(Clone, Debug)]
pub struct SkillRoots {
    /// `~/.agents/skills`, when a home directory is known.
    pub shared: Option<PathBuf>,
    /// `<data-root>/skills`.
    pub harness: PathBuf,
}

/// The skill and role instruction owner.
pub struct InstructionService {
    pub(crate) store: Arc<dyn ApplicationStore>,
    pub(crate) artifacts: Arc<ArtifactService>,
    pub(crate) source: Arc<dyn SourceTree>,
    pub(crate) files: Arc<dyn SkillFiles>,
    pub(crate) roots: SkillRoots,
    pub(crate) ids: Arc<dyn IdSource>,
    pub(crate) roles: Arc<dyn PrincipalRoles>,
    pub(crate) clock: Arc<dyn crate::ports::Clock>,
    /// One host sync at a time (observation is idempotent, but serializing avoids racing
    /// duplicate captures of one change).
    pub(crate) sync_lock: Mutex<()>,
}

/// The host's skill directories and their file access.
#[derive(Clone)]
pub struct SkillAccess {
    pub source: Arc<dyn SourceTree>,
    pub files: Arc<dyn SkillFiles>,
    pub roots: SkillRoots,
}

impl InstructionService {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        artifacts: Arc<ArtifactService>,
        access: SkillAccess,
        ids: Arc<dyn IdSource>,
        roles: Arc<dyn PrincipalRoles>,
        clock: Arc<dyn crate::ports::Clock>,
    ) -> Self {
        Self {
            store,
            artifacts,
            source: access.source,
            files: access.files,
            roots: access.roots,
            ids,
            roles,
            clock,
            sync_lock: Mutex::new(()),
        }
    }
    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }
}

// ---------------------------------------------------------------------------------------------
// Shared rules

pub(crate) fn refusal(code: DomainErrorCode, kind: &'static str, id: Option<&Id>) -> Failure {
    Refusal::new(code, kind, id.cloned()).into()
}
pub(crate) fn invalid(kind: &'static str) -> Failure {
    refusal(DomainErrorCode::InvalidArgument, kind, None)
}
pub(crate) fn denied(kind: &'static str) -> Failure {
    refusal(DomainErrorCode::PermissionDenied, kind, None)
}
pub(crate) fn stale(item: &StoredInstructionItem, expected: i64) -> Failure {
    Failure::Storage(StorageError::conflict(
        crate::protocol::RevisionDimension::Source,
        "instruction",
        item.id.clone(),
        rev(expected),
        rev(item.latest_revision),
    ))
}

/// The configuration scope a request names, within the invocation's project.
pub(crate) fn config_scope(
    ctx: &InvocationContext,
    scope: &DataScope,
) -> Result<ConfigScope, Failure> {
    match scope {
        DataScope::Workspace => Ok(ConfigScope::Workspace),
        DataScope::Project { project_id } => {
            crate::services::sessions::require_project(ctx, project_id)?;
            Ok(ConfigScope::Project(project_id.clone()))
        }
    }
}
fn data_scope(scope: &ConfigScope) -> DataScope {
    match scope {
        ConfigScope::Workspace => DataScope::Workspace,
        ConfigScope::Project(p) => DataScope::Project {
            project_id: p.clone(),
        },
    }
}
/// Whether the invocation may see an item of `scope`.
pub(crate) fn visible(ctx: &InvocationContext, scope: &ConfigScope) -> bool {
    crate::services::sessions::project_visible(ctx, scope.project_id())
}
/// The item if it exists, is of `kind` and is visible to the invocation (else `NotFound`).
pub(crate) fn visible_item(
    read: &dyn crate::ports::storage::InstructionRead,
    ctx: &InvocationContext,
    id: &Id,
    kind: InstructionKind,
) -> Result<StoredInstructionItem, Failure> {
    match read.instruction_item(id)? {
        Some(item) if item.kind == kind && visible(ctx, &item.scope) => Ok(item),
        _ => Err(not_found(target_kind(kind), id).into()),
    }
}
pub(crate) fn target_kind(kind: InstructionKind) -> &'static str {
    match kind {
        InstructionKind::Skill => "skill",
        InstructionKind::Role => "role",
    }
}

/// Who may author content: humans save, agents with a writable role draft. Read-only roles
/// (code reviewers), unresolved roles and every other principal are refused.
pub(crate) fn require_author(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    roles: &dyn PrincipalRoles,
) -> Result<(), Failure> {
    match ctx.principal_kind() {
        PrincipalKind::Human => require_human(tx, ctx),
        PrincipalKind::Agent => match roles.role(ctx) {
            None => Err(denied("role_unresolved")),
            Some(role) if role.read_only() => Err(denied("read_only_role")),
            Some(_) => Ok(()),
        },
        _ => Err(denied("instruction_author")),
    }
}
pub(crate) fn require_human(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
) -> Result<(), Failure> {
    if ctx.principal_kind() == PrincipalKind::Human
        && tx.actors().actor_kind(ctx.principal())? == Some(PrincipalKind::Human)
    {
        Ok(())
    } else {
        Err(denied("human_decision"))
    }
}

/// Deterministic identity of a role item, so every surface (and the retained prompt list)
/// addresses `roles.read` without a separate listing operation.
pub fn role_item_id(role: AgentRole, scope: &ConfigScope) -> Id {
    let key = role.as_str();
    let id = match scope {
        ConfigScope::Workspace => format!("role-{key}"),
        ConfigScope::Project(p) => format!(
            "role-{key}-{}",
            &crate::fingerprint::sha256_hex(p.0.as_str().as_bytes())[..24]
        ),
    };
    Id::new(id).expect("valid role item id")
}

/// The wire record of one revision. `applicability` carries the item's visible status labels
/// (`origin:*`, `state:*`, `active`, `effective`/`overridden`/`conflict`, `disabled`, `missing`) followed by
/// role applicability, so every surface shows origin and which version is active.
pub(crate) fn record(
    item: &StoredInstructionItem,
    revision: &StoredInstructionRevision,
    standing: Option<queries::Standing>,
) -> Result<InstructionRecord, Failure> {
    let content: ArtifactRef = serde_json::from_value(revision.content.clone())
        .map_err(|e| StorageError::InvalidRecord(format!("instruction content: {e}")))?;
    let mut labels = vec![
        format!("origin:{}", item.origin.as_str().to_ascii_lowercase()),
        format!("state:{}", revision.state.as_str().to_ascii_lowercase()),
    ];
    if item.active_revision == Some(revision.revision) {
        labels.push("active".into());
    }
    if let Some(standing) = standing {
        labels.push(standing.label().into());
    }
    if !item.enabled {
        labels.push("disabled".into());
    }
    if !item.present {
        labels.push("missing".into());
    }
    labels.extend(revision.applicability.iter().cloned());
    labels.truncate(128);
    let source_path = match (&item.origin, &item.source_path) {
        (InstructionOrigin::Shared | InstructionOrigin::Harness, Some(_)) => Some(RegisteredPath {
            root_id: Id::new(if item.origin == InstructionOrigin::Shared {
                SHARED_ROOT_ID
            } else {
                HARNESS_ROOT_ID
            })
            .expect("static root id"),
            relative: RelativePath(item.name.clone()),
        }),
        _ => None,
    };
    Ok(InstructionRecord {
        id: item.id.clone(),
        revision: rev(revision.revision),
        name: label(&item.name, "instruction"),
        scope: data_scope(&item.scope),
        content,
        source_path,
        approved: revision.state == InstructionRevisionState::Approved,
        applicability: BoundedList(labels.iter().map(|l| label(l, "label")).collect()),
    })
}

/// The revision a read without an explicit revision shows: the active one, else the latest.
pub(crate) fn current_revision(
    read: &dyn crate::ports::storage::InstructionRead,
    item: &StoredInstructionItem,
) -> Result<StoredInstructionRevision, Failure> {
    let number = item.active_revision.unwrap_or(item.latest_revision);
    read.instruction_revision(&item.id, number)?
        .ok_or_else(|| not_found(target_kind(item.kind), &item.id).into())
}

/// Event provenance of the invocation (responsible agent, work and session).
pub(crate) fn provenance(ctx: &InvocationContext) -> Value {
    serde_json::json!({
        "principal": ctx.principal().0.as_str(),
        "principalKind": format!("{:?}", ctx.principal_kind()),
        "invocationId": ctx.invocation_id().as_str(),
        "sessionId": ctx.session_id().map(Id::as_str),
        "projectId": ctx.project_id().map(|p| p.0.as_str()),
        "rootId": ctx.root_id().map(|r| r.0.as_str()),
        "assignmentId": ctx.assignment_id().map(|a| a.0.as_str()),
    })
}

pub(crate) fn event(
    tx: &mut dyn WriteTransaction,
    actor: &crate::protocol::ActorId,
    item: &StoredInstructionItem,
    kind: &str,
    payload: Value,
) -> Result<(), StorageError> {
    let id = crate::protocol::EventId(tx.new_id(IdKind::Event));
    tx.history().append(crate::ports::storage::NewEvent {
        id,
        actor: actor.clone(),
        project_id: item.scope.project_id().cloned(),
        root_id: None,
        operation_id: None,
        aggregate_kind: AGGREGATE.into(),
        aggregate_id: item.id.as_str().to_owned(),
        aggregate_revision: item.version,
        kind: kind.into(),
        payload,
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Handlers

impl QueryHandler for InstructionService {
    fn prepare(
        &self,
        _ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, DomainFault> {
        // Observe changed skill files and role settings before answering, so discovery shows
        // the current sources. Observation is host provenance, complete on its own.
        if env.stopped(self.clock.now_ms().get()) {
            return Err(query_fault(
                StorageError::Cancelled.into(),
                "instruction",
                self.correlation(),
            ));
        }
        self.sync_kind(kind_of(query.input()))
            .map_err(|e| query_fault(e.into(), "instruction", self.correlation()))?;
        Ok(None)
    }
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        read: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let result = match query.input() {
            OperationInput::Skills(input) => self.skill_query(read, ctx, input),
            OperationInput::Roles(input) => self.role_query(read, ctx, input),
            _ => Err(invalid("instruction_operation")),
        };
        let (result, revision) =
            result.map_err(|e| query_fault(e, "instruction", self.correlation()))?;
        Ok(QueryAnswer {
            stamp: read
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(revision)),
            result,
        })
    }
}

impl CommandHandler for InstructionService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        // Observe live sources first: a skill directory or role setting changed since the
        // caller read it becomes a newer revision, so a stale expectedRevision is refused as a
        // conflict instead of a save materializing an older snapshot over the change.
        if let Some(kind) = kind_of(command.input()) {
            let report = self.sync_kind(Some(kind))?;
            // A save or activation may replace the harness directory of its target: refuse it
            // while that directory (or the harness root) cannot be observed completely, so a
            // stale snapshot never replaces live changes that could not be captured.
            if kind == InstructionKind::Skill
                && let Some(name) = self.target_skill_name(command)?
                && (report.failures.iter().any(|f| f.starts_with("harness"))
                    || report.skipped.iter().any(|(n, reason)| {
                        n.eq_ignore_ascii_case(&name) && reason.starts_with("harness:")
                    }))
            {
                return Ok(Some(Prepared::new(commands::PreparedRefusal(
                    Refusal::new(DomainErrorCode::SourceChanged, "skill_source", None),
                ))));
            }
        }
        let result = match command.input() {
            OperationInput::Skills(skill_ops::Input::Draft(i)) => {
                self.prepare_draft(ctx, env, &i.content)
            }
            OperationInput::Roles(role_ops::Input::Draft(i)) => {
                self.prepare_draft(ctx, env, &i.content)
            }
            OperationInput::Skills(skill_ops::Input::Rollback(i)) => {
                self.prepare_activation(ctx, env, &i.instruction_id, i.target_revision.get())
            }
            OperationInput::Roles(role_ops::Input::Rollback(i)) => {
                self.prepare_activation(ctx, env, &i.instruction_id, i.target_revision.get())
            }
            _ => Ok(None),
        };
        match result {
            Ok(p) => Ok(p),
            Err(Failure::Storage(e)) => Err(e),
            Err(Failure::Refused(r)) => Ok(Some(Prepared::new(commands::PreparedRefusal(r)))),
        }
    }
    fn settle(
        &self,
        _ctx: &InvocationContext,
        prepared: Option<&Prepared>,
        reply: Option<&crate::protocol::results::DomainCommandReply>,
        env: &Preparation<'_>,
    ) {
        self.settle_command(prepared, reply, env);
    }
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        if let Some(refusal) = command.prepared::<commands::PreparedRefusal>() {
            return decided(tx, Err(refusal.0.clone().into()));
        }
        let outcome = self.decide_command(ctx, command, tx);
        decided(tx, outcome)
    }
}

impl InstructionService {
    /// The skill name a draft or activation writes (`None` for other commands).
    fn target_skill_name(&self, command: &AdmittedCommand) -> Result<Option<String>, StorageError> {
        let id = match command.input() {
            OperationInput::Skills(skill_ops::Input::Draft(i)) => {
                return Ok(Some(i.name.0.trim().to_owned()));
            }
            OperationInput::Skills(skill_ops::Input::Rollback(i)) => i.instruction_id.clone(),
            _ => return Ok(None),
        };
        crate::ports::storage::read_snapshot(&*self.store, &Default::default(), |read| {
            Ok(read.instructions().instruction_item(&id)?.map(|i| i.name))
        })
    }
}

pub(crate) fn kind_of(input: &OperationInput) -> Option<InstructionKind> {
    match input {
        OperationInput::Skills(_) => Some(InstructionKind::Skill),
        OperationInput::Roles(_) => Some(InstructionKind::Role),
        _ => None,
    }
}

pub(crate) fn not_found_failure(kind: InstructionKind, id: &Id) -> Failure {
    not_found(target_kind(kind), id).into()
}
