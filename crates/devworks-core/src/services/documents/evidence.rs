//! Exact document sources owned by a tool result. History keeps the original citation,
//! while access to the saved result continues to follow the retained source's current audience.
use super::retrieval;
use crate::{
    context::InvocationContext,
    ports::storage::{ArtifactRead, ArtifactScope, StorageError, StoredSession, WriteTransaction},
    protocol::{
        data::{Citation, DataScope},
        provider::ArtifactRef,
    },
    services::{
        artifacts::{
            Viewer,
            documents::{active_project, document_scope},
            reference_from_transcript,
        },
        kernel::Failure,
    },
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// The session context source kind for an exact, scoped document tool result.
pub const CONTEXT_SOURCE_KIND: &str = "document_retrieval";

/// Host provenance for the authorized input scope and exact source versions of a saved result.
/// The result bytes also retain every passage's locator, digest and extraction revision.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocumentEvidence {
    scope: Option<DataScope>,
    sources: Vec<ArtifactRef>,
}

impl DocumentEvidence {
    /// Validate the same exact citations at the response commit, before retaining them for
    /// the session and its work history. A changed audience leaves no saved result or links.
    pub fn record(
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        session: &StoredSession,
        scope: Option<DataScope>,
        citations: &[Citation],
    ) -> Result<Option<Self>, StorageError> {
        let scope = scope.or_else(|| {
            session
                .project_id
                .clone()
                .map(|project_id| DataScope::Project { project_id })
        });
        let Some(sources) = authorized_sources(tx, ctx, scope.as_ref(), citations)? else {
            return Ok(None);
        };
        for source in &sources {
            reference_from_transcript(tx, ctx, source, &session.id, session.root_id.clone())?;
        }
        Ok(Some(Self { scope, sources }))
    }

    /// Reading a saved tool result must still satisfy the original query scope and the reader's
    /// current scope. Tool output does not grant access to a source after a move or deletion.
    pub(crate) fn visible_to(
        &self,
        read: &dyn ArtifactRead,
        viewer: &Viewer<'_>,
    ) -> Result<bool, StorageError> {
        let scope = self.scope.as_ref().map(|scope| match scope {
            DataScope::Workspace => ArtifactScope::Workspace,
            DataScope::Project { project_id } => ArtifactScope::Project(project_id.clone()),
        });
        for source in &self.sources {
            if !is_visible(retrieval::visible_version(
                read,
                viewer,
                scope.as_ref(),
                source,
            ))? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn authorized_sources(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    scope: Option<&DataScope>,
    citations: &[Citation],
) -> Result<Option<Vec<ArtifactRef>>, StorageError> {
    let checked = (|| -> Result<_, Failure> {
        let scope = scope
            .map(|scope| document_scope(tx.authority(), ctx, scope))
            .transpose()?;
        if let Some(project) = scope.as_ref().and_then(ArtifactScope::project) {
            active_project(tx.projects(), project)?;
        }
        let viewer = Viewer::resolve(tx.authority(), ctx)?;
        let mut seen = HashSet::new();
        let mut sources = Vec::new();
        for citation in citations {
            let version = retrieval::citation_version(citation)?;
            retrieval::visible_version(tx.artifacts(), &viewer, scope.as_ref(), version)?;
            retrieval::resolve_passage(tx.documents(), citation)?;
            if seen.insert(version.version_id.clone()) {
                sources.push(version.clone());
            }
        }
        Ok(sources)
    })();
    match checked {
        Ok(sources) => Ok(Some(sources)),
        Err(Failure::Refused(_) | Failure::Storage(StorageError::ScopeDenied)) => Ok(None),
        Err(Failure::Storage(error)) => Err(error),
    }
}

fn is_visible<T>(result: Result<T, Failure>) -> Result<bool, StorageError> {
    match result {
        Ok(_) => Ok(true),
        Err(Failure::Refused(_) | Failure::Storage(StorageError::ScopeDenied)) => Ok(false),
        Err(Failure::Storage(error)) => Err(error),
    }
}
