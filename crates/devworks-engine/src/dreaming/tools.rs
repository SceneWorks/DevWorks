//! Execute selected private calls and retain their exact results before publishing continuation.
use super::{NativeDreaming, RoundStep, invalid};
use devworks_core::context::InvocationContext;
use devworks_core::ports::storage::{
    ArtifactScope, DreamActionRow, DreamClaimWindow, DreamPhase, DreamRunRow, DreamStream,
    KnowledgeReviewKind, ReadOptions, StorageError, WriteOptions, WriteTransaction, read_snapshot,
    run_write,
};
use devworks_core::protocol::provider::{ProviderBody, ToolSchema};
use devworks_core::protocol::{EventId, Id};
use devworks_core::providers::context::{
    ContentBlock, Message, MessageRole, ProviderContext, ResultBlock,
};
use devworks_core::services::artifacts::{StagedFile, host::HostArtifactPublication, version_ref};
use devworks_core::services::dreaming::windows::{
    WindowPosition, json_fragment, sign_cursor, verify_cursor,
};
use devworks_core::services::dreaming::{DreamTools, SweepLease};
use serde_json::{Value, json};

const TOOL_RESULT_LIMIT: usize = 48 * 1024;

impl NativeDreaming {
    pub(super) fn apply_tool_round(
        &self,
        lease: &SweepLease,
        ctx: &InvocationContext,
        run: &DreamRunRow,
        bodies: Vec<ProviderBody>,
    ) -> Result<RoundStep, StorageError> {
        let checkpoint = run
            .checkpoint
            .as_ref()
            .ok_or_else(|| invalid("missing Dreaming checkpoint"))?;
        let request = checkpoint
            .request
            .as_ref()
            .ok_or_else(|| invalid("missing Dreaming round"))?;
        let bytes = self.exact(ctx, &request.frame, 2 * 1024 * 1024)?;
        let mut context: ProviderContext =
            serde_json::from_slice(&bytes).map_err(|_| invalid("invalid last Dreaming context"))?;
        let mut assistant = Vec::new();
        let mut response = Vec::new();
        for body in bodies {
            match body {
                ProviderBody::TextDelta { text, .. } => {
                    assistant.push(ContentBlock::Text { text, cache: None })
                }
                ProviderBody::ToolCallComplete { tool_call } => {
                    let alias = DreamTools::frozen()
                        .aliases()
                        .into_iter()
                        .find(|known| {
                            known.capability_id == tool_call.capability_id
                                && known.input_schema.sha256 == tool_call.input_schema.sha256
                                && known.input_schema.revision == tool_call.input_schema.revision
                                && known.input_schema.schema_id == tool_call.input_schema.schema_id
                        })
                        .ok_or(StorageError::ScopeDenied)?;
                    if request.attempt_id.as_ref() != Some(&tool_call.call.attempt_id) {
                        return Err(StorageError::ScopeDenied);
                    }
                    let raw = self.exact(ctx, &tool_call.arguments, 512 * 1024)?;
                    let args = DreamTools::frozen().validate(self.validator.as_ref(), &alias, &raw);
                    let identity = json!([
                        request.logical_id,
                        tool_call.call.ordinal,
                        tool_call.call.native_call_id
                    ]);
                    let action = Id::new(format!(
                        "dream-action-{}",
                        devworks_core::fingerprint::sha256_hex(identity.to_string().as_bytes())
                    ))
                    .map_err(|_| invalid("invalid action identity"))?;
                    let mut receipt = self.record_call(lease, ctx, run, &alias, &action, args)?;
                    if receipt.action["result"]["reviewRequired"] == true {
                        let original = &receipt.action["arguments"];
                        let evidence = original["evidence"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter(|entry| entry["kind"] == "entry")
                            .map(|entry| json!({"kind":"entry","id":entry["reference"]}))
                            .collect::<Vec<_>>();
                        let proposal = json!({
                            "action":"merge", "target":original["id"],
                            "expected_version":original["expected_version"], "other":original["same_as"],
                            "other_expected_version":receipt.action["result"]["otherRevision"],
                            "rationale":"Dreaming found conflicting evidence for a possible duplicate lesson.",
                            "prevalence":original["prevalence"], "evidence":evidence
                        });
                        let proposal_action = Id::new(format!(
                            "dream-review-{}",
                            devworks_core::fingerprint::sha256_hex(action.as_str().as_bytes())
                        ))
                        .map_err(|_| invalid("invalid Dreaming review identity"))?;
                        let review =
                            self.record_proposal(lease, run, &proposal_action, &proposal)?;
                        receipt = review;
                    }
                    let call_id = tool_call.call.native_call_id;
                    assistant.push(ContentBlock::ToolCall {
                        call_id: call_id.clone(),
                        wire_name: alias.wire_name,
                        arguments: tool_call.arguments,
                    });
                    let is_error = receipt.action["result"].get("error").is_some();
                    let rendered = result_text(&receipt)?;
                    response.push(ContentBlock::ToolResult {
                        call_id,
                        operation_id: None,
                        is_error,
                        cache: None,
                        content: vec![ResultBlock::Text { text: rendered }],
                    });
                }
                _ => return Err(invalid("unexpected Dreaming provider content")),
            }
        }
        if response.is_empty() {
            return Ok(RoundStep::Finish(
                DreamPhase::Partial,
                "ProviderToolResultWasEmpty",
            ));
        }
        context.messages.push(Message {
            role: MessageRole::Assistant,
            content: assistant,
        });
        context.messages.push(Message {
            role: MessageRole::User,
            content: response,
        });
        let next =
            serde_json::to_vec(&context).map_err(|_| invalid("Dreaming continuation encoding"))?;
        let material = self.publish(
            lease,
            ctx,
            "Dreaming continuation",
            "application/vnd.devworks.provider-context+json",
            &next,
        )?;
        let lease = lease.clone();
        run_write(self.store.as_ref(), WriteOptions::observed(), move |tx| {
            lease
                .continue_with(tx, material)
                .map(|row| RoundStep::Continue(Box::new(row)))
        })
    }

    fn record_call(
        &self,
        lease: &SweepLease,
        ctx: &InvocationContext,
        run: &DreamRunRow,
        alias: &ToolSchema,
        action: &Id,
        args: Result<Value, StorageError>,
    ) -> Result<DreamActionRow, StorageError> {
        let tool = alias.wire_name.clone();
        let graph = if let Ok(args) = &args
            && matches!(tool.as_str(), "dream_search_graph" | "dream_read_source")
        {
            let prior = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
                read.dreaming()?.dream_action(&run.id, action)
            })?;
            if let Some(prior) = prior {
                return Ok(prior);
            }
            self.read_graph(ctx, run, action, &tool, args)?
        } else {
            None
        };
        if tool == "dream_propose"
            && let Ok(args) = &args
        {
            return self.record_proposal(lease, run, action, args);
        }
        let checked_sources = if let Ok(args) = &args {
            self.refresh_proofs(run, args)?
        } else {
            Default::default()
        };
        let lease = lease.clone();
        let action = action.clone();
        run_write(self.store.as_ref(), WriteOptions::observed(), move |tx| {
            lease.context(tx)?;
            let row = lease.read(tx, true)?;
            if let Some(recorded) = tx.dreaming()?.dream_action(&row.id, &action)? {
                return Ok(recorded);
            }
            let args = match args {
                Ok(value) => value,
                Err(_) => {
                    let result = json!({"error":"ArgumentsDoNotMatchSelectedTool"});
                    return tx.dreaming()?.put_dream_action(
                        &row.id,
                        row.generation,
                        &action,
                        &json!({"tool":tool,"result":result}),
                    );
                }
            };
            let result = match graph.or(lease.consolidate(tx, &tool, &args, &checked_sources)?) {
                Some(result) => result,
                None => match execute_read(tx, &row, &tool, &args) {
                    Ok(result) => result,
                    Err(StorageError::ViewUnavailable | StorageError::ScopeDenied) => {
                        json!({"error":"SelectedEvidenceUnavailable"})
                    }
                    Err(StorageError::CursorExpired) => json!({"error":"CursorExpired"}),
                    Err(error) => return Err(error),
                },
            };
            tx.dreaming()?.put_dream_action(
                &row.id,
                row.generation,
                &action,
                &json!({"tool":tool,"arguments":args,"result":result}),
            )
        })
    }

    fn record_proposal(
        &self,
        lease: &SweepLease,
        run: &DreamRunRow,
        action: &Id,
        args: &Value,
    ) -> Result<DreamActionRow, StorageError> {
        if let Some(prior) = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
            read.dreaming()?.dream_action(&run.id, action)
        })? {
            return Ok(prior);
        }
        let draft = serde_json::to_vec(args).map_err(|_| invalid("Dreaming proposal encoding"))?;
        let staged = self
            .artifacts
            .stage_host_bytes(
                self.store.as_ref(),
                &draft,
                json!({"kind":"DreamingProposalDraft","runId":run.id,"actionId":action}),
            )?
            .map_err(|_| invalid("Dreaming proposal staging failed"))?;
        let pending = staged.clone();
        let run = run.id.clone();
        let (action, args, lease, artifacts) = (
            action.clone(),
            args.clone(),
            lease.clone(),
            self.artifacts.clone(),
        );
        let committed = run_write(self.store.as_ref(), WriteOptions::observed(), move |tx| {
            lease.context(tx)?;
            let row = lease.read(tx, true)?;
            if let Some(prior) = tx.dreaming()?.dream_action(&run, &action)? {
                return Ok((prior, false));
            }
            let point = tx.savepoint()?;
            let artifact = publish_draft(tx, &artifacts, &pending, &row, &action)?;
            let (result, keep) = match lease.stage_proposal(tx, &args, artifact.clone())? {
                Some(result) if result["proposal"]["artifact"] == json!(artifact) => {
                    tx.release(point)?;
                    (result, true)
                }
                Some(result) => {
                    // Suppressed duplicate: keep the original proposal and discard the new draft.
                    tx.rollback_to(point)?;
                    (result, false)
                }
                None => {
                    tx.rollback_to(point)?;
                    (json!({"error":"DreamingProposalPolicyRefused"}), false)
                }
            };
            let receipt = tx.dreaming()?.put_dream_action(
                &row.id,
                row.generation,
                &action,
                &json!({"tool":"dream_propose","result":result}),
            )?;
            Ok((receipt, keep))
        });
        self.artifacts.settle_host_bytes(
            self.store.as_ref(),
            &staged,
            matches!(&committed, Ok((_, true)) | Err(StorageError::CommitUnknown)),
        );
        committed.map(|(result, _)| result)
    }
}

