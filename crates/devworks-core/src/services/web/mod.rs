//! `WebAdapter`: the shared web tools `web.search` and `web.fetch` (DW-51; TOOL-01..05, TOOL-08),
//! registered through [`crate::services::UseCases::with_web`]. The native UI and agent tools
//! submit to the same gateway with the same service; there is no second implementation.
//!
//! # Admission and authority
//!
//! Both are Queries (`ExternalRead`, `ScopedRead`): the gateway admits them only under current
//! authority — each tool has its own Agent tools switch (`tools.webSearch`, `tools.webFetch`,
//! both on by default), read-only code reviewers keep both as first-class reads, and principals
//! confined to a project use only that project's scope. The network work runs in
//! [`QueryHandler::prepare`], after authority and outside any read snapshot or lock; the
//! publication point re-checks authority before the answer is returned.
//!
//! # Fetch
//!
//! * **Requests** are plain `GET`s with fixed headers. No cookie, authorization, proxy or
//!   DevWorks credential is ever sent, and a URL carrying a user name or password is refused.
//! * **Egress** ([`egress`]): every hop resolves its host once, classifies **every** address the
//!   resolver returned and connects only to an address it checked, so DNS rebinding between the
//!   check and the connection cannot redirect it. Loopback, private, shared and unique-local
//!   ranges need `web.fetchAllowPrivateNetwork`; link-local, metadata, unspecified, multicast,
//!   broadcast, documentation and reserved ranges are never reachable.
//! * **Redirects** are followed up to `web.fetchMaxRedirects`, each hop parsed, resolved and
//!   checked again; the chain (each hop's redacted URL and 3xx status) is recorded and returned.
//! * **Limits**: the caller's `maxBytes` (at most 1 MiB) bounds the body kept (`truncated` says
//!   it was cut); the caller's `timeoutMs`, capped by `web.timeoutMs`, bounds the whole fetch
//!   (resolution, connection, TLS, every hop and the body), and cancellation or shutdown stops it
//!   between bounded waits.
//! * **Content** ([`extract`]): HTML is reduced to text, plain and structured text pass
//!   through, any other media type (or a compressed body) is refused before the body is read.
//!   The kept text is redacted (known secret shapes, credential headers, secret assignments) and
//!   stored as an explicitly retained artifact of the requested scope, tagged `web` and
//!   `untrusted`, with its provenance (URLs, chain, status, media type, time, digest and length
//!   of the bytes received, every egress decision) on its version and in its history, linked to
//!   the invoking work.
//! * **Outcomes**: a complete answer that is not 2xx is `ExternalFailure`; nothing sent is
//!   `ExternalFailure` (or `Cancelled`); once request bytes may have reached the server, a
//!   missing or incomplete answer is `OutcomeUnknown`, never success.
//!
//! # Search
//!
//! A configured external provider (`web.searchProvider`; its API key is a secret-store reference
//! in `web.searchCredential`, resolved only to send it to that provider). No provider is
//! `UnsupportedCapability`, a missing or unavailable key `CredentialsRequired`; nothing is ever
//! answered with invented or empty-as-success results. Pages are bounded by the provider's page
//! size and the cursor binds the query, provider, scope, principal and configuration revision.
//!
//! # Records and trust
//!
//! Every search and fetch, refused or not, is recorded as a host diagnostic (`web.fetch`,
//! `web.search`; host-reserved components) with its egress decisions, outcome and redacted URLs,
//! and the fault of a refusal names that record (`diagnosticId`). Fetched and searched content
//! is untrusted data: results carry `trust: Untrusted` (schema revision 2), artifacts the
//! `untrusted` tag and provenance, and [`untrusted_operation`] tells context assembly to treat
//! them as evidence, never instructions or authority.
pub mod egress;
pub mod extract;
mod fetch;
mod search;

