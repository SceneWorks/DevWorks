//! `*.draft`, `*.propose` and `*.rollback` (activation).
use super::approval::{self, binding_matches};
use super::scan::{self, valid_name};
use super::sync::{RecordedRevision, StagedCapture, new_role_item, record_capture};
use super::{
    InstructionService, config_scope, current_revision, denied, event, invalid, kind_of,
    not_found_failure, provenance, record, refusal, require_author, require_human, stale,
    target_kind, visible_item,
};
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::gateway::AdmittedCommand;
use crate::operations::{OperationInput, roles as role_ops, skills as skill_ops};
use crate::ports::storage::{
    ConfigScope, InstructionFilter, InstructionKind, InstructionOrigin, InstructionRevisionState,
    StorageError, StoredInstructionItem, StoredInstructionRevision, WriteTransaction,
};
use crate::ports::{IdKind, Preparation, Prepared};
use crate::protocol::data::DataScope;
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{InstructionRecord, ProposalRecord};
use crate::protocol::results::{DomainCommandReply, DomainDecision, OperationResult};
use crate::protocol::support::Label;
use crate::protocol::{DomainErrorCode, Id, Revision};
use crate::services::sessions::{Failure, Refusal};
use crate::settings::catalog::AgentRole;
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) struct PreparedRefusal(pub Refusal);

/// A draft's validated text, staged as a host copy the instruction owns.
pub(crate) struct PreparedDraft {
    text: String,
    upload: ArtifactRef,
    staged: Arc<StagedCapture>,
}
/// The target text of a role activation (for the settings mirror).
pub(crate) struct PreparedActivation {
    text: Option<String>,
}

impl InstructionService {
    pub(crate) fn prepare_draft(
        &self,
        ctx: &InvocationContext,
        env: &Preparation<'_>,
        content: &ArtifactRef,
    ) -> Result<Option<Prepared>, Failure> {
        if content.byte_length.get() as u64 > super::MAX_TEXT_BYTES {
            return Err(invalid("instruction_content"));
        }
        let artifacts = self.artifacts.clone();
        let lease = env.read(|read| {
            Ok(artifacts.resolve_visible_upload(read, ctx, content, "instruction_content"))
        })??;
        let mut stopped = || env.stopped(self.clock.now_ms().get());
        let bytes = lease
            .read_range_controlled(0, super::MAX_TEXT_BYTES as usize + 1, &mut stopped)
            .map_err(|_| {
                refusal(
                    DomainErrorCode::NotFound,
                    "instruction_content",
                    Some(&content.artifact_id),
                )
            })?;
        if bytes.len() as u64 > super::MAX_TEXT_BYTES || sha256_hex(&bytes) != content.sha256.0 {
            return Err(invalid("instruction_content"));
        }
        let text = String::from_utf8(bytes).map_err(|_| invalid("instruction_content"))?;
        // Retained prompt-editor rule: instruction text is required.
        if text.trim().is_empty() {
            return Err(invalid("instruction_content"));
        }
        let capture = super::sync::Capture::text(&text);
        let staged = self.stage_capture(
            &capture,
            json!({"kind": "InstructionDraft", "upload": content}),
        )?;
        Ok(Some(Prepared::new(PreparedDraft {
            text,
            upload: content.clone(),
            staged: Arc::new(staged),
        })))
    }

    pub(crate) fn prepare_activation(
        &self,
        ctx: &InvocationContext,
        env: &Preparation<'_>,
        id: &Id,
        target: i64,
    ) -> Result<Option<Prepared>, Failure> {
        let revision = env.read(|read| {
            Ok((|| {
                let item = match read.instructions().instruction_item(id)? {
                    Some(item) if super::visible(ctx, &item.scope) => item,
                    _ => return Ok(None),
                };
                if item.kind != InstructionKind::Role {
                    return Ok(None);
                }
                read.instructions().instruction_revision(id, target)
            })())
        })??;
        let text = match revision {
            Some(r) => Some(
                String::from_utf8(self.artifact_bytes(&r.content)?)
                    .map_err(|_| invalid("instruction_content"))?,
            ),
            None => None,
        };
        Ok(Some(Prepared::new(PreparedActivation { text })))
    }

