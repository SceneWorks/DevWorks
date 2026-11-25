//! Repository documentation belongs to the same owned-worktree journal as Git writes.
use super::*;
use crate::operations::repository_docs::GenerateInput;
use crate::ports::indexing::{SourceFault, SourceTree};
use crate::ports::storage::EventFilter;
use crate::services::graph::repository_analysis::{
    AnalysisService, DocumentationDraft, DocumentationSourceFacts, synthesis_request,
};
use std::fmt::Write;

const MAX_DOC_BYTES: usize = 256 * 1024;

pub(super) type PreparedDocs = Result<DocumentationDraft, Failure>;

impl GitService {
    pub(super) fn prepare_repository_docs(
        &self,
        ctx: &InvocationContext,
        request: &GenerateInput,
        env: &Preparation<'_>,
        run_id: Option<&Id>,
    ) -> Result<Option<Prepared>, StorageError> {
        if env.stopped(self.clock.now_ms().get()) {
            return Err(StorageError::Cancelled);
        }
        let prepared: PreparedDocs = (|| {
            let analysis = self.documentation_analysis().ok_or_else(|| {
                refuse(
                    DomainErrorCode::UnsupportedCapability,
                    "repository_analysis",
                    &request.repository_id,
                )
            })?;
            let ownership = ctx.worktree().ok_or_else(|| {
                refuse(
                    DomainErrorCode::PermissionDenied,
                    "owned_worktree",
                    &request.repository_id,
                )
            })?;
            let (tree, repository) =
                read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                    Ok((
                        read.git().git_worktree(ownership.id())?,
                        read.projects().repository(&request.repository_id)?,
                    ))
                })?;
            let tree = tree
                .filter(|tree| {
                    tree.repository_id == request.repository_id
                        && tree.registered_root_id == request.output_root_id
                        && &tree.owner == ctx.principal()
                        && tree.ownership_revision == ownership.ownership_revision()
                        && !tree.removed
                })
                .ok_or_else(|| {
                    refuse(
                        DomainErrorCode::PermissionDenied,
                        "documentation_target",
                        &request.output_root_id,
                    )
                })?;
            let repository = repository.ok_or(StorageError::ScopeDenied)?;
            let stop = || env.stopped(self.clock.now_ms().get());
            let git = Git {
                runner: &*self.helpers,
                ctx,
                program: &self.config.program,
                deadline: self.config.deadline,
                stop: &stop,
            };
            let path = Path::new(&tree.path);
            self.verify_target(
                &git,
                &Target {
                    repository,
                    worktree: Some(tree.clone()),
                },
            )
            .map_err(|error| read_failure(error, &request.repository_id))?;
            let (entries, _) = git
                .tree(path, &tree.head.hex)
                .map_err(|error| read_failure(error, &request.repository_id))?;
            let facts = source_facts(&analysis, &git, path, entries)
                .map_err(|error| read_failure(error, &request.repository_id))?;
            let draft = analysis.prepare_documentation(ctx, request, &facts, run_id)?;
            if draft.evidence.view.snapshot.head != tree.head {
                return Err(refuse(
                    DomainErrorCode::SourceChanged,
                    "documentation_source",
                    &tree.id,
                ));
            }
            let source = analysis
                .documentation_source()
                .ok_or(StorageError::ScopeDenied)?;
            prepare_patch(&git, source.as_ref(), path, &tree, &draft).map_err(|error| {
                if matches!(error, GitFault::SourceChanged) {
                    refuse(
                        DomainErrorCode::SourceChanged,
                        "documentation_target",
                        &tree.id,
                    )
                } else {
                    read_failure(error, &request.repository_id)
                }
            })?;
            Ok(draft)
        })();
        Ok(Some(Prepared::new(prepared)))
    }

    /// Admit the event's exact, still-owned synthesis through the same gateway and journal
    /// used by an explicit generation command. Replaying the delivery reuses its receipt.
    pub fn admit_repository_documentation(
        self: &Arc<Self>,
        authority: &dyn crate::ports::AuthorityPort,
        entry: &QueueEntry,
        event: &EventRecord,
        cancellation: &Cancellation,
    ) -> Result<crate::protocol::results::DomainCommandReply, Failure> {
        if matches!(
            event.kind.as_str(),
            "RepositoryDocumentationGenerated" | "RepositoryDocumentationCommitted"
        ) {
            return self.request_documentation_review(authority, entry, event, cancellation);
        }
        let (ctx, input, run_id) = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            Ok(synthesis_request(read, entry, event))
        })??;
        let mut body = serde_json::to_value(&input)
            .map_err(|error| StorageError::InvalidRecord(error.to_string()))?;
        body["kind"] = json!("repository_docs.generate");
        let frame = serde_json::to_vec(&json!({
            "mode": "Command", "protocol": "1.0-draft.1", "schemaRevision": 14,
            "requestKey": crate::fingerprint::sha256_hex(format!("repository-docs:{}", event.id.0.as_str()).as_bytes()),
            "body": body
        })).map_err(|error| StorageError::InvalidRecord(error.to_string()))?;
        crate::gateway::Gateway {
            store: &*self.store,
            authority,
            ids: &*self.ids,
        }
        .command_with(
            &ctx,
            "repository_docs.generate",
            &frame,
            Arc::new(SynthesisCommand {
                git: self.clone(),
                run_id,
                delivery: entry.clone(),
            }),
            WriteOptions {
                cancellation: Some(cancellation.clone()),
                ..WriteOptions::command()
            },
        )
        .map_err(|error| StorageError::InvalidRecord(error.to_string()).into())
    }

    fn request_documentation_review(
        &self,
        authority: &dyn crate::ports::AuthorityPort,
        entry: &QueueEntry,
        event: &EventRecord,
        cancellation: &Cancellation,
    ) -> Result<crate::protocol::results::DomainCommandReply, Failure> {
        let operation = event
            .operation_id
            .as_ref()
            .ok_or(StorageError::ScopeDenied)?;
        if entry.event_id != event.id
            || entry.consumer != crate::services::graph::repository_analysis::DOCS_CONSUMER
        {
            return Err(StorageError::ScopeDenied.into());
        }
        let (record, tree) = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            let record = read
                .operations()
                .operation(operation)?
                .ok_or(StorageError::ScopeDenied)?;
            let id = serde_json::from_value::<Id>(event.payload["worktreeId"].clone())
                .map_err(|_| StorageError::ScopeDenied)?;
            let tree = read
                .git()
                .git_worktree(&id)?
                .ok_or(StorageError::ScopeDenied)?;
            if tree.last_operation_id.as_ref() != Some(operation) || tree.removed {
                return Err(StorageError::ScopeDenied);
            }
            if tree.pending_operation_id.is_some() {
                return Err(StorageError::Busy);
            }
            Ok((record, tree))
        })?;
        if record.state != OperationState::ObservedComplete {
            return Err(StorageError::Busy.into());
        }
        let job =
            serde_json::from_value::<Job>(record.payload).map_err(|_| StorageError::ScopeDenied)?;
        if job.tree.id != tree.id
            || job.actor != event.actor
            || job.project != event.project_id.clone().ok_or(StorageError::ScopeDenied)?
        {
            return Err(StorageError::ScopeDenied.into());
        }
        let ctx = crate::services::git::context::bind_worktree(
            &*self.store,
            job.context(self, operation)
                .ok_or(StorageError::ScopeDenied)?,
            &tree.id,
        )?;
        let (name, command) = match (event.kind.as_str(), record.capability.as_str()) {
            ("RepositoryDocumentationGenerated", "repository_docs.generate")
                if matches!(
                    event.payload["delivery"].as_str(),
                    Some("Commit" | "CommitAndPush")
                ) =>
            {
                (
                    "git.commit",
                    json!({"kind":"Commit", "worktreeId":tree.id,
                    "expectedHead":tree.head, "message": event.payload["commitMessage"]}),
                )
            }
            ("RepositoryDocumentationCommitted", "git.commit")
                if event.payload["delivery"] == "CommitAndPush" =>
            {
                (
                    "git.push",
                    json!({"kind":"Push", "worktreeId":tree.id,
                    "remoteId":"origin", "targetRef":tree.branch, "expectedHead":tree.head, "mode":"Normal"}),
                )
            }
            _ => return Err(StorageError::ScopeDenied.into()),
        };
        let commit_manifest = if name == "git.commit" {
            let path = Path::new(&tree.path);
            let stopped = || cancellation.is_cancelled();
            let git = Git {
                runner: &*self.helpers,
                ctx: &ctx,
                program: &self.config.program,
                deadline: self.config.deadline,
                stop: &stopped,
            };
            let manifest = git
                .commit_manifest(path, &tree.head.hex)
                .map_err(|error| read_failure(error, &tree.repository_id))?;
            let files = manifest["files"]
                .as_array()
                .ok_or(StorageError::ScopeDenied)?;
            let expected = event.payload["files"]
                .as_array()
                .ok_or(StorageError::ScopeDenied)?;
            let source = self
                .documentation_analysis()
                .and_then(|analysis| analysis.documentation_source())
                .ok_or(StorageError::ScopeDenied)?;
            if files.is_empty()
                || files
                    .iter()
                    .any(|file| !expected.iter().any(|entry| entry["path"] == file["path"]))
                || !expected.iter().all(|file| {
                    let (Some(name), Some(digest)) =
                        (file["path"].as_str(), file["sha256"].as_str())
                    else {
                        return false;
                    };
                    owned_doc(name)
                        && source
                            .read_file(path, name, MAX_DOC_BYTES as u64)
                            .is_ok_and(|bytes| crate::fingerprint::sha256_hex(&bytes) == digest)
                })
            {
                return Err(StorageError::ScopeDenied.into());
            }
            Some(crate::services::git::approval::compact_commit_manifest(
                &manifest,
            )?)
        } else {
            None
        };
        let input = json!({"command": command});
        let request =
            crate::services::approvals::requests::decode(name, 1, &input, true).map_err(|_| {
                StorageError::InvalidRecord("invalid documentation delivery operation".into())
            })?;
        let frame = serde_json::json!({
            "mode":"Command", "protocol":"1.0-draft.1", "schemaRevision":3,
            "requestKey":crate::fingerprint::sha256_hex(format!("documentation-review:{}", event.id.0.as_str()).as_bytes()),
            "body":{"kind":"approvals.propose", "scope":{"kind":"Project","projectId":job.project},
                "operationKind":name, "target":{"id":tree.id,"revision":tree.revision.to_string()},
                "contentDigest":request.fingerprint.as_str(), "summary":"Review generated repository documentation",
                "payload":{"kind":"SavedOperation","schemaRevision":"1","input":input}}
        }).to_string();
        let approvals = self
            .documentation_approvals
            .get()
            .and_then(std::sync::Weak::upgrade)
            .ok_or(StorageError::Busy)?;
        crate::gateway::Gateway {
            store: &*self.store,
            authority,
            ids: &*self.ids,
        }
        .command_with(
            &ctx,
            "approvals.propose",
            frame.as_bytes(),
            Arc::new(ReviewCommand {
                approvals,
                delivery: entry.clone(),
                commit_manifest,
            }),
            WriteOptions {
                cancellation: Some(cancellation.clone()),
                ..WriteOptions::command()
            },
        )
        .map_err(|_| {
            StorageError::InvalidRecord("documentation review could not be admitted".into()).into()
        })
    }
}

