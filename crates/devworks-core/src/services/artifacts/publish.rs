//! The one publication protocol behind [`ArtifactService::publish_evidence`] and
//! [`ArtifactService::publish_observed`].
//!
//! The scope's file limit and storage limit are checked first, a staging operation is journaled
//! and the bytes are flushed to their content-addressed generation, then **one** write re-checks
//! the producer's authority (the [`Gate`]), records the artifact through the caller's `record`
//! function, checks the storage limit again with the new version in place, and runs the caller's
//! own records (`then`). That write is the publication's linearization point: everything it
//! records sits behind one savepoint, rolled back before a typed refusal is returned, so a
//! refused publication commits nothing and its staging operation is resolved at once (or at the
//! next recovery).
use super::observed::AUTHORITY_CHANGED;
use super::{
    ArtifactService, Failure, Refusal, StagedFile, check_quota, quota_refusal, setting_i64, usage,
};
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::ports::files::{ArtifactFiles, SourceOrigin};
use crate::ports::storage::{
    ApplicationStore, ArtifactScope, BlobState, StorageError, WriteOptions, WriteTransaction,
    run_write,
};
use crate::ports::{Authorization, Preparation};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::{ActorId, DomainErrorCode};
use crate::settings::catalog::{ARTIFACT_MAX_FILE_KEY, ARTIFACT_QUOTA_KEY};
use serde_json::{Value, json};

/// What the publishing write re-checks, inside the write, before it records anything.
pub(super) enum Gate {
    /// The invocation's authorization: store and restore epoch, every authority fence and every
    /// grant it relied on.
    Authorization(InvocationContext, Authorization),
    /// A producer named without an invocation: a principal revoked, re-bound or newly bound
    /// since the publication started publishes nothing. Principals without a binding (humans,
    /// the host) hold no revocable authority, and gaining one is a change too.
    Producer(ActorId),
    /// [`Gate::Producer`] with the binding revision captured when the publication started
    /// (`None`: no binding then). Built by [`ArtifactService::publish_bytes`].
    PinnedProducer(ActorId, Option<i64>),
}

impl Gate {
    fn holds(&self, tx: &mut dyn WriteTransaction) -> Result<bool, StorageError> {
        match self {
            Self::Authorization(ctx, authorization) => {
                crate::authority::authorization_holds_in(tx, ctx, authorization)
            }
            Self::Producer(actor) => Ok(tx
                .authority()
                .principal(actor)?
                .is_none_or(|binding| binding.active)),
            Self::PinnedProducer(actor, revision) => {
                let binding = tx.authority().principal(actor)?;
                Ok(binding.as_ref().map(|b| b.revision) == *revision
                    && binding.is_none_or(|b| b.active))
            }
        }
    }
}

/// What the recording write established, or why it did not.
enum Recorded<T> {
    Done(ArtifactRef, T),
    Refused(Refusal),
}

