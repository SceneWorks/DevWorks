//! Fence checks that include the fences a service owns. Authority evaluates its own fences and
//! asks [`Owners`] about the rest; every caller outside the gateway, authority and kernel checks
//! fences here.
use crate::authority::{ServiceFences, authorization_holds_in_with, authorization_holds_with};
use crate::context::InvocationContext;
use crate::ports::storage::{ReadSnapshot, StorageError, WriteTransaction};
use crate::ports::{AuthorityFence, Authorization};

/// The services that own the document index, document vision, dreaming sweep and qualified
/// embedding fences.
pub struct Owners;

impl ServiceFences for Owners {
    fn holds_in(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: Option<&InvocationContext>,
        fence: &AuthorityFence,
    ) -> Result<bool, StorageError> {
        Ok(match fence {
            AuthorityFence::DocumentIndex { run, generation } => {
                super::documents::indexing::embedding_source_in(tx, run, *generation)?.is_some()
            }
            AuthorityFence::DreamingSweep {
                run,
                generation,
                request,
            } => super::dreaming::inference_fence_in(tx, run, *generation, request)?,
            AuthorityFence::QualifiedEmbedding { source_digest } => {
                let Some(ctx) = ctx else { return Ok(false) };
                let current = super::router::embedding_spaces::resolve_selected_in(tx, ctx)?;
                current.binding().is_some() && current.source_digest() == source_digest
            }
            AuthorityFence::DocumentVision { settings_witness } => {
                let current = super::documents::vision::policy(tx.configuration())?;
                current.enabled && current.witness == *settings_witness
            }
            _ => false,
        })
    }

    fn holds(&self, s: &dyn ReadSnapshot, fence: &AuthorityFence) -> Result<bool, StorageError> {
        Ok(match fence {
            AuthorityFence::DocumentIndex { run, generation } => {
                super::documents::indexing::embedding_fence_read(s, run, *generation)?
            }
            AuthorityFence::DocumentVision { settings_witness } => {
                let current = super::documents::vision::policy(s.configuration())?;
                current.enabled && current.witness == *settings_witness
            }
            _ => false,
        })
    }
}

/// Re-checks every fence inside the write transaction (the commit linearization point).
pub fn fences_hold(
    tx: &mut dyn WriteTransaction,
    fences: &[AuthorityFence],
) -> Result<bool, StorageError> {
    crate::authority::fences_hold_with(tx, None, fences, &Owners)
}

/// Checks the exact qualified embedding choice in the real native invocation context.
pub fn inference_fences_hold(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    fences: &[AuthorityFence],
) -> Result<bool, StorageError> {
    crate::authority::fences_hold_with(tx, Some(ctx), fences, &Owners)
}

/// [`authorization_holds`] inside a write transaction, together with the invocation's store and
/// restore epoch.
pub fn authorization_holds_in(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    authorization: &Authorization,
) -> Result<bool, StorageError> {
    authorization_holds_in_with(tx, ctx, authorization, &Owners)
}

/// Re-checks fences and the grants an authorization relies on against a fresh snapshot.
pub fn authorization_holds(
    s: &dyn ReadSnapshot,
    authorization: &Authorization,
) -> Result<bool, StorageError> {
    authorization_holds_with(s, authorization, &Owners)
}
