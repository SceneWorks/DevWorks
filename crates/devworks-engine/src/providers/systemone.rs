//! Owned selection and transport for asynchronous SystemOne decisions.
use devworks_core::context::InvocationContext;
use devworks_core::ports::AuthorityFence;
use devworks_core::ports::http::HttpRequest;
use devworks_core::ports::provider_http::{HttpStream, SendPhase, StreamLimits, StreamingHttp};
use devworks_core::ports::secrets::Secret;
use devworks_core::ports::storage::{Cancellation, StorageError, StoredInferenceAttempt};
use devworks_core::providers::systemone::{MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES};
use devworks_core::services::router::{RequestAdmission, accounting::ReportCategory};
use devworks_core::services::usage::accounting::{AttemptEnd, AttemptRequest, Observation};
use devworks_core::services::usage::host::{InferenceFrame, OwnedInferenceSource};
use devworks_core::services::usage::meter::{MeasuredQuantity, ObservationSemantics, Quantities};
use serde_json::Value;
use std::sync::{Arc, Mutex};

mod delivery;
pub(crate) use delivery::{SystemOne, completion};

pub(crate) struct DecisionProduction {
    pub context: InvocationContext,
    pub logical: RequestAdmission,
    pub request: AttemptRequest,
    pub category: ReportCategory,
    pub source_fences: Vec<AuthorityFence>,
    pub source: Box<dyn OwnedInferenceSource>,
    pub cancellation: Cancellation,
}

pub(super) enum Reply {
    Read {
        raw: Vec<u8>,
        value: Value,
    },
    Refused {
        status: u16,
        retry_after_seconds: Option<u64>,
    },
    Malformed,
    NotSent,
}

pub(super) struct Source {
    pub http: Arc<dyn StreamingHttp>,
    pub request: Option<HttpRequest>,
    pub timeout_ms: u64,
    pub reply: Arc<Mutex<Option<Reply>>>,
    stream: Option<Box<dyn HttpStream>>,
    terminal: Option<AttemptEnd>,
    failure: AttemptEnd,
}

impl Source {
    pub(super) fn new(
        http: Arc<dyn StreamingHttp>,
        request: HttpRequest,
        timeout_ms: u64,
        reply: Arc<Mutex<Option<Reply>>>,
    ) -> Self {
        Self {
            http,
            request: Some(request),
            timeout_ms,
            reply,
            stream: None,
            terminal: None,
            failure: AttemptEnd::UnknownTail,
        }
    }
    fn set(&self, reply: Reply) {
        *self
            .reply
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(reply);
    }
}

pub(super) fn request(endpoint: &str, token: Option<&Secret>, body: Vec<u8>) -> HttpRequest {
    let endpoint = format!("{}/systemone", endpoint.trim_end_matches('/'));
    let request =
        HttpRequest::post(endpoint, "application/json", body).header("Accept", "application/json");
    if let Some(token) = token {
        request.secret_header(
            "Authorization",
            Secret::new(format!("Bearer {}", token.expose())),
        )
    } else {
        request
    }
}

impl OwnedInferenceSource for Source {
    fn start(&mut self, attempt: &StoredInferenceAttempt) -> Result<(), StorageError> {
        self.start_cancellable(attempt, &Cancellation::new())
    }

    fn start_cancellable(
        &mut self,
        _: &StoredInferenceAttempt,
        cancellation: &Cancellation,
    ) -> Result<(), StorageError> {
        let request = self.request.take().ok_or(StorageError::ScopeDenied)?;
        let limits = StreamLimits {
            max_request_bytes: MAX_REQUEST_BYTES,
            deadline_ms: self.timeout_ms,
            idle_timeout_ms: self.timeout_ms,
            max_response_bytes: MAX_RESPONSE_BYTES as u64,
            max_header_bytes: 32 * 1024,
            max_headers: 128,
        };
        match self.http.open(&request, &limits, cancellation) {
            Ok(stream) => self.stream = Some(stream),
            Err(failure) if failure.phase == SendPhase::NotSent => {
                self.failure = AttemptEnd::Failed;
                self.set(Reply::NotSent);
                self.terminal = Some(AttemptEnd::Failed);
            }
            Err(_) => self.terminal = Some(AttemptEnd::UnknownTail),
        }
        Ok(())
    }

