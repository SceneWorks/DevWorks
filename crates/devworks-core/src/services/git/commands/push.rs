//! Resolve push destination before reserving it, then use an explicit server-side lease.
use super::*;
use crate::protocol::data::PushMode;

pub(super) struct StagedPush(Result<Destination, Failure>);
struct Destination {
    tree: StoredGitWorktree,
    repository_revision: i64,
    url: String,
    observed: crate::services::indexing::git::RevisionObservation,
    transport_digest: String,
}

/// A Git remote transport may execute a helper. Accept only explicit supported transports
/// and literal local paths registered by the host; never ext::, option syntax or credentials.
fn transport(url: &str) -> bool {
    if url.is_empty() || url.starts_with('-') || url.chars().any(char::is_control) {
        return false;
    }
    if Path::new(url).is_absolute() {
        return true;
    }
    if let Some(path) = url.strip_prefix("git@github.com:") {
        return path.split('/').count() == 2
            && path
                .split('/')
                .all(|p| !p.is_empty() && p != "." && p != "..");
    }
    let Ok(url) = url::Url::parse(url) else {
        return false;
    };
    matches!(url.scheme(), "https" | "ssh" | "file")
        && url.password().is_none()
        && (url.username().is_empty() || (url.scheme() == "ssh" && url.username() == "git"))
        && url.query().is_none()
        && url.fragment().is_none()
}
fn local_server(url: &str) -> bool {
    Path::new(url).is_absolute()
        || matches!(url::Url::parse(url),Ok(parsed) if parsed.scheme()=="file")
}
fn server_review(url: &str) -> Value {
    let local = local_server(url);
    json!({"kind":if local{"LocalGitServer"}else{"RemoteGitServer"},"authority":"TrustedByThisExactPushApproval","processing":"Destination server may run its own hooks and receive-pack configuration","serverHooksConfined":false,"clientPolicy":"IsolatedGitDirectoryWithTrustedHostAuthenticationOnly"})
}
impl GitService {
    pub(super) fn prepare_push(
        &self,
        ctx: &InvocationContext,
        input: &GitCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        self.prepare_push_inner(ctx, input, env, false)
    }
    fn prepare_push_inner(
        &self,
        ctx: &InvocationContext,
        input: &GitCommand,
        env: &Preparation<'_>,
        preview: bool,
    ) -> Result<Option<Prepared>, StorageError> {
        let GitCommand::Push {
            worktree_id,
            remote_id,
            expected_head,
            ..
        } = input
        else {
            return Ok(None);
        };
        let destination = (|| {
            let (tree, repository) = env.read(|s| {
                let tree = s
                    .git()
                    .git_worktree(worktree_id)?
                    .ok_or(StorageError::ScopeDenied)?;
                let target = self.target(s, ctx, &tree.repository_id, Some(worktree_id));
                Ok(target.map(|target| (tree, target.repository)))
            })??;
            let stopped = || env.stopped(self.clock.now_ms().get());
            let git = Git {
                runner: &*self.helpers,
                ctx,
                program: &self.config.program,
                deadline: self.config.deadline,
                stop: &stopped,
            };
            self.verify_target(
                &git,
                &Target {
                    repository: repository.clone(),
                    worktree: Some(tree.clone()),
                },
            )
            .map_err(|e| read_failure(e, &repository.id))?;
            let observed = git
                .head_observation(Path::new(&tree.path))
                .map_err(|e| read_failure(e, &repository.id))?;
            if observed.head != expected_head.hex
                || observed.branch.as_deref() != Some(&tree.branch)
                || tree.head.format != expected_head.format
            {
                return Err(refuse(
                    DomainErrorCode::SourceChanged,
                    "git_push_source",
                    worktree_id,
                ));
            }
            let urls = git
                .push_urls(Path::new(&tree.path), remote_id.as_str())
                .map_err(|e| read_failure(e, &repository.id))?;
            let [url] = urls.as_slice() else {
                return Err(refuse(
                    DomainErrorCode::InvalidArgument,
                    "git_push_destinations",
                    remote_id,
                ));
            };
            // A remote name cannot substitute an unregistered repository, including a separate
            // pushurl. Explicit registration is necessary before it can receive source code.
            if !transport(url) || repository.remote_url.as_deref() != Some(url) {
                return Err(refuse(
                    DomainErrorCode::PermissionDenied,
                    "git_push_destination",
                    remote_id,
                ));
            }
            if matches!(
                input,
                GitCommand::Push {
                    mode: PushMode::ForceWithLease,
                    ..
                }
            ) && !Path::new(url).is_absolute()
                && !matches!(url::Url::parse(url),Ok(parsed) if parsed.scheme()=="file")
                && crate::services::git::github::Repository::from_remote(url).is_none()
            {
                return Err(refuse(
                    DomainErrorCode::UnsupportedTarget,
                    "git_force_remote_qualification",
                    remote_id,
                ));
            }
            if !preview
                && local_server(url)
                && ctx.principal_kind() != crate::context::PrincipalKind::Human
                && ctx.reviewed_operation().is_none()
            {
                return Err(Refusal::new(
                    DomainErrorCode::ApprovalRequired,
                    "git_local_server_review",
                    Some(remote_id.clone()),
                )
                .into());
            }
            let transport = self.push_transport(
                &git,
                Path::new(&tree.path),
                &tree.head.format,
                &repository.id,
            )?;
            Ok(Destination {
                transport_digest: transport.digest.clone(),
                tree,
                repository_revision: repository.revision,
                url: url.clone(),
                observed,
            })
        })();
        Ok(Some(Prepared::new(StagedPush(destination))))
    }