use crate::authority::redact::SecretRef;
use crate::context::InvocationContext;
use crate::gateway::AdmittedQuery;
use crate::operations::{OperationInput, web as ops};
use crate::ports::secrets::SecretResolver;
use crate::ports::storage::{
    ApplicationStore, Cancellation, ConfigScope, ConfigurationRead, ReadOptions, ReadSnapshot,
    StorageError, read_snapshot,
};
use crate::ports::web::{HttpTransport, Resolver, SearchProvider};
use crate::ports::{Clock, IdKind, IdSource, Preparation, Prepared, QueryAnswer, QueryHandler};
use crate::protocol::data::DataScope;
use crate::protocol::records::{WebDocument, WebResult};
use crate::protocol::results::OperationResult;
use crate::protocol::support::{BoundedList, BoundedText, Page};
use crate::protocol::{
    CorrelationId, DomainErrorCode, DomainFault, Id, ProjectId, RetryAdvice, Revision,
};
use crate::services::artifacts::ArtifactService;
use crate::services::diagnostics::DiagnosticService;
use crate::settings::catalog::{
    WEB_FETCH_MAX_REDIRECTS_KEY, WEB_FETCH_PRIVATE_KEY, WEB_SEARCH_CREDENTIAL_KEY,
    WEB_SEARCH_PROVIDER_KEY, WEB_TIMEOUT_KEY, descriptor,
};
use crate::settings::model::{chain, current_layer};
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The operations [`WebService`] is registered for.
pub const OPERATIONS: [&str; 2] = ["web.search", "web.fetch"];
/// Diagnostic component of fetches.
pub const FETCH_COMPONENT: &str = "web.fetch";
/// Diagnostic component of searches.
pub const SEARCH_COMPONENT: &str = "web.search";
/// Tags of every fetched-content artifact.
pub const ARTIFACT_TAGS: [&str; 3] = ["untrusted", "web", "web.fetch"];
/// The `User-Agent` of every request.
pub const USER_AGENT: &str = concat!("DevWorks/", env!("CARGO_PKG_VERSION"), " (web.fetch)");

/// Whether results of `operation` are untrusted external content: context assembly presents
/// them as evidence, never as instructions, and nothing in them is authority.
pub fn untrusted_operation(operation: &str) -> bool {
    OPERATIONS.contains(&operation)
}

/// The network-facing ports of the web tools.
#[derive(Clone)]
pub struct WebPorts {
    pub resolver: Arc<dyn Resolver>,
    pub transport: Arc<dyn HttpTransport>,
    /// Search providers this build can reach, by [`SearchProvider::id`].
    pub providers: Vec<Arc<dyn SearchProvider>>,
    pub secrets: Arc<dyn SecretResolver>,
}

/// The web tools' domain service.
pub struct WebService {
    store: Arc<dyn ApplicationStore>,
    artifacts: Arc<ArtifactService>,
    diagnostics: Arc<DiagnosticService>,
    ports: WebPorts,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdSource>,
    /// Set at shutdown: in-flight requests stop at their next bounded wait, new ones are refused.
    shutdown: Cancellation,
}

impl WebService {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        artifacts: Arc<ArtifactService>,
        diagnostics: Arc<DiagnosticService>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
        ports: WebPorts,
    ) -> Self {
        Self {
            store,
            artifacts,
            diagnostics,
            ports,
            clock,
            ids,
            shutdown: Cancellation::new(),
        }
    }

    /// Stops every in-flight request at its next bounded wait and refuses new ones (window
    /// close). Never blocks.
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }

    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }

    /// Whether the caller cancelled, its deadline passed or the host is shutting down.
    fn stopped(&self, env: &Preparation<'_>) -> bool {
        self.shutdown.is_cancelled() || env.stopped(self.clock.now_ms().get())
    }

    /// The cancellation signals a transport checks between bounded waits.
    fn cancels(&self, env: &Preparation<'_>) -> Vec<Cancellation> {
        let mut all = vec![self.shutdown.clone()];
        all.extend(env.cancellation.cloned());
        all
    }

    /// The end of the request: `limit` from now, earlier when the caller's deadline is.
    fn deadline(&self, env: &Preparation<'_>, limit: Duration) -> Instant {
        let mut end = Instant::now() + limit;
        if let Some(d) = env.deadline_ms {
            let left = d.saturating_sub(self.clock.now_ms().get()).max(0) as u64;
            end = end.min(Instant::now() + Duration::from_millis(left));
        }
        end
    }
}

