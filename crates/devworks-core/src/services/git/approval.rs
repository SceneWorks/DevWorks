//! Exact native Git review over current owned work and read-only physical observations.
use super::*;
use crate::admission::DecodedRequest;
use crate::ports::Preparation;
use crate::ports::storage::*;
use crate::protocol::data::{GitCommand, PushMode};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::GitActionKind;
use crate::services::approvals::OwnerPreview;
use crate::services::approvals::owner_preview::artifact_binding;
use serde_json::{Value, json};
use std::io::Read;

fn requested(request: &DecodedRequest) -> Result<(&Id, Option<&Id>, Option<&Id>), Failure> {
    match &request.input {
        OperationInput::Git(ops::Input::Worktree(i)) => {
            Ok((&i.repository_id, Some(&i.work_id), i.worktree_id.as_ref()))
        }
        _ => Err(Refusal::new(
            DomainErrorCode::UnsupportedCapability,
            "saved_git_operation",
            None,
        )
        .into()),
    }
}
fn repository_value(r: &StoredRepository) -> Value {
    json!({"id":r.id,"revision":r.revision,"location":r.location,"remoteUrl":r.remote_url,
        "primary":r.primary,"lifecycle":r.lifecycle})
}
fn work_value(w: &StoredWork) -> Value {
    json!({"id":w.id,"projectId":w.project_id,"revision":w.revision,"scopeRevision":w.scope_revision,
        "parentFeatureId":w.parent_feature_id,"kind":w.kind,"brief":w.brief})
}
fn git_binding(
    repository: &StoredRepository,
    work: &StoredWork,
    tree: Option<&StoredGitWorktree>,
    project: Value,
    assignment: Value,
) -> Value {
    json!({"repository":repository_value(repository),"work":work_value(work),"tree":tree,"project":project,"assignment":assignment})
}
fn project_value(project: Option<StoredProject>) -> Result<Value, Failure> {
    let p = project.ok_or_else(|| Refusal::new(DomainErrorCode::NotFound, "project", None))?;
    Ok(
        json!({"id":p.id,"revision":p.revision,"scopeRevision":p.scope_revision,"lifecycle":p.lifecycle,"archiveEpoch":p.archive_epoch}),
    )
}
fn assignment_value(assignment: Option<StoredAssignment>) -> Value {
    assignment.map_or(Value::Null,|a|json!({"id":a.id,"revision":a.revision,"generation":a.generation,
        "rootId":a.root_id,"workId":a.work_id,"sessionId":a.session_id,"state":a.state.as_str(),"executionId":a.execution_id}))
}
impl GitService {
    /// Shared ordinary commit-message preparation and approval review; immutable artifact bytes
    /// are read with the artifact owner's exact-version reader and the same bounded text rules.
    pub(super) fn commit_message_text(
        &self,
        ctx: &InvocationContext,
        message: &ArtifactRef,
    ) -> Result<String, Failure> {
        let mut version = self
            .artifacts
            .open_exact_version(&*self.store, ctx, message)
            .map_err(|e| Refusal::new(e.code, "git_message", Some(message.version_id.clone())))?;
        let mut bytes = Vec::new();
        version
            .reader
            .by_ref()
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| StorageError::unavailable(Unavailable::Io, e.to_string()))?;
        if bytes.len() > 64 * 1024 || bytes.contains(&0) {
            return Err(Refusal::new(
                DomainErrorCode::InvalidArgument,
                "git_message",
                Some(message.version_id.clone()),
            )
            .into());
        }
        String::from_utf8(bytes)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                Refusal::new(
                    DomainErrorCode::InvalidArgument,
                    "git_message",
                    Some(message.version_id.clone()),
                )
                .into()
            })
    }
    pub(crate) fn approval_preview(
        &self,
        ctx: &InvocationContext,
        request: &DecodedRequest,
        env: &Preparation<'_>,
    ) -> Result<OwnerPreview, Failure> {
        let (repository, work, tree, binding) = env.read(|snapshot| {
            Ok((|| -> Result<_, Failure> {
                let (repository_id, work_id, tree_id) = match &request.input {
                    OperationInput::Git(ops::Input::Commit(i) | ops::Input::Push(i)) => {
                        let tree_id = match &i.command {
                            GitCommand::Commit { worktree_id, .. }
                            | GitCommand::Push { worktree_id, .. } => worktree_id,
                            _ => {
                                return Err(Refusal::new(
                                    DomainErrorCode::InvalidArgument,
                                    "git_command",
                                    None,
                                )
                                .into());
                            }
                        };
                        let tree = snapshot.git().git_worktree(tree_id)?.ok_or_else(|| {
                            Refusal::new(
                                DomainErrorCode::NotFound,
                                "worktree",
                                Some(tree_id.clone()),
                            )
                        })?;
                        (tree.repository_id, tree.work_id, Some(tree_id.clone()))
                    }
                    _ => {
                        let (repo, work, tree) = requested(request)?;
                        (
                            repo.clone(),
                            work.expect("worktree work").clone(),
                            tree.cloned(),
                        )
                    }
                };
                let target = self.target(snapshot, ctx, &repository_id, tree_id.as_ref())?;
                let work = snapshot.work().work(&work_id)?.ok_or_else(|| {
                    Refusal::new(DomainErrorCode::NotFound, "work", Some(work_id.clone()))
                })?;
                let project = project_value(snapshot.projects().project(&work.project_id)?)?;
                let assignment = assignment_value(
                    ctx.assignment_id()
                        .map(|id| snapshot.runs().assignment(id))
                        .transpose()?
                        .flatten(),
                );
                let binding = git_binding(
                    &target.repository,
                    &work,
                    target.worktree.as_ref(),
                    project,
                    assignment,
                );
                Ok((target.repository, work, target.worktree, binding))
            })())
        })??;
        let target = Target {
            repository: repository.clone(),
            worktree: tree.clone(),
        };
        let stopped = || env.stopped(self.clock.now_ms().get());
        let git = Git {
            runner: &*self.helpers,
            ctx,
            program: &self.config.program,
            deadline: self.config.deadline,
            stop: &stopped,
        };
        self.verify_target(&git, &target)
            .map_err(|e| read_failure(e, &repository.id))?;
        let physical = git
            .revision_observation(target.path())
            .map_err(|e| read_failure(e, &repository.id))?;
        let (native, artifact, contents) = match &request.input {
            OperationInput::Git(ops::Input::Push(i)) => (
                self.approval_push_preflight(ctx, &i.command, env)?,
                None,
                Value::Null,
            ),
            OperationInput::Git(ops::Input::Commit(i)) => {
                let GitCommand::Commit {
                    expected_head,
                    message,
                    ..
                } = &i.command
                else {
                    unreachable!("selected commit")
                };
                if physical.head != expected_head.hex
                    || physical.branch.as_deref() != tree.as_ref().map(|t| t.branch.as_str())
                {
                    return Err(Refusal::new(
                        DomainErrorCode::SourceChanged,
                        "git_commit_source",
                        tree.as_ref().map(|t| t.id.clone()),
                    )
                    .into());
                }
                let text = self.commit_message_text(ctx, message)?;
                let manifest = self.commit_source_manifest(ctx, &i.command, env)?;
                let artifact = env.read(|s| Ok(artifact_binding(s.artifacts(), message)))??;
                (json!({"message":text}), Some(artifact), manifest)
            }
            OperationInput::Git(ops::Input::Worktree(i)) => {
                let root = self.approval_root_witness(&i.registered_root_id)?;
                (
                    json!({"registeredRootId":i.registered_root_id,"rootWitness":root,"base":i.base,"action":i.action}),
                    None,
                    Value::Null,
                )
            }
            _ => {
                return Err(Refusal::new(
                    DomainErrorCode::UnsupportedCapability,
                    "saved_git_operation",
                    None,
                )
                .into());
            }
        };
        let (contents, commit_evidence) = if contents.is_null() {
            (contents, None)
        } else {
            let evidence = self.commit_review_evidence(ctx, request, env, &contents)?;
            (compact_commit_manifest(&contents)?, Some(evidence))
        };
        let mut binding = binding;
        binding["commitEvidence"] = match &commit_evidence {
            Some(reference) => env.read(|s| Ok(artifact_binding(s.artifacts(), reference)))??,
            None => Value::Null,
        };
        binding["native"] = native.clone();
        binding["artifact"] = json!(artifact);
        binding["commitManifest"] = contents.clone();
        let (target_id, target_revision) = tree.as_ref().map_or_else(
            || (work.id.clone(), work.revision),
            |t| (t.id.clone(), t.revision),
        );
        let mut preview = OwnerPreview {
            scope: ConfigScope::Project(work.project_id),
            target_id,
            target_revision,
            summary: format!("Review {} for {}", request.descriptor.name, repository.name),
            reviewed: json!({"input":request.normalized,"repository":repository_value(&repository),"worktree":tree,
                "currentHead":physical.head,"currentBranch":physical.branch,"dirty":physical.dirty,"native":native,"commitManifest":contents,"artifact":commit_evidence,"commitEvidence":commit_evidence}),
            binding,
        };
        if preview.reviewed["artifact"].is_null() {
            preview
                .reviewed
                .as_object_mut()
                .expect("owner preview")
                .remove("artifact");
        }
        Ok(preview)
    }

    pub(crate) fn validate_approval_preview(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        request: &DecodedRequest,
        preview: &OwnerPreview,
    ) -> Result<(), Failure> {
        let (repository, work, tree) = match &request.input {
            OperationInput::Git(ops::Input::Commit(i) | ops::Input::Push(i)) => {
                let (id, head) = match &i.command {
                    GitCommand::Commit {
                        worktree_id,
                        expected_head,
                        ..
                    }
                    | GitCommand::Push {
                        worktree_id,
                        expected_head,
                        ..
                    } => (worktree_id, expected_head),
                    _ => {
                        return Err(Refusal::new(
                            DomainErrorCode::InvalidArgument,
                            "git_command",
                            None,
                        )
                        .into());
                    }
                };
                let (tree, repository, work) = self.owned_command(tx, ctx, id)?;
                if tree.head.format != head.format
                    || (matches!(request.input, OperationInput::Git(ops::Input::Commit(_)))
                        && tree.head != *head)
                {
                    return Err(refuse(DomainErrorCode::SourceChanged, "git_head", id));
                }
                if let GitCommand::Push {
                    target_ref,
                    mode,
                    expected_remote,
                    ..
                } = &i.command
                {
                    if *target_ref != tree.branch {
                        return Err(refuse(
                            DomainErrorCode::SourceChanged,
                            "git_push_binding",
                            id,
                        ));
                    }
                    if matches!(mode, PushMode::ForceWithLease)
                        && (expected_remote.is_none()
                            || matches!(
                                target_ref.to_ascii_lowercase().as_str(),
                                "refs/heads/main" | "refs/heads/master"
                            ))
                    {
                        return Err(refuse(
                            DomainErrorCode::PermissionDenied,
                            "protected_branch",
                            id,
                        ));
                    }
                }
                (repository, work, Some(tree))
            }
            OperationInput::Git(ops::Input::Worktree(i)) => {
                let work = tx.work().work(&i.work_id)?.ok_or_else(|| {
                    Refusal::new(DomainErrorCode::NotFound, "work", Some(i.work_id.clone()))
                })?;
                let repository = tx.projects().repository(&i.repository_id)?.ok_or_else(|| {
                    Refusal::new(
                        DomainErrorCode::NotFound,
                        "repository",
                        Some(i.repository_id.clone()),
                    )
                })?;
                self.check_work(tx, ctx, &work, &repository)?;
                if work.revision != i.expected_revision.get() {
                    return Err(refuse(DomainErrorCode::Conflict, "work_revision", &work.id));
                }
                let tree = match i.action {
                    GitActionKind::Create => {
                        self.validate_new_worktree(tx, &work, &repository, i)?;
                        None
                    }
                    GitActionKind::Attach => Some(self.approval_attach_binding(
                        tx,
                        ctx,
                        i.worktree_id.as_ref().ok_or_else(|| {
                            refuse(DomainErrorCode::InvalidArgument, "worktree", &work.id)
                        })?,
                    )?),
                    GitActionKind::Inspect => Some(
                        self.owned_command(
                            tx,
                            ctx,
                            i.worktree_id.as_ref().ok_or_else(|| {
                                refuse(DomainErrorCode::InvalidArgument, "worktree", &work.id)
                            })?,
                        )?
                        .0,
                    ),
                };
                if tree.as_ref().is_some_and(|tree| {
                    tree.work_id != work.id
                        || tree.registered_root_id != i.registered_root_id
                        || tree.head != i.base
                }) {
                    return Err(refuse(
                        DomainErrorCode::Conflict,
                        "git_ownership",
                        tree.as_ref().map(|t| &t.id).expect("existing tree"),
                    ));
                }
                (repository, work, tree)
            }
            _ => {
                return Err(Refusal::new(
                    DomainErrorCode::UnsupportedCapability,
                    "saved_git_operation",
                    None,
                )
                .into());
            }
        };
        if let OperationInput::Git(ops::Input::Worktree(i)) = &request.input
            && self.approval_root_witness(&i.registered_root_id)?
                != preview.binding["native"]["rootWitness"]
        {
            return Err(refuse(
                DomainErrorCode::SourceChanged,
                "git_root",
                &i.registered_root_id,
            ));
        }
        let project = project_value(tx.projects().project(&work.project_id)?)?;
        let assignment = assignment_value(
            ctx.assignment_id()
                .map(|id| tx.runs().assignment(id))
                .transpose()?
                .flatten(),
        );
        let current = git_binding(&repository, &work, tree.as_ref(), project, assignment);
        for key in ["repository", "work", "tree", "project", "assignment"] {
            if current[key] != preview.binding[key] {
                return Err(Refusal::new(
                    DomainErrorCode::SourceChanged,
                    "saved_git_operation",
                    Some(preview.target_id.clone()),
                )
                .into());
            }
        }
        if let Some(reference) = preview.binding.pointer("/commitEvidence/reference") {
            let reference: ArtifactRef = serde_json::from_value(reference.clone())
                .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
            if artifact_binding(tx.artifacts(), &reference)? != preview.binding["commitEvidence"] {
                return Err(refuse(
                    DomainErrorCode::SourceChanged,
                    "commit_review_evidence",
                    &reference.version_id,
                ));
            }
        }
        if let OperationInput::Git(ops::Input::Commit(i)) = &request.input {
            let GitCommand::Commit { message, .. } = &i.command else {
                unreachable!("bound commit")
            };
            if artifact_binding(tx.artifacts(), message)? != preview.binding["artifact"] {
                return Err(refuse(
                    DomainErrorCode::SourceChanged,
                    "git_message",
                    &message.version_id,
                ));
            }
        }
        Ok(())
    }
}