    pub(super) fn decide_push(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
        input: &GitCommand,
    ) -> Result<(StoredGitWorktree, StoredRepository, StoredWork, Action), Failure> {
        let GitCommand::Push {
            worktree_id,
            remote_id,
            target_ref,
            expected_head,
            expected_remote,
            mode,
        } = input
        else {
            unreachable!("selected push")
        };
        let (mut tree, repository, work) = self.owned_command(tx, ctx, worktree_id)?;
        if ctx.principal_kind() != crate::context::PrincipalKind::Human
            && ctx.reviewed_operation().is_none()
            && repository.remote_url.as_deref().is_some_and(local_server)
        {
            return Err(Refusal::new(
                DomainErrorCode::ApprovalRequired,
                "git_local_server_review",
                Some(remote_id.clone()),
            )
            .into());
        }
        let staged = command
            .prepared::<StagedPush>()
            .ok_or_else(|| refuse(DomainErrorCode::Conflict, "git_push_destination", remote_id))?;
        let destination = match &staged.0 {
            Ok(d) => d,
            Err(Failure::Refused(r)) => return Err(r.clone().into()),
            Err(_) => {
                return Err(refuse(
                    DomainErrorCode::ExternalFailure,
                    "git_push_destination",
                    remote_id,
                ));
            }
        };
        if let Some(evidence) = ctx.reviewed_operation() {
            let current = json!({"tree":destination.tree,"repositoryRevision":destination.repository_revision,
                "url":destination.url,"head":destination.observed.head,"branch":destination.observed.branch,"transportDigest":destination.transport_digest,"destinationServer":server_review(&destination.url)});
            if evidence.owner_binding()["native"] != current {
                return Err(refuse(
                    DomainErrorCode::SourceChanged,
                    "git_push_destination",
                    remote_id,
                ));
            }
        }
        if destination.tree != tree
            || destination.repository_revision != repository.revision
            || target_ref != &tree.branch
            || destination.observed.head != expected_head.hex
        {
            return Err(refuse(
                DomainErrorCode::SourceChanged,
                "git_push_binding",
                worktree_id,
            ));
        }
        let force = matches!(mode, PushMode::ForceWithLease);
        if force
            && (expected_remote.is_none()
                || matches!(
                    target_ref.to_ascii_lowercase().as_str(),
                    "refs/heads/main" | "refs/heads/master"
                ))
        {
            return Err(refuse(
                DomainErrorCode::PermissionDenied,
                "protected_branch",
                worktree_id,
            ));
        }
        tree.head = expected_head.clone();
        Ok((
            tree,
            repository,
            work,
            Action::Push {
                transport_digest: Some(destination.transport_digest.clone()),
                remote: remote_id.as_str().into(),
                url: destination.url.clone(),
                target: target_ref.clone(),
                expected: expected_head.clone(),
                remote_head: expected_remote.clone(),
                force,
            },
        ))
    }