    /// After the outcome: release a draft copy that was not recorded, and write an activated
    /// harness skill into its directory (a failure is repaired by the next sync).
    pub(crate) fn settle_command(
        &self,
        prepared: Option<&Prepared>,
        reply: Option<&DomainCommandReply>,
        _env: &Preparation<'_>,
    ) {
        let applied = applied_record(reply);
        if let Some(draft) = prepared.and_then(|p| p.get::<PreparedDraft>()) {
            let committed = applied
                .as_ref()
                .is_some_and(|r| r.content.sha256.0 == draft.staged.instruction.digest);
            self.settle_capture(&draft.staged, committed);
        }
        if let Some(record) = applied
            && record.approved
            && record
                .source_path
                .as_ref()
                .is_some_and(|p| p.root_id.as_str() == super::HARNESS_ROOT_ID)
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

    pub(crate) fn decide_command(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<DomainDecision, Failure> {
        let kind = kind_of(command.input()).ok_or_else(|| invalid("instruction_operation"))?;
        match command.input() {
            OperationInput::Skills(skill_ops::Input::Draft(i)) => {
                let draft = Draft {
                    scope: &i.scope,
                    instruction_id: i.instruction_id.as_ref(),
                    expected: i.expected_revision.as_ref(),
                    name: &i.name,
                };
                self.draft(ctx, command, tx, kind, draft)
            }
            OperationInput::Roles(role_ops::Input::Draft(i)) => {
                let draft = Draft {
                    scope: &i.scope,
                    instruction_id: i.instruction_id.as_ref(),
                    expected: i.expected_revision.as_ref(),
                    name: &i.name,
                };
                self.draft(ctx, command, tx, kind, draft)
            }
            OperationInput::Skills(skill_ops::Input::Propose(i)) => self.propose(
                ctx,
                tx,
                kind,
                &i.instruction_id,
                i.expected_revision.get(),
                &i.content_digest.0,
            ),
            OperationInput::Roles(role_ops::Input::Propose(i)) => self.propose(
                ctx,
                tx,
                kind,
                &i.instruction_id,
                i.expected_revision.get(),
                &i.content_digest.0,
            ),
            OperationInput::Skills(skill_ops::Input::Rollback(i)) => {
                let a = Activation {
                    id: &i.instruction_id,
                    expected: i.expected_revision.get(),
                    target: i.target_revision.get(),
                    proposal: i.proposal_id.as_ref(),
                };
                self.activate(ctx, command, tx, kind, a)
            }
            OperationInput::Roles(role_ops::Input::Rollback(i)) => {
                let a = Activation {
                    id: &i.instruction_id,
                    expected: i.expected_revision.get(),
                    target: i.target_revision.get(),
                    proposal: i.proposal_id.as_ref(),
                };
                self.activate(ctx, command, tx, kind, a)
            }
            _ => Err(invalid("instruction_operation")),
        }
    }

    fn draft(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
        kind: InstructionKind,
        input: Draft<'_>,
    ) -> Result<DomainDecision, Failure> {
        require_author(tx, ctx, &*self.roles)?;
        let prepared = command
            .prepared::<PreparedDraft>()
            .ok_or_else(|| invalid("instruction_content"))?;
        let scope = config_scope(ctx, input.scope)?;
        let human = ctx.principal_kind() == PrincipalKind::Human;
        let name = input.name.0.trim().to_owned();
        let now = tx.now_ms();
        let target = match kind {
            InstructionKind::Skill => self.skill_target(tx, ctx, &scope, &name, &input, now)?,
            InstructionKind::Role => role_target(tx, ctx, &scope, &name, &input, now)?,
        };
        // Role text must satisfy the role setting's own rules for every author, so approved
        // content can always be applied by the activation that mirrors it into settings.
        if kind == InstructionKind::Role
            && let Some(d) = AgentRole::parse(&name)
                .and_then(|r| crate::settings::catalog::descriptor(r.instructions_key()))
            && crate::settings::validate::check_value(d, &Value::String(prepared.text.clone()))
                .iter()
                .any(|issue| issue.blocking)
        {
            return Err(invalid("instruction_content"));
        }
        let Target {
            mut item,
            base,
            overrides,
            created,
        } = target;
        let resources = match (&kind, &base) {
            (InstructionKind::Skill, Some(b)) => b.resources.clone(),
            _ => Value::Array(Vec::new()),
        };
        let resource_digests: Vec<(String, String, bool)> = resources
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| {
                (
                    r["path"].as_str().unwrap_or_default().to_owned(),
                    r["sha256"].as_str().unwrap_or_default().to_owned(),
                    r["executable"].as_bool().unwrap_or(false),
                )
            })
            .collect();
        let digest =
            scan::revision_digest(&sha256_hex(prepared.text.as_bytes()), &resource_digests);
        if human
            && !created
            && let Some(active) = item.active_revision
        {
            let current = tx
                .instructions()
                .instruction_revision(&item.id, active)?
                .ok_or_else(|| not_found_failure(kind, &item.id))?;
            if current.content_digest == digest {
                return Ok(DomainDecision::NoChange {
                    result: result(kind, Op::Draft, record(&item, &current, None)?),
                });
            }
        }
        let staged = StagedCapture {
            instruction: prepared.staged.instruction.clone(),
            instruction_media: prepared.staged.instruction_media,
            resources: Vec::new(),
            digest,
        };
        let role = AgentRole::parse(&item.name);
        let revision = record_capture(
            tx,
            &self.artifacts,
            &item,
            &staged,
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
                    "baseRevision": base.as_ref().map(|b| b.revision),
                    "overrides": overrides.as_ref().map(|(id, r)| json!({"instructionId": id, "revision": r})),
                    "upload": prepared.upload,
                    "validation": {"performed": "format", "outcome": "UTF-8 text within bounds"},
                }),
                applicability: if kind == InstructionKind::Role {
                    role.map(|r| vec![r.as_str().to_owned()])
                        .unwrap_or_default()
                } else {
                    Vec::new()
                },
                title: format!("{} {}", target_kind(kind), item.name),
                // Resources carry over from the base revision; they are immutable artifacts.
                carried_resources: Some(resources),
            },
        )?;
        let previous = item.active_revision;
        item.latest_revision = revision.revision;
        if human {
            item.active_revision = Some(revision.revision);
        }
        item.version += 1;
        item.updated_at_ms = now;
        tx.instructions().update_instruction_item(&item)?;
        if human && let (InstructionKind::Role, Some(role)) = (kind, role) {
            crate::settings::service::mirror_role_instructions(
                tx,
                ctx,
                &item.scope,
                role,
                &prepared.text,
                json!({"kind": "RoleInstruction", "instructionId": item.id.as_str(), "revision": revision.revision}),
            )
            .map_err(settings_failure)?;
        }
        event(
            tx,
            ctx.principal(),
            &item,
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
            result: result(kind, Op::Draft, record(&item, &revision, None)?),
        })
    }

    fn skill_target(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        scope: &ConfigScope,
        name: &str,
        input: &Draft<'_>,
        now: i64,
    ) -> Result<Target, Failure> {
        // File-backed skill roots are workspace resources.
        if *scope != ConfigScope::Workspace {
            return Err(invalid("skill_scope"));
        }
        if !valid_name(name) {
            return Err(invalid("skill_name"));
        }
        let harness = |tx: &mut dyn WriteTransaction| harness_named(tx, name);
        match input.instruction_id {
            Some(id) => {
                let source = visible_item(tx.instructions(), ctx, id, InstructionKind::Skill)?;
                let expected = input
                    .expected
                    .ok_or_else(|| invalid("expected_revision"))?
                    .get();
                if source.latest_revision != expected {
                    return Err(stale(&source, expected));
                }
                if source.name != name {
                    return Err(invalid("skill_name"));
                }
                let base = current_revision(tx.instructions(), &source).ok();
                if source.origin == InstructionOrigin::Shared {
                    // An improvement to a shared skill becomes a same-name harness override; the
                    // shared original and its files stay untouched.
                    let overrides = Some((source.id.clone(), base.as_ref().map(|b| b.revision)));
                    // An existing override is edited through its own identity and revision, so a
                    // concurrent edit of it can never be overwritten silently.
                    if let Some(existing) = harness(tx)? {
                        return Err(refusal(
                            DomainErrorCode::Conflict,
                            "skill_override",
                            Some(&existing.id),
                        ));
                    }
                    Ok(Target {
                        item: self.new_harness_item(tx, name, now)?,
                        base,
                        overrides,
                        created: true,
                    })
                } else {
                    Ok(Target {
                        item: source,
                        base,
                        overrides: None,
                        created: false,
                    })
                }
            }
            None => {
                if input.expected.is_some() {
                    return Err(invalid("expected_revision"));
                }
                if let Some(existing) = harness(tx)? {
                    return Err(refusal(
                        DomainErrorCode::Conflict,
                        "skill",
                        Some(&existing.id),
                    ));
                }
                Ok(Target {
                    item: self.new_harness_item(tx, name, now)?,
                    base: None,
                    overrides: None,
                    created: true,
                })
            }
        }
    }

    fn new_harness_item(
        &self,
        tx: &mut dyn WriteTransaction,
        name: &str,
        now: i64,
    ) -> Result<StoredInstructionItem, Failure> {
        let root = self
            .files
            .root()
            .unwrap_or_else(|_| self.roots.harness.clone());
        let item = StoredInstructionItem {
            id: tx.new_id(IdKind::Operation),
            kind: InstructionKind::Skill,
            name: name.to_owned(),
            scope: ConfigScope::Workspace,
            origin: InstructionOrigin::Harness,
            root_path: Some(root.to_string_lossy().into_owned()),
            source_path: Some(root.join(name).to_string_lossy().into_owned()),
            latest_revision: 0,
            active_revision: None,
            enabled: true,
            materialized_digest: None,
            observed_digest: None,
            present: false,
            version: 1,
            created_at_ms: now,
            updated_at_ms: now,
        };
        tx.instructions().insert_instruction_item(&item)?;
        Ok(item)
    }

    fn propose(
        &self,
        ctx: &InvocationContext,
        tx: &mut dyn WriteTransaction,
        kind: InstructionKind,
        id: &Id,
        expected: i64,
        digest: &str,
    ) -> Result<DomainDecision, Failure> {
        if ctx.principal_kind() == PrincipalKind::Human {
            // A person saves directly (`*.draft`); proposals carry agent content to a person.
            return Err(invalid("human_saves_directly"));
        }
        require_author(tx, ctx, &*self.roles)?;
        let item = visible_item(tx.instructions(), ctx, id, kind)?;
        let revision = tx
            .instructions()
            .instruction_revision(&item.id, expected)?
            .ok_or_else(|| not_found_failure(kind, &item.id))?;
        if revision.state != InstructionRevisionState::Draft {
            return Err(refusal(
                DomainErrorCode::Conflict,
                "instruction_revision",
                Some(&item.id),
            ));
        }
        if revision.author != *ctx.principal() {
            return Err(denied("draft_author"));
        }
        if revision.content_digest != digest {
            return Err(refusal(
                DomainErrorCode::SourceChanged,
                "instruction_revision",
                Some(&item.id),
            ));
        }
        let proposal_id = tx.new_id(IdKind::Operation);
        let operation = activation_operation(kind);
        let args = json!({
            "instructionId": item.id.as_str(),
            "expectedRevision": item.latest_revision.to_string(),
            "targetRevision": revision.revision.to_string(),
            "proposalId": proposal_id.as_str(),
        });
        let request = crate::services::approvals::requests::decode(operation, 1, &args, true)
            .map_err(|_| invalid("saved_activation"))?;
        let preview = approval::preview(&item, &revision, &proposal_id, item.latest_revision);
        let scope = match &item.scope {
            ConfigScope::Workspace => json!({"kind": "Workspace"}),
            ConfigScope::Project(p) => json!({"kind": "Project", "projectId": p}),
        };
        let input: crate::operations::approvals::ProposeInput = serde_json::from_value(json!({
            "scope": scope,
            "operationKind": operation,
            "target": {"id": item.id.as_str(), "revision": revision.revision.to_string()},
            "contentDigest": request.fingerprint.as_str(),
            "summary": preview.summary,
            "artifact": revision.content,
            "payload": {"kind": "SavedOperation", "schemaRevision": "1", "input": args},
        }))
        .map_err(|e| StorageError::InvalidRecord(format!("instruction proposal: {e}")))?;
        let approval =
            crate::services::approvals::stage_saved(tx, ctx, &input, &request, &preview)?;
        tx.instructions().set_instruction_revision_state(
            &item.id,
            revision.revision,
            InstructionRevisionState::Draft,
            InstructionRevisionState::Proposed,
            Some(&approval.id),
            None,
        )?;
        let mut item = item;
        item.version += 1;
        item.updated_at_ms = tx.now_ms();
        tx.instructions().update_instruction_item(&item)?;
        event(
            tx,
            ctx.principal(),
            &item,
            "InstructionProposed",
            json!({
                "revision": revision.revision, "contentDigest": revision.content_digest,
                "approvalId": approval.id.as_str(), "proposalId": proposal_id.as_str(),
                "provenance": provenance(ctx),
            }),
        )?;
        let record = proposal_record(&approval)?;
        Ok(DomainDecision::Applied {
            result: match kind {
                InstructionKind::Skill => OperationResult::SkillsPropose { value: record },
                InstructionKind::Role => OperationResult::RolesPropose { value: record },
            },
        })
    }

    fn activate(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
        kind: InstructionKind,
        a: Activation<'_>,
    ) -> Result<DomainDecision, Failure> {
        let mut item = visible_item(tx.instructions(), ctx, a.id, kind)?;
        if item.latest_revision != a.expected {
            return Err(stale(&item, a.expected));
        }
        let target = tx
            .instructions()
            .instruction_revision(&item.id, a.target)?
            .ok_or_else(|| not_found_failure(kind, &item.id))?;
        let approval_id = match a.proposal {
            // Activation of reviewed agent content: only the exact approved proposal.
            Some(proposal) => {
                let approval_id = target.approval_id.clone().ok_or_else(|| {
                    refusal(
                        DomainErrorCode::Conflict,
                        "instruction_proposal",
                        Some(proposal),
                    )
                })?;
                let approval = tx.decisions().approval(&approval_id)?.ok_or_else(|| {
                    refusal(
                        DomainErrorCode::NotFound,
                        "instruction_proposal",
                        Some(proposal),
                    )
                })?;
                if approval.state != "Approved"
                    || !binding_matches(&approval.payload["binding"], &item, &target, proposal)
                {
                    return Err(denied("instruction_proposal"));
                }
                if target.state == InstructionRevisionState::Proposed {
                    let now = tx.now_ms();
                    tx.instructions().set_instruction_revision_state(
                        &item.id,
                        target.revision,
                        InstructionRevisionState::Proposed,
                        InstructionRevisionState::Approved,
                        Some(&approval_id),
                        Some(now),
                    )?;
                } else if target.state != InstructionRevisionState::Approved {
                    return Err(refusal(
                        DomainErrorCode::Conflict,
                        "instruction_revision",
                        Some(&item.id),
                    ));
                }
                Some(approval_id)
            }
            None => {
                // A person restores any previously approved revision directly. Unapproved agent
                // content is activated only through its reviewed proposal.
                require_human(tx, ctx)?;
                if target.state != InstructionRevisionState::Approved {
                    return Err(refusal(
                        DomainErrorCode::ApprovalRequired,
                        "instruction_revision",
                        Some(&item.id),
                    ));
                }
                // A shared skill's files are never written: restoring a revision other than the
                // one on disk becomes a harness override with that revision's files, so what is
                // in effect is what is executed.
                if item.origin == InstructionOrigin::Shared
                    && item.observed_digest.as_deref() != Some(target.content_digest.as_str())
                {
                    return self.restore_into_override(ctx, tx, &item, &target);
                }
                None
            }
        };
        let target = tx
            .instructions()
            .instruction_revision(&item.id, a.target)?
            .ok_or_else(|| not_found_failure(kind, &item.id))?;
        if item.active_revision == Some(target.revision) {
            return Ok(DomainDecision::NoChange {
                result: result(kind, Op::Rollback, record(&item, &target, None)?),
            });
        }
        let previous = item.active_revision;
        item.active_revision = Some(target.revision);
        item.version += 1;
        item.updated_at_ms = tx.now_ms();
        tx.instructions().update_instruction_item(&item)?;
        if kind == InstructionKind::Role
            && let Some(role) = AgentRole::parse(&item.name)
        {
            let text = command
                .prepared::<PreparedActivation>()
                .and_then(|p| p.text.clone())
                .ok_or_else(|| invalid("instruction_content"))?;
            if sha256_hex(text.as_bytes()) != target.content["sha256"].as_str().unwrap_or_default()
            {
                return Err(refusal(
                    DomainErrorCode::SourceChanged,
                    "instruction_revision",
                    Some(&item.id),
                ));
            }
            crate::settings::service::mirror_role_instructions(
                tx,
                ctx,
                &item.scope,
                role,
                &text,
                json!({"kind": "RoleInstruction", "instructionId": item.id.as_str(), "revision": target.revision, "approvalId": approval_id.as_ref().map(Id::as_str)}),
            )
            .map_err(settings_failure)?;
        }
        event(
            tx,
            ctx.principal(),
            &item,
            "InstructionActivated",
            json!({
                "revision": target.revision, "previousActiveRevision": previous,
                "contentDigest": target.content_digest,
                "approvalId": approval_id.as_ref().map(Id::as_str),
                "proposalId": a.proposal.map(Id::as_str),
                "provenance": provenance(ctx),
            }),
        )?;
        Ok(DomainDecision::Applied {
            result: result(kind, Op::Rollback, record(&item, &target, None)?),
        })
    }
}

