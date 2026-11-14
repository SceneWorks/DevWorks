//! `web.fetch`: the hop loop, its egress decisions, content handling and publication.
use super::egress::{self, AddressClass, Destination, UrlRefusal};
use super::extract::{self, ContentKind};
use super::{ARTIFACT_TAGS, FETCH_COMPONENT, USER_AGENT, WebPolicy, WebService, fault};
use crate::authority::redact::{REDACTED, Redactor};
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::operations::web::FetchInput;
use crate::ports::web::{HopRequest, HopResponse, NetFault};
use crate::ports::{Authorization, Preparation};
use crate::protocol::provider::Digest;
use crate::protocol::records::{
    DiagnosticErrorClass, DiagnosticOutcome, Severity, TrustLevel, WebDocument, WebRedirect,
};
use crate::protocol::support::{BoundedList, BoundedText, Url};
use crate::protocol::{
    CorrelationId, Counter, DomainErrorCode, DomainFault, Id, ProjectId, RetryAdvice,
};
use crate::services::artifacts::observed::{AUTHORITY_CHANGED, Observed, ObservedFailure};
use crate::services::diagnostics::LogEntry;
use serde_json::{Value, json};
use std::io::{ErrorKind, Read};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

/// Bytes read from a body per wait.
const CHUNK: usize = 16 * 1024;
/// Most redirect statuses that are followed.
const REDIRECTS: [u16; 5] = [301, 302, 303, 307, 308];
/// The `Accept` header of a fetch.
const ACCEPT: &str =
    "text/html,application/xhtml+xml,text/plain;q=0.9,application/json;q=0.9,text/*;q=0.8";

/// A refused or failed fetch, before it is recorded.
struct Stop {
    code: DomainErrorCode,
    retry: RetryAdvice,
    target: String,
    class: DiagnosticErrorClass,
    /// Whether request bytes may have reached a server.
    sent: bool,
    message: String,
}
impl Stop {
    fn new(
        code: DomainErrorCode,
        target: impl Into<String>,
        class: DiagnosticErrorClass,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            retry: RetryAdvice::Never,
            target: target.into(),
            class,
            sent: false,
            message: message.into(),
        }
    }
    fn retry(mut self, retry: RetryAdvice) -> Self {
        self.retry = retry;
        self
    }
    fn sent(mut self, sent: bool) -> Self {
        self.sent = sent;
        self
    }
    /// A network fault: before dispatch it is an external failure; once request bytes may have
    /// reached the server, a missing answer is an unknown outcome.
    fn net(f: &NetFault, url: &str) -> Self {
        let class = if f.timed_out() {
            DiagnosticErrorClass::Timeout
        } else {
            DiagnosticErrorClass::Transient
        };
        match f {
            NetFault::Cancelled { sent } => Self::new(
                DomainErrorCode::Cancelled,
                "web_cancelled",
                DiagnosticErrorClass::Cancelled,
                format!("Fetch of {url} was cancelled"),
            )
            .sent(*sent)
            .retry(RetryAdvice::SameCommand),
            NetFault::NotSent { reason, .. } => Self::new(
                DomainErrorCode::ExternalFailure,
                if f.timed_out() {
                    "web_timeout"
                } else {
                    "web_unreachable"
                },
                class,
                format!("Fetch of {url} was not sent: {reason}"),
            )
            .retry(RetryAdvice::SameCommand),
            NetFault::AfterSend { reason, .. } => Self::new(
                DomainErrorCode::OutcomeUnknown,
                if f.timed_out() {
                    "web_timeout_after_send"
                } else {
                    "web_incomplete_response"
                },
                class,
                format!("Fetch of {url} was sent and no complete answer arrived: {reason}"),
            )
            .sent(true)
            // What the server did is unknown: the recorded diagnostic is what reconciles it.
            .retry(RetryAdvice::ReconcileOperation),
        }
    }
}

/// What the hop loop established.
struct Fetched {
    final_url: url::Url,
    status: u16,
    media: extract::MediaType,
    kind: ContentKind,
    body: Vec<u8>,
    truncated: bool,
    observed_at_ms: i64,
}

