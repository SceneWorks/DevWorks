//! The durable native Dreaming domain. The gateway, scheduler and desktop share this service,
//! and the provider worker operates on an exact owned run rather than on caller-supplied tools.
mod intake;
pub(crate) mod proposals;
mod queries;
mod revert;
mod tools;
pub mod windows;
mod worker;

pub use intake::SweepDelivery;
pub use tools::DreamTools;
pub use worker::{SweepLease, inference_fence_in};

use super::artifacts::{ArtifactService, host::HostArtifactPublication, version_ref};
use super::kernel::{Failure, Refusal, decided, rev};
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, dreaming as ops};
use crate::ports::storage::{
    ApplicationStore, ArtifactScope, ArtifactState, DreamInstruction, DreamRunRow, DreamScopeState,
    KnowledgeScope, ProjectRead, ReadOptions, ReadSnapshot, StorageError, WriteOptions,
    WriteTransaction, read_snapshot, run_write,
};
use crate::ports::{
    CommandHandler, Decided, HandlerFailure, IdKind, IdSource, QueryAnswer, QueryHandler,
};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{
    DreamConfig, DreamSettingsRecord, InstructionRecord, InstructionState,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, Limit, Page, PositiveCounter};
use crate::protocol::{CorrelationId, DomainErrorCode, DomainFault};
use serde_json::json;
use std::sync::Arc;

pub const OPERATIONS: &[&str] = &[
    "dreaming.run",
    "dreaming.runs",
    "dreaming.report",
    "dreaming.settings",
    "dreaming.instructions",
    "dreaming.propose",
    "dreaming.revert",
    "dreaming.review",
];

pub const REVISION: u32 = 9;
pub const CONSUMER: &str = "dreaming_sweep";
pub const NIGHTLY_JOB_ID: &str = "dream-projects";
const INSTRUCTIONS_LIMIT: i64 = 65_536;
const BASE: &[u8] = include_bytes!("base-instructions.md");

pub struct DreamingService {
    ids: Arc<dyn IdSource>,
}

impl DreamingService {
    pub fn new(ids: Arc<dyn IdSource>) -> Self {
        Self { ids }
    }

    pub fn completed_operation(run: DreamRunRow) -> Result<OperationResult, StorageError> {
        let value = queries::render_run(run).map_err(|failure| match failure {
            Failure::Storage(error) => error,
            Failure::Refused(_) => {
                StorageError::InvalidRecord("invalid retained Dreaming run".into())
            }
        })?;
        Ok(OperationResult::DreamingRun { value })
    }

    /// The base instructions are a real versioned artifact, including for legacy queries.
    pub fn ensure_base(
        store: &dyn ApplicationStore,
        artifacts: Arc<ArtifactService>,
    ) -> Result<(), StorageError> {
        if read_snapshot(store, &ReadOptions::default(), |snapshot| {
            Ok(!snapshot
                .dreaming()?
                .dream_instructions(&KnowledgeScope::Workspace, None, 1)?
                .is_empty())
        })? {
            return Ok(());
        }
        let stage = artifacts
            .stage_host_bytes(store, BASE, json!({"kind":"DreamingBase"}))?
            .map_err(|problem| {
                StorageError::unavailable(crate::ports::storage::Unavailable::Io, problem)
            })?;
        let (publish, staged) = (artifacts.clone(), stage.clone());
        let committed = run_write(store, WriteOptions::host_state(), move |tx| {
            let scope = KnowledgeScope::Workspace;
            if !tx
                .dreaming()?
                .dream_instructions(&scope, None, 1)?
                .is_empty()
            {
                // The other initializer owns the base; retire this staging witness.
                return Ok(false);
            }
            let owner = tx.store().host_actor.clone();
            let version = publish.commit_host_bytes_in_scope(
                tx,
                &staged,
                HostArtifactPublication {
                    title: "Dreaming base instructions",
                    media_type: "text/markdown; charset=utf-8",
                    provenance: json!({
                        "kind":super::artifacts::derivation::DerivedKind::Instruction.as_str(),
                        "purpose":"DreamingBase",
                        "sourceRevision":"afe73560215bbe7593fc59451de5cc3237e3eeca",
                    }),
                    scope: ArtifactScope::Workspace,
                    owner: owner.clone(),
                },
            )?;
            tx.dreaming()?
                .add_dream_instruction(&scope, 0, &version_ref(&version), &owner)?;
            Ok(true)
        });
        artifacts.settle_host_bytes(
            store,
            &stage,
            matches!(committed, Ok(true) | Err(StorageError::CommitUnknown)),
        );
        committed.map(|_| ())
    }