impl InstructionService {
    fn restore_into_override(
        &self,
        ctx: &InvocationContext,
        tx: &mut dyn WriteTransaction,
        shared: &StoredInstructionItem,
        target: &StoredInstructionRevision,
    ) -> Result<DomainDecision, Failure> {
        let now = tx.now_ms();
        let existing = harness_named(tx, &shared.name)?;
        let mut item = match existing {
            Some(item) => item,
            None => self.new_harness_item(tx, &shared.name, now)?,
        };
        let revision = StoredInstructionRevision {
            instruction_id: item.id.clone(),
            revision: item.latest_revision + 1,
            content: target.content.clone(),
            content_digest: target.content_digest.clone(),
            state: InstructionRevisionState::Approved,
            author: ctx.principal().clone(),
            origin: json!({
                "kind": "Rollback",
                "restoredFrom": {"instructionId": shared.id.as_str(), "revision": target.revision},
                "provenance": provenance(ctx),
            }),
            resources: target.resources.clone(),
            applicability: target.applicability.clone(),
            approval_id: None,
            created_at_ms: now,
            decided_at_ms: Some(now),
        };
        tx.instructions().insert_instruction_revision(&revision)?;
        let previous = item.active_revision;
        item.latest_revision = revision.revision;
        item.active_revision = Some(revision.revision);
        item.version += 1;
        item.updated_at_ms = now;
        tx.instructions().update_instruction_item(&item)?;
        event(
            tx,
            ctx.principal(),
            &item,
            "InstructionActivated",
            json!({
                "revision": revision.revision, "previousActiveRevision": previous,
                "contentDigest": revision.content_digest,
                "restoredFrom": {"instructionId": shared.id.as_str(), "revision": target.revision},
                "provenance": provenance(ctx),
            }),
        )?;
        Ok(DomainDecision::Applied {
            result: result(
                InstructionKind::Skill,
                Op::Rollback,
                record(&item, &revision, None)?,
            ),
        })
    }
}