impl ArtifactService {
    /// Publishes `bytes` into `scope`: `record` writes the artifact for the staged file and
    /// `then` runs in the same write with the new version's reference.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn publish_bytes<T, F, R>(
        &self,
        store: &dyn ApplicationStore,
        scope: ArtifactScope,
        source: &Value,
        gate: Gate,
        bytes: &[u8],
        record: R,
        then: F,
    ) -> Result<(ArtifactRef, T), Failure>
    where
        T: Send + 'static,
        F: FnOnce(&mut dyn WriteTransaction, &ArtifactRef) -> Result<T, StorageError>
            + Send
            + 'static,
        R: FnOnce(
                &mut dyn WriteTransaction,
                &dyn ArtifactFiles,
                &StagedFile,
            ) -> Result<ArtifactRef, Failure>
            + Send
            + 'static,
    {
        let env = Preparation::new(store, None, None);
        let digest = sha256_hex(bytes);
        let n = bytes.len() as i64;
        let (checked, d) = (scope.clone(), digest.clone());
        let producer = match &gate {
            Gate::Producer(actor) | Gate::PinnedProducer(actor, _) => Some(actor.clone()),
            Gate::Authorization(..) => None,
        };
        let (max, quota, used, reuse, bound) = env.read(move |s| {
            let max = setting_i64(s.configuration(), None, ARTIFACT_MAX_FILE_KEY);
            let quota = setting_i64(s.configuration(), checked.project(), ARTIFACT_QUOTA_KEY);
            let reuse = s
                .artifacts()
                .blob(&d)?
                .filter(|b| matches!(b.state, BlobState::Present | BlobState::UnlinkPending))
                .map(|b| b.generation);
            let bound = match &producer {
                Some(actor) => Some(s.authority().principal(actor)?.map(|b| b.revision)),
                None => None,
            };
            Ok((max, quota, usage(s.artifacts(), &checked)?, reuse, bound))
        })?;
        let gate = match (gate, bound) {
            (Gate::Producer(actor), Some(revision)) => Gate::PinnedProducer(actor, revision),
            (gate, _) => gate,
        };
        let (max, quota) = (max?, quota?);
        if n > max {
            return Err(Refusal::new(
                DomainErrorCode::InvalidArgument,
                "artifact_file_limit",
                None,
            )
            .into());
        }
        if quota > 0 && reuse.is_none() && used.saturating_add(n) > quota {
            return Err(quota_refusal(&scope).into());
        }
        let payload = json!({
            "digest": digest, "byteCount": n,
            "projectId": scope.project().map(|p| p.0.as_str()),
            "source": source,
        });
        let mut reader = bytes;
        let (staging, published) =
            self.journal_and_publish(&env, &mut reader, &digest, n, reuse.as_deref(), payload)?;
        let published = match published {
            Ok(p) => p,
            Err(fault) => {
                self.abandon_or_defer(&env, &staging, json!({"failed": format!("{fault:?}")}));
                return Err(super::file_failure(fault).into());
            }
        };
        let staged = StagedFile {
            operation_id: staging.operation_id.clone(),
            digest,
            byte_count: n,
            generation: published.generation,
            reused: published.reused,
            relative_path: published.relative_path,
            origin: SourceOrigin::default(),
            source: source.clone(),
            fence: staging.fence.clone(),
        };
        let files = self.files.clone();
        let owned = staged.clone();
        let outcome = run_write(store, WriteOptions::observed(), move |tx| {
            let savepoint = tx.savepoint()?;
            // The producer's authority is re-checked here, at the write's linearization point: a
            // principal revoked or re-bound, or a policy narrowed, while the content was being
            // produced publishes nothing.
            let recorded = if gate.holds(tx)? {
                record(tx, &*files, &owned).and_then(|reference| {
                    // The storage limit is checked again with the new version in place:
                    // concurrent publications that each passed the check above cannot together
                    // exceed it.
                    check_quota(tx, &scope).map(|()| reference)
                })
            } else {
                Err(Refusal::new(DomainErrorCode::PermissionDenied, AUTHORITY_CHANGED, None).into())
            };
            match recorded {
                Ok(reference) => {
                    let value = then(tx, &reference)?;
                    tx.release(savepoint)?;
                    Ok(Recorded::Done(reference, value))
                }
                Err(Failure::Refused(r)) => {
                    tx.rollback_to(savepoint)?;
                    Ok(Recorded::Refused(r))
                }
                Err(Failure::Storage(e)) => Err(e),
            }
        });
        match outcome {
            Ok(Recorded::Done(reference, value)) => {
                // A concurrent save kept its own generation: ours is an orphan of a now resolved
                // operation that nothing can bind any more.
                if !staged.reused {
                    let own = staged.operation_id.0.as_str().to_owned();
                    let d = staged.digest.clone();
                    let bound = env.read(move |s| {
                        Ok(s.artifacts().blob(&d)?.is_some_and(|b| b.generation == own))
                    });
                    if matches!(bound, Ok(false)) {
                        let _ = self
                            .files
                            .unlink(&staged.digest, staged.operation_id.0.as_str());
                    }
                }
                Ok((reference, value))
            }
            Ok(Recorded::Refused(r)) => {
                self.abandon_or_defer(&env, &staging, json!({"refused": r.target_kind}));
                Err(Failure::Refused(r))
            }
            Err(e) => {
                // An unknown commit outcome is resolved the same way: the resolution re-reads the
                // durable record under the fence and completes the operation if it committed.
                self.abandon_or_defer(&env, &staging, json!({"abandoned": format!("{e:?}")}));
                Err(Failure::Storage(e))
            }
        }
    }
}
