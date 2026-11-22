//! Exact document sources owned by a tool result. History keeps the original citation,
//! while access to the saved result continues to follow the retained source's current audience.
use super::retrieval;
use crate::{
    context::InvocationContext,
    ports::storage::{
        ArtifactRead, ArtifactScope, ReadSnapshot, StorageError, StoredArtifact, StoredSession,
        WriteTransaction,
    },
    protocol::{
        Id,
        data::{Citation, DataScope, DocumentVersions},
        provider::ArtifactRef,
    },
    services::{
        artifacts::{
            Viewer,
            documents::{active_project, document_scope},
            preview::preview_id,
            reference_from_transcript, version_ref, visible,
        },
        kernel::Failure,
    },
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// The session context source kind for an exact, scoped document tool result.
pub const CONTEXT_SOURCE_KIND: &str = "document_retrieval";
/// JSON that must keep its exact source dependencies when reused in provider context.
pub const MEDIA_TYPE: &str = "application/vnd.devworks.document-evidence+json";

/// The published corpus eligible for one admitted search. This includes sources omitted by
/// the result limit because they can still change rank or completion. The witness is internal
/// to tool dispatch, and does not change the public retrieval result.
pub struct DocumentSearchWitness {
    scope: DataScope,
    versions: Option<Vec<Id>>,
    digest: String,
}

impl DocumentSearchWitness {
    pub fn capture(
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        scope: &DataScope,
        versions: &DocumentVersions,
    ) -> Result<Option<Self>, StorageError> {
        let Some(resolved) = resolved_scope(document_scope(read.authority(), ctx, scope))? else {
            return Ok(None);
        };
        let viewer = Viewer::resolve(read.authority(), ctx)?;
        let versions = match versions {
            DocumentVersions::Current => None,
            DocumentVersions::Exact { versions } => Some(
                versions
                    .iter()
                    .map(|source| source.version_id.clone())
                    .collect::<Vec<_>>(),
            ),
        };
        let owner = (!viewer.sees_owned(None)).then_some(ctx.principal());
        let digest =
            read.documents()
                .document_corpus_digest(&resolved, owner, versions.as_deref())?;
        Ok(Some(Self {
            scope: scope.clone(),
            versions,
            digest,
        }))
    }

    pub fn scope(&self) -> &DataScope {
        &self.scope
    }

    pub fn holds(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
    ) -> Result<bool, StorageError> {
        let Some(scope) = resolved_scope(document_scope(tx.authority(), ctx, &self.scope))? else {
            return Ok(false);
        };
        let viewer = Viewer::resolve(tx.authority(), ctx)?;
        let owner = (!viewer.sees_owned(None)).then_some(ctx.principal());
        Ok(tx
            .documents()
            .document_corpus_digest(&scope, owner, self.versions.as_deref())?
            == self.digest)
    }
}

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
        let evidence = Self { scope, sources };
        evidence.retain(tx, ctx, session)?;
        Ok(Some(evidence))
    }

    /// If an exact artifact read copies a previous tool result, preserve its original
    /// source dependencies. Both the result artifact and its sources must still be readable.
    pub fn inherit_from(
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        session: &StoredSession,
        reference: &ArtifactRef,
    ) -> Result<Option<Self>, StorageError> {
        let artifact = tx
            .artifacts()
            .artifact(&reference.artifact_id)?
            .filter(|artifact| artifact.state == crate::ports::storage::ArtifactState::Live)
            .ok_or(StorageError::ScopeDenied)?;
        let viewer = Viewer::resolve(tx.authority(), ctx)?;
        if !visible(tx.artifacts(), &viewer, &artifact)? {
            return Err(StorageError::ScopeDenied);
        }
        if let Some(evidence) = Self::of_artifact(&artifact)? {
            let exact = if reference.version_id.as_str().starts_with("pv-") {
                tx.artifacts()
                    .versions(&artifact.id)?
                    .into_iter()
                    .any(|version| {
                        version.live()
                            && preview_id(version.id.as_str()) == reference.version_id.as_str()
                    })
            } else {
                tx.artifacts()
                    .version(&reference.version_id)?
                    .is_some_and(|version| {
                        version.live()
                            && super::indexing::same_source(&version_ref(&version), reference)
                    })
            };
            if !exact {
                return Err(StorageError::ScopeDenied);
            }
            evidence.retain(tx, ctx, session)?;
            return Ok(Some(evidence));
        }
        Ok(None)
    }

    pub(crate) fn of_artifact(artifact: &StoredArtifact) -> Result<Option<Self>, StorageError> {
        if artifact.provenance["principalKind"] != "Host"
            || artifact.provenance["derived"]["kind"] != "ToolResult"
        {
            return Ok(None);
        }
        artifact.provenance["derived"]
            .get("documentEvidence")
            .map(|provenance| {
                serde_json::from_value(provenance.clone()).map_err(|_| {
                    StorageError::InvalidRecord("tool result document evidence".into())
                })
            })
            .transpose()
    }

    fn retain(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        session: &StoredSession,
    ) -> Result<(), StorageError> {
        for source in &self.sources {
            reference_from_transcript(tx, ctx, source, &session.id, session.root_id.clone())?;
        }
        Ok(())
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
        if let Some(project) = scope.as_ref().and_then(ArtifactScope::project)
            && !read.project_exists(project)?
        {
            return Ok(false);
        }
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

fn resolved_scope<T>(result: Result<T, Failure>) -> Result<Option<T>, StorageError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(Failure::Refused(_) | Failure::Storage(StorageError::ScopeDenied)) => Ok(None),
        Err(Failure::Storage(error)) => Err(error),
    }
}

fn is_visible<T>(result: Result<T, Failure>) -> Result<bool, StorageError> {
    Ok(resolved_scope(result)?.is_some())
}