fn publish_draft(
    tx: &mut dyn WriteTransaction,
    artifacts: &devworks_core::services::artifacts::ArtifactService,
    staged: &StagedFile,
    run: &DreamRunRow,
    action: &Id,
) -> Result<devworks_core::protocol::provider::ArtifactRef, StorageError> {
    let scope = run
        .scope
        .project()
        .cloned()
        .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
    let owner = tx.store().host_actor.clone();
    let saved = artifacts.commit_host_bytes_in_scope(
        tx,
        staged,
        HostArtifactPublication {
            title: "Dreaming proposal draft",
            media_type: "application/json",
            provenance: json!({"kind":"DreamingProposalDraft","runId":run.id,"actionId":action}),
            scope,
            owner,
        },
    )?;
    Ok(version_ref(&saved))
}

fn result_text(receipt: &DreamActionRow) -> Result<String, StorageError> {
    let result = &receipt.action["result"];
    let encoded = result.to_string();
    let hash = devworks_core::fingerprint::sha256_hex(encoded.as_bytes());
    if encoded.len() <= TOOL_RESULT_LIMIT {
        return Ok(
            json!({"action_index":receipt.position,"expected_hash":hash,"result":result})
                .to_string(),
        );
    }
    let (fragment, following) = json_fragment(&encoded, 0, TOOL_RESULT_LIMIT)?;
    Ok(json!({
        "action_index":receipt.position,
        "content":fragment,
        "next_offset_chars":following,
        "expected_hash":hash,
        "encoding":"exactUtf8Json",
    })
    .to_string())
}

