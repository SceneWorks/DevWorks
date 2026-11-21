use super::publish::{destination, repository_target};
use super::source::Captured;
use super::{ImportSources, SnapshotError, SourceRecord};
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::{canonical_digest, sha256_hex};
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, imports as ops};
use crate::ports::storage::{
    ApplicationStore, ArtifactScope, ReadSnapshot, StorageError, StoredVersion, WorkImportConfig,
    WorkImportCounts, WorkImportMapping, WorkImportMode, WorkImportRepositoryBinding,
    WorkImportRun, WriteOptions, WriteTransaction, run_write,
};
use crate::ports::{
    CommandHandler, Decided, HandlerFailure, IdKind, Preparation, Prepared, QueryAnswer,
    QueryHandler,
};
use crate::protocol::provider::{ArtifactRef, Digest};
use crate::protocol::records::{
    ImportConfig, ImportIntentMode, ImportRun, ImportSettingsView, MutationSummary,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, Page};
use crate::protocol::{
    CorrelationId, DomainErrorCode, DomainFault, Id, OperationId, OperationState, ProjectId,
    RetryAdvice, RevisionDimension,
};
use crate::services::artifacts::ArtifactService;
use crate::services::artifacts::host::HostArtifactPublication;
use crate::services::cursor::CursorBinding;
use crate::services::kernel::{Failure, Refusal, counter, decided, not_found, rev};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::sync::Arc;

pub const OPERATIONS: [&str; 9] = [
    "imports.preview",
    "imports.configure",
    "imports.apply",
    "imports.runs",
    "imports.inspect",
    "imports.settings",
    "imports.history",
    "imports.cancel",
    "imports.sync",
];

pub struct ImportService {
    pub(super) store: Arc<dyn ApplicationStore>,
    pub(super) sources: ImportSources,
    pub(super) artifacts: Arc<ArtifactService>,
}

pub(super) struct Preview {
    pub(super) scope_digest: String,
    pub(super) binding_digest: String,
    pub(super) since: Option<i64>,
    pub(super) counts: WorkImportCounts,
    pub(super) repository_bindings: Vec<WorkImportRepositoryBinding>,
    pub(super) rows: i64,
    pub(super) bytes: Vec<u8>,
}

struct PreparedSource {
    namespace: Id,
    digest: String,
    root: Option<crate::authority::destructive::FileIdentity>,
}