struct Draft<'a> {
    scope: &'a DataScope,
    instruction_id: Option<&'a Id>,
    expected: Option<&'a Revision>,
    name: &'a Label,
}
struct Activation<'a> {
    id: &'a Id,
    expected: i64,
    target: i64,
    proposal: Option<&'a Id>,
}
struct Target {
    item: StoredInstructionItem,
    base: Option<StoredInstructionRevision>,
    overrides: Option<(Id, Option<i64>)>,
    created: bool,
}

fn role_target(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    scope: &ConfigScope,
    name: &str,
    input: &Draft<'_>,
    now: i64,
) -> Result<Target, Failure> {
    let role =
        AgentRole::parse(name).ok_or_else(|| refusal(DomainErrorCode::NotFound, "role", None))?;
    let id = super::role_item_id(role, scope);
    if input.instruction_id.is_some_and(|given| *given != id) {
        return Err(invalid("role_item"));
    }
    match tx.instructions().instruction_item(&id)? {
        Some(item) => {
            if !super::visible(ctx, &item.scope) {
                return Err(not_found_failure(InstructionKind::Role, &id));
            }
            let expected = input
                .expected
                .ok_or_else(|| invalid("expected_revision"))?
                .get();
            if item.latest_revision != expected {
                return Err(stale(&item, expected));
            }
            let base = current_revision(tx.instructions(), &item).ok();
            Ok(Target {
                item,
                base,
                overrides: None,
                created: false,
            })
        }
        None => {
            let item = new_role_item(role, scope, now);
            tx.instructions().insert_instruction_item(&item)?;
            Ok(Target {
                item,
                base: None,
                overrides: None,
                created: true,
            })
        }
    }
}

