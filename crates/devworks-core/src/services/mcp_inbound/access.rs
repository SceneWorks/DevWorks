//! A live native credential and durable scope witness back each selected gateway call.

use super::targets::{
    DocumentTarget, IndexRemoval, IndexTargets, WorktreeTarget, document_targets,
};
use super::{AuthenticatedClient, Binding, Configuration, current, selected_operation};
use crate::admission::{project_targets, work_targets};
use crate::authority::DenialReason;
use crate::context::{InvocationContext, PrincipalKind, ReceiptIdentity};
use crate::fingerprint::sha256_hex;
use crate::kernel::work_brief::WorkBrief;
use crate::operations::{OperationInput, conventions, features, history, indexing, issues, tasks};
use crate::ports::storage::{
    ArtifactRead, DELETED, EventFilter, HistoryRead, LedgerEntry, ProjectRead, ReadSnapshot,
    StorageError, StoredWork, WorkRead, WriteTransaction,
};
use crate::ports::{AuthorityFence, AuthorityRequest, Authorization};
use crate::protocol::data::DataScope;
use crate::protocol::{
    ActorId, CommandId, Counter, Id, OperationId, ProjectId, Query, Revision, StoreId,
};
use crate::registry::Mode;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

// Equality compares the lifetime identity, so closing an application does not change
// equality of contexts already held by the same request.
#[derive(Clone, Debug)]
struct ClientLifetime(Arc<AtomicBool>);