    fn next(
        &mut self,
        cancellation: &Cancellation,
    ) -> Result<Option<InferenceFrame>, StorageError> {
        if let Some(end) = self.terminal.take() {
            return Ok(Some(InferenceFrame::Terminal(end)));
        }
        let Some(stream) = self.stream.as_mut() else {
            return Ok(None);
        };
        let status = stream.head().status;
        let retry_after_seconds = stream
            .head()
            .header("retry-after")
            .and_then(|value| value.parse().ok());
        let mut body = Vec::new();
        let mut chunk = [0; 16 * 1024];
        loop {
            let length = match stream.read(&mut chunk, cancellation) {
                Ok(0) => break,
                Ok(length) if body.len() + length <= MAX_RESPONSE_BYTES => length,
                _ => return Ok(Some(InferenceFrame::Terminal(AttemptEnd::UnknownTail))),
            };
            body.extend_from_slice(&chunk[..length]);
        }
        if !(200..300).contains(&status) {
            self.set(Reply::Refused {
                status,
                retry_after_seconds,
            });
            return Ok(Some(InferenceFrame::Terminal(AttemptEnd::Failed)));
        }
        let Ok(value) = devworks_core::framing::parse(&body) else {
            self.set(Reply::Malformed);
            return Ok(Some(InferenceFrame::Terminal(AttemptEnd::Incomplete)));
        };
        let usage = observation(&value);
        self.set(Reply::Read { raw: body, value });
        self.terminal = Some(AttemptEnd::Completed);
        Ok(Some(InferenceFrame::Usage(usage)))
    }

    fn close(&mut self) -> Result<(), StorageError> {
        if let Some(mut stream) = self.stream.take() {
            stream.close().map_err(|_| StorageError::ViewUnavailable)?;
        }
        Ok(())
    }

    fn start_failure_outcome(&self) -> AttemptEnd {
        self.failure
    }
}

fn observation(value: &Value) -> Observation {
    let usage = &value["usage"];
    let count = |key: &str| usage[key].as_i64().filter(|&value| value >= 0);
    // The retained format reports exclusive input tokens and defaults omitted cache counters
    // to zero when a usage object is present. The raw provider object preserves omissions.
    let cache = |key: &str| {
        if usage.get(key).is_some() {
            count(key)
        } else {
            usage.is_object().then_some(0)
        }
    };
    let cache_write = cache("cache_creation_input_tokens");
    let hourly = usage["cache_creation"]["ephemeral_1h_input_tokens"]
        .as_i64()
        .filter(|&hourly| hourly >= 0 && cache_write.is_some_and(|total| hourly <= total));
    let additional = if let Some((total, hourly)) = cache_write.zip(hourly) {
        [(total - hourly, 300_000), (hourly, 3_600_000)]
            .into_iter()
            .map(|(quantity, ttl)| MeasuredQuantity {
                modality: "Text".into(),
                charge_kind: "CacheWrite".into(),
                unit: "Token".into(),
                cache_write_ttl_ms: Some(ttl),
                quantity,
            })
            .collect()
    } else {
        Vec::new()
    };
    Observation {
        key: "systemone_response".into(),
        sequence: 1,
        semantics: ObservationSemantics::Cumulative,
        quantities: Quantities {
            input_tokens: count("input_tokens"),
            output_tokens: count("output_tokens"),
            cached_read_tokens: cache("cache_read_input_tokens"),
            cache_write_tokens: cache_write,
            additional,
            ..Default::default()
        },
        reported_cost: None,
        provider_model_id: value["model"]
            .as_str()
            .filter(|id| id.len() <= 4096 && !id.is_empty() && !id.chars().any(char::is_control))
            .map(str::to_owned),
    }
}