fn source_facts(
    analysis: &AnalysisService,
    git: &Git<'_>,
    path: &Path,
    entries: Vec<crate::services::git::client::TreeEntry>,
) -> Result<DocumentationSourceFacts, GitFault> {
    // Measure only captured tracked source, with the same language inventory as indexing.
    // Keep one bounded batch. A large or non-text source explicitly makes the totals partial.
    const SOURCE_BYTES: usize = 16 * 1024 * 1024;
    const SOURCE_FILES: usize = 10_000;
    let mut tracked = Vec::new();
    let mut sources = Vec::new();
    let mut oids = Vec::new();
    let mut expected = 0;
    let mut languages_partial = false;
    for entry in entries {
        if entry.submodule() || entry.link() {
            continue;
        }
        if let Some(language) = analysis.documentation_language(&entry.path) {
            if let Ok(size) = usize::try_from(entry.size)
                && size <= SOURCE_BYTES - expected
                && sources.len() < SOURCE_FILES
            {
                expected += size;
                sources.push(language);
                oids.push(entry.oid);
            } else {
                languages_partial = true;
            }
        }
        tracked.push(entry.path);
    }
    let mut lines = std::collections::BTreeMap::<_, usize>::new();
    for (language, bytes) in sources
        .into_iter()
        .zip(git.documentation_blobs(path, &oids, expected)?)
    {
        if let Ok(source) = std::str::from_utf8(&bytes) {
            *lines.entry(language).or_default() += source
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count();
        } else {
            languages_partial = true;
        }
    }
    let total: usize = lines.values().sum();
    let mut lines: Vec<_> = lines
        .into_iter()
        .filter(|(_, count)| *count != 0 && *count * 10 >= total)
        .collect();
    lines.sort_by(|(a, left), (b, right)| right.cmp(left).then(a.cmp(b)));
    let languages = lines
        .into_iter()
        .map(|(language, count)| {
            (
                language.to_owned(),
                format!(
                    "{count} nonblank lines ({:.1}%)",
                    count as f64 * 100.0 / total as f64
                ),
            )
        })
        .collect();
    Ok(DocumentationSourceFacts {
        tracked,
        languages,
        languages_partial,
    })
}

