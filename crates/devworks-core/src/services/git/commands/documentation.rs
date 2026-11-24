//! Repository documentation belongs to the same owned-worktree journal as Git writes.
use super::*;
use crate::operations::repository_docs::GenerateInput;
use crate::services::graph::repository_analysis::{DocumentationDraft, synthesis_request};
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
            let analysis = self.repository_analysis.get().ok_or_else(|| {
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
                .ok_or(StorageError::ScopeDenied)?;
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
            let paths: Vec<_> = entries
                .into_iter()
                .filter(|entry| !entry.link() && !entry.submodule())
                .map(|entry| entry.path)
                .collect();
            let draft = analysis.prepare_documentation(ctx, request, &paths, run_id)?;
            if draft.evidence.view.snapshot.head != tree.head {
                return Err(refuse(
                    DomainErrorCode::SourceChanged,
                    "documentation_source",
                    &tree.id,
                ));
            }
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
        let (ctx, input, run_id) = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            Ok(synthesis_request(read, entry, event))
        })??;
        let mut body = serde_json::to_value(&input)
            .map_err(|error| StorageError::InvalidRecord(error.to_string()))?;
        body["kind"] = json!("repository_docs.generate");
        let frame = serde_json::to_vec(&json!({
            "mode": "Command", "protocol": "1.0-draft.1",
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
        let current = tx.queue().entry(&self.delivery.id)?;
        if !current.is_some_and(|entry| {
            entry.state == QueueState::Claimed
                && entry.fence() == self.delivery.fence()
                && entry.cancellation_epoch == self.delivery.cancellation_epoch
                && !entry.cancel_requested
                && entry.claim_boot_id == tx.store().boot_id
                && entry
                    .lease_until_ms
                    .is_some_and(|expires| expires > tx.now_ms())
        }) {
            return Err(StorageError::Cancelled.into());
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
    path: &Path,
    job: &Job,
    operation: &OperationId,
    draft: &DocumentationDraft,
) -> Result<(), GitFault> {
    if git.head(path)?.as_deref() != Some(job.tree.head.hex.as_str())
        || git.branch(path)?.as_deref() != Some(job.tree.branch.as_str())
    {
        return Err(GitFault::SourceChanged);
    }
    let (tracked, _) = git.tree(path, &job.tree.head.hex)?;
    let mut patch = String::new();
    for (name, rendered) in &draft.files {
        if !owned_doc(name) || rendered.len() > MAX_DOC_BYTES {
            return Err(GitFault::SourceChanged);
        }
        if git.index_has_exact_file(path, name, rendered.as_bytes())? {
            continue;
        }
        let old = match tracked.iter().find(|entry| &entry.path == name) {
            Some(entry) if entry.mode == "100644" => {
                let bytes = git.file_at(path, &job.tree.head.hex, name, MAX_DOC_BYTES)?;
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
            None => None,
        };
        if old.as_deref() == Some(rendered.as_str()) {
            return Err(GitFault::SourceChanged);
        }
        append_patch(&mut patch, name, old.as_deref(), rendered);
    }
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

pub(super) fn observed(git: &Git<'_>, path: &Path, draft: &DocumentationDraft) -> bool {
    !draft.files.is_empty()
        && draft.files.iter().all(|(name, rendered)| {
            owned_doc(name)
                && git
                    .index_has_exact_file(path, name, rendered.as_bytes())
                    .is_ok_and(|ok| ok)
        })
}