impl PartialEq for ClientLifetime {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for ClientLifetime {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClientProof {
    store: StoreId,
    epoch: i64,
    client: Id,
    settings_revision: Revision,
    client_revision: Revision,
    generation: Revision,
    binding: Binding,
    expires_ms: i64,
    live: ClientLifetime,
}

/// Durable, secret-free authority fact. Query publication, writers, and deferred work can
/// recheck this data without holding a socket or the caller's credentials.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessFence {
    store: StoreId,
    epoch: i64,
    client: Id,
    settings_revision: Revision,
    client_revision: Revision,
    generation: Revision,
    binding: Binding,
    expires_ms: i64,
    operation: String,
    command: bool,
    project: ProjectId,
    repositories: Vec<Id>,
    full_project_repositories: bool,
    recovering_receipt: Option<CommandId>,
    recovering_operation: Option<OperationId>,
    worktrees: Vec<WorktreeTarget>,
    #[serde(default)]
    documents: Vec<DocumentTarget>,
    #[serde(default)]
    index_removal: Option<IndexRemoval>,
    work: Vec<WorkScope>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct WorkScope {
    id: Id,
    scope_revision: i64,
}

impl AuthenticatedClient {
    /// Bind the selected project to this request. The authority still checks the grant and
    /// independently resolves each operation target against live application state.
    pub fn invocation(
        &self,
        invocation_id: Id,
        project: ProjectId,
        policy_revision: Revision,
    ) -> InvocationContext {
        let identity = format!(
            "{}:{}:{}",
            self.store.0.as_str(),
            self.epoch,
            self.id.as_str()
        );
        let namespace = Id::new(sha256_hex(identity.as_bytes())).expect("hex digest is an ID");
        let mut context = InvocationContext::new(
            ActorId(self.id.clone()),
            PrincipalKind::McpClient,
            invocation_id,
            namespace,
            self.store.clone(),
            Counter::new(self.epoch).expect("stored epoch"),
            policy_revision,
        )
        .with_lineage(Some(project), None, None);
        context.set_mcp_client_proof(ClientProof {
            store: self.store.clone(),
            epoch: self.epoch,
            client: self.id.clone(),
            settings_revision: self.settings_revision.clone(),
            client_revision: self.client_revision.clone(),
            generation: self.generation.clone(),
            binding: self.binding,
            expires_ms: self.expires_ms,
            live: ClientLifetime(self.live.clone()),
        });
        context
    }
}

pub(crate) fn request_live(ctx: &InvocationContext) -> bool {
    ctx.principal_kind() != PrincipalKind::McpClient
        || ctx
            .mcp_client_proof()
            .is_some_and(|proof| proof.live.0.load(Ordering::Acquire))
}

fn selected_repositories(input: &OperationInput) -> Vec<Id> {
    match input {
        OperationInput::Features(features::Input::List(input)) => input.repository_ids.0.clone(),
        OperationInput::Issues(issues::Input::List(input)) => input.repository_ids.0.clone(),
        OperationInput::Tasks(tasks::Input::List(input)) => input.repository_ids.0.clone(),
        OperationInput::Issues(issues::Input::Create(input)) => input.repository_ids.0.clone(),
        OperationInput::Issues(issues::Input::Link(input)) => vec![input.repository_id.clone()],
        OperationInput::Issues(issues::Input::Publish(input)) => vec![input.repository_id.clone()],
        OperationInput::Tasks(tasks::Input::Create(input)) => input.repository_ids.0.clone(),
        OperationInput::Indexing(indexing::Input::Index(input)) => input.repository_ids.0.clone(),
        OperationInput::Indexing(indexing::Input::Reindex(input)) => input.repository_ids.0.clone(),
        OperationInput::Features(features::Input::Edit(input)) => input
            .changes
            .repository_ids
            .as_ref()
            .map_or_else(Vec::new, |repos| repos.0.clone()),
        OperationInput::Issues(issues::Input::Edit(input)) => {
            edited_repositories(&input.changes.repository_ids, &input.changes.touches)
        }
        OperationInput::Tasks(tasks::Input::Edit(input)) => {
            edited_repositories(&input.changes.repository_ids, &input.changes.touches)
        }
        _ => Vec::new(),
    }
}

fn edited_repositories(
    repositories: &Option<crate::protocol::support::Items<Id>>,
    touches: &Option<crate::protocol::support::Items<crate::protocol::records::TouchRef>>,
) -> Vec<Id> {
    let mut selected = repositories
        .as_ref()
        .map_or_else(Vec::new, |ids| ids.0.clone());
    if let Some(touches) = touches {
        selected.extend(touches.0.iter().map(|touch| touch.repository_id.clone()));
    }
    selected
}

// These views, plans and linked updates involve several work items or disclose their repo
// context. Their existing services take one project boundary. Require the whole repository
// boundary for those operations, including when a list's filter matches a multi-repo item.
fn full_project_repositories(operation: &str) -> bool {
    matches!(
        operation,
        "projects.get"
            | "features.list"
            | "features.create"
            | "features.edit_plan"
            | "features.prioritize"
            | "features.dependencies"
            | "features.request_plan_approval"
            | "issues.list"
            | "issues.prioritize"
            | "tasks.list"
            | "tasks.prioritize"
            | "tasks.dependency_links"
            | "tasks.sync_plan"
            | "workflows.reassign_and_remove"
            | "indexing.configure_sources"
    )
}

/// Project-shaped operations whose exact targets are understood by this access layer. Never
/// allow a new operation based on its family name or a request field alone.
pub(super) fn supported_target(operation: &str) -> bool {
    matches!(
        operation,
        "projects.get"
            | "features.list"
            | "features.get"
            | "features.create"
            | "features.edit"
            | "features.edit_plan"
            | "features.prioritize"
            | "features.dependencies"
            | "features.request_plan_approval"
            | "issues.list"
            | "issues.get"
            | "issues.create"
            | "issues.edit"
            | "issues.link"
            | "issues.publish"
            | "issues.prioritize"
            | "tasks.list"
            | "tasks.get"
            | "tasks.create"
            | "tasks.edit"
            | "tasks.prioritize"
            | "tasks.move"
            | "tasks.assign"
            | "tasks.dependency_links"
            | "tasks.acceptance_evidence"
            | "tasks.sync_plan"
            | "tasks.set_feature"
            | "workflows.list"
            | "workflows.create_status"
            | "workflows.rename_status"
            | "workflows.order"
            | "workflows.reassign_and_remove"
            | "knowledge.read"
            | "knowledge.search"
            | "knowledge.record"
            | "knowledge.revise"
            | "knowledge.relate"
            | "knowledge.tag"
            | "knowledge.verify"
            | "knowledge.retract"
            | "knowledge.scope"
            | "knowledge.delete_by_source"
            | "knowledge.delete_selected"
            | "knowledge.promote"
            | "knowledge.challenge"
            | "history.command_receipt"
            | "history.operation_status"
            | "documents.import"
            | "documents.index"
            | "documents.retry"
            | "documents.disable_retrieval"
            | "documents.remove_index"
            | "conventions.edit"
            | "conventions.applicability"
            | "conventions.retire"
            | "conventions.index"
            | "conventions.import"
            | "conventions.delete"
            | "conventions.move"
            | "conventions.delete_resource"
            | "indexing.index"
            | "indexing.reindex"
            | "indexing.configure_sources"
            | "indexing.refresh_overlay"
            | "indexing.retire_view"
            | "indexing.cancel"
            | "code.set_trust"
            | "repositories.remove_index"
    )
}

pub(crate) fn authorize(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    request: &AuthorityRequest<'_>,
) -> Result<AccessFence, DenialReason> {
    let proof = ctx
        .mcp_client_proof()
        .filter(|proof| proof.client == ctx.principal().0)
        .ok_or(DenialReason::UnboundPrincipal)?;
    if !request_live(ctx) {
        return Err(DenialReason::StaleInvocation);
    }
    let operation = request.descriptor.name;
    if !selected_operation(operation, request.binding.mode) || !supported_target(operation) {
        return Err(DenialReason::ToolNotSelected);
    }
    let project = ctx.project_id().ok_or(DenialReason::OutOfProjectScope)?;
    let other_scope = match request.input {
        OperationInput::Conventions(conventions::Input::Move(input)) => Some(&input.destination),
        OperationInput::Conventions(conventions::Input::Index(input)) => input.all_scope.as_ref(),
        _ => None,
    };
    if other_scope.is_some_and(
        |scope| !matches!(scope, DataScope::Project { project_id } if project_id == project),
    ) {
        return Err(DenialReason::OutOfProjectScope);
    }
    let normalized = request
        .input
        .body()
        .map_err(|_| DenialReason::AuthorityUnavailable)?;
    if project_targets(&normalized)
        .iter()
        .any(|target| target != project)
    {
        return Err(DenialReason::OutOfProjectScope);
    }
    let mut work = Vec::new();
    let (_, config) =
        current(read.configuration()).map_err(|_| DenialReason::AuthorityUnavailable)?;
    let client = config
        .client(&proof.client)
        .ok_or(DenialReason::UnboundPrincipal)?;
    let exact = IndexTargets::resolve(read, ctx, request.input, &client.access.worktree_ids.0)
        .map_err(target_error)?;
    let mut repositories = selected_repositories(request.input);
    repositories.extend(exact.repositories);
    let mut worktrees = exact.worktrees;
    let mut documents =
        document_targets(read.artifacts(), project, request.input).map_err(target_error)?;
    documents.extend(exact.document_run);
    let full_project_repositories = full_project_repositories(operation);
    if full_project_repositories {
        repositories.extend(
            read.projects()
                .members(project)
                .map_err(|_| DenialReason::AuthorityUnavailable)?,
        );
    }
    let mut work_targets = work_targets(&normalized);
    if let OperationInput::Tasks(tasks::Input::Create(input)) = request.input
        && let Some(parent) = &input.parent_id
        && !work_targets.contains(&parent.0)
    {
        work_targets.push(parent.0.clone());
    }

    let recovering_receipt = recovery_scope(read, ctx, request.input)?;
    let recovering_operation = operation_scope(read, ctx, request.input)?;
    for original in recovering_receipt
        .iter()
        .map(|(_, access)| access)
        .chain(recovering_operation.iter().map(|(_, access)| access))
    {
        repositories.extend(original.repositories.iter().cloned());
        worktrees.extend(original.worktrees.iter().cloned());
        for prior in &original.documents {
            documents.push(
                prior
                    .current(read.artifacts(), project)
                    .map_err(target_error)?,
            );
        }
        for witness in &original.work {
            if !work_targets.contains(&witness.id) {
                work_targets.push(witness.id.clone());
            }
        }
    }
    let mut next = 0;
    while let Some(id) = work_targets.get(next).cloned() {
        next += 1;
        let row = read
            .work()
            .work(&id)
            .map_err(target_error)?
            .filter(|row| &row.project_id == project)
            .ok_or(DenialReason::OutOfProjectScope)?;
        repositories.extend(work_repositories(read.work(), &row).map_err(target_error)?);
        if let Some(parent) = row.parent_feature_id
            && !work_targets.contains(&parent)
        {
            work_targets.push(parent);
        }
        work.push(WorkScope {
            id,
            scope_revision: row.scope_revision,
        });
    }
    repositories.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    repositories.dedup();

    let fence = AccessFence {
        store: proof.store.clone(),
        epoch: proof.epoch,
        client: proof.client.clone(),
        settings_revision: proof.settings_revision.clone(),
        client_revision: proof.client_revision.clone(),
        generation: proof.generation.clone(),
        binding: proof.binding,
        expires_ms: proof.expires_ms,
        operation: operation.into(),
        command: request.binding.mode == Mode::Command,
        project: project.clone(),
        repositories,
        full_project_repositories,
        recovering_receipt: recovering_receipt.map(|(command, _)| command),
        recovering_operation: recovering_operation.map(|(operation, _)| operation),
        worktrees,
        documents,
        index_removal: exact.removal,
        work,
    };
    if fence
        .holds(read)
        .map_err(|_| DenialReason::AuthorityUnavailable)?
    {
        Ok(fence)
    } else {
        Err(DenialReason::OutOfProjectScope)
    }
}

fn work_repositories(read: &dyn WorkRead, row: &StoredWork) -> Result<Vec<Id>, StorageError> {
    let mut repositories = read.work_repositories(&row.id)?;
    repositories.extend(
        WorkBrief::of(row)?
            .touches
            .into_iter()
            .map(|touch| touch.repository_id),
    );
    Ok(repositories)
}

fn target_error(error: StorageError) -> DenialReason {
    match error {
        StorageError::ScopeDenied => DenialReason::OutOfProjectScope,
        _ => DenialReason::AuthorityUnavailable,
    }
}

/// Bind receipt recovery to the original owning client/project and its target scopes. A
/// concurrent receipt that did not exist here cannot be published under an empty target proof.
fn recovery_scope(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    input: &OperationInput,
) -> Result<Option<(CommandId, AccessFence)>, DenialReason> {
    let OperationInput::History(history::Input::CommandReceipt(query)) = input else {
        return Ok(None);
    };
    let Query::Receipt { request_key } = &query.query else {
        return Err(DenialReason::OutOfProjectScope);
    };
    let identity = ReceiptIdentity::bind(ctx, request_key.clone());
    let ledger = read
        .receipts()
        .lookup(&identity)
        .map_err(|_| DenialReason::AuthorityUnavailable)?;
    let (command, sequence) = match ledger {
        None => return Ok(None),
        Some(LedgerEntry::Current(stored)) => {
            (stored.receipt.command_id, stored.receipt.committed_sequence)
        }
        Some(LedgerEntry::Purged(stored)) => {
            (stored.outcome.command_id, stored.outcome.committed_sequence)
        }
        Some(LedgerEntry::Incompatible { .. }) => return Err(DenialReason::OutOfProjectScope),
    };
    let original = receipt_fence(read.history(), &command, &sequence)
        .map_err(|_| DenialReason::AuthorityUnavailable)?
        .filter(|access| access.store_matches(ctx) && access.command)
        .ok_or(DenialReason::OutOfProjectScope)?;
    Ok(Some((command, original)))
}

pub(crate) const OPERATION_AGGREGATE: &str = "incoming_mcp_operation";
pub(crate) const OPERATION_ADMITTED: &str = "McpOperationAdmitted";

fn operation_scope(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    input: &OperationInput,
) -> Result<Option<(OperationId, AccessFence)>, DenialReason> {
    let OperationInput::History(history::Input::OperationStatus(query)) = input else {
        return Ok(None);
    };
    let Query::Operation { operation_id } = &query.query else {
        return Err(DenialReason::OutOfProjectScope);
    };
    let access = operation_fence(read.history(), ctx, operation_id)
        .map_err(target_error)?
        .ok_or(DenialReason::OutOfProjectScope)?;
    Ok(Some((operation_id.clone(), access)))
}

pub(crate) fn operation_fence(
    history: &dyn HistoryRead,
    ctx: &InvocationContext,
    operation_id: &OperationId,
) -> Result<Option<AccessFence>, StorageError> {
    let page = history.page(
        &EventFilter::Aggregate {
            kind: OPERATION_AGGREGATE.into(),
            id: operation_id.0.as_str().into(),
        },
        0,
        1,
    )?;
    let Some(event) = page.events.first().filter(|event| {
        event.kind == OPERATION_ADMITTED
            && &event.actor == ctx.principal()
            && event.operation_id.as_ref() == Some(operation_id)
    }) else {
        return Ok(None);
    };
    let access: AccessFence = serde_json::from_value(event.payload["incomingMcpAccess"].clone())
        .map_err(|_| StorageError::InvalidRecord("incoming MCP operation access".into()))?;
    Ok((access.store_matches(ctx) && access.command).then_some(access))
}

pub(crate) fn operation_recovery_matches(
    authorization: &Authorization,
    operation: &OperationId,
) -> bool {
    command_fence(authorization)
        .is_some_and(|access| access.recovering_operation.as_ref() == Some(operation))
}

pub(crate) fn receipt_recovery_matches(authorization: &Authorization, command: &CommandId) -> bool {
    command_fence(authorization)
        .is_some_and(|access| access.recovering_receipt.as_ref() == Some(command))
}

/// The authority fact persisted by a command recorder. It contains no key or token.
pub(crate) fn command_fence(authorization: &Authorization) -> Option<&AccessFence> {
    authorization.fences().iter().find_map(|fence| match fence {
        AuthorityFence::ExternalMcp(access) => Some(access.as_ref()),
        _ => None,
    })
}

pub(crate) fn receipt_fence(
    history: &dyn HistoryRead,
    command: &CommandId,
    sequence: &Revision,
) -> Result<Option<AccessFence>, StorageError> {
    let page = history.page(
        &EventFilter::Aggregate {
            kind: "command".into(),
            id: command.0.as_str().into(),
        },
        sequence.get() - 1,
        1,
    )?;
    let Some(event) = page
        .events
        .first()
        .filter(|event| event.kind == "CommandDecided" && event.sequence == sequence.get())
    else {
        return Ok(None);
    };
    match event.payload.get("incomingMcpAccess") {
        Some(encoded) if !encoded.is_null() => {
            serde_json::from_value::<AccessFence>(encoded.clone())
                .map(Some)
                .map_err(|_| StorageError::InvalidRecord("incoming MCP receipt access".into()))
        }
        _ => Ok(None),
    }
}

impl AccessFence {
    fn store_matches(&self, ctx: &InvocationContext) -> bool {
        &self.store == ctx.store_id()
            && self.epoch == ctx.restore_epoch().get()
            && self.client == ctx.principal().0
            && ctx.project_id() == Some(&self.project)
    }