struct ReviewCommand {
    approvals: Arc<crate::services::approvals::ApprovalService>,
    delivery: QueueEntry,
    commit_manifest: Option<Value>,
}

impl CommandHandler for ReviewCommand {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        let prepared = CommandHandler::prepare(&*self.approvals, ctx, command, env)?;
        if let Some(expected) = self.commit_manifest.as_ref()
            && !prepared.as_ref().is_some_and(|prepared| {
                crate::services::approvals::prepared_commit_matches(prepared, expected)
            })
        {
            return Err(StorageError::ScopeDenied);
        }
        Ok(prepared)
    }

    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        check_delivery(tx, &self.delivery)?;
        self.approvals.decide(ctx, command, tx)
    }
}

fn check_delivery(
    tx: &mut dyn WriteTransaction,
    delivery: &QueueEntry,
) -> Result<(), StorageError> {
    let current = tx.queue().entry(&delivery.id)?;
    if !current.is_some_and(|entry| {
        entry.state == QueueState::Claimed
            && entry.fence() == delivery.fence()
            && entry.cancellation_epoch == delivery.cancellation_epoch
            && !entry.cancel_requested
            && entry.claim_boot_id == tx.store().boot_id
            && entry
                .lease_until_ms
                .is_some_and(|expires| expires > tx.now_ms())
    }) {
        return Err(StorageError::Cancelled);
    }
    Ok(())
}