    pub(super) fn execute_push(
        &self,
        job: &Job,
        operation: &OperationRecord,
        git: &Git<'_>,
        path: &Path,
    ) -> EffectOutcome {
        let Action::Push {
            remote,
            url,
            target,
            expected,
            remote_head,
            force,
            transport_digest,
        } = &job.action
        else {
            unreachable!("push effect")
        };
        if !transport(url) || target != &job.tree.branch {
            return denied("push destination binding changed");
        }
        match git.push_urls(path, remote) {
            Ok(urls) if urls.as_slice() == std::slice::from_ref(url) => {}
            _ => return denied("configured push destination changed"),
        }
        if git.literal_push_url(path, url).as_ref() != Ok(url) {
            return denied("literal push URL is rewritten or cannot be bound");
        }
        let transport =
            match self.push_transport(git, path, &expected.format, &job.tree.repository_id) {
                Ok(transport) if transport_digest.as_ref() == Some(&transport.digest) => transport,
                _ => {
                    return denied(
                        "trusted push transport configuration changed or is unavailable",
                    );
                }
            };
        let network = Git {
            runner: &transport,
            ctx: git.ctx,
            program: git.program,
            deadline: git.deadline,
            stop: git.stop,
        };
        match network.remote_symbolic_target(path, url, target) {
            Ok(None) => {}
            Ok(Some(_)) => {
                return denied("push destination is a symbolic ref, not the owned branch");
            }
            Err(_) => return changed("remote symbolic-ref state cannot be established"),
        }
        let observation = match git.reviewed_revision_observation(path) {
            Ok(o) if o.head == expected.hex && o.branch.as_deref() == Some(target) => o,
            _ => return changed("local push source changed"),
        };
        match network.remote_default(path, url) {
            Ok(Some(default)) if &default == target => {
                return denied("the remote default branch is protected");
            }
            Ok(None) if *force => {
                return denied("remote default branch could not be resolved for force push");
            }
            Err(_) => return denied("remote default branch is unavailable"),
            _ => {}
        }
        let observed = match network.remote_head(path, url, target) {
            Ok(head) => head,
            Err(_) => return denied("remote head is unavailable"),
        };
        if observed.as_deref() != remote_head.as_ref().map(|h| h.hex.as_str()) {
            return changed("remote lease is stale");
        }
        // The lease also guards a normal push against races. It may authorize a rewrite at
        // Git's transport layer, so prove ancestry independently before using it in Normal.
        if !force
            && let Some(base) = &observed
            && git.is_ancestor(path, base, &expected.hex) != Ok(true)
        {
            return changed("normal push is not a verified fast forward");
        }
        let lease = format!(
            "--force-with-lease={target}:{}",
            observed.as_deref().unwrap_or("")
        );
        let refspec = format!("{}:{target}", expected.hex);
        let sent = network.mutation(
            path,
            &[
                "push",
                "--porcelain",
                "--no-follow-tags",
                "--recurse-submodules=no",
                &lease,
                "--",
                url,
                &refspec,
            ],
            None,
            &operation.id,
            true,
        );
        // A timeout/nonzero status does not establish failure. Observe the remote ref even
        // after transport loss; never repeat the guarded write as recovery.
        match network.remote_head(path, url, target) {
            Ok(Some(head)) if head == expected.hex => {
                self.finish_git(job, &operation.id, observation)
            }
            _ if sent.is_err() => {
                EffectOutcome::Unknown("push response and remote outcome are unverified".into())
            }
            _ => EffectOutcome::Unknown("remote push postcondition was not observed".into()),
        }
    }
}

impl GitService {
    /// The ordinary push preparation is read-only and already verifies physical source and
    /// registered destination. Review exposes those exact facts, never a guessed remote name.
    pub(crate) fn approval_push_preflight(
        &self,
        ctx: &InvocationContext,
        input: &GitCommand,
        env: &Preparation<'_>,
    ) -> Result<Value, Failure> {
        let prepared = self
            .prepare_push_inner(ctx, input, env, true)?
            .ok_or_else(|| Refusal::new(DomainErrorCode::InvalidArgument, "git_push", None))?;
        let staged = prepared
            .get::<StagedPush>()
            .ok_or_else(|| Refusal::new(DomainErrorCode::InvalidArgument, "git_push", None))?;
        match &staged.0 {
            Ok(d) => Ok(
                json!({"tree":d.tree,"repositoryRevision":d.repository_revision,"url":d.url,
                "head":d.observed.head,"branch":d.observed.branch,"transportDigest":d.transport_digest,"destinationServer":server_review(&d.url)}),
            ),
            Err(Failure::Refused(r)) => Err(r.clone().into()),
            Err(_) => {
                Err(
                    Refusal::new(DomainErrorCode::ExternalFailure, "git_push_preflight", None)
                        .into(),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::transport;
    #[test]
    fn push_transport_refuses_executable_helpers_and_embedded_credentials() {
        for url in [
            "ext::sh -c bad",
            "-x",
            "https://user:secret@github.com/o/r",
            "https://github.com/o/r?token=secret",
            "git@github.com:../r",
            "ssh://user:secret@host/r",
        ] {
            assert!(!transport(url), "{url}");
        }
        for url in [
            "https://github.com/o/r.git",
            "git@github.com:o/r.git",
            "ssh://git@github.com/o/r.git",
            "file:///tmp/registered.git",
        ] {
            assert!(transport(url), "{url}");
        }
    }
}