    pub(crate) fn receipt_can_replay(
        &self,
        current_request: &Self,
        work: &dyn WorkRead,
    ) -> Result<bool, StorageError> {
        if self.store != current_request.store
            || self.epoch != current_request.epoch
            || self.client != current_request.client
            || self.project != current_request.project
            || self.operation != current_request.operation
            || !self.command
            || !current_request.command
            || self
                .worktrees
                .iter()
                .any(|tree| !current_request.worktrees.contains(tree))
            || self
                .repositories
                .iter()
                .any(|repo| !current_request.repositories.contains(repo))
            || self.documents.iter().any(|previous| {
                !current_request
                    .documents
                    .iter()
                    .any(|current| current.covers(previous))
            })
        {
            return Ok(false);
        }
        self.work_holds(work, &current_request.repositories, false)
    }

    fn work_holds(
        &self,
        work: &dyn WorkRead,
        allowed_repositories: &[Id],
        require_original_revision: bool,
    ) -> Result<bool, StorageError> {
        for witness in &self.work {
            let Some(row) = work.work(&witness.id)? else {
                return Ok(false);
            };
            if row.project_id != self.project
                || (require_original_revision && row.scope_revision != witness.scope_revision)
                || row
                    .parent_feature_id
                    .as_ref()
                    .is_some_and(|parent| !self.work.iter().any(|scope| &scope.id == parent))
                || work_repositories(work, &row)?
                    .iter()
                    .any(|repository| !allowed_repositories.contains(repository))
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn registrations_hold(
        &self,
        configuration: &Configuration,
        store: &StoreId,
        epoch: i64,
        now_ms: i64,
    ) -> bool {
        if self.store != *store
            || self.epoch != epoch
            || self.expires_ms <= now_ms
            || self.settings_revision != configuration.settings.revision
            || !self.binding.enabled(configuration, epoch)
        {
            return false;
        }
        let Some(client) = configuration.client(&self.client) else {
            return false;
        };
        let access = &client.access;
        if !access.enabled
            || client.authorized_epoch != Some(epoch)
            || self.client_revision != access.revision
            || self.generation != access.credential_generation
            || !access.project_ids.0.contains(&self.project)
            || self
                .worktrees
                .iter()
                .any(|tree| !access.worktree_ids.0.contains(&tree.id))
            || self
                .repositories
                .iter()
                .any(|repository| !access.repository_ids.0.contains(repository))
        {
            return false;
        }
        let operations = if self.command {
            &access.command_operations
        } else {
            &access.read_operations
        };
        operations
            .0
            .iter()
            .any(|operation| operation.0 == self.operation)
    }

    fn project_holds(&self, projects: &dyn ProjectRead) -> Result<bool, StorageError> {
        if !projects
            .project(&self.project)?
            .is_some_and(|p| p.lifecycle != DELETED)
        {
            return Ok(false);
        }
        let members = projects.members(&self.project)?;
        Ok(self
            .repositories
            .iter()
            .all(|repository| members.contains(repository))
            && (!self.full_project_repositories || members.len() == self.repositories.len()))
    }

    fn worktrees_hold(
        &self,
        indexing: &dyn crate::ports::storage::IndexRead,
    ) -> Result<bool, StorageError> {
        for tree in &self.worktrees {
            if !tree.holds(indexing)? {
                return Ok(false);
            }
        }
        self.index_removal
            .as_ref()
            .map_or(Ok(true), |removal| removal.holds(indexing))
    }

    fn documents_hold(&self, artifacts: &dyn ArtifactRead) -> Result<bool, StorageError> {
        for target in &self.documents {
            if !target.holds(artifacts, &self.project)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(crate) fn holds(&self, read: &dyn ReadSnapshot) -> Result<bool, StorageError> {
        let stamp = read.stamp();
        let (_, configuration) = current(read.configuration())?;
        Ok(self.registrations_hold(
            &configuration,
            &stamp.store_id,
            stamp.restore_epoch,
            stamp.now_ms,
        ) && self.project_holds(read.projects())?
            && self.worktrees_hold(read.indexing())?
            && self.work_holds(read.work(), &self.repositories, true)?
            && self.documents_hold(read.artifacts())?)
    }

    /// A successful command may itself advance its work item's scope revision.
    /// Disclose its receipt only when the resulting work still belongs to the admitted scope.
    pub(crate) fn publication_holds(&self, read: &dyn ReadSnapshot) -> Result<bool, StorageError> {
        let stamp = read.stamp();
        let (_, configuration) = current(read.configuration())?;
        Ok(self.registrations_hold(
            &configuration,
            &stamp.store_id,
            stamp.restore_epoch,
            stamp.now_ms,
        ) && self.project_holds(read.projects())?
            && self.worktrees_hold(read.indexing())?
            && self.work_holds(read.work(), &self.repositories, false)?
            && self.documents_hold(read.artifacts())?)
    }

    pub(crate) fn holds_in(&self, tx: &mut dyn WriteTransaction) -> Result<bool, StorageError> {
        let (_, configuration) = current(tx.configuration())?;
        let stamp = tx.store();
        Ok(self.registrations_hold(
            &configuration,
            &stamp.store_id,
            stamp.restore_epoch,
            tx.now_ms(),
        ) && self.project_holds(tx.projects())?
            && self.worktrees_hold(tx.indexing())?
            && self.work_holds(tx.work(), &self.repositories, true)?
            && self.documents_hold(tx.artifacts())?)
    }
}