pub(super) fn request(
    tx: &mut dyn WriteTransaction,
    event: EventId,
    job: &Job,
) -> Result<(), StorageError> {
    let id = tx.new_id(IdKind::QueueEntry);
    let now = tx.now_ms();
    tx.queue().enqueue(NewQueueEntry {
        id,
        consumer: crate::services::graph::repository_analysis::DOCS_CONSUMER.into(),
        event_id: event,
        root_id: job.root.clone(),
        operation_id: None,
        partition: None,
        priority: 0,
        ready_at_ms: now,
        payload: json!({"worktreeId":job.tree.id}),
    })?;
    Ok(())
}

struct SynthesisCommand {
    git: Arc<GitService>,
    run_id: Id,
    delivery: QueueEntry,
}

impl CommandHandler for SynthesisCommand {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        let OperationInput::RepositoryDocs(crate::operations::repository_docs::Input::Generate(
            input,
        )) = command.input()
        else {
            return Err(StorageError::ScopeDenied);
        };
        self.git
            .prepare_repository_docs(ctx, input, env, Some(&self.run_id))
    }

    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        check_delivery(tx, &self.delivery)?;
        if let Some(worktree) = ctx.worktree()
            && tx
                .git()
                .git_worktree(worktree.id())?
                .is_some_and(|row| row.pending_operation_id.is_some())
        {
            // A competing Git effect has not allowed a decision yet. Preserve the event's
            // request key for a real decision after that custody reservation clears.
            return Err(StorageError::Busy.into());
        }
        self.git.decide(ctx, command, tx)
    }
}

