//! `web.search`: the configured provider, its credential, bounded pages and their record.
use super::fetch::invoker;
use super::{SEARCH_COMPONENT, WebPolicy, WebService, egress, extract, fault};
use crate::authority::redact::Redactor;
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::operations::web::SearchInput;
use crate::ports::secrets::SecretUnavailable;
use crate::ports::storage::{StorageError, WriteOptions, run_write};
use crate::ports::web::{NetFault, SearchFault, SearchRequest};
use crate::ports::{Authorization, Preparation};
use crate::protocol::data::DataScope;
use crate::protocol::records::{
    DiagnosticErrorClass, DiagnosticOutcome, Severity, TrustLevel, WebResult,
};
use crate::protocol::support::{BoundedList, BoundedText, Page, Url};
use crate::protocol::{
    CorrelationId, Counter, DomainErrorCode, DomainFault, ProjectId, RetryAdvice,
};
use crate::services::artifacts::observed::AUTHORITY_CHANGED;
use crate::services::cursor::CursorBinding;
use crate::services::diagnostics::{LogEntry, LogRefusal};
use serde_json::json;
use std::time::Duration;

/// Most characters of a result title (the contract's `Label`).
const TITLE_LIMIT: usize = 256;
/// Most characters of a result snippet (the contract's `Content`).
const SNIPPET_LIMIT: usize = 65_536;