impl GitService {
    pub(super) fn approval_root_witness(&self, id: &Id) -> Result<Value, Failure> {
        let path = self.root_path(id)?;
        let resolved = self
            .config
            .filesystem
            .as_ref()
            .ok_or_else(|| refuse(DomainErrorCode::UnsupportedTarget, "git_filesystem", id))?
            .resolve(
                path.to_str()
                    .ok_or_else(|| refuse(DomainErrorCode::UnsupportedTarget, "git_root", id))?,
                None,
            )
            .map_err(|_| refuse(DomainErrorCode::SourceChanged, "git_root", id))?;
        Ok(json!({"path":path,"identity":resolved.target.and_then(|t|t.identity)}))
    }
    pub(super) fn validate_reviewed_git_command(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        command: &crate::gateway::AdmittedCommand,
        binding: &Value,
    ) -> Result<(), Failure> {
        let input = match command.input() {
            OperationInput::Git(ops::Input::Worktree(i)) => {
                serde_json::to_value(i).map_err(|e| StorageError::InvalidRecord(e.to_string()))?
            }
            OperationInput::Git(ops::Input::Commit(i) | ops::Input::Push(i)) => {
                json!({"command":i.command})
            }
            _ => {
                return Err(Refusal::new(
                    DomainErrorCode::UnsupportedCapability,
                    "saved_git_operation",
                    None,
                )
                .into());
            }
        };
        let request = crate::services::approvals::requests::decode(
            command.descriptor().name,
            command.schema_revision().0,
            &input,
            true,
        )
        .map_err(|_| {
            Refusal::new(
                DomainErrorCode::InvalidArgument,
                "saved_git_operation",
                None,
            )
        })?;
        if request.fingerprint.as_str() != command.fingerprint().as_str() {
            return Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                "saved_git_operation",
                None,
            )
            .into());
        }
        let scope = binding
            .pointer("/work/projectId")
            .and_then(Value::as_str)
            .and_then(|s| Id::new(s).ok())
            .map(crate::protocol::ProjectId)
            .map(ConfigScope::Project)
            .ok_or_else(|| {
                Refusal::new(
                    DomainErrorCode::InvalidArgument,
                    "saved_git_operation",
                    None,
                )
            })?;
        let source = if binding["tree"].is_null() {
            &binding["work"]
        } else {
            &binding["tree"]
        };
        let target_id = source["id"]
            .as_str()
            .and_then(|s| Id::new(s).ok())
            .ok_or_else(|| {
                Refusal::new(
                    DomainErrorCode::InvalidArgument,
                    "saved_git_operation",
                    None,
                )
            })?;
        let target_revision = source["revision"].as_i64().ok_or_else(|| {
            Refusal::new(
                DomainErrorCode::InvalidArgument,
                "saved_git_operation",
                None,
            )
        })?;
        let preview = OwnerPreview {
            scope,
            target_id,
            target_revision,
            summary: String::new(),
            reviewed: Value::Null,
            binding: binding.clone(),
        };
        self.validate_approval_preview(tx, ctx, &request, &preview)
    }
    pub(super) fn commit_source_manifest(
        &self,
        ctx: &InvocationContext,
        input: &GitCommand,
        env: &Preparation<'_>,
    ) -> Result<Value, Failure> {
        let GitCommand::Commit {
            worktree_id,
            expected_head,
            ..
        } = input
        else {
            return Err(Refusal::new(DomainErrorCode::InvalidArgument, "git_commit", None).into());
        };
        let target = env.read(|s| {
            Ok((|| -> Result<_, Failure> {
                let tree = s.git().git_worktree(worktree_id)?.ok_or_else(|| {
                    Refusal::new(
                        DomainErrorCode::NotFound,
                        "worktree",
                        Some(worktree_id.clone()),
                    )
                })?;
                self.target(s, ctx, &tree.repository_id, Some(worktree_id))
            })())
        })??;
        let stopped = || env.stopped(self.clock.now_ms().get());
        let git = Git {
            runner: &*self.helpers,
            ctx,
            program: &self.config.program,
            deadline: self.config.deadline,
            stop: &stopped,
        };
        self.verify_target(&git, &target)
            .map_err(|e| read_failure(e, &target.repository.id))?;
        let manifest = git
            .commit_manifest(target.path(), &expected_head.hex)
            .map_err(|e| read_failure(e, &target.repository.id))?;
        if manifest["head"] != expected_head.hex
            || manifest["branch"].as_str() != target.worktree.as_ref().map(|t| t.branch.as_str())
        {
            return Err(refuse(
                DomainErrorCode::SourceChanged,
                "git_commit_source",
                worktree_id,
            ));
        }
        Ok(manifest)
    }
}

pub(super) fn compact_commit_manifest(manifest: &Value) -> Result<Value, Failure> {
    let bytes = crate::jcs::canonicalize(manifest)
        .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
    let files = manifest["files"]
        .as_array()
        .ok_or_else(|| Refusal::new(DomainErrorCode::InvalidArgument, "commit_manifest", None))?;
    Ok(
        json!({"head":manifest["head"],"branch":manifest["branch"],"sha256":crate::fingerprint::sha256_hex(bytes.as_bytes()),
        "byteLength":bytes.len(),"fileCount":files.len()}),
    )
}
impl GitService {
    fn commit_review_evidence(
        &self,
        ctx: &InvocationContext,
        request: &DecodedRequest,
        env: &Preparation<'_>,
        manifest: &Value,
    ) -> Result<ArtifactRef, Failure> {
        crate::services::approvals::owner_preview::publish_review_evidence(
            &self.artifacts,
            &*self.store,
            ctx,
            env,
            request,
            (
                "GitCommitApprovalEvidence",
                "Exact Git commit contents for human review",
            ),
            manifest,
        )
    }
}