/// The harness skill whose directory `name` resolves to. Names are compared case-insensitively:
/// on Windows and default macOS volumes `foo` and `Foo` are one directory, so a creation must
/// never resolve onto (and overwrite) another skill's files.
fn harness_named(
    tx: &mut dyn WriteTransaction,
    name: &str,
) -> Result<Option<StoredInstructionItem>, StorageError> {
    let folded = name.to_lowercase();
    let mut after: Option<Id> = None;
    loop {
        let page = tx.instructions().instruction_items(
            &InstructionFilter {
                kind: Some(InstructionKind::Skill),
                scopes: vec![ConfigScope::Workspace],
                name: None,
            },
            after.as_ref(),
            crate::ports::storage::MAX_PAGE,
        )?;
        let done = page.len() < crate::ports::storage::MAX_PAGE as usize;
        after = page.last().map(|i| i.id.clone());
        if let Some(found) = page
            .into_iter()
            .find(|i| i.origin == InstructionOrigin::Harness && i.name.to_lowercase() == folded)
        {
            return Ok(Some(found));
        }
        if done {
            return Ok(None);
        }
    }
}

pub(crate) fn activation_operation(kind: InstructionKind) -> &'static str {
    match kind {
        InstructionKind::Skill => "skills.rollback",
        InstructionKind::Role => "roles.rollback",
    }
}