// ---------------------------------------------------------------------------------------------
// Settings

/// The web settings in effect for one request, read fresh from the store each time.
#[derive(Clone, Debug)]
pub struct WebPolicy {
    /// The configured search provider (`None` for `none`).
    pub provider: Option<String>,
    pub credential: Option<SecretRef>,
    pub max_redirects: u32,
    pub timeout_ms: u64,
    pub allow_private: bool,
    /// The workspace configuration revision the values came from.
    pub revision: i64,
}

/// Reads the web settings. A stored value written against another schema fails closed.
pub fn read_policy(read: &dyn ConfigurationRead) -> Result<WebPolicy, StorageError> {
    let layer = current_layer(read, &ConfigScope::Workspace)?;
    let value = |key: &str| -> Result<Value, StorageError> {
        let d = descriptor(key).expect("catalog key");
        let entries = chain(d, std::slice::from_ref(&layer))
            .map_err(|e| StorageError::IncompatibleSchema(format!("{key}: {e:?}")))?;
        Ok(entries
            .into_iter()
            .next()
            .map(|e| e.value.value.0)
            .unwrap_or_else(|| d.default_value()))
    };
    let provider = value(WEB_SEARCH_PROVIDER_KEY)?
        .as_str()
        .filter(|p| *p != "none")
        .map(str::to_owned);
    let credential = SecretRef::from_setting(&value(WEB_SEARCH_CREDENTIAL_KEY)?);
    let int = |key: &str| -> Result<i64, StorageError> {
        value(key)?
            .as_i64()
            .ok_or_else(|| StorageError::IncompatibleSchema(format!("{key} is not an integer")))
    };
    Ok(WebPolicy {
        provider,
        credential,
        max_redirects: int(WEB_FETCH_MAX_REDIRECTS_KEY)?.clamp(0, 20) as u32,
        timeout_ms: int(WEB_TIMEOUT_KEY)?.clamp(1_000, 120_000) as u64,
        allow_private: value(WEB_FETCH_PRIVATE_KEY)?.as_bool().unwrap_or(false),
        revision: layer.revision,
    })
}

// ---------------------------------------------------------------------------------------------
// Faults

/// A typed fault of a web query, linked to the diagnostic that recorded it.
pub(crate) fn fault(
    code: DomainErrorCode,
    retry: RetryAdvice,
    target: impl Into<String>,
    correlation: &CorrelationId,
    diagnostic: Option<Id>,
) -> DomainFault {
    let mut target: String = target.into();
    target.truncate(256);
    DomainFault {
        code,
        retry,
        correlation_id: correlation.clone(),
        target_id: None,
        target_kind: Some(BoundedText(target)),
        revision_dimension: None,
        expected_revision: None,
        current_revision: None,
        blocker_ids: BoundedList(Vec::new()),
        diagnostic_id: diagnostic,
    }
}

fn storage_fault(e: &StorageError, correlation: &CorrelationId) -> DomainFault {
    let retry = match e {
        StorageError::IncompatibleSchema(_) => RetryAdvice::Never,
        _ => RetryAdvice::SameCommand,
    };
    let code = match e {
        StorageError::IncompatibleSchema(_) => DomainErrorCode::SchemaMismatch,
        StorageError::Cancelled => DomainErrorCode::Cancelled,
        _ => DomainErrorCode::StorageUnavailable,
    };
    fault(code, retry, "web_store", correlation, None)
}