impl ImportService {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        sources: ImportSources,
        artifacts: Arc<ArtifactService>,
    ) -> Self {
        Self {
            store,
            sources,
            artifacts,
        }
    }

    fn preview_query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
        input: &ops::PreviewInput,
    ) -> Result<WorkImportRun, Failure> {
        human(ctx)?;
        let capture = self
            .sources
            .capture(&input.config.source, None, &|| stopped(env))
            .map_err(source_failure)?;
        let mode = match input.mode {
            None | Some(ImportIntentMode::Full) => WorkImportMode::Full,
            Some(ImportIntentMode::Delta) => WorkImportMode::Delta,
        };
        if mode == WorkImportMode::Full && input.updated_since_ms.is_some() {
            return Err(refuse(DomainErrorCode::InvalidArgument, "import_window"));
        }
        let preview = env.read(|read| {
            Ok(calculate(
                read,
                ctx.principal(),
                &capture,
                &input.config,
                mode,
                input.updated_since_ms.as_ref().map(|time| time.get()),
                &|| stopped(env),
            ))
        })??;
        self.record_preview(
            ctx,
            query.authorization(),
            &capture,
            ctx.principal(),
            mode,
            preview,
        )
    }

    pub(super) fn record_preview(
        &self,
        ctx: &InvocationContext,
        authorization: &crate::ports::Authorization,
        source: &Captured,
        owner: &crate::protocol::ActorId,
        mode: WorkImportMode,
        preview: Preview,
    ) -> Result<WorkImportRun, Failure> {
        let preview_digest = sha256_hex(&preview.bytes);
        if let Some(existing) =
            crate::ports::storage::read_snapshot(&*self.store, &Default::default(), |read| {
                read.work_imports()?.import_preview(owner, &preview_digest)
            })?
        {
            return Ok(existing);
        }
        let staged = self
            .artifacts
            .stage_host_bytes(
                &*self.store,
                &preview.bytes,
                json!({"kind":"WorkImportPreview"}),
            )?
            .map_err(|_| StorageError::ViewUnavailable)?;
        let held = staged.clone();
        let actor = ctx.clone();
        let owner = owner.clone();
        let auth = authorization.clone();
        let artifacts = self.artifacts.clone();
        let namespace = source.snapshot.manifest.namespace.clone();
        let snapshot = source.snapshot.digest.clone();
        let covered = source.snapshot.manifest.captured_at_ms;
        let root = source.root_identity;
        // The sole side effect of preview is recording its evidence. It never edits live work,
        // configures an import, or queues it. Recheck the same grant just before publication.
        let result = run_write(&*self.store, WriteOptions::host_state(), move |tx| {
            if !crate::authority::authorization_holds_in(tx, &actor, &auth)? {
                return Err(StorageError::ScopeDenied);
            }
            if let Some(existing) = tx.work_imports()?.import_preview(&owner, &preview_digest)? {
                return Ok((existing, false));
            }
            let report = artifacts.commit_host_bytes_in_scope(
                tx,
                &held,
                HostArtifactPublication {
                    title: "Work import preview",
                    media_type: "application/json",
                    provenance: json!({"kind":"WorkImportPreview","sourceNamespace":namespace}),
                    scope: ArtifactScope::Workspace,
                    owner: owner.clone(),
                },
            )?;
            let run = WorkImportRun {
                id: tx.new_id(IdKind::Operation),
                owner: owner.clone(),
                revision: 1,
                source_namespace: namespace,
                source_snapshot: snapshot,
                captured_through_ms: covered,
                scope_digest: preview.scope_digest,
                mode: WorkImportMode::DryRun,
                requested_mode: mode,
                updated_since_ms: preview.since,
                config_id: None,
                config_revision: None,
                preview_digest,
                binding_digest: preview.binding_digest,
                root_identity: root,
                state: OperationState::ObservedComplete,
                report: artifact_ref(report),
                input: None,
                cursor: 0,
                imported_ids: Vec::new(),
                repository_bindings: preview.repository_bindings,
                issues: Vec::new(),
                issue_count: 0,
                rows: preview.rows,
                counts: preview.counts,
                started_at_ms: tx.now_ms(),
                completed_at_ms: Some(tx.now_ms()),
            };
            tx.work_imports()?.put_import_run(&run, None)?;
            Ok((run, true))
        });
        self.artifacts.settle_host_bytes(
            &*self.store,
            &staged,
            result.as_ref().is_ok_and(|(_, published)| *published)
                || matches!(result, Err(StorageError::CommitUnknown)),
        );
        result.map(|(run, _)| run).map_err(source_failure)
    }

    fn configure(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
        input: &ops::ConfigureInput,
    ) -> Result<DomainDecision, Failure> {
        human(ctx)?;
        let prepared = command
            .prepared::<PreparedSource>()
            .ok_or_else(|| invalid("missing prepared import settings"))?;
        let current = tx
            .work_imports()?
            .import_settings(ctx.principal(), &prepared.namespace)?;
        match (
            current.as_ref(),
            input.config_id.as_ref(),
            input.expected_revision.as_ref(),
        ) {
            (None, None, None) => {}
            (Some(saved), Some(id), Some(expected)) if saved.id == *id => {
                check_revision(&saved.id, expected.get(), saved.revision)?;
            }
            _ => return Err(refuse(DomainErrorCode::RevisionConflict, "import_settings")),
        }
        let scope = scope_digest(&prepared.namespace, &input.config)?;
        let approved = input.approve.unwrap_or(command.schema_revision().0 == 1);
        if approved
            && tx
                .work_imports()?
                .matching_preview(
                    ctx.principal(),
                    &prepared.namespace,
                    &scope,
                    &prepared.digest,
                )?
                .is_none_or(|preview| preview.root_identity != prepared.root)
        {
            return Err(refuse(DomainErrorCode::SourceChanged, "import_preview"));
        }
        if let Some(saved) = &current
            && saved.approved == approved
            && prepared.root == saved.root_identity
            && scope_digest(&saved.source_namespace, &saved.settings)? == scope
        {
            return Ok(DomainDecision::NoChange {
                result: OperationResult::ImportsConfigure {
                    value: MutationSummary {
                        id: saved.id.clone(),
                        revision: rev(saved.revision),
                        changed: false,
                    },
                },
            });
        }
        let settings = WorkImportConfig {
            id: current
                .as_ref()
                .map(|s| s.id.clone())
                .unwrap_or_else(|| tx.new_id(IdKind::Operation)),
            owner: ctx.principal().clone(),
            revision: current.as_ref().map_or(1, |s| s.revision + 1),
            source_namespace: prepared.namespace.clone(),
            root_identity: prepared.root,
            settings: input.config.clone(),
            all_projects: input.config.all_projects.unwrap_or(false),
            approved,
            allow_scheduled_delta: approved && input.config.allow_scheduled_delta.unwrap_or(false),
            updated_at_ms: tx.now_ms(),
        };
        tx.work_imports()?
            .put_import_config(&settings, current.as_ref().map(|s| s.revision))?;
        Ok(DomainDecision::Applied {
            result: OperationResult::ImportsConfigure {
                value: MutationSummary {
                    id: settings.id,
                    revision: rev(settings.revision),
                    changed: true,
                },
            },
        })
    }
}

