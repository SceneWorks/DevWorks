//! `evals.suites`, `.suite`, `.suite_history`, `.draft_suite`, `.propose_suite`,
//! `.activate_suite` and `.accept_exception`: eval suites through the instruction owner.
//!
//! Proposal and activation are the skill paths ([`InstructionService::propose`] and
//! `activate`) with the item kind `Suite`. Drafting writes a complete suite revision; accepting
//! an exception writes a new approved revision whose `exceptions.json` adds one exception with
//! its approver. Both validate the documentation and definition first, so every recorded suite
//! revision is complete.
use super::commands::{
    FolderRequest, Names, PreparedActivation, Target, harness_named, supersede_disk,
};
use super::scan::{self, ScannedFile, valid_name};
use super::suite::{self, Definition, Parsed};
use super::sync::{Capture, RecordedRevision, StagedCapture, record_capture};
use super::{
    InstructionService, config_scope, draft_confinement, event, event_in, invalid, item_visible,
    provenance, refusal, require_author, require_human, revision_visible, stale, suite_record,
    visible_current, visible_item, visible_latest, visible_revision,
};
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::gateway::AdmittedCommand;
use crate::operations::evals as ops;
use crate::ports::storage::{
    ConfigScope, InstructionFilter, InstructionKind, InstructionOrigin, InstructionRevisionState,
    ReadSnapshot, StorageError, StoredInstructionItem, StoredInstructionRevision, WriteTransaction,
};
use crate::ports::{Preparation, Prepared};
use crate::protocol::data::DataScope;
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{EvalAcceptedException, EvalSuitePage, EvalSuiteRecord};
use crate::protocol::results::{DomainCommandReply, DomainDecision, OperationResult};
use crate::protocol::support::Check;
use crate::protocol::{DomainErrorCode, Id};
use crate::services::cursor::{self, CursorBinding};
use crate::services::kernel::{Failure, counter, not_found};
use serde_json::{Value, json};
use std::sync::Arc;

const KIND: InstructionKind = InstructionKind::Suite;

/// A suite draft's validated files, staged as host copies the instruction owner keeps.
pub(crate) struct PreparedSuiteDraft {
    staged: Arc<StagedCapture>,
    parsed: Parsed,
    upload: ArtifactRef,
    /// The base revision the draft was prepared against and its exceptions file entry.
    base: Option<i64>,
    carried: Option<Value>,
}
/// An accepted exception's new exceptions file and the revision it extends.
pub(crate) struct PreparedException {
    staged: Arc<StagedCapture>,
    parsed: Parsed,
    exception: EvalAcceptedException,
    base: i64,
    carried: Vec<Value>,
}

impl InstructionService {
    pub(crate) fn prepare_suite_command(
        &self,
        ctx: &InvocationContext,
        env: &Preparation<'_>,
        input: &ops::Input,
    ) -> Result<Option<Prepared>, Failure> {
        match input {
            ops::Input::DraftSuite(i) => self.prepare_suite_draft(ctx, env, i),
            ops::Input::ProposeSuite(i) => {
                self.prepare_proposal(ctx, env, &i.suite_id, i.expected_revision.get())
            }
            ops::Input::ActivateSuite(i) => {
                self.prepare_activation(ctx, env, &i.suite_id, i.target_revision.get())
            }
            ops::Input::AcceptException(i) => self.prepare_exception(ctx, env, i),
            _ => Ok(None),
        }
    }

    /// The exact bytes of an upload the invocation may read, at most `max` of them.
    fn read_upload(
        &self,
        ctx: &InvocationContext,
        env: &Preparation<'_>,
        content: &ArtifactRef,
        max: u64,
        kind: &'static str,
    ) -> Result<Vec<u8>, Failure> {
        if content.byte_length.get() as u64 > max {
            return Err(invalid(kind));
        }
        let artifacts = self.artifacts.clone();
        let lease =
            env.read(|read| Ok(artifacts.resolve_visible_upload(read, ctx, content, kind)))??;
        let mut stopped = || env.stopped(self.clock.now_ms().get());
        let bytes = lease
            .read_range_controlled(0, max as usize + 1, &mut stopped)
            .map_err(|_| refusal(DomainErrorCode::NotFound, kind, Some(&content.artifact_id)))?;
        if bytes.len() as u64 > max || sha256_hex(&bytes) != content.sha256.0 {
            return Err(invalid(kind));
        }
        Ok(bytes)
    }