fn owned_doc(path: &str) -> bool {
    path.split('/').all(|part| {
        !part.is_empty()
            && !matches!(part.to_ascii_lowercase().as_str(), "." | ".." | ".git")
            && !part.ends_with([' ', '.'])
            && !part.starts_with('"')
            && !part.contains(['\\', ':', '\r', '\n', '\t', '\0'])
    }) && (path == "CODEGRAPH.md" || path.ends_with("/CODEGRAPH.md"))
}

pub(super) fn apply(
    git: &Git<'_>,
    source: &dyn SourceTree,
    path: &Path,
    job: &Job,
    operation: &OperationId,
    draft: &DocumentationDraft,
) -> Result<(), GitFault> {
    let patch = prepare_patch(git, source, path, &job.tree, draft)?;
    if !patch.is_empty() {
        let policy = git.reviewed_commit_policy(path)?;
        git.reviewed_mutation(
            path,
            &super::DOCUMENTATION_PATCH_ARGS,
            Some(patch.into_bytes()),
            operation,
            &policy,
        )?;
    }
    Ok(())
}

fn prepare_patch(
    git: &Git<'_>,
    source: &dyn SourceTree,
    path: &Path,
    tree: &StoredGitWorktree,
    draft: &DocumentationDraft,
) -> Result<String, GitFault> {
    if git.head(path)?.as_deref() != Some(tree.head.hex.as_str())
        || git.branch(path)?.as_deref() != Some(tree.branch.as_str())
    {
        return Err(GitFault::SourceChanged);
    }
    let (tracked, _) = git.tree(path, &tree.head.hex)?;
    let mut patch = String::new();
    for (name, rendered) in &draft.files {
        if !owned_doc(name) || rendered.len() > MAX_DOC_BYTES {
            return Err(GitFault::SourceChanged);
        }
        let current = match source.read_file(path, name, MAX_DOC_BYTES as u64) {
            Ok(bytes) if matches!(source.executable(path, name), Ok(false)) => Some(bytes),
            Err(SourceFault::Missing) => None,
            _ => return Err(GitFault::SourceChanged),
        };
        if current.as_deref() == Some(rendered.as_bytes())
            && git.index_has_exact_blob(path, name, rendered.as_bytes())?
        {
            continue;
        }
        let old = match tracked.iter().find(|entry| &entry.path == name) {
            Some(entry) if entry.mode == "100644" => {
                let bytes = git.file_at(path, &tree.head.hex, name, MAX_DOC_BYTES)?;
                if current.as_deref() != Some(bytes.as_slice())
                    || !git.index_has_exact_blob(path, name, &bytes)?
                {
                    return Err(GitFault::SourceChanged);
                }
                let content = String::from_utf8(bytes).map_err(|_| GitFault::SourceChanged)?;
                if !content.contains("Generated by DevWorks from repository analysis.")
                    && !content
                        .contains("This file is auto-generated by CodeGraph. Do not edit manually.")
                {
                    return Err(GitFault::SourceChanged);
                }
                Some(content)
            }
            Some(_) => return Err(GitFault::SourceChanged),
            None if current.is_none() && !git.index_tracked(path, name)? => None,
            None => return Err(GitFault::SourceChanged),
        };
        append_patch(&mut patch, name, old.as_deref(), rendered);
    }
    Ok(patch)
}