    fn change(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let OperationInput::Dreaming(input) = command.input() else {
            return decided(tx, Err(invalid("dreaming").into()));
        };
        if let ops::Input::Run(input) = input {
            let result = self.run(ctx, command, tx, input);
            return match result {
                Ok(decision) => Ok(decision),
                Err(error) => decided(tx, Err(error)),
            };
        }
        let result = match input {
            ops::Input::SettingsCommand(input) => {
                self.save_settings(tx, ctx, command.schema_revision().0, input)
            }
            ops::Input::InstructionsCommand(input) => self.save_instructions(tx, ctx, input),
            ops::Input::Revert(input) => self.revert(tx, ctx, command.schema_revision().0, input),
            ops::Input::Propose(input) => self.propose(tx, ctx, input),
            ops::Input::Review(input) => self.review(tx, ctx, input),
            _ => Err(Refusal::new(DomainErrorCode::UnsupportedCapability, "dreaming", None).into()),
        };
        decided(tx, result.map(|result| DomainDecision::Applied { result }))
    }

    fn save_settings(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        revision: u32,
        input: &ops::SettingsCommandInput,
    ) -> Result<OperationResult, Failure> {
        let scope = KnowledgeScope::from(&input.scope);
        authorize(ctx, &scope)?;
        human_or_host(ctx)?;
        active(tx.projects(), &scope)?;
        let prior = tx.dreaming()?.dream_scope(&scope)?;
        let mut policy = prior.policy;
        let config = &input.config;
        policy.enabled = config.enabled;
        policy.schedule_id = config.schedule_id.clone();
        policy.batch_limit = config.batch_limit.get() as u16;
        if revision >= REVISION {
            policy.model_id = config.model_id.clone();
            policy.effort = config.effort.clone();
            policy.max_micro_usd = None;
        }
        if let Some(interval) = &config.minimum_interval_ms {
            policy.minimum_interval_ms =
                u64::try_from(interval.get()).map_err(|_| invalid("dreaming_interval"))?;
        }
        if let Some(turns) = &config.max_turns {
            policy.max_turns = turns
                .0
                .parse::<u16>()
                .map_err(|_| invalid("dreaming_turn_budget"))?;
        }
        if let Some(tokens) = &config.max_tokens {
            policy.max_tokens = tokens
                .0
                .parse::<u32>()
                .map_err(|_| invalid("dreaming_token_budget"))?;
        }
        if let Some(elapsed) = &config.max_elapsed_ms {
            policy.max_elapsed_ms = elapsed
                .0
                .parse::<u32>()
                .map_err(|_| invalid("dreaming_time_budget"))?;
        }
        if let Some(cost) = &config.max_micro_usd {
            policy.max_micro_usd =
                Some(u64::try_from(cost.get()).map_err(|_| invalid("dreaming_cost_budget"))?);
        }
        if let Some(id) = &policy.schedule_id {
            let job = tx
                .jobs()
                .job(id)?
                .ok_or_else(|| super::kernel::not_found("dreaming_schedule", id))?;
            if job.project_id.as_ref() != scope.project() {
                return Err(StorageError::ScopeDenied.into());
            }
        }
        let current = current_instruction(tx.dreaming()?, &scope)?;
        let mut update = None;
        if current
            .as_ref()
            .is_none_or(|old| old.artifact != config.instructions)
        {
            checked_instruction(tx, &scope, &config.instructions)?;
            let revision = tx
                .dreaming()?
                .dream_instructions(&scope, None, 1)?
                .first()
                .map_or(0, |row| row.revision);
            update = Some(revision);
        }
        let state =
            tx.dreaming()?
                .set_dream_policy(&scope, input.expected_revision.get(), &policy)?;
        if let Some(revision) = update {
            tx.dreaming()?.add_dream_instruction(
                &scope,
                revision,
                &config.instructions,
                ctx.principal(),
            )?;
        }
        Ok(OperationResult::DreamingSettings {
            value: settings(&scope, state, config.instructions.clone())?,
        })
    }

    fn save_instructions(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        input: &ops::InstructionsCommandInput,
    ) -> Result<OperationResult, Failure> {
        let scope = KnowledgeScope::from(&input.scope);
        authorize(ctx, &scope)?;
        human_or_host(ctx)?;
        active(tx.projects(), &scope)?;
        checked_instruction(tx, &scope, &input.content)?;
        let repo = tx.dreaming()?;
        let prior = repo.dream_instructions(&scope, None, 1)?;
        let mut expected = input.expected_revision.get();
        if prior.is_empty() && expected == 1 {
            expected = 0;
        }
        let saved =
            repo.add_dream_instruction(&scope, expected, &input.content, ctx.principal())?;
        Ok(OperationResult::DreamingInstructions {
            value: Page {
                items: BoundedList(vec![instruction(&scope, saved)]),
                has_more: false,
                next_cursor: None,
            },
        })
    }
}