fn execute_read(
    tx: &mut dyn WriteTransaction,
    row: &DreamRunRow,
    tool: &str,
    args: &Value,
) -> Result<Value, StorageError> {
    let limit = args["limit"]
        .as_u64()
        .and_then(|n| u16::try_from(n).ok())
        .unwrap_or(20);
    let scope = row.scope.key();
    match tool {
        "dream_read_prior" => {
            let index = args["action_index"]
                .as_i64()
                .ok_or_else(|| invalid("invalid prior action"))?;
            let previous = tx
                .dreaming()?
                .dream_actions(&row.id, index.checked_add(1), 1)?
                .into_iter()
                .find(|row| row.position == index)
                .ok_or(StorageError::ViewUnavailable)?;
            let raw = previous.action["result"].to_string();
            let digest = devworks_core::fingerprint::sha256_hex(raw.as_bytes());
            if args["expected_hash"]
                .as_str()
                .is_some_and(|expected| expected != digest)
            {
                return Ok(json!({"error":"ActionHashChanged"}));
            }
            let offset = args["offset_chars"]
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(0);
            let (fragment, next) = json_fragment(&raw, offset, TOOL_RESULT_LIMIT)?;
            Ok(
                json!({"content":fragment,"next_offset_chars":next,"expected_hash":digest,
                      "encoding":"exactUtf8Json","historyOnly":true}),
            )
        }
        "dream_read" | "dream_lessons" => read_knowledge(tx, row, tool, args, limit),
        "dream_similar" => super::similar::read(tx, row, args, limit),
        "dream_review_queue" => {
            if args["cursor"].is_string() {
                return Ok(json!({"error":"CursorExpired"}));
            }
            let k = tx.knowledge();
            let mut review = Vec::new();
            for kind in [
                KnowledgeReviewKind::Challenge,
                KnowledgeReviewKind::ProposedRule,
            ] {
                for item in
                    k.knowledge_review_candidates(std::slice::from_ref(&scope), kind, limit)?
                {
                    review.push(json!({"claimId":item.id,"revision":item.revision,
                        "kind":format!("{kind:?}"),"scope":scope,"value":item.body.value}));
                }
            }
            Ok(json!({"items":review}))
        }
        "dream_work_trail" if args["id"].is_string() => {
            let id = EventId(
                Id::new(args["id"].as_str().unwrap())
                    .map_err(|_| invalid("invalid work evidence ID"))?,
            );
            let event = tx
                .history()
                .event(&id)?
                .filter(|event| {
                    event.project_id.as_ref() == row.scope.project()
                        && event.sequence <= row.cutoffs.reviews
                        && event.kind == "PlanReviewSettled"
                })
                .ok_or(StorageError::ViewUnavailable)?;
            let raw = json!({"id":event.id,"kind":event.kind,"atMs":event.created_at_ms,
                "revision":event.aggregate_revision,"payload":event.payload});
            let digest = devworks_core::fingerprint::sha256_hex(raw.to_string().as_bytes());
            if args["expected_hash"]
                .as_str()
                .is_some_and(|expected| expected != digest)
            {
                return Ok(json!({"error":"WorkEvidenceChanged"}));
            }
            Ok(json!({"evidence":raw,"expected_hash":digest}))
        }
        "dream_work_trail" => {
            let reviews = tx.dreaming()?.dream_signals(
                &row.scope,
                DreamStream::Reviews,
                0,
                row.cutoffs.reviews,
                limit,
            )?;
            Ok(json!({"items":reviews.iter().map(|item| json!({
                "id":item.reference,"kind":"review","observedAtMs":item.observed_at_ms,
            })).collect::<Vec<_>>()}))
        }
        _ => Ok(json!({"error":"SelectedOperationUnavailable"})),
    }
}