/// The owner scope a request names, checked against current authority (the same rule as an
/// artifact save): a project the invocation may act in, or the workspace for a workspace
/// invocation of a principal whose authorized scope is the workspace.
fn checked_scope(
    s: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    scope: &DataScope,
) -> Result<Option<ProjectId>, (DomainErrorCode, &'static str)> {
    let project = match scope {
        DataScope::Project { project_id } => Some(project_id.clone()),
        DataScope::Workspace => None,
    };
    match crate::services::artifacts::named_scope(s.authority(), ctx, project.as_ref()) {
        Ok(_) => {}
        Err(crate::services::artifacts::Failure::Refused(r)) => {
            return Err((r.code, r.target_kind));
        }
        Err(crate::services::artifacts::Failure::Storage(_)) => {
            return Err((DomainErrorCode::StorageUnavailable, "web_store"));
        }
    }
    if let Some(p) = &project {
        match s.artifacts().project_exists(p) {
            Ok(true) => {}
            Ok(false) => return Err((DomainErrorCode::NotFound, "project")),
            Err(_) => return Err((DomainErrorCode::StorageUnavailable, "web_store")),
        }
    }
    Ok(project)
}

// ---------------------------------------------------------------------------------------------
// The use case

/// What preparation established for the snapshot step.
enum Answer {
    Fetched(WebDocument, i64),
    Searched(Page<WebResult>, i64),
}

impl QueryHandler for WebService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, DomainFault> {
        let correlation = self.correlation();
        let OperationInput::Web(input) = query.input() else {
            return Err(fault(
                DomainErrorCode::UnsupportedCapability,
                RetryAdvice::Never,
                "operation",
                &correlation,
                None,
            ));
        };
        let read = read_snapshot(&*self.store, &ReadOptions::default(), |s| {
            let policy = read_policy(s.configuration());
            let scope = match input {
                ops::Input::Search(i) => checked_scope(s, ctx, &i.scope),
                ops::Input::Fetch(i) => checked_scope(s, ctx, &i.scope),
            };
            Ok((policy, scope))
        });
        let (policy, project) = match read {
            Ok((Ok(policy), Ok(project))) => (policy, project),
            Ok((_, Err((code, target)))) => {
                return Err(fault(code, RetryAdvice::Never, target, &correlation, None));
            }
            Ok((Err(e), _)) | Err(e) => return Err(storage_fault(&e, &correlation)),
        };
        if self.stopped(env) {
            return Err(fault(
                DomainErrorCode::Cancelled,
                RetryAdvice::SameCommand,
                "web_cancelled",
                &correlation,
                None,
            ));
        }
        let answer = match input {
            ops::Input::Fetch(i) => {
                let doc = self.fetch(
                    ctx,
                    env,
                    i,
                    &policy,
                    project,
                    &correlation,
                    query.authorization(),
                )?;
                Answer::Fetched(doc, policy.revision)
            }
            ops::Input::Search(i) => {
                let page = self.search(
                    ctx,
                    env,
                    i,
                    &policy,
                    project,
                    &correlation,
                    query.authorization(),
                )?;
                Answer::Searched(page, policy.revision)
            }
        };
        Ok(Some(Prepared::new(answer)))
    }

    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let correlation = self.correlation();
        let stamp = |revision: i64| {
            snapshot.stamp().read_stamp(
                ctx.policy_revision().clone(),
                Revision::new(revision.max(1)).expect("positive"),
            )
        };
        match query.prepared::<Answer>() {
            Some(Answer::Fetched(doc, revision)) => {
                // The content must still be there at publication (a delete may have raced).
                let live = snapshot
                    .artifacts()
                    .version(&doc.content.version_id)
                    .map_err(|e| storage_fault(&e, &correlation))?
                    .is_some_and(|v| v.live());
                if !live {
                    return Err(fault(
                        DomainErrorCode::NotFound,
                        RetryAdvice::SameCommand,
                        "artifact_version",
                        &correlation,
                        None,
                    ));
                }
                Ok(QueryAnswer {
                    stamp: stamp(*revision),
                    result: OperationResult::WebFetch { value: doc.clone() },
                })
            }
            Some(Answer::Searched(page, revision)) => Ok(QueryAnswer {
                stamp: stamp(*revision),
                result: OperationResult::WebSearch {
                    value: page.clone(),
                },
            }),
            None => Err(fault(
                DomainErrorCode::UnsupportedCapability,
                RetryAdvice::Never,
                "operation",
                &correlation,
                None,
            )),
        }
    }
}