fn inspect(
    read: &dyn ReadSnapshot,
    actor: &InvocationContext,
    id: &Id,
) -> Result<ImportRun, Failure> {
    let run = read
        .work_imports()?
        .import_run(id)?
        .filter(|run| run.owner == *actor.principal())
        .ok_or_else(|| not_found("work_import_run", id))?;
    view(read, &run).map_err(Into::into)
}

fn view(read: &dyn ReadSnapshot, run: &WorkImportRun) -> Result<ImportRun, StorageError> {
    let mut result = run_view(run);
    if run.mode != WorkImportMode::DryRun
        && run.completed_at_ms.is_none()
        && let Some(op) = read.operations().operation(&OperationId(run.id.clone()))?
    {
        result.state = op.state;
    }
    Ok(result)
}

pub(super) fn run_view(run: &WorkImportRun) -> ImportRun {
    ImportRun {
        id: run.id.clone(),
        revision: rev(run.revision),
        source_snapshot: Digest(run.source_snapshot.clone()),
        state: run.state,
        imported_ids: BoundedList(run.imported_ids.clone()),
        report: run.report.clone(),
        preview_digest: Some(Digest(run.preview_digest.clone())),
        source_namespace: Some(run.source_namespace.clone()),
        mode: Some(match run.requested_mode {
            WorkImportMode::DryRun | WorkImportMode::Full => ImportIntentMode::Full,
            WorkImportMode::Delta => ImportIntentMode::Delta,
        }),
        updated_since_ms: run.updated_since_ms.map(counter),
        processed: Some(counter(run.cursor as i64)),
        total: Some(counter(run.rows)),
    }
}