impl WebService {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn search(
        &self,
        ctx: &InvocationContext,
        env: &Preparation<'_>,
        input: &SearchInput,
        policy: &WebPolicy,
        project: Option<ProjectId>,
        correlation: &CorrelationId,
        authorization: &Authorization,
    ) -> Result<Page<WebResult>, DomainFault> {
        let started = self.clock.now_ms().get();
        let requested = input.provider_id.as_str();
        let mut redactor = Redactor::new();
        // Records hold a digest of the query, never its text (it may hold what an agent read).
        let query_digest = sha256_hex(input.query.0.as_bytes());
        let refuse = |code: DomainErrorCode,
                      retry: RetryAdvice,
                      target: &str,
                      class: DiagnosticErrorClass,
                      message: String,
                      sent: bool,
                      redactor: &Redactor| {
            let mut e = LogEntry::new(
                SEARCH_COMPONENT,
                target,
                if code == DomainErrorCode::OutcomeUnknown {
                    Severity::Error
                } else {
                    Severity::Warning
                },
                redactor.redact_text(&message),
            );
            self.attribute(&mut e, ctx, project.as_ref(), correlation, started);
            e.outcome = Some(match code {
                DomainErrorCode::Cancelled => DiagnosticOutcome::Cancelled,
                _ if retry == RetryAdvice::SameCommand => DiagnosticOutcome::Retryable,
                _ => DiagnosticOutcome::Failed,
            });
            e.error_class = Some(class);
            e.output = Some(
                json!({
                    "invokedBy": invoker(ctx), "providerId": requested, "querySha256": query_digest,
                    "code": format!("{code:?}"), "sent": sent,
                })
                .to_string(),
            );
            let diagnostic = self.diagnostics.log(e).ok().map(|d| d.id);
            fault(code, retry, target, correlation, diagnostic)
        };
        // The configured provider, its adapter and its credential.
        let Some(configured) = policy.provider.as_deref() else {
            return Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                RetryAdvice::Never,
                "web_search_provider",
                DiagnosticErrorClass::Configuration,
                "Web search is not available: no search provider is configured in Settings".into(),
                false,
                &redactor,
            ));
        };
        if configured != requested {
            return Err(refuse(
                DomainErrorCode::NotFound,
                RetryAdvice::Never,
                "web_search_provider",
                DiagnosticErrorClass::NotFound,
                format!("Search provider {requested} is not the configured provider"),
                false,
                &redactor,
            ));
        }
        let Some(provider) = self
            .ports
            .providers
            .iter()
            .find(|p| p.id() == configured)
            .cloned()
        else {
            return Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                RetryAdvice::Never,
                "web_search_provider",
                DiagnosticErrorClass::Configuration,
                format!("Search provider {configured} is not available in this build"),
                false,
                &redactor,
            ));
        };
        let credential = if provider.requires_credential() {
            let Some(reference) = &policy.credential else {
                return Err(refuse(
                    DomainErrorCode::CredentialsRequired,
                    RetryAdvice::AfterBlockerClears,
                    "web_search_credential",
                    DiagnosticErrorClass::Authentication,
                    format!("Search provider {configured} needs an API key; none is configured"),
                    false,
                    &redactor,
                ));
            };
            match self.ports.secrets.resolve(reference) {
                Ok(value) => {
                    redactor.register(value.expose());
                    Some(value)
                }
                Err(why) => {
                    let reason = match why {
                        SecretUnavailable::NotFound => "is not in the secret store",
                        SecretUnavailable::Locked => {
                            "could not be read: the secret store is locked"
                        }
                        SecretUnavailable::Unsupported => {
                            "could not be read: no native secret store is available in this build"
                        }
                    };
                    return Err(refuse(
                        DomainErrorCode::CredentialsRequired,
                        RetryAdvice::AfterBlockerClears,
                        "web_search_credential",
                        DiagnosticErrorClass::Authentication,
                        format!("The {configured} API key {reason}"),
                        false,
                        &redactor,
                    ));
                }
            }
        } else {
            None
        };
        // The page: the cursor binds the query, provider, scope, principal and configuration.
        let scope_key = match &input.scope {
            DataScope::Workspace => "workspace".to_owned(),
            DataScope::Project { project_id } => format!("project:{}", project_id.0.as_str()),
        };
        let binding = CursorBinding::new(
            ctx,
            "web.search",
            &[&query_digest, configured, &scope_key],
            policy.revision,
        );
        let offset = match &input.page.cursor {
            None => 0,
            Some(c) => match binding
                .decode(&c.0)
                .ok()
                .and_then(|p| p.parse::<u32>().ok())
                .filter(|o| *o <= provider.offset_max())
            {
                Some(o) => o,
                None => {
                    return Err(fault(
                        DomainErrorCode::CursorExpired,
                        RetryAdvice::RefreshAndNewRequest,
                        "web_search_cursor",
                        correlation,
                        None,
                    ));
                }
            },
        };
        let count = input.page.limit.get().min(provider.page_max()).max(1);
        if self.stopped(env) {
            return Err(refuse(
                DomainErrorCode::Cancelled,
                RetryAdvice::SameCommand,
                "web_cancelled",
                DiagnosticErrorClass::Cancelled,
                "Web search was cancelled before it was sent".into(),
                false,
                &redactor,
            ));
        }
        let deadline = self.deadline(env, Duration::from_millis(policy.timeout_ms));
        let answered = provider.search(&SearchRequest {
            query: &input.query.0,
            offset,
            count,
            credential: credential.as_ref(),
            deadline,
            cancel: self.cancels(env),
        });
        let page = match answered {
            Ok(p) => p,
            Err(f) => {
                let (code, retry, target, class, sent, what) = match &f {
                    SearchFault::Net(n @ NetFault::Cancelled { sent }) => (
                        DomainErrorCode::Cancelled,
                        RetryAdvice::SameCommand,
                        "web_cancelled",
                        DiagnosticErrorClass::Cancelled,
                        *sent,
                        n.reason().to_owned(),
                    ),
                    SearchFault::Net(n @ NetFault::NotSent { .. }) => (
                        DomainErrorCode::ExternalFailure,
                        RetryAdvice::SameCommand,
                        if n.timed_out() {
                            "web_timeout"
                        } else {
                            "web_unreachable"
                        },
                        if n.timed_out() {
                            DiagnosticErrorClass::Timeout
                        } else {
                            DiagnosticErrorClass::Transient
                        },
                        false,
                        format!("was not sent: {}", n.reason()),
                    ),
                    // Sent and unanswered: the provider may have run (and counted) the search.
                    SearchFault::Net(n @ NetFault::AfterSend { .. }) => (
                        DomainErrorCode::OutcomeUnknown,
                        RetryAdvice::ReconcileOperation,
                        if n.timed_out() {
                            "web_timeout_after_send"
                        } else {
                            "web_incomplete_response"
                        },
                        if n.timed_out() {
                            DiagnosticErrorClass::Timeout
                        } else {
                            DiagnosticErrorClass::Transient
                        },
                        true,
                        format!("was sent and no complete answer arrived: {}", n.reason()),
                    ),
                    SearchFault::CredentialRejected => (
                        DomainErrorCode::CredentialsRequired,
                        RetryAdvice::AfterBlockerClears,
                        "web_search_credential",
                        DiagnosticErrorClass::Authentication,
                        true,
                        "was refused: the provider rejected the API key".to_owned(),
                    ),
                    SearchFault::RateLimited => (
                        DomainErrorCode::CapacityWait,
                        RetryAdvice::SameCommand,
                        "web_search_rate_limit",
                        DiagnosticErrorClass::ResourceExhausted,
                        true,
                        "was refused: the provider's rate limit was reached".to_owned(),
                    ),
                    SearchFault::Rejected { status } => (
                        DomainErrorCode::ExternalFailure,
                        RetryAdvice::Never,
                        "web_search_rejected",
                        DiagnosticErrorClass::Dependency,
                        true,
                        format!("was refused by the provider ({status})"),
                    ),
                    SearchFault::Malformed(why) => (
                        DomainErrorCode::ExternalFailure,
                        RetryAdvice::SameCommand,
                        "web_search_malformed",
                        DiagnosticErrorClass::Dependency,
                        true,
                        format!("answered in an unexpected format: {why}"),
                    ),
                };
                return Err(refuse(
                    code,
                    retry,
                    target,
                    class,
                    format!("Search with {configured} {what}"),
                    sent,
                    &redactor,
                ));
            }
        };
        let observed = Counter::new(self.clock.now_ms().get().max(0)).expect("non-negative");
        let mut items = Vec::new();
        let mut skipped = 0usize;
        for hit in page.hits.iter().take(count as usize) {
            let Ok(url) = egress::parse(hit.url.trim()) else {
                skipped += 1;
                continue;
            };
            let shown = egress::redacted(&url, &redactor);
            let title = clean(&hit.title, TITLE_LIMIT, &redactor);
            let title = if title.is_empty() {
                url.host_str()
                    .unwrap_or("result")
                    .chars()
                    .take(TITLE_LIMIT)
                    .collect()
            } else {
                title
            };
            items.push(WebResult {
                title: BoundedText(title),
                url: Url(shown),
                snippet: BoundedText(clean(&hit.snippet, SNIPPET_LIMIT, &redactor)),
                observed_at_ms: observed.clone(),
                trust: Some(TrustLevel::Untrusted),
                provider_id: Some(input.provider_id.clone()),
            });
        }
        // The cursor steps over exactly the provider positions this page consumed (results with
        // unusable URLs included), never the requested count: a provider that answers a short
        // page while it has more must not have the rest skipped. A page that consumed nothing
        // cannot advance, so it ends the listing rather than repeating itself.
        let consumed = page.hits.len().min(count as usize) as u32;
        let next = offset.saturating_add(consumed);
        let has_more = page.more && consumed > 0 && next <= provider.offset_max();
        // The activity is retained before any result is published (TOOL-08).
        let mut e = LogEntry::new(
            SEARCH_COMPONENT,
            "searched",
            Severity::Info,
            format!(
                "Searched {configured}: {} result{} from offset {offset}{}",
                items.len(),
                if items.len() == 1 { "" } else { "s" },
                if skipped > 0 {
                    format!(" ({skipped} with unusable URLs left out)")
                } else {
                    String::new()
                }
            ),
        );
        self.attribute(&mut e, ctx, project.as_ref(), correlation, started);
        e.outcome = Some(DiagnosticOutcome::Succeeded);
        e.output = Some(
            json!({
                "invokedBy": invoker(ctx), "providerId": configured, "querySha256": query_digest,
                "trust": "Untrusted", "offset": offset, "count": count, "hasMore": has_more,
                "skipped": skipped,
                "urls": items.iter().map(|i| i.url.0.as_str()).collect::<Vec<_>>(),
            })
            .to_string(),
        );
        // Recorded only while the invocation's authority still holds, checked in the same write:
        // a principal revoked or re-bound while the provider answered records no success.
        let (diagnostics, owned_ctx, authorization) =
            (self.diagnostics.clone(), ctx.clone(), authorization.clone());
        let recorded = run_write(&*self.store, WriteOptions::default(), move |tx| {
            if !crate::authority::authorization_holds_in(tx, &owned_ctx, &authorization)? {
                return Ok(false);
            }
            diagnostics.log_in(tx, &e).map_err(|e| match e {
                LogRefusal::Storage(s) => s,
                LogRefusal::Invalid(why) => StorageError::InvalidRecord(why.into()),
            })?;
            Ok(true)
        });
        match recorded {
            Ok(true) => self.diagnostics.committed(),
            Ok(false) => {
                return Err(refuse(
                    DomainErrorCode::PermissionDenied,
                    RetryAdvice::Never,
                    AUTHORITY_CHANGED,
                    DiagnosticErrorClass::Permission,
                    format!(
                        "Search with {configured} answered, but the caller's authority changed meanwhile; nothing was published"
                    ),
                    true,
                    &redactor,
                ));
            }
            Err(_) => {
                return Err(fault(
                    DomainErrorCode::StorageUnavailable,
                    RetryAdvice::SameCommand,
                    "web_store",
                    correlation,
                    None,
                ));
            }
        }
        Ok(Page {
            items: BoundedList(items),
            has_more,
            next_cursor: has_more.then(|| BoundedText(binding.encode(&next.to_string()))),
        })
    }
}

/// Provider text as a bounded, tag-free, redacted single string.
fn clean(raw: &str, limit: usize, redactor: &Redactor) -> String {
    let (text, _) = extract::html_to_text(raw);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    redactor
        .redact_text(&text)
        .chars()
        .filter(|c| *c != '\0')
        .take(limit)
        .collect()
}