impl WebService {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn fetch(
        &self,
        ctx: &InvocationContext,
        env: &Preparation<'_>,
        input: &FetchInput,
        policy: &WebPolicy,
        project: Option<ProjectId>,
        correlation: &CorrelationId,
        authorization: &Authorization,
    ) -> Result<WebDocument, DomainFault> {
        let started = self.clock.now_ms().get();
        let redactor = Redactor::new();
        let max_bytes = input.max_bytes.get() as usize;
        let asked_ms = input.timeout_ms.0.parse::<u64>().unwrap_or(u64::MAX);
        let timeout_ms = asked_ms.min(policy.timeout_ms);
        let deadline = self.deadline(env, Duration::from_millis(timeout_ms));
        let mut trail = Trail {
            decisions: Vec::new(),
            redirects: Vec::new(),
        };
        let limits = json!({
            "maxBytes": max_bytes, "timeoutMs": timeout_ms, "requestedTimeoutMs": asked_ms,
            "maxRedirects": policy.max_redirects, "allowPrivateNetwork": policy.allow_private,
        });
        let requested = match egress::parse(&input.url.0) {
            Ok(u) => u,
            Err(r) => {
                // The refused URL is never recorded as given: it may carry the credentials.
                let stop = Stop::new(
                    DomainErrorCode::InvalidArgument,
                    r.target(),
                    DiagnosticErrorClass::InvalidInput,
                    format!("Fetch refused: {}", refusal_text(r)),
                );
                return Err(self.refuse(
                    ctx,
                    project,
                    correlation,
                    started,
                    None,
                    &trail,
                    &limits,
                    stop,
                ));
            }
        };
        let shown = egress::redacted(&requested, &redactor);
        let fetched = match self.hops(
            env,
            requested.clone(),
            policy,
            max_bytes,
            deadline,
            &redactor,
            &mut trail,
        ) {
            Ok(f) => f,
            Err(stop) => {
                return Err(self.refuse(
                    ctx,
                    project,
                    correlation,
                    started,
                    Some(&shown),
                    &trail,
                    &limits,
                    stop,
                ));
            }
        };
        // Content: decode, reduce HTML to text, redact.
        let charset = fetched.media.charset.clone().or_else(|| {
            (fetched.kind == ContentKind::Html)
                .then(|| extract::html_charset(&fetched.body))
                .flatten()
        });
        let decoded = extract::decode(&fetched.body, charset.as_deref());
        let (text, title) = match fetched.kind {
            ContentKind::Html => extract::html_to_text(&decoded),
            ContentKind::Text | ContentKind::Json => (decoded, None),
        };
        let before = text.matches(REDACTED).count();
        let kept = redactor.redact_text(&text);
        let redactions = kept.matches(REDACTED).count().saturating_sub(before);
        let final_shown = egress::redacted(&fetched.final_url, &redactor);
        let media_label: String = if fetched.media.essence.is_empty() {
            "text/plain".to_owned()
        } else {
            fetched.media.essence.chars().take(256).collect()
        };
        let stored_media = match fetched.kind {
            ContentKind::Json => "application/json".to_owned(),
            ContentKind::Html => "text/plain; charset=utf-8".to_owned(),
            ContentKind::Text => format!(
                "{}; charset=utf-8",
                media_label.chars().take(100).collect::<String>()
            ),
        };
        let source_digest = sha256_hex(&fetched.body);
        let source_len = fetched.body.len() as i64;
        let title = title
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| final_shown.clone());
        let title: String = redactor.redact_text(&title).chars().take(256).collect();
        let redirects = trail.redirects.clone();
        let source = json!({
            "kind": "WebFetch", "trust": "Untrusted",
            "url": shown, "finalUrl": final_shown, "status": fetched.status,
            "mediaType": media_label, "charset": charset,
            "redirects": redirects.iter().map(|r| json!({"url": r.url.0, "status": r.status})).collect::<Vec<_>>(),
            "sourceSha256": source_digest, "sourceByteLength": source_len,
            "truncated": fetched.truncated, "redactions": redactions,
            "fetchedAtMs": fetched.observed_at_ms, "limits": limits,
            "decisions": trail.decisions, "invokedBy": invoker(ctx),
        });
        let entry = {
            let mut e = LogEntry::new(
                FETCH_COMPONENT,
                "fetched",
                Severity::Info,
                format!(
                    "Fetched {final_shown} ({} {media_label}, {source_len} bytes{}{})",
                    fetched.status,
                    if fetched.truncated { ", truncated" } else { "" },
                    match redirects.len() {
                        0 => String::new(),
                        n => format!(", {n} redirect{}", if n == 1 { "" } else { "s" }),
                    }
                ),
            );
            self.attribute(&mut e, ctx, project.as_ref(), correlation, started);
            e.outcome = Some(DiagnosticOutcome::Succeeded);
            e.output = Some(source.to_string());
            e
        };
        let diagnostics = self.diagnostics.clone();
        let published = self.artifacts.publish_observed(
            &*self.store,
            ctx,
            authorization,
            Observed {
                project: project.clone(),
                title,
                media_type: stored_media,
                tags: ARTIFACT_TAGS.iter().map(|t| (*t).to_owned()).collect(),
                source: source.clone(),
                actor_audience: false,
            },
            kept.as_bytes(),
            move |tx, reference| {
                let mut entry = entry;
                entry.evidence = Some(reference.clone());
                diagnostics
                    .log_in(tx, &entry)
                    .map(|_| ())
                    .map_err(|e| match e {
                        crate::services::diagnostics::LogRefusal::Storage(s) => s,
                        crate::services::diagnostics::LogRefusal::Invalid(why) => {
                            crate::ports::storage::StorageError::InvalidRecord(why.into())
                        }
                    })
            },
        );
        let content = match published {
            Ok((reference, ())) => {
                self.diagnostics.committed();
                reference
            }
            Err(ObservedFailure::Refused(r)) => {
                let stop = Stop::new(
                    r.code,
                    r.target_kind,
                    if r.target_kind == AUTHORITY_CHANGED {
                        DiagnosticErrorClass::Permission
                    } else {
                        DiagnosticErrorClass::ResourceExhausted
                    },
                    format!(
                        "Fetched {final_shown} but its content was not kept ({})",
                        r.target_kind
                    ),
                )
                .sent(true)
                .retry(r.retry);
                return Err(self.refuse(
                    ctx,
                    project,
                    correlation,
                    started,
                    Some(&shown),
                    &trail,
                    &limits,
                    stop,
                ));
            }
            Err(ObservedFailure::Storage(e)) => {
                let stop = Stop::new(
                    DomainErrorCode::StorageUnavailable,
                    "web_store",
                    DiagnosticErrorClass::Dependency,
                    format!("Fetched {final_shown} but its content could not be stored: {e:?}"),
                )
                .sent(true)
                .retry(RetryAdvice::SameCommand);
                return Err(self.refuse(
                    ctx,
                    project,
                    correlation,
                    started,
                    Some(&shown),
                    &trail,
                    &limits,
                    stop,
                ));
            }
        };
        Ok(WebDocument {
            url: url_value(&shown),
            final_url: url_value(&final_shown),
            content,
            media_type: BoundedText(media_label),
            observed_at_ms: Counter::new(fetched.observed_at_ms.max(0)).expect("non-negative"),
            truncated: fetched.truncated,
            trust: Some(TrustLevel::Untrusted),
            status: Some(i32::from(fetched.status)),
            redirects: Some(BoundedList(redirects)),
            source_sha256: Some(Digest(source_digest)),
            source_byte_length: Some(Counter::new(source_len).expect("non-negative")),
        })
    }

    /// Follows the hops of one fetch to a 2xx answer whose content is kept.
    #[allow(clippy::too_many_arguments)]
    fn hops(
        &self,
        env: &Preparation<'_>,
        mut url: url::Url,
        policy: &WebPolicy,
        max_bytes: usize,
        deadline: Instant,
        redactor: &Redactor,
        trail: &mut Trail,
    ) -> Result<Fetched, Stop> {
        loop {
            let shown = egress::redacted(&url, redactor);
            if self.stopped(env) {
                return Err(Stop::net(&NetFault::Cancelled { sent: false }, &shown));
            }
            if Instant::now() >= deadline {
                return Err(Stop::net(
                    &NetFault::NotSent {
                        reason: "the time limit passed".into(),
                        timed_out: true,
                    },
                    &shown,
                ));
            }
            let port = egress::port(&url);
            let addrs = self.admit(env, &url, port, policy, deadline, &shown, trail)?;
            let response = self.send(env, &url, port, &addrs, deadline, &shown, trail)?;
            let status = response.status;
            if REDIRECTS.contains(&status)
                && let Some(location) = response.header("location").map(str::to_owned)
            {
                drop(response);
                if trail.redirects.len() >= policy.max_redirects as usize {
                    trail.decide(json!({"step": "redirect", "url": shown, "status": status, "verdict": "refused", "reason": "limit"}));
                    return Err(Stop::new(
                        DomainErrorCode::ExternalFailure,
                        "web_redirect_limit",
                        DiagnosticErrorClass::Dependency,
                        format!(
                            "{shown} redirected more than {} times",
                            policy.max_redirects
                        ),
                    )
                    .sent(true));
                }
                let next = match egress::join(&url, &location) {
                    Ok(n) => n,
                    Err(r) => {
                        trail.decide(json!({"step": "redirect", "url": shown, "status": status, "verdict": "refused", "reason": r.target()}));
                        let code = match r {
                            UrlRefusal::Credentials | UrlRefusal::Scheme => {
                                DomainErrorCode::PermissionDenied
                            }
                            _ => DomainErrorCode::ExternalFailure,
                        };
                        return Err(Stop::new(
                            code,
                            format!("web_redirect_{}", r.target().trim_start_matches("web_")),
                            DiagnosticErrorClass::Permission,
                            format!(
                                "{shown} redirected to a refused location: {}",
                                refusal_text(r)
                            ),
                        )
                        .sent(true));
                    }
                };
                trail.redirects.push(WebRedirect {
                    url: url_value(&shown),
                    status: i32::from(status),
                });
                trail.decide(json!({"step": "redirect", "url": shown, "status": status, "verdict": "followed", "to": egress::redacted(&next, redactor)}));
                url = next;
                continue;
            }
            if !(200..=299).contains(&status) {
                drop(response);
                return Err(Stop::new(
                    DomainErrorCode::ExternalFailure,
                    format!("web_http_status_{status}"),
                    DiagnosticErrorClass::Dependency,
                    format!("{shown} answered {status}"),
                )
                .sent(true));
            }
            return self.content(env, url, response, max_bytes, deadline, &shown, trail);
        }
    }

    /// Resolves (once) and checks every address of a hop's host.
    #[allow(clippy::too_many_arguments)]
    fn admit(
        &self,
        env: &Preparation<'_>,
        url: &url::Url,
        port: u16,
        policy: &WebPolicy,
        deadline: Instant,
        shown: &str,
        trail: &mut Trail,
    ) -> Result<Vec<IpAddr>, Stop> {
        let blocked = |trail: &mut Trail, addrs: &[(String, &str)], class: AddressClass| {
            trail.decide(json!({
                "step": "destination", "url": shown, "verdict": "blocked", "class": class.as_str(),
                "addresses": addrs.iter().map(|(a, c)| json!({"address": a, "class": c})).collect::<Vec<_>>(),
            }));
            Stop::new(
                DomainErrorCode::PermissionDenied,
                format!("web_destination_{}", class.as_str()),
                DiagnosticErrorClass::Permission,
                format!("Fetch of {shown} refused: {} address", class.as_str()),
            )
        };
        let addrs = match egress::destination(url) {
            Destination::Literal(ip) => vec![ip],
            Destination::Domain(domain) => {
                if let Some(class) = egress::name_class(&domain)
                    && !class.allowed(policy.allow_private)
                {
                    return Err(blocked(trail, &[(domain, class.as_str())], class));
                }
                match self
                    .ports
                    .resolver
                    .resolve(&domain, port, deadline, &self.cancels(env))
                {
                    Ok(a) if !a.is_empty() => a,
                    Ok(_) => {
                        trail.decide(json!({"step": "resolve", "url": shown, "verdict": "failed", "reason": "no addresses"}));
                        return Err(Stop::net(
                            &NetFault::NotSent {
                                reason: "the name has no addresses".into(),
                                timed_out: false,
                            },
                            shown,
                        ));
                    }
                    Err(f) => {
                        trail.decide(json!({"step": "resolve", "url": shown, "verdict": "failed", "reason": f.reason()}));
                        return Err(Stop::net(&f, shown));
                    }
                }
            }
        };
        let classes: Vec<(IpAddr, AddressClass)> = addrs
            .iter()
            .map(|ip| (*ip, egress::classify(*ip)))
            .collect();
        let listed: Vec<(String, &str)> = classes
            .iter()
            .map(|(ip, c)| (ip.to_string(), c.as_str()))
            .collect();
        // Every answer must be allowed: one refused address refuses the hop, so a resolver
        // cannot mix a public and a private answer to slip past the check.
        if let Some((_, class)) = classes
            .iter()
            .find(|(_, c)| !c.allowed(policy.allow_private))
        {
            return Err(blocked(trail, &listed, *class));
        }
        trail.decide(json!({
            "step": "destination", "url": shown, "verdict": "allowed",
            "addresses": listed.iter().map(|(a, c)| json!({"address": a, "class": c})).collect::<Vec<_>>(),
        }));
        Ok(addrs)
    }

    /// Sends the hop to the checked addresses in order until one connects.
    #[allow(clippy::too_many_arguments)]
    fn send(
        &self,
        env: &Preparation<'_>,
        url: &url::Url,
        port: u16,
        addrs: &[IpAddr],
        deadline: Instant,
        shown: &str,
        trail: &mut Trail,
    ) -> Result<HopResponse, Stop> {
        let host_header = egress::host_header(url);
        let server_name = egress::server_name(url);
        let target = egress::request_target(url);
        let mut last = NetFault::NotSent {
            reason: "no address".into(),
            timed_out: false,
        };
        for ip in addrs {
            let request = HopRequest {
                scheme: egress::scheme(url),
                host_header: &host_header,
                server_name: &server_name,
                target: &target,
                addr: SocketAddr::new(*ip, port),
                deadline,
                cancel: self.cancels(env),
                user_agent: USER_AGENT,
                accept: ACCEPT,
                // A fetch never carries a credential, cookie or authorization of any kind.
                headers: &[],
            };
            match self.ports.transport.get(&request) {
                Ok(r) => {
                    trail.decide(json!({"step": "connect", "url": shown, "address": ip.to_string(), "verdict": "answered", "status": r.status}));
                    return Ok(r);
                }
                // Not sent: the next checked address may still connect.
                Err(f @ NetFault::NotSent { .. }) if !f.timed_out() => {
                    trail.decide(json!({"step": "connect", "url": shown, "address": ip.to_string(), "verdict": "failed", "reason": f.reason()}));
                    last = f;
                }
                Err(f) => {
                    trail.decide(json!({"step": "connect", "url": shown, "address": ip.to_string(), "verdict": "failed", "sent": f.dispatched(), "reason": f.reason()}));
                    return Err(Stop::net(&f, shown));
                }
            }
        }
        Err(Stop::net(&last, shown))
    }

    /// Checks a 2xx answer's media type and encoding, then reads its body within the limits.
    #[allow(clippy::too_many_arguments)]
    fn content(
        &self,
        env: &Preparation<'_>,
        url: url::Url,
        mut response: HopResponse,
        max_bytes: usize,
        deadline: Instant,
        shown: &str,
        trail: &mut Trail,
    ) -> Result<Fetched, Stop> {
        let status = response.status;
        let observed_at_ms = self.clock.now_ms().get();
        if let Some(enc) = response.header("content-encoding")
            && !enc.trim().eq_ignore_ascii_case("identity")
            && !enc.trim().is_empty()
        {
            trail.decide(json!({"step": "content", "url": shown, "verdict": "refused", "contentEncoding": enc}));
            return Err(Stop::new(
                DomainErrorCode::UnsupportedTarget,
                "web_content_encoding",
                DiagnosticErrorClass::InvalidInput,
                format!("{shown} answered with an encoded body ({enc})"),
            )
            .sent(true));
        }
        let media = response
            .header("content-type")
            .map(extract::media_type)
            .unwrap_or(extract::MediaType {
                essence: String::new(),
                charset: None,
            });
        let declared = (!media.essence.is_empty()).then(|| extract::kind(&media.essence));
        if let Some(None) = declared {
            trail.decide(json!({"step": "content", "url": shown, "verdict": "refused", "mediaType": media.essence}));
            return Err(Stop::new(
                DomainErrorCode::UnsupportedTarget,
                "web_content_type",
                DiagnosticErrorClass::InvalidInput,
                format!("{shown} is {}, which is not text", media.essence),
            )
            .sent(true));
        }
        // Bounded read: at most `max_bytes` are kept and one byte more tells it was cut.
        let mut body = Vec::with_capacity(max_bytes.min(CHUNK * 4));
        let mut chunk = vec![0u8; CHUNK];
        let mut truncated = false;
        loop {
            if self.stopped(env) {
                return Err(Stop::net(&NetFault::Cancelled { sent: true }, shown));
            }
            if Instant::now() >= deadline {
                return Err(Stop::net(
                    &NetFault::AfterSend {
                        reason: "the time limit passed while reading the body".into(),
                        timed_out: true,
                    },
                    shown,
                ));
            }
            let want = (max_bytes + 1 - body.len()).min(CHUNK);
            match response.body.read(&mut chunk[..want]) {
                Ok(0) => break,
                Ok(n) => {
                    body.extend_from_slice(&chunk[..n]);
                    if body.len() > max_bytes {
                        body.truncate(max_bytes);
                        truncated = true;
                        break;
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted && !self.stopped(env) => continue,
                Err(e) => {
                    let f = match e.kind() {
                        ErrorKind::Interrupted => NetFault::Cancelled { sent: true },
                        ErrorKind::TimedOut | ErrorKind::WouldBlock => NetFault::AfterSend {
                            reason: "the time limit passed while reading the body".into(),
                            timed_out: true,
                        },
                        _ => NetFault::AfterSend {
                            reason: format!("the body could not be read: {e}"),
                            timed_out: false,
                        },
                    };
                    return Err(Stop::net(&f, shown));
                }
            }
        }
        drop(response);
        let kind = match declared {
            Some(Some(k)) => k,
            // No declared media type: kept only as text that is valid UTF-8 without NUL bytes.
            _ => match std::str::from_utf8(&body) {
                Ok(s) if !s.contains('\0') => {
                    let head = s.trim_start().get(..15).unwrap_or("").to_ascii_lowercase();
                    if head.starts_with("<!doctype html") || head.starts_with("<html") {
                        ContentKind::Html
                    } else {
                        ContentKind::Text
                    }
                }
                _ => {
                    trail.decide(json!({"step": "content", "url": shown, "verdict": "refused", "mediaType": null}));
                    return Err(Stop::new(
                        DomainErrorCode::UnsupportedTarget,
                        "web_content_type",
                        DiagnosticErrorClass::InvalidInput,
                        format!("{shown} declared no media type and is not text"),
                    )
                    .sent(true));
                }
            },
        };
        trail.decide(json!({"step": "content", "url": shown, "verdict": "kept", "status": status, "mediaType": media.essence, "bytes": body.len(), "truncated": truncated}));
        Ok(Fetched {
            final_url: url,
            status,
            media,
            kind,
            body,
            truncated,
            observed_at_ms,
        })
    }

    /// Records a refused or failed fetch and returns its fault, naming the record.
    #[allow(clippy::too_many_arguments)]
    fn refuse(
        &self,
        ctx: &InvocationContext,
        project: Option<ProjectId>,
        correlation: &CorrelationId,
        started: i64,
        url: Option<&str>,
        trail: &Trail,
        limits: &Value,
        stop: Stop,
    ) -> DomainFault {
        let severity = match stop.code {
            DomainErrorCode::OutcomeUnknown | DomainErrorCode::StorageUnavailable => {
                Severity::Error
            }
            _ => Severity::Warning,
        };
        let mut e = LogEntry::new(
            FETCH_COMPONENT,
            stop.target.clone(),
            severity,
            stop.message.clone(),
        );
        self.attribute(&mut e, ctx, project.as_ref(), correlation, started);
        e.outcome = Some(match stop.code {
            DomainErrorCode::Cancelled => DiagnosticOutcome::Cancelled,
            _ if stop.retry == RetryAdvice::SameCommand => DiagnosticOutcome::Retryable,
            _ => DiagnosticOutcome::Failed,
        });
        e.error_class = Some(stop.class);
        e.output = Some(
            json!({
                "invokedBy": invoker(ctx),
                // `sent`: whether this hop's request may have reached its server; earlier hops of a
                // redirect chain were sent and answered (`answeredHops`).
                "url": url, "code": format!("{:?}", stop.code), "sent": stop.sent,
                "answeredHops": trail.redirects.len(),
                "redirects": trail.redirects.iter().map(|r| json!({"url": r.url.0, "status": r.status})).collect::<Vec<_>>(),
                "decisions": trail.decisions, "limits": limits,
            })
            .to_string(),
        );
        let diagnostic = self.diagnostics.log(e).ok().map(|d| d.id);
        fault(stop.code, stop.retry, stop.target, correlation, diagnostic)
    }

    /// The attribution every web diagnostic carries.
    pub(super) fn attribute(
        &self,
        e: &mut LogEntry,
        ctx: &InvocationContext,
        project: Option<&ProjectId>,
        correlation: &CorrelationId,
        started: i64,
    ) {
        e.correlation_id = Some(correlation.0.clone());
        e.project_id = project.cloned().or_else(|| ctx.project_id().cloned());
        e.operation = Some(e.component.clone());
        e.started_at_ms = Some(started);
        e.finished_at_ms = Some(self.clock.now_ms().get().max(started));
    }
}

/// Who invoked a web tool, for its record (the diagnostic itself is the host's).
pub(super) fn invoker(ctx: &InvocationContext) -> Value {
    json!({
        "principal": ctx.principal().0.as_str(),
        "principalKind": ctx.principal_kind().as_str().to_string(),
        "sessionId": ctx.session_id().map(Id::as_str),
        "rootId": ctx.root_id().map(|r| r.0.as_str()),
        "assignmentId": ctx.assignment_id().map(|a| a.0.as_str()),
        "invocationId": ctx.invocation_id().as_str(),
    })
}

/// The decisions and redirect chain of one fetch.
struct Trail {
    decisions: Vec<Value>,
    redirects: Vec<WebRedirect>,
}
impl Trail {
    fn decide(&mut self, d: Value) {
        self.decisions.push(d);
    }
}

fn refusal_text(r: UrlRefusal) -> &'static str {
    match r {
        UrlRefusal::Invalid => "the URL is not valid",
        UrlRefusal::Scheme => "only http and https URLs are fetched",
        UrlRefusal::Credentials => "URLs carrying a user name or password are never fetched",
        UrlRefusal::Host => "the URL has no usable host or port",
        UrlRefusal::TooLong => "the URL is longer than 4096 bytes",
    }
}

/// A redacted URL as a contract `Url` (it is http(s), whitespace-free and bounded).
fn url_value(s: &str) -> Url {
    Url(s.to_owned())
}