impl QueryHandler for ImportService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, DomainFault> {
        let OperationInput::Imports(ops::Input::Preview(input)) = query.input() else {
            return Ok(None);
        };
        self.preview_query(ctx, query, env, input)
            .map(|run| Some(Prepared::new(run)))
            .map_err(|e| e.query_fault("work_import", CorrelationId(query.query_id().0.clone())))
    }

    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        read: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let answer = (|| -> Result<OperationResult, Failure> {
            if let OperationInput::Imports(ops::Input::History(input)) = query.input() {
                return Ok(OperationResult::ImportsHistory {
                    value: super::history::history(read, ctx, input)?,
                });
            }
            human(ctx)?;
            Ok(match query.input() {
                OperationInput::Imports(ops::Input::Preview(_)) => {
                    let run = query
                        .prepared::<WorkImportRun>()
                        .ok_or_else(|| invalid("missing prepared preview"))?;
                    OperationResult::ImportsPreview {
                        value: run_view(run),
                    }
                }
                OperationInput::Imports(ops::Input::Inspect(input)) => {
                    OperationResult::ImportsInspect {
                        value: inspect(read, ctx, &input.run_id)?,
                    }
                }
                OperationInput::Imports(ops::Input::Runs(input)) => {
                    let binding = CursorBinding::new(
                        ctx,
                        "imports.runs",
                        &[input.source_namespace.as_ref().map_or("", Id::as_str)],
                        1,
                    );
                    let before = binding
                        .resume(&input.page)?
                        .map(|pos| pos.parse())
                        .transpose()
                        .map_err(|_| StorageError::CursorExpired)?;
                    let limit = input.page.limit.get();
                    let rows = read.work_imports()?.import_runs(
                        ctx.principal(),
                        input.source_namespace.as_ref(),
                        before,
                        limit + 1,
                    )?;
                    let has_more = rows.len() > limit as usize;
                    let next_cursor =
                        has_more.then(|| binding.cursor(&rows[limit as usize - 1].0.to_string()));
                    let items = rows
                        .iter()
                        .take(limit as usize)
                        .map(|(_, run)| view(read, run))
                        .collect::<Result<Vec<_>, _>>()?;
                    OperationResult::ImportsRuns {
                        value: Page {
                            items: BoundedList(items),
                            next_cursor,
                            has_more,
                        },
                    }
                }
                OperationInput::Imports(ops::Input::Settings(input)) => {
                    let saved = read
                        .work_imports()?
                        .import_settings(ctx.principal(), &input.source_namespace)?;
                    let proposed = input.proposal.clone();
                    let selected = proposed
                        .clone()
                        .or_else(|| saved.as_ref().map(|s| s.settings.clone()))
                        .ok_or_else(|| not_found("import_settings", &input.source_namespace))?;
                    OperationResult::ImportsSettings {
                        value: ImportSettingsView {
                            id: saved.as_ref().map(|s| s.id.clone()),
                            revision: saved.as_ref().map(|s| rev(s.revision)),
                            source_namespace: input.source_namespace.clone(),
                            approved: proposed.is_none() && saved.is_some_and(|s| s.approved),
                            config: selected,
                        },
                    }
                }
                _ => return Err(refuse(DomainErrorCode::InvalidArgument, "import_operation")),
            })
        })();
        answer
            .map(|result| QueryAnswer {
                stamp: read
                    .stamp()
                    .read_stamp(ctx.policy_revision().clone(), rev(1)),
                result,
            })
            .map_err(|e| e.query_fault("work_import", CorrelationId(query.query_id().0.clone())))
    }
}

impl CommandHandler for ImportService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        let OperationInput::Imports(input) = command.input() else {
            return Ok(None);
        };
        let source = match input {
            ops::Input::Configure(input) => {
                human(ctx).map_err(Failure::into_storage)?;
                let captured = self
                    .sources
                    .capture(&input.config.source, None, &|| stopped(env))?;
                captured
                    .snapshot
                    .select(&input.config, input.config.all_projects.unwrap_or(false))
                    .map_err(|_| invalid("invalid import selection"))?;
                PreparedSource {
                    namespace: captured.snapshot.manifest.namespace,
                    digest: captured.snapshot.digest,
                    root: captured.root_identity,
                }
            }
            ops::Input::Apply(input) => return self.prepare_apply(ctx, env, input),
            ops::Input::Sync(input) => return self.prepare_sync(ctx, command, env, input),
            _ => return Ok(None),
        };
        Ok(Some(Prepared::new(source)))
    }

    fn settle(
        &self,
        _ctx: &InvocationContext,
        prepared: Option<&Prepared>,
        reply: Option<&crate::protocol::results::DomainCommandReply>,
        _env: &Preparation<'_>,
    ) {
        self.settle_prepared_run(prepared, reply);
    }

    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let result = match command.input() {
            OperationInput::Imports(ops::Input::Configure(input)) => {
                self.configure(ctx, command, tx, input)
            }
            OperationInput::Imports(ops::Input::Apply(input)) => {
                self.apply(ctx, command, tx, input)
            }
            OperationInput::Imports(ops::Input::Sync(input)) => self.sync(ctx, command, tx, input),
            OperationInput::Imports(ops::Input::Cancel(input)) => {
                super::worker::cancel(ctx, tx, input)
            }
            _ => Err(refuse(DomainErrorCode::InvalidArgument, "import_operation")),
        };
        let operation_id = match &result {
            Ok(DomainDecision::Accepted { operation_id }) => Some(operation_id.clone()),
            _ => None,
        };
        let mut decision = decided(tx, result)?;
        decision.operation_id = operation_id;
        Ok(decision)
    }
}