fn settings_failure(f: crate::settings::service::Failure) -> Failure {
    match f {
        crate::settings::service::Failure::Storage(e) => Failure::Storage(e),
        crate::settings::service::Failure::Refused(r) => {
            Refusal::new(r.code, r.target_kind, r.target_id).into()
        }
    }
}

pub(crate) fn proposal_record(
    approval: &crate::ports::storage::StoredApproval,
) -> Result<ProposalRecord, Failure> {
    let state = match approval.state.as_str() {
        "Approved" => "Approved",
        "Rejected" => "Rejected",
        "Superseded" => "Superseded",
        "Expired" => "Expired",
        _ => "Pending",
    };
    serde_json::from_value(json!({
        "id": approval.id.as_str(),
        "revision": approval.revision.max(1).to_string(),
        "contentDigest": approval.content_digest,
        "state": state,
        "operationKind": approval.operation_kind,
        "targetId": approval.target_id.as_str(),
        "targetRevision": approval.target_revision.max(1).to_string(),
        "summary": approval.summary,
        "artifact": approval.artifact,
    }))
    .map_err(|e| StorageError::InvalidRecord(format!("proposal record: {e}")).into())
}

#[derive(Clone, Copy)]
enum Op {
    Draft,
    Rollback,
}
fn result(kind: InstructionKind, op: Op, value: InstructionRecord) -> OperationResult {
    match (kind, op) {
        (InstructionKind::Skill, Op::Draft) => OperationResult::SkillsDraft { value },
        (InstructionKind::Skill, Op::Rollback) => OperationResult::SkillsRollback { value },
        (InstructionKind::Role, Op::Draft) => OperationResult::RolesDraft { value },
        (InstructionKind::Role, Op::Rollback) => OperationResult::RolesRollback { value },
    }
}

fn applied_record(reply: Option<&DomainCommandReply>) -> Option<InstructionRecord> {
    let value = serde_json::to_value(reply?).ok()?;
    let decision = value.pointer("/receipt/decision")?;
    if decision["kind"] != "Applied" {
        return None;
    }
    let result = decision.get("result")?;
    match result["kind"].as_str()? {
        "skills.draft" | "skills.rollback" | "roles.draft" | "roles.rollback" => {
            serde_json::from_value(result["value"].clone()).ok()
        }
        _ => None,
    }
}