impl CommandHandler for DreamingService {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        self.change(ctx, command, tx)
    }
}

impl QueryHandler for DreamingService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        self.answer(ctx, query, snapshot).map_err(|error| {
            super::kernel::query_fault(
                error,
                "dreaming",
                CorrelationId(self.ids.next_id(IdKind::Correlation)),
            )
        })
    }
}

fn invalid(kind: &'static str) -> Refusal {
    Refusal::new(DomainErrorCode::InvalidArgument, kind, None)
}

fn authorize(ctx: &InvocationContext, scope: &KnowledgeScope) -> Result<(), StorageError> {
    if ctx
        .project_id()
        .is_some_and(|project| Some(project) != scope.project())
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
}

fn active(read: &dyn ProjectRead, scope: &KnowledgeScope) -> Result<(), StorageError> {
    if let Some(project) = scope.project()
        && !read
            .project(project)?
            .is_some_and(|value| value.lifecycle == "Active")
    {
        return Err(StorageError::ViewUnavailable);
    }
    Ok(())
}

fn human_or_host(ctx: &InvocationContext) -> Result<(), StorageError> {
    if !matches!(
        ctx.principal_kind(),
        PrincipalKind::Human | PrincipalKind::Host
    ) {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
}

fn checked_instruction(
    tx: &mut dyn WriteTransaction,
    scope: &KnowledgeScope,
    input: &ArtifactRef,
) -> Result<(), Failure> {
    let storage = tx.artifacts();
    let Some(artifact) = storage.artifact(&input.artifact_id)? else {
        return Err(super::kernel::not_found("dreaming_instructions", &input.artifact_id).into());
    };
    let expected_scope = match scope {
        KnowledgeScope::Workspace => ArtifactScope::Workspace,
        KnowledgeScope::Project(project) => ArtifactScope::Project(project.clone()),
    };
    if artifact.scope != expected_scope || artifact.state != ArtifactState::Live {
        return Err(StorageError::ScopeDenied.into());
    }
    let valid = storage.version(&input.version_id)?.is_some_and(|version| {
        version.live()
            && version.byte_count <= INSTRUCTIONS_LIMIT
            && version.media_type.starts_with("text/")
            && version_ref(&version) == *input
    });
    if !valid {
        return Err(invalid("dreaming_instruction_artifact").into());
    }
    Ok(())
}

fn current_instruction(
    repo: &dyn crate::ports::storage::DreamingRead,
    scope: &KnowledgeScope,
) -> Result<Option<DreamInstruction>, StorageError> {
    let local = repo.dream_instructions(scope, None, 1)?.into_iter().next();
    if local.is_some() || *scope == KnowledgeScope::Workspace {
        return Ok(local);
    }
    Ok(repo
        .dream_instructions(&KnowledgeScope::Workspace, None, 1)?
        .into_iter()
        .next())
}

fn instruction(scope: &KnowledgeScope, row: DreamInstruction) -> InstructionRecord {
    InstructionRecord {
        id: row.artifact.artifact_id.clone(),
        revision: rev(row.revision),
        name: super::kernel::label(
            if *scope == KnowledgeScope::Workspace {
                "Base instructions"
            } else {
                "Project instructions"
            },
            "Dreaming instructions",
        ),
        scope: scope.into(),
        content: row.artifact,
        source_path: None,
        approved: true,
        applicability: BoundedList(vec![]),
        author: Some(row.editor),
        session_id: None,
        rationale: None,
        state: Some(InstructionState::Approved),
        approval_outcome: None,
        lesson_ids: None,
    }
}

fn settings(
    scope: &KnowledgeScope,
    row: DreamScopeState,
    instructions: ArtifactRef,
) -> Result<DreamSettingsRecord, Failure> {
    let config = row.policy;
    Ok(DreamSettingsRecord {
        scope: scope.into(),
        revision: rev(row.revision),
        config: DreamConfig {
            enabled: config.enabled,
            instructions,
            schedule_id: config.schedule_id,
            batch_limit: Limit::new(u32::from(config.batch_limit))
                .ok_or_else(|| invalid("dreaming_batch_limit"))?,
            model_id: config.model_id,
            effort: config.effort,
            minimum_interval_ms: Some(super::kernel::counter(config.minimum_interval_ms as i64)),
            max_turns: Some(PositiveCounter(config.max_turns.to_string())),
            max_tokens: Some(PositiveCounter(config.max_tokens.to_string())),
            max_elapsed_ms: Some(PositiveCounter(config.max_elapsed_ms.to_string())),
            max_micro_usd: config
                .max_micro_usd
                .map(|cost| super::kernel::counter(cost as i64)),
        },
    })
}