fn read_knowledge(
    tx: &mut dyn WriteTransaction,
    row: &DreamRunRow,
    tool: &str,
    args: &Value,
    limit: u16,
) -> Result<Value, StorageError> {
    let key = row
        .checkpoint
        .as_ref()
        .and_then(|checkpoint| checkpoint.cursor_key.as_ref())
        .ok_or(StorageError::CursorExpired)?;
    let (selection, after) = if let Some(cursor) = args["cursor"].as_str() {
        let position = verify_cursor(key, cursor, row.id.as_str(), &row.scope.key(), tool)
            .or_else(|_| {
                row.continued_from
                    .as_ref()
                    .ok_or(StorageError::CursorExpired)
                    .and_then(|parent| {
                        verify_cursor(key, cursor, parent.as_str(), &row.scope.key(), tool)
                    })
            })?;
        let filters: Value =
            serde_json::from_str(&position.selection).map_err(|_| StorageError::CursorExpired)?;
        (filters, Some(position.window))
    } else {
        let mut selection = args.clone();
        if let Some(fields) = selection.as_object_mut() {
            fields.remove("limit");
        }
        (selection, None)
    };
    let sign = |scalar: String| -> Result<String, StorageError> {
        let selection = devworks_core::jcs::canonicalize(&selection)
            .map_err(|_| invalid("invalid Dreaming selection"))?;
        sign_cursor(
            key,
            &WindowPosition {
                run: row.id.as_str().to_owned(),
                scope: row.scope.key(),
                tool: tool.to_owned(),
                selection,
                window: scalar,
                offset: 0,
            },
        )
    };
    if tool == "dream_lessons" && selection["view"] == "unlearned_reviews" {
        let start = after
            .as_deref()
            .map(str::parse::<i64>)
            .transpose()
            .map_err(|_| StorageError::CursorExpired)?
            .unwrap_or(0);
        let mut rows = tx.dreaming()?.dream_signals(
            &row.scope,
            DreamStream::Reviews,
            start,
            row.cutoffs.reviews,
            limit + 1,
        )?;
        let more = rows.len() > usize::from(limit);
        rows.truncate(usize::from(limit));
        let next = if more {
            rows.last()
                .map(|last| sign(last.sequence.to_string()))
                .transpose()?
        } else {
            None
        };
        return Ok(json!({"items":rows.iter().map(|signal| json!({
            "id":signal.reference,"sequence":signal.sequence,"observedAtMs":signal.observed_at_ms,
        })).collect::<Vec<_>>(),"has_more":more,"next_cursor":next}));
    }
    let after = after
        .as_deref()
        .map(Id::new)
        .transpose()
        .map_err(|_| StorageError::CursorExpired)?;
    let exact = selection["id"]
        .as_str()
        .map(Id::new)
        .transpose()
        .map_err(|_| invalid("invalid Knowledge ID"))?;
    let normalized = |value: &str| {
        let mut chars = value.chars();
        chars
            .next()
            .into_iter()
            .flat_map(char::to_uppercase)
            .chain(chars)
            .collect::<String>()
    };
    let kind = if tool == "dream_lessons" {
        Some("Lesson".to_owned())
    } else {
        selection["kind"].as_str().map(normalized)
    };
    let status = selection["status"].as_str().map(normalized);
    let mut refs = tx.dreaming()?.dream_claim_refs(&DreamClaimWindow {
        scope: &row.scope,
        through: row.cutoffs.knowledge,
        after: after.as_ref(),
        exact: exact.as_ref(),
        kind: kind.as_deref(),
        status: status.as_deref(),
        limit: limit + 1,
    })?;
    let more = refs.len() > usize::from(limit);
    refs.truncate(usize::from(limit));
    let next = if more {
        refs.last()
            .map(|last| sign(last.id.as_str().to_owned()))
            .transpose()?
    } else {
        None
    };
    let include = exact.is_some() || selection["detail"] == "full" || tool == "dream_lessons";
    let mut items = Vec::new();
    let mut proofs = Vec::new();
    for reference in refs {
        let mut item = json!({"id":reference.id,"revision":reference.revision,
            "kind":reference.entry_kind,"lifecycle":reference.lifecycle});
        if include {
            let snapshot = tx
                .knowledge()
                .knowledge_history_at(&reference.id, reference.revision)?
                .ok_or(StorageError::ViewUnavailable)?;
            let canonical = devworks_core::jcs::canonicalize(&snapshot.snapshot)
                .map_err(|_| invalid("invalid frozen Knowledge snapshot"))?;
            let fingerprint = devworks_core::fingerprint::sha256_hex(canonical.as_bytes());
            item["snapshot"] = snapshot.snapshot;
            item["fingerprint"] = json!(fingerprint);
            if reference.current_revision == reference.revision && include && exact.is_some() {
                proofs.push(
                    json!({"kind":"entry","id":reference.id,"revision":reference.revision,
                    "fingerprint":fingerprint}),
                );
            }
        }
        items.push(item);
    }
    Ok(json!({"items":items,"has_more":more,"next_cursor":next,"proofs":proofs}))
}