pub(super) fn calculate(
    read: &dyn ReadSnapshot,
    actor: &crate::protocol::ActorId,
    source: &Captured,
    config: &ImportConfig,
    mode: WorkImportMode,
    explicit_since: Option<i64>,
    stopped: &dyn Fn() -> bool,
) -> Result<Preview, Failure> {
    let snapshot = &source.snapshot;
    let namespace = &snapshot.manifest.namespace;
    let scope_digest = scope_digest(namespace, config)?;
    let imports = read.work_imports()?;
    let config_revision = imports
        .import_settings(actor, namespace)?
        .filter(|stored| {
            stored.approved
                && super::service::scope_digest(namespace, &stored.settings)
                    .is_ok_and(|scope| scope == scope_digest)
        })
        .map(|stored| stored.revision);
    let old = imports.latest_import(actor, namespace, &scope_digest, config_revision)?;
    let high_water = old.as_ref().map(|run| run.captured_through_ms);
    if high_water.is_some_and(|bound| bound > snapshot.manifest.captured_at_ms) {
        return Err(refuse(DomainErrorCode::SourceChanged, "import_coverage"));
    }
    let since = if mode == WorkImportMode::Delta {
        Some(
            snapshot
                .delta_start(explicit_since, high_water)
                .map_err(selection_failure)?,
        )
    } else {
        None
    };
    let records = snapshot
        .select(config, config.all_projects.unwrap_or(false))
        .map_err(selection_failure)?
        .records;
    let delta = DeltaSelection::new(
        read,
        actor,
        namespace,
        &scope_digest,
        config_revision,
        since,
        &records,
    )?;
    let mut hash = Sha256::new();
    let mut counts = WorkImportCounts::default();
    let mut repository_bindings = Vec::new();
    let mut missing = std::collections::BTreeMap::<&str, usize>::new();
    let mut row_counts = std::collections::BTreeMap::<&str, usize>::new();
    for record in &records {
        if stopped() {
            return Err(StorageError::Cancelled.into());
        }
        *row_counts.entry(record.kind()).or_default() += 1;
        if let SourceRecord::History {
            history_kind,
            evidence,
            ..
        } = record
        {
            match history_kind {
                super::HistoryKind::Attachment if !evidence["bytesBase64"].is_string() => {
                    *missing.entry("Attachment").or_default() += 1
                }
                super::HistoryKind::Session if evidence["transcriptAvailability"].is_string() => {
                    *missing.entry("HarnessTranscript").or_default() += 1
                }
                _ => {}
            }
        }
        let mapping = read
            .work_imports()?
            .import_mapping(namespace, record.kind(), record.id())?;
        let local = destination(namespace, record.kind(), record.id());
        if let SourceRecord::Repository { remote, .. } = record {
            let project = ProjectId(destination(namespace, "Project", record.project()));
            repository_bindings.push(repository_target(
                read.projects(),
                record.id(),
                &project,
                remote.as_deref(),
            )?);
        }
        let current = match record {
            SourceRecord::Project { .. } => read
                .projects()
                .project(&ProjectId(local.clone()))?
                .map(|row| (row.revision, row.lifecycle)),
            SourceRecord::Feature { .. } | SourceRecord::Item { .. } => read
                .work()
                .work(&local)?
                .map(|row| (row.revision, row.queue())),
            _ => None,
        };
        let digest = record.digest().map_err(selection_failure)?;
        let skip = delta.outside_window(record, mapping.as_ref());
        if skip {
            counts.skipped += 1;
        } else if mapping.as_ref().is_some_and(|m| m.source_digest == digest) {
            counts.unchanged += 1;
        } else if current.as_ref().map(|(rev, _)| *rev)
            != mapping.as_ref().and_then(|m| m.destination_revision)
        {
            counts.failures += 1;
        } else if mapping.is_some() {
            counts.changed += 1;
        } else {
            counts.added += 1;
        }
        hash.update(
            serde_json::to_vec(&(
                record.kind(),
                record.id(),
                local,
                digest,
                skip,
                current,
                mapping.map(|m| {
                    (
                        m.source_digest,
                        m.destination_id,
                        m.destination_revision,
                        m.observed_through_ms,
                    )
                }),
            ))
            .map_err(|_| invalid("import witnesses"))?,
        );
        hash.update(b"\n");
    }
    let witness = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let binding_digest = digest(&json!({
        "sourceSnapshot":snapshot.digest, "scopeDigest":scope_digest, "mode":mode, "updatedSinceMs":since,
        "rootIdentity":source.root_identity, "destinationWitness":witness,
        "repositoryBindings":repository_bindings,
    }))?;
    let bytes = serde_json::to_vec(&json!({
        "format": "DevWorksWorkImportPreview", "version":1,
        "source":{"namespace":namespace,"revision":snapshot.manifest.source_revision,
            "coverageFromMs":snapshot.manifest.coverage_from_ms,
            "capturedThroughMs":snapshot.manifest.captured_at_ms},
        "sourceSnapshot":snapshot.digest, "scopeDigest":scope_digest,
        "bindingDigest":binding_digest, "mode":mode, "updatedSinceMs":since, "counts":counts,
        "selectedKinds":row_counts, "missingSelectedEvidence":missing, "selection":config,
        "repositories":repository_bindings,
    }))
    .map_err(|_| invalid("import preview report"))?;
    Ok(Preview {
        scope_digest,
        binding_digest,
        since,
        counts,
        repository_bindings,
        rows: records.len() as i64,
        bytes,
    })
}