    fn prepare_suite_draft(
        &self,
        ctx: &InvocationContext,
        env: &Preparation<'_>,
        input: &ops::DraftSuiteInput,
    ) -> Result<Option<Prepared>, Failure> {
        let doc = self.read_upload(
            ctx,
            env,
            &input.documentation,
            scan::MAX_INSTRUCTION_BYTES,
            "eval_suite_documentation",
        )?;
        let text =
            String::from_utf8(doc.clone()).map_err(|_| invalid("eval_suite_documentation"))?;
        // The suite's own files are written by DevWorks; a suite never carries a skill.
        let mut folded = std::collections::BTreeSet::new();
        for reserved in [
            suite::SUITE_FILE,
            suite::DEFINITION_FILE,
            suite::EXCEPTIONS_FILE,
        ] {
            folded.insert(reserved.to_lowercase());
        }
        if input.resources.0.len() + 2 > scan::MAX_RESOURCES {
            return Err(invalid("eval_suite_resources"));
        }
        let mut resources = Vec::new();
        let mut total = doc.len() as u64;
        for r in &input.resources.0 {
            let path = r.path.0.as_str();
            if suite::is_skill_manifest(path) {
                return Err(refusal(
                    DomainErrorCode::InvalidArgument,
                    "eval_suite_carries_subject",
                    None,
                ));
            }
            if path.is_empty()
                || path
                    .split('/')
                    .any(|s| s.is_empty() || s.starts_with('.') || !scan::materializable(s))
                || !folded.insert(path.to_lowercase())
            {
                return Err(invalid("eval_suite_resource"));
            }
            let bytes = self.read_upload(
                ctx,
                env,
                &r.artifact,
                scan::MAX_RESOURCE_BYTES,
                "eval_suite_resource",
            )?;
            total += bytes.len() as u64;
            if total > scan::MAX_SKILL_TOTAL_BYTES {
                return Err(invalid("eval_suite_size"));
            }
            resources.push(ScannedFile::new(path.to_owned(), bytes));
        }
        // The base the draft replaces, and the accepted exceptions it keeps.
        let base = match &input.suite_id {
            Some(id) => env.read(|read| {
                Ok((|| -> Result<_, Failure> {
                    let item = visible_item(read.instructions(), ctx, id, KIND)?;
                    let current = visible_current(read.instructions(), ctx, &item)?;
                    Ok(Some(current))
                })())
            })??,
            None => None,
        };
        let exceptions = match &base {
            Some(b) => suite::parsed_of(b)?.exceptions,
            None => Vec::new(),
        };
        let carried = base
            .as_ref()
            .and_then(|b| suite::resource(b, suite::EXCEPTIONS_FILE).cloned());
        let definition = Definition {
            measures: input.measures.0.clone(),
            checks: input.checks.0.clone(),
        };
        let checked = suite::check_definition(&definition)
            .and_then(|()| suite::check_exceptions(&definition.checks, &exceptions))
            .and_then(|()| suite::validate_documentation(&text, &definition.checks));
        if checked.is_err() {
            return Err(invalid("eval_suite_definition"));
        }
        resources.push(ScannedFile::new(
            suite::DEFINITION_FILE.into(),
            suite::definition_bytes(&definition),
        ));
        resources.sort_by(|a, b| a.path.cmp(&b.path));
        let mut digests: Vec<(String, String, bool)> = resources
            .iter()
            .map(|r| (r.path.clone(), r.sha256.clone(), false))
            .collect();
        if let Some(c) = &carried {
            digests.push((
                suite::EXCEPTIONS_FILE.into(),
                c["sha256"].as_str().unwrap_or_default().to_owned(),
                false,
            ));
        }
        let capture = Capture {
            instruction: ScannedFile::new(suite::SUITE_FILE.into(), doc.clone()),
            resources,
            digest: suite::revision_digest(&doc, &digests),
        };
        let staged = self.stage_capture(
            &capture,
            json!({"kind": "EvalSuiteDraft", "upload": input.documentation}),
        )?;
        Ok(Some(Prepared::new(PreparedSuiteDraft {
            staged: Arc::new(staged),
            parsed: Parsed {
                description: scan::description(&text).map(|d| d.chars().take(4096).collect()),
                measures: definition.measures,
                checks: definition.checks,
                exceptions,
            },
            upload: input.documentation.clone(),
            base: base.map(|b| b.revision),
            carried,
        })))
    }