fn append_patch(patch: &mut String, path: &str, before: Option<&str>, after: &str) {
    let lines_before = before.map_or(0, |text| text.lines().count());
    let lines_after = after.lines().count();
    if before.is_some() {
        let _ = writeln!(patch, "--- a/{path}\t");
    } else {
        patch.push_str("--- /dev/null\n");
    }
    let start_before = u8::from(lines_before != 0);
    let _ = writeln!(
        patch,
        "+++ b/{path}\t\n@@ -{start_before},{lines_before} +1,{lines_after} @@"
    );
    for (marker, text) in [('-', before.unwrap_or_default()), ('+', after)] {
        for line in text.split_inclusive('\n') {
            patch.push(marker);
            patch.push_str(line);
            if !line.ends_with('\n') {
                patch.push_str("\n\\ No newline at end of file\n");
            }
        }
    }
}

/// The complete generated files still exist byte-for-byte in the Git commit observed remotely.
pub(super) fn matching_commit(
    store: &dyn ApplicationStore,
    git: &Git<'_>,
    path: &Path,
    job: &Job,
    head: &str,
) -> Result<Option<Value>, Failure> {
    let evidence = read_snapshot(store, &ReadOptions::default(), |read| {
        let filter = EventFilter::Aggregate {
            kind: "repository_documentation".into(),
            id: job.tree.id.as_str().into(),
        };
        let mut after = 0;
        let mut last = None;
        loop {
            let page = read.history().page(&filter, after, 100)?;
            for event in page.events {
                if event.kind == "RepositoryDocumentationGenerated" {
                    last = Some(event.payload);
                }
            }
            if !page.has_more {
                return Ok(last);
            }
            after = page.next_after;
        }
    })?;
    let Some(mut evidence) = evidence else {
        return Ok(None);
    };
    let Some(source) = evidence["sourceHead"]["hex"].as_str() else {
        return Ok(None);
    };
    if source == head || !git.is_ancestor(path, source, head).unwrap_or(false) {
        return Ok(None);
    }
    let Some(files) = evidence["files"]
        .as_array()
        .filter(|files| !files.is_empty())
    else {
        return Ok(None);
    };
    for file in files {
        let (Some(name), Some(expected)) = (file["path"].as_str(), file["sha256"].as_str()) else {
            return Ok(None);
        };
        if !owned_doc(name)
            || !git
                .file_at(path, head, name, MAX_DOC_BYTES)
                .is_ok_and(|bytes| crate::fingerprint::sha256_hex(&bytes) == expected)
        {
            return Ok(None);
        }
    }
    evidence["deliveredHead"] = json!(head);
    Ok(Some(evidence))
}

pub(super) fn observed(
    git: &Git<'_>,
    source: &dyn SourceTree,
    path: &Path,
    draft: &DocumentationDraft,
) -> bool {
    !draft.files.is_empty()
        && draft.files.iter().all(|(name, rendered)| {
            owned_doc(name)
                && matches!(source.executable(path, name), Ok(false))
                && source
                    .read_file(path, name, rendered.len() as u64)
                    .is_ok_and(|bytes| bytes == rendered.as_bytes())
                && git
                    .index_has_exact_blob(path, name, rendered.as_bytes())
                    .is_ok_and(|ok| ok)
        })
}