#[derive(Default)]
pub(super) struct DeltaSelection {
    since: Option<i64>,
    retries: std::collections::HashMap<String, std::collections::HashSet<Id>>,
    required_work: std::collections::HashSet<Id>,
}

impl DeltaSelection {
    pub(super) fn new(
        read: &dyn ReadSnapshot,
        actor: &crate::protocol::ActorId,
        namespace: &Id,
        scope: &str,
        approval: Option<i64>,
        since: Option<i64>,
        records: &[&SourceRecord],
    ) -> Result<Self, StorageError> {
        let mut delta = Self {
            since,
            ..Default::default()
        };
        if since.is_none() {
            return Ok(delta);
        }
        if let Some(revision) = approval {
            for (kind, id) in read
                .work_imports()?
                .import_retry_keys(actor, namespace, scope, revision)?
            {
                delta.retries.entry(kind).or_default().insert(id);
            }
        }
        for record in records {
            if !delta.in_window(record) {
                continue;
            }
            match record {
                SourceRecord::History { subject_id, .. } => {
                    delta.required_work.insert(subject_id.clone());
                }
                SourceRecord::Dependency {
                    blocked, blocker, ..
                } => {
                    delta.required_work.insert(blocked.clone());
                    delta.required_work.insert(blocker.clone());
                }
                _ => {}
            }
        }
        Ok(delta)
    }

    pub(super) fn retries(&self, record: &SourceRecord) -> bool {
        self.retries
            .get(record.kind())
            .is_some_and(|ids| ids.contains(record.id()))
    }