    fn prepare_exception(
        &self,
        ctx: &InvocationContext,
        env: &Preparation<'_>,
        input: &ops::AcceptExceptionInput,
    ) -> Result<Option<Prepared>, Failure> {
        // Only a person accepts an exception; the gateway refuses every other caller first.
        if ctx.principal_kind() != PrincipalKind::Human {
            return Err(refusal(
                DomainErrorCode::PermissionDenied,
                "human_decision",
                None,
            ));
        }
        let reason = input.reason.0.trim();
        if reason.is_empty()
            || crate::authority::redact::Redactor::new().redact_text(reason) != reason
        {
            return Err(invalid("eval_exception_reason"));
        }
        let active = env.read(|read| {
            Ok((|| -> Result<_, Failure> {
                let item = visible_item(read.instructions(), ctx, &input.suite_id, KIND)?;
                let Some(n) = item.active_revision else {
                    return Err(refusal(
                        DomainErrorCode::Conflict,
                        "eval_suite_inactive",
                        Some(&item.id),
                    ));
                };
                Ok(read
                    .instructions()
                    .instruction_revision(&item.id, n)?
                    .ok_or_else(|| not_found("eval_suite", &item.id))?)
            })())
        })??;
        let mut parsed = suite::parsed_of(&active)?;
        if !parsed.checks.iter().any(|c| c.id == input.check_id) {
            return Err(not_found("eval_check", &input.check_id).into());
        }
        let exception = EvalAcceptedException {
            check_id: input.check_id.clone(),
            matched_instance: input.matched_instance.clone(),
            reason: crate::services::kernel::content(reason),
            approver: ctx.principal().clone(),
            accepted_at_ms: counter(self.clock.now_ms().get()),
        };
        exception.check().map_err(|_| invalid("eval_exception"))?;
        parsed.exceptions.push(exception.clone());
        if let Err(reason) = suite::check_exceptions(&parsed.checks, &parsed.exceptions) {
            return Err(refusal(
                if reason.contains("two exceptions") {
                    DomainErrorCode::Conflict
                } else {
                    DomainErrorCode::InvalidArgument
                },
                "eval_exception",
                Some(&input.check_id),
            ));
        }
        let doc = self.artifact_bytes(&active.content)?;
        let carried: Vec<Value> = active
            .resources
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| r["path"] != suite::EXCEPTIONS_FILE)
            .cloned()
            .collect();
        let file = ScannedFile::new(
            suite::EXCEPTIONS_FILE.into(),
            suite::exceptions_bytes(&parsed.exceptions),
        );
        let mut digests: Vec<(String, String, bool)> = carried
            .iter()
            .map(|r| {
                (
                    r["path"].as_str().unwrap_or_default().to_owned(),
                    r["sha256"].as_str().unwrap_or_default().to_owned(),
                    r["executable"].as_bool().unwrap_or(false),
                )
            })
            .collect();
        digests.push((file.path.clone(), file.sha256.clone(), false));
        let capture = Capture {
            instruction: ScannedFile::new(suite::SUITE_FILE.into(), doc.clone()),
            resources: vec![file],
            digest: suite::revision_digest(&doc, &digests),
        };
        let staged = self.stage_capture(
            &capture,
            json!({"kind": "EvalExceptionAccepted", "suiteId": input.suite_id.as_str()}),
        )?;
        Ok(Some(Prepared::new(PreparedException {
            staged: Arc::new(staged),
            parsed,
            exception,
            base: active.revision,
            carried,
        })))
    }

    /// A confined suite draft's files staged again for release to the workspace when a person
    /// approves it: its documentation and every resource.
    pub(crate) fn prepare_suite_release(
        &self,
        id: &Id,
        revision: &StoredInstructionRevision,
    ) -> Result<PreparedActivation, Failure> {
        let doc = self.artifact_bytes(&revision.content)?;
        let mut resources = Vec::new();
        for r in revision.resources.as_array().into_iter().flatten() {
            let mut file = ScannedFile::new(
                r["path"].as_str().unwrap_or_default().to_owned(),
                self.artifact_bytes(&r["artifact"])?,
            );
            file.executable = r["executable"].as_bool().unwrap_or(false);
            resources.push(file);
        }
        let capture = Capture {
            instruction: ScannedFile::new(suite::SUITE_FILE.into(), doc),
            resources,
            digest: revision.content_digest.clone(),
        };
        let staged = self.stage_capture(
            &capture,
            json!({"kind": "InstructionRelease", "instructionId": id.as_str(), "revision": revision.revision}),
        )?;
        Ok(PreparedActivation::release(Arc::new(staged)))
    }

    /// Publishes a confined suite draft's staged files at the workspace and releases the
    /// revision to it.
    pub(crate) fn release_suite(
        &self,
        tx: &mut dyn WriteTransaction,
        staged: &StagedCapture,
        item: &StoredInstructionItem,
        target: &StoredInstructionRevision,
    ) -> Result<(), Failure> {
        let title = format!("Eval suite {}", item.name);
        let content =
            super::sync::publish_instruction_text(tx, &self.artifacts, item, staged, &title)?;
        let resources = super::sync::publish_resources(tx, &self.artifacts, item, staged, &title)?;
        tx.instructions().release_instruction_revision(
            &item.id,
            target.revision,
            &content,
            Some(&resources),
        )?;
        Ok(())
    }

    pub(crate) fn decide_suite(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
        input: &ops::Input,
    ) -> Result<DomainDecision, Failure> {
        match input {
            ops::Input::DraftSuite(i) => self.draft_suite(ctx, command, tx, i),
            ops::Input::AcceptException(i) => self.accept_exception(ctx, command, tx, i),
            _ => Err(invalid("instruction_operation")),
        }
    }

    fn draft_suite(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
        input: &ops::DraftSuiteInput,
    ) -> Result<DomainDecision, Failure> {
        require_author(tx, ctx, &*self.roles)?;
        let rationale = input
            .rationale
            .as_ref()
            .map(|r| r.0.trim())
            .filter(|r| !r.is_empty());
        if let Some(why) = rationale
            && crate::authority::redact::Redactor::new().redact_text(why) != why
        {
            return Err(invalid("instruction_rationale_unreviewable"));
        }
        let prepared = command
            .prepared::<PreparedSuiteDraft>()
            .ok_or_else(|| invalid("eval_suite_documentation"))?;
        let scope = config_scope(ctx, &DataScope::Workspace)?;
        let human = ctx.principal_kind() == PrincipalKind::Human;
        let confined = draft_confinement(tx.authority(), ctx, &scope)?;
        let name = input.name.0.trim().to_owned();
        if !valid_name(&name) {
            return Err(invalid(Names::of(KIND).name));
        }
        let now = tx.now_ms();
        let Target {
            mut item,
            base,
            overrides,
            created,
        } = self.folder_target(
            tx,
            ctx,
            KIND,
            &scope,
            &name,
            FolderRequest {
                id: input.suite_id.as_ref(),
                expected: input.expected_revision.as_ref(),
                confined: confined.as_ref(),
            },
            now,
        )?;
        // The base and the exceptions it holds are the ones the draft was prepared against.
        let carried_digest = prepared
            .carried
            .as_ref()
            .and_then(|c| c["sha256"].as_str().map(str::to_owned));
        if base.as_ref().map(|b| b.revision) != prepared.base
            || base.as_ref().and_then(suite::exceptions_digest) != carried_digest
        {
            return Err(refusal(
                DomainErrorCode::SourceChanged,
                "eval_suite",
                Some(&item.id),
            ));
        }
        let digest = prepared.staged.digest.clone();
        if human
            && !created
            && let Some(active) = item.active_revision
        {
            let current = tx
                .instructions()
                .instruction_revision(&item.id, active)?
                .ok_or_else(|| not_found("eval_suite", &item.id))?;
            if current.content_digest == digest {
                self.check_materializable(&item, &current.resources)?;
                return Ok(DomainDecision::NoChange {
                    result: OperationResult::EvalsDraftSuite {
                        value: suite_record(tx.decisions(), &item, &current, item.latest_revision)?,
                    },
                });
            }
        }
        if human {
            self.check_materializable(
                &item,
                &paths_of(&prepared.staged, prepared.carried.as_ref()),
            )?;
        }
        let revision = record_capture(
            tx,
            &self.artifacts,
            &item,
            &prepared.staged,
            RecordedRevision {
                state: if human {
                    InstructionRevisionState::Approved
                } else {
                    InstructionRevisionState::Draft
                },
                author: ctx.principal().clone(),
                origin: json!({
                    "kind": if human { "HumanSaved" } else { "AgentDraft" },
                    "provenance": provenance(ctx),
                    "rationale": rationale,
                    "baseRevision": base.as_ref().map(|b| b.revision),
                    "overrides": overrides.as_ref().map(|(id, r)| json!({"instructionId": id, "revision": r})),
                    "upload": prepared.upload,
                    "validation": suite::validation(),
                    "suite": serde_json::to_value(&prepared.parsed).map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
                }),
                applicability: Vec::new(),
                title: format!("Eval suite {}", item.name),
                carried_resources: None,
                extra_resources: prepared.carried.iter().cloned().collect(),
                confined: confined.clone(),
            },
        )?;
        let previous = item.active_revision;
        item.latest_revision = revision.revision;
        if human {
            item.active_revision = Some(revision.revision);
            item.confined_project = None;
            supersede_disk(&mut item);
        }
        item.version += 1;
        item.updated_at_ms = now;
        tx.instructions().update_instruction_item(&item)?;
        event_in(
            tx,
            ctx.principal(),
            &item,
            confined.as_ref(),
            if human {
                "InstructionSaved"
            } else {
                "InstructionDrafted"
            },
            json!({
                "revision": revision.revision, "contentDigest": revision.content_digest,
                "previousActiveRevision": previous, "provenance": provenance(ctx),
                "overrides": overrides.as_ref().map(|(id, r)| json!({"instructionId": id, "revision": r})),
            }),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::EvalsDraftSuite {
                value: {
                    let latest = visible_latest(tx.instructions(), ctx, &item)?;
                    suite_record(tx.decisions(), &item, &revision, latest)?
                },
            },
        })
    }

    fn accept_exception(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
        input: &ops::AcceptExceptionInput,
    ) -> Result<DomainDecision, Failure> {
        require_human(tx, ctx)?;
        let prepared = command
            .prepared::<PreparedException>()
            .ok_or_else(|| invalid("eval_exception"))?;
        let source = visible_item(tx.instructions(), ctx, &input.suite_id, KIND)?;
        let current = visible_latest(tx.instructions(), ctx, &source)?;
        if current != input.expected_revision.get() {
            return Err(stale(&source, input.expected_revision.get(), current));
        }
        if source.active_revision != Some(prepared.base) {
            return Err(refusal(
                DomainErrorCode::SourceChanged,
                "eval_suite",
                Some(&source.id),
            ));
        }
        let now = tx.now_ms();
        // A shared suite's files are never written: the exception goes to a same-name harness
        // suite holding the shared revision's files plus the exception.
        let (mut item, overrides) = if source.origin == InstructionOrigin::Shared {
            if let Some(existing) = harness_named(tx, KIND, &ConfigScope::Workspace, &source.name)?
            {
                return Err(refusal(
                    DomainErrorCode::Conflict,
                    Names::of(KIND).overrides,
                    Some(&existing.id),
                ));
            }
            let item = self.folder_target(
                tx,
                ctx,
                KIND,
                &ConfigScope::Workspace,
                &source.name,
                FolderRequest {
                    id: Some(&source.id),
                    expected: Some(&input.expected_revision),
                    confined: None,
                },
                now,
            )?;
            (item.item, Some((source.id.clone(), prepared.base)))
        } else {
            (source.clone(), None)
        };
        let paths = paths_of(&prepared.staged, None);
        let mut all = paths.as_array().cloned().unwrap_or_default();
        all.extend(prepared.carried.iter().map(|r| json!({"path": r["path"]})));
        self.check_materializable(&item, &Value::Array(all))?;
        let revision = record_capture(
            tx,
            &self.artifacts,
            &item,
            &prepared.staged,
            RecordedRevision {
                state: InstructionRevisionState::Approved,
                author: ctx.principal().clone(),
                origin: json!({
                    "kind": "ExceptionAccepted",
                    "provenance": provenance(ctx),
                    "baseRevision": prepared.base,
                    "overrides": overrides.as_ref().map(|(id, r)| json!({"instructionId": id, "revision": r})),
                    "exception": prepared.exception,
                    "validation": suite::validation(),
                    "suite": serde_json::to_value(&prepared.parsed).map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
                }),
                applicability: Vec::new(),
                title: format!("Eval suite {}", item.name),
                carried_resources: None,
                extra_resources: prepared.carried.clone(),
                confined: None,
            },
        )?;
        let previous = item.active_revision;
        item.latest_revision = revision.revision;
        item.active_revision = Some(revision.revision);
        item.confined_project = None;
        supersede_disk(&mut item);
        item.version += 1;
        item.updated_at_ms = now;
        tx.instructions().update_instruction_item(&item)?;
        event(
            tx,
            ctx.principal(),
            &item,
            "EvalExceptionAccepted",
            json!({
                "revision": revision.revision, "previousActiveRevision": previous,
                "contentDigest": revision.content_digest,
                "exception": prepared.exception,
                "overrides": overrides.as_ref().map(|(id, r)| json!({"instructionId": id, "revision": r})),
                "provenance": provenance(ctx),
            }),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::EvalsAcceptException {
                value: {
                    let latest = visible_latest(tx.instructions(), ctx, &item)?;
                    suite_record(tx.decisions(), &item, &revision, latest)?
                },
            },
        })
    }

    /// After the outcome: release staged copies that were not recorded, and write an approved
    /// active harness suite into its folder (a failure is repaired by the next sync).
    pub(crate) fn settle_suite(
        &self,
        prepared: Option<&Prepared>,
        reply: Option<&DomainCommandReply>,
    ) {
        let applied = suite_reply(reply, "Applied");
        let staged = prepared.and_then(|p| {
            p.get::<PreparedSuiteDraft>()
                .map(|d| d.staged.clone())
                .or_else(|| p.get::<PreparedException>().map(|e| e.staged.clone()))
        });
        if let Some(staged) = staged {
            let committed = applied
                .as_ref()
                .is_some_and(|r| r.content_digest.as_str() == staged.digest);
            self.settle_capture(&staged, committed);
        }
        if let Some(record) = applied.or_else(|| suite_reply(reply, "NoChange"))
            && record.approved
            && record
                .location
                .as_ref()
                .is_some_and(|p| p.root_id.as_str() == suite::HARNESS_ROOT_ID)
        {
            let item =
                crate::ports::storage::read_snapshot(&*self.store, &Default::default(), |read| {
                    read.instructions().instruction_item(&record.id)
                });
            if let Ok(Some(item)) = item
                && item.active_revision == Some(record.revision.get())
            {
                let _ = self.materialize(&item);
            }
        }
    }

    pub(crate) fn suite_query(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::Input,
    ) -> Result<(OperationResult, i64), Failure> {
        let read = snapshot.instructions();
        match input {
            ops::Input::Suites(i) => {
                let mut items = Vec::new();
                let mut after: Option<Id> = None;
                loop {
                    let page = read.instruction_items(
                        &InstructionFilter {
                            kind: Some(KIND),
                            scopes: vec![ConfigScope::Workspace],
                            name: None,
                        },
                        after.as_ref(),
                        crate::ports::storage::MAX_PAGE,
                    )?;
                    let done = page.len() < crate::ports::storage::MAX_PAGE as usize;
                    after = page.last().map(|i| i.id.clone());
                    items.extend(page.into_iter().filter(|i| item_visible(ctx, i)));
                    if done {
                        break;
                    }
                }
                let mut records = Vec::new();
                let mut fence = 0i64;
                for item in &items {
                    fence = fence.max(item.version);
                    let revision = visible_current(read, ctx, item)?;
                    let latest = visible_latest(read, ctx, item)?;
                    records.push(suite_record(snapshot.decisions(), item, &revision, latest)?);
                }
                let listed = sha256_hex(
                    serde_json::to_string(
                        &records
                            .iter()
                            .map(|r| (r.id.as_str(), r.revision.get()))
                            .collect::<Vec<_>>(),
                    )
                    .expect("binding")
                    .as_bytes(),
                );
                let page = cursor::slice(
                    &CursorBinding::new(ctx, "evals.suites", &[&listed], 1),
                    records,
                    &i.page,
                )?;
                Ok((
                    OperationResult::EvalsSuites {
                        value: EvalSuitePage {
                            items: page.items,
                            next_cursor: page.next_cursor,
                        },
                    },
                    fence.max(1),
                ))
            }
            ops::Input::Suite(i) => {
                let item = visible_item(read, ctx, &i.suite_id, KIND)?;
                let revision = match &i.revision {
                    Some(n) => visible_revision(read, ctx, &item, n.get())?,
                    None => visible_current(read, ctx, &item)?,
                };
                let latest = visible_latest(read, ctx, &item)?;
                Ok((
                    OperationResult::EvalsSuite {
                        value: suite_record(snapshot.decisions(), &item, &revision, latest)?,
                    },
                    item.version,
                ))
            }
            ops::Input::SuiteHistory(i) => {
                let item = visible_item(read, ctx, &i.suite_id, KIND)?;
                let binding =
                    CursorBinding::new(ctx, "evals.suite_history", &[item.id.as_str()], 1);
                let rows = cursor::page(
                    &binding,
                    &i.page,
                    |before, n| {
                        let before = before
                            .map(|b| b.parse::<i64>().map_err(|_| StorageError::CursorExpired))
                            .transpose()?;
                        cursor::scan(
                            before,
                            n,
                            |before, batch| read.instruction_revisions(&item.id, *before, batch),
                            |r| Some(r.revision),
                            |r| Ok(revision_visible(ctx, r)),
                        )
                    },
                    |r| r.revision.to_string(),
                )?;
                let latest = visible_latest(read, ctx, &item)?;
                let page =
                    rows.try_into_page(|r| suite_record(snapshot.decisions(), &item, &r, latest))?;
                Ok((
                    OperationResult::EvalsSuiteHistory {
                        value: EvalSuitePage {
                            items: page.items,
                            next_cursor: page.next_cursor,
                        },
                    },
                    item.version,
                ))
            }
            _ => Err(invalid("instruction_operation")),
        }
    }
}

/// The paths a revision of these staged files and carried entry would hold, as resources.
fn paths_of(staged: &StagedCapture, carried: Option<&Value>) -> Value {
    Value::Array(
        staged
            .resources
            .iter()
            .map(|(f, _)| json!({"path": f.path}))
            .chain(carried.map(|c| json!({"path": c["path"]})))
            .collect(),
    )
}

/// The suite record a reply decided with `kind` (`Applied` or `NoChange`).
pub(crate) fn suite_reply(
    reply: Option<&DomainCommandReply>,
    kind: &str,
) -> Option<EvalSuiteRecord> {
    let value = serde_json::to_value(reply?).ok()?;
    let decision = value.pointer("/receipt/decision")?;
    if decision["kind"] != kind {
        return None;
    }
    let result = decision.get("result")?;
    match result["kind"].as_str()? {
        "evals.draft_suite" | "evals.activate_suite" | "evals.accept_exception" => {
            serde_json::from_value(result["value"].clone()).ok()
        }
        _ => None,
    }
}