    fn in_window(&self, record: &SourceRecord) -> bool {
        self.since.is_none_or(|since| {
            let updated = match record {
                SourceRecord::Dependency { created_at_ms, .. } => Some(*created_at_ms),
                _ => record.stamp().map(|stamp| stamp.updated_at_ms),
            };
            updated.is_none_or(|time| time >= since) || self.retries(record)
        })
    }

    pub(super) fn outside_window(
        &self,
        record: &SourceRecord,
        mapping: Option<&WorkImportMapping>,
    ) -> bool {
        if self.in_window(record) {
            return false;
        }
        let necessary = matches!(
            record,
            SourceRecord::Project { .. } | SourceRecord::Feature { .. }
        ) || matches!(record, SourceRecord::Item { .. })
            && self.required_work.contains(record.id());
        !(necessary && mapping.is_none())
    }
}

pub(super) fn scope_digest(namespace: &Id, config: &ImportConfig) -> Result<String, StorageError> {
    let mut projects: Vec<_> = config.project_ids.0.iter().map(|p| p.0.as_str()).collect();
    let mut work: Vec<_> = config.work_ids.0.iter().map(Id::as_str).collect();
    projects.sort_unstable();
    projects.dedup();
    work.sort_unstable();
    work.dedup();
    digest(&json!({
        "namespace":namespace,"source":config.source,"projectIds":projects,"workIds":work,
        "allProjects":config.all_projects.unwrap_or(false),"includeHistory":config.include_history,
        "includeGitLinks":config.include_git_links,"scheduled":config.allow_scheduled_delta.unwrap_or(false)
    }))
}

pub(super) fn digest(value: &impl Serialize) -> Result<String, StorageError> {
    let value: Value = serde_json::to_value(value).map_err(|_| invalid("import binding"))?;
    canonical_digest(&value).map_err(|_| invalid("import binding"))
}

pub(super) fn check_revision(id: &Id, expected: i64, actual: i64) -> Result<(), StorageError> {
    if expected != actual {
        return Err(StorageError::conflict(
            RevisionDimension::Store,
            "import_settings",
            id.clone(),
            rev(expected),
            rev(actual),
        ));
    }
    Ok(())
}

pub(super) fn artifact_ref(version: StoredVersion) -> ArtifactRef {
    ArtifactRef {
        artifact_id: version.artifact_id,
        version_id: version.id,
        sha256: Digest(version.blob_digest),
        byte_length: counter(version.byte_count),
        media_type: version.media_type,
    }
}

pub(super) fn human(ctx: &InvocationContext) -> Result<(), Failure> {
    if ctx.principal_kind() != PrincipalKind::Human
        || ctx.project_id().is_some()
        || ctx.root_id().is_some()
        || ctx.session_id().is_some()
        || ctx.job_run_id().is_some()
    {
        return Err(refuse(
            DomainErrorCode::PermissionDenied,
            "work_import_owner",
        ));
    }
    Ok(())
}

pub(super) fn refuse(code: DomainErrorCode, subject: &'static str) -> Failure {
    Refusal::new(code, subject, None).into()
}

fn source_failure(error: StorageError) -> Failure {
    match error {
        StorageError::ScopeDenied => refuse(DomainErrorCode::PermissionDenied, "import_source"),
        StorageError::InvalidRecord(_) => refuse(DomainErrorCode::InvalidArgument, "import_source"),
        other => other.into(),
    }
}

fn selection_failure(error: SnapshotError) -> Failure {
    match error {
        SnapshotError::WindowNotCovered => {
            Refusal::new(DomainErrorCode::SourceChanged, "import_coverage", None)
                .retry(RetryAdvice::RefreshAndNewRequest)
                .into()
        }
        _ => refuse(DomainErrorCode::InvalidArgument, "import_selection"),
    }
}

pub(super) fn invalid(reason: &str) -> StorageError {
    StorageError::InvalidRecord(reason.into())
}

pub(super) fn stopped(env: &Preparation<'_>) -> bool {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_millis() as i64);
    env.stopped(millis)
}
