//! Model discovery and the connection test (ROUTE-10): an authenticated model-list request per
//! profile, never an inference. Ported from the reference's model-list sources
//! (`Codex/CodexModelList.cs`, `Claude/ClaudeModelList.cs`, `OpenRouter/OpenRouterModelList.cs`,
//! `Local/LocalHealth.cs`). A partial list is a failed fetch, never a shorter list: models
//! missing from it would wrongly become ineligible.
use super::profiles::{ANTHROPIC_VERSION, AuthEndpoints, CLAUDE_OAUTH_BETA, CODEX_ORIGINATOR};
use crate::ports::http::{HttpRequest, HttpResponse, HttpTransport, percent};
use crate::ports::secrets::Secret;
use crate::ports::storage::DiscoveredModel;
use serde_json::Value;
use std::collections::BTreeSet;

/// Most pages the Anthropic model list is followed for (reference `MaxPages`).
const MAX_PAGES: usize = 20;

/// The credential a discovery request authenticates with.
pub enum Auth<'a> {
    None,
    /// `Authorization: Bearer` (OAuth access token or OpenAI-style key).
    Bearer(&'a Secret),
    /// The Anthropic API key header (`x-api-key`).
    AnthropicKey(&'a Secret),
    /// The Codex backend: bearer token plus the ChatGPT account it acts for.
    Codex(&'a Secret, &'a str),
    /// The Claude subscription: bearer token plus the OAuth beta flag.
    ClaudeSubscription(&'a Secret),
}

/// Extra per-model facts some providers report (OpenRouter), applied to catalog models.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reported {
    pub tools: Option<bool>,
    pub vision: Option<bool>,
    /// `EffortName` wire names of the supported reasoning efforts.
    pub efforts: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Discovery {
    pub models: Vec<DiscoveredModel>,
    pub reported: Vec<(String, Reported)>,
    /// Exact identities the provider positively declares to generate text.
    pub conversational: BTreeSet<String>,
}

/// Why a model list could not be read. Never carries a body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListFailure {
    /// 401/403: the credential was refused.
    Unauthorized(u16),
    /// Any other status (outage, rate limit, redirect).
    Status(u16),
    Malformed(&'static str),
    Unreachable(String),
}
impl ListFailure {
    pub fn reason(&self) -> String {
        match self {
            Self::Unauthorized(s) => format!("the provider rejected the credential (HTTP {s})"),
            Self::Status(s) => format!("the model list answered HTTP {s}"),
            Self::Malformed(what) => format!("the provider sent a malformed {what}"),
            Self::Unreachable(why) => why.clone(),
        }
    }
}

fn authorize(r: HttpRequest, auth: &Auth<'_>) -> HttpRequest {
    match auth {
        Auth::None => r,
        Auth::Bearer(t) => r.secret_header(
            "Authorization",
            Secret::new(format!("Bearer {}", t.expose())),
        ),
        Auth::AnthropicKey(k) => r
            .secret_header("x-api-key", (*k).clone())
            .header("anthropic-version", ANTHROPIC_VERSION),
        Auth::Codex(t, account) => r
            .secret_header(
                "Authorization",
                Secret::new(format!("Bearer {}", t.expose())),
            )
            .header("ChatGPT-Account-ID", account)
            .header("originator", CODEX_ORIGINATOR)
            .header("User-Agent", CODEX_ORIGINATOR),
        Auth::ClaudeSubscription(t) => r
            .secret_header(
                "Authorization",
                Secret::new(format!("Bearer {}", t.expose())),
            )
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("anthropic-beta", CLAUDE_OAUTH_BETA),
    }
}

fn get(http: &dyn HttpTransport, url: String, auth: &Auth<'_>) -> Result<Value, ListFailure> {
    let r: HttpResponse = http
        .send(&authorize(HttpRequest::get(url), auth))
        .map_err(|e| ListFailure::Unreachable(e.to_string()))?;
    match r.status {
        200..=299 => {}
        401 | 403 => return Err(ListFailure::Unauthorized(r.status)),
        s => return Err(ListFailure::Status(s)),
    }
    serde_json::from_slice::<Value>(&r.body)
        .ok()
        .filter(Value::is_object)
        .ok_or(ListFailure::Malformed("model list"))
}

fn positive(v: &Value) -> Option<i64> {
    v.as_i64().filter(|n| *n > 0)
}

/// A corrupt entry fails the entire refresh: skipping it would publish a shorter inventory.
fn model_id<'a>(model: &'a Value, field: &str) -> Result<&'a str, ListFailure> {
    model[field]
        .as_str()
        .filter(|id| !id.trim().is_empty() && id.len() <= 4096)
        .ok_or(ListFailure::Malformed("model identifier"))
}

fn context(model: &Value, field: &str) -> Result<Option<i64>, ListFailure> {
    let value = &model[field];
    if value.is_null() {
        Ok(None)
    } else {
        positive(value)
            .map(Some)
            .ok_or(ListFailure::Malformed("context length"))
    }
}

/// These endpoints promise a complete unpaged list. A continuation is not a complete result.
fn unpaged(body: &Value) -> Result<(), ListFailure> {
    if (!body["has_more"].is_null() && body["has_more"].as_bool() != Some(false))
        || ["next", "next_cursor", "next_page", "next_page_token"]
            .iter()
            .any(|key| !body[key].is_null() && body[key].as_str() != Some(""))
    {
        return Err(ListFailure::Malformed("model list (incomplete)"));
    }
    Ok(())
}

fn string_list(value: &Value) -> Result<Option<Vec<&str>>, ListFailure> {
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_array()
        .ok_or(ListFailure::Malformed("model capabilities"))?
        .iter()
        .map(|v| {
            v.as_str()
                .ok_or(ListFailure::Malformed("model capabilities"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn generates_text(model: &Value) -> Result<bool, ListFailure> {
    let architecture = &model["architecture"];
    if !architecture.is_null() && !architecture.is_object() {
        return Err(ListFailure::Malformed("model capabilities"));
    }
    Ok(
        string_list(&architecture["output_modalities"])?.is_some_and(|m| m.contains(&"text"))
            || string_list(&model["output_modalities"])?.is_some_and(|m| m.contains(&"text")),
    )
}

fn object_or_absent(value: &Value) -> Result<(), ListFailure> {
    if !value.is_null() && !value.is_object() {
        Err(ListFailure::Malformed("model capabilities"))
    } else {
        Ok(())
    }
}

fn supported(value: &Value) -> Result<Option<bool>, ListFailure> {
    object_or_absent(value)?;
    let flag = &value["supported"];
    if flag.is_null() {
        Ok(None)
    } else {
        flag.as_bool()
            .map(Some)
            .ok_or(ListFailure::Malformed("model capabilities"))
    }
}

fn anthropic_reported(model: &Value) -> Result<Reported, ListFailure> {
    let caps = &model["capabilities"];
    object_or_absent(caps)?;
    object_or_absent(&caps["effort"])?;
    let mut efforts = Vec::new();
    for (key, name) in [
        ("low", "Low"),
        ("medium", "Medium"),
        ("high", "High"),
        ("xhigh", "XHigh"),
        ("max", "Max"),
    ] {
        if supported(&caps["effort"][key])? == Some(true) {
            efforts.push(name.to_owned());
        }
    }
    Ok(Reported {
        tools: None,
        vision: supported(&caps["image_input"])?,
        efforts,
    })
}

/// `{base}/{path}` without doubling or dropping a slash (base path prefixes are kept as given).
pub fn join(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// The model list of a connection of `profile` at `endpoint`.
pub fn list(
    http: &dyn HttpTransport,
    ep: &AuthEndpoints,
    profile: &str,
    endpoint: &str,
    auth: &Auth<'_>,
) -> Result<Discovery, ListFailure> {
    let discovered = list_inner(http, ep, profile, endpoint, auth)?;
    let mut seen = BTreeSet::new();
    if discovered.models.iter().any(|m| !seen.insert(&m.id)) {
        return Err(ListFailure::Malformed("duplicate model identifier"));
    }
    Ok(discovered)
}

fn list_inner(
    http: &dyn HttpTransport,
    ep: &AuthEndpoints,
    profile: &str,
    endpoint: &str,
    auth: &Auth<'_>,
) -> Result<Discovery, ListFailure> {
    match profile {
        "CodexSubscriptionResponses" => {
            let url = format!(
                "{}?client_version={}",
                join(endpoint, "models"),
                percent(&ep.codex_client_version)
            );
            let body = get(http, url, auth)?;
            let models = body["models"]
                .as_array()
                .ok_or(ListFailure::Malformed("model list"))?;
            unpaged(&body)?;
            Ok(Discovery {
                models: models
                    .iter()
                    .map(|m| {
                        Ok(DiscoveredModel {
                            id: model_id(m, "slug")?.to_owned(),
                            context_tokens: context(m, "context_window")?,
                        })
                    })
                    .collect::<Result<_, ListFailure>>()?,
                reported: Vec::new(),
                conversational: BTreeSet::new(),
            })
        }
        "ClaudeSubscriptionMessages" | "AnthropicMessages" => {
            let mut models = Vec::new();
            let mut reported = Vec::new();
            let mut after: Option<String> = None;
            let mut cursors = BTreeSet::new();
            for page in 1.. {
                let mut url = format!("{}?limit=1000", join(endpoint, "v1/models"));
                if let Some(a) = &after {
                    url.push_str(&format!("&after_id={}", percent(a)));
                }
                let body = get(http, url, auth)?;
                let data = body["data"]
                    .as_array()
                    .ok_or(ListFailure::Malformed("model list"))?;
                for m in data {
                    let id = model_id(m, "id")?;
                    models.push(DiscoveredModel {
                        id: id.to_owned(),
                        context_tokens: context(m, "max_input_tokens")?,
                    });
                    reported.push((id.to_owned(), anthropic_reported(m)?));
                }
                match body["has_more"].as_bool() {
                    Some(false) => break,
                    Some(true) => {}
                    None => return Err(ListFailure::Malformed("model list pagination")),
                }
                let last = body["last_id"].as_str().filter(|s| !s.is_empty());
                match last {
                    Some(l)
                        if data.last().and_then(|m| m["id"].as_str()) == Some(l)
                            && cursors.insert(l.to_owned())
                            && page < MAX_PAGES =>
                    {
                        after = Some(l.to_owned());
                    }
                    // A partial list would mark the missing models unknown.
                    _ => {
                        return Err(ListFailure::Malformed(
                            "model list (it could not be paged to the end)",
                        ));
                    }
                }
            }
            Ok(Discovery {
                models,
                reported,
                conversational: BTreeSet::new(),
            })
        }
        "OpenRouterChat" => {
            let body = get(http, join(endpoint, "models"), auth)?;
            let data = body["data"]
                .as_array()
                .ok_or(ListFailure::Malformed("model list"))?;
            unpaged(&body)?;
            let mut out = Discovery::default();
            for m in data {
                let id = model_id(m, "id")?;
                // Auto routers pick a different model per request: no fixed facts.
                if id.starts_with("openrouter/auto") {
                    out.models.push(DiscoveredModel {
                        id: id.to_owned(),
                        context_tokens: None,
                    });
                    continue;
                }
                let context = context(m, "context_length")?;
                for key in ["architecture", "reasoning"] {
                    if !m[key].is_null() && !m[key].is_object() {
                        return Err(ListFailure::Malformed("model capabilities"));
                    }
                }
                let parameters = string_list(&m["supported_parameters"])?;
                let modalities = string_list(&m["architecture"]["input_modalities"])?;
                if generates_text(m)? {
                    out.conversational.insert(id.to_owned());
                }
                let mut efforts = Vec::new();
                for e in string_list(&m["reasoning"]["supported_efforts"])?.unwrap_or_default() {
                    if let Some(name) = effort_name(e)
                        && !efforts.iter().any(|o| o == name)
                    {
                        efforts.push(name.to_owned());
                    }
                }
                efforts.sort_by_key(|e| EFFORT_ORDER.iter().position(|o| o == e));
                out.models.push(DiscoveredModel {
                    id: id.to_owned(),
                    context_tokens: context,
                });
                out.reported.push((
                    id.to_owned(),
                    Reported {
                        tools: parameters.map(|p| p.contains(&"tools")),
                        vision: modalities.map(|m| m.contains(&"image")),
                        efforts,
                    },
                ));
            }
            Ok(out)
        }
        // OpenAI-compatible model lists (OpenAI, Ollama, LM Studio, SystemOne, embeddings).
        _ => {
            let body = get(http, join(endpoint, "models"), auth)?;
            let data = body["data"]
                .as_array()
                .ok_or(ListFailure::Malformed("model list"))?;
            unpaged(&body)?;
            let mut out = Discovery::default();
            for m in data {
                let id = model_id(m, "id")?;
                out.models.push(DiscoveredModel {
                    id: id.to_owned(),
                    context_tokens: context(m, "context_length")?,
                });
                if generates_text(m)? {
                    out.conversational.insert(id.to_owned());
                }
            }
            Ok(out)
        }
    }
}

const EFFORT_ORDER: [&str; 6] = ["Minimal", "Low", "Medium", "High", "XHigh", "Max"];
/// The `EffortName` of a provider effort level (`none` is not an explicit effort).
fn effort_name(level: &str) -> Option<&'static str> {
    Some(match level.trim().to_ascii_lowercase().as_str() {
        "minimal" => "Minimal",
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "XHigh",
        "max" => "Max",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ports::http::HttpError;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct Fixture {
        replies: Mutex<VecDeque<(u16, Value)>>,
        requests: Mutex<Vec<HttpRequest>>,
    }
    impl Fixture {
        fn new(replies: Vec<(u16, Value)>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
                requests: Mutex::default(),
            }
        }
        fn list(&self, profile: &str) -> Result<Discovery, ListFailure> {
            list(
                self,
                &AuthEndpoints::default(),
                profile,
                "https://provider.test/v1",
                &Auth::Bearer(&Secret::new("fixture-key")),
            )
        }
    }
    impl HttpTransport for Fixture {
        fn send(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
            self.requests.lock().unwrap().push(request.clone());
            let (status, body) = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("bounded fixture request");
            Ok(HttpResponse {
                status,
                body: body.to_string().into_bytes(),
            })
        }
    }

    #[test]
    fn corrupt_or_partial_entries_never_publish_a_shorter_inventory() {
        for (profile, field, id_field) in [
            ("CodexSubscriptionResponses", "models", "slug"),
            ("OpenAiResponses", "data", "id"),
            ("CompatibleChat", "data", "id"),
            ("SystemOne", "data", "id"),
            ("OpenRouterChat", "data", "id"),
            ("AnthropicMessages", "data", "id"),
            ("ClaudeSubscriptionMessages", "data", "id"),
        ] {
            for invalid in [
                json!({}),
                json!({id_field: 42}),
                json!({id_field: ""}),
                json!({id_field: " "}),
            ] {
                let body = json!({field: [{id_field: "valid"}, invalid], "has_more": false});
                assert!(
                    matches!(
                        Fixture::new(vec![(200, body)]).list(profile),
                        Err(ListFailure::Malformed(_))
                    ),
                    "{profile}"
                );
            }
            let body = json!({field: [{id_field: "valid"}], "has_more": true});
            assert!(
                matches!(
                    Fixture::new(vec![(200, body)]).list(profile),
                    Err(ListFailure::Malformed(_))
                ),
                "{profile}"
            );
            let duplicate =
                json!({field: [{id_field: "same"}, {id_field: "same"}], "has_more": false});
            assert!(
                matches!(
                    Fixture::new(vec![(200, duplicate)]).list(profile),
                    Err(ListFailure::Malformed(_))
                ),
                "{profile}"
            );
        }
    }

    #[test]
    fn anthropic_pages_to_explicit_completion_and_keeps_authentication() {
        let f = Fixture::new(vec![
            (
                200,
                json!({"data": [{"id": "first / model", "max_input_tokens": 8000}], "has_more": true, "last_id": "first / model"}),
            ),
            (200, json!({"data": [{"id": "second"}], "has_more": false})),
        ]);
        let d = f.list("AnthropicMessages").unwrap();
        assert_eq!(d.models.len(), 2);
        assert_eq!(d.models[0].context_tokens, Some(8000));
        let sent = f.requests.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert!(sent[1].url.ends_with("&after_id=first%20%2F%20model"));
        assert!(
            sent.iter()
                .all(|r| r.header_value("authorization") == Some("Bearer fixture-key"))
        );
    }

    #[test]
    fn anthropic_requires_valid_completion_cursor_and_bounded_pages() {
        for body in [
            json!({"data": [{"id": "one"}]}),
            json!({"data": [{"id": "one"}], "has_more": "false"}),
            json!({"data": [{"id": "one"}], "has_more": true, "last_id": "other"}),
            json!({"data": [], "has_more": true, "last_id": "one"}),
            json!({"data": [{"id": "one", "max_input_tokens": -1}], "has_more": false}),
        ] {
            assert!(matches!(
                Fixture::new(vec![(200, body)]).list("AnthropicMessages"),
                Err(ListFailure::Malformed(_))
            ));
        }
        let pages = (0..MAX_PAGES).map(|n| (200, json!({"data": [{"id": format!("model-{n}")}], "has_more": true, "last_id": format!("model-{n}")}))).collect();
        let f = Fixture::new(pages);
        assert!(matches!(
            f.list("AnthropicMessages"),
            Err(ListFailure::Malformed(_))
        ));
        assert_eq!(f.requests.lock().unwrap().len(), MAX_PAGES);
        let f = Fixture::new(vec![
            (
                200,
                json!({"data": [{"id": "first"}], "has_more": true, "last_id": "first"}),
            ),
            (503, json!({})),
        ]);
        assert!(matches!(
            f.list("AnthropicMessages"),
            Err(ListFailure::Status(503))
        ));
    }

    #[test]
    fn capabilities_are_exact_reported_facts_and_malformed_facts_fail_the_refresh() {
        let f = Fixture::new(vec![(
            200,
            json!({"data": [
                {"id": "a/b", "context_length": 16000, "supported_parameters": ["tools"], "architecture": {"input_modalities": ["text", "image"]}, "reasoning": {"supported_efforts": ["high", "low"]}},
                {"id": "a/c"}
            ]}),
        )]);
        let d = f.list("OpenRouterChat").unwrap();
        assert_eq!(
            d.reported[0].1,
            Reported {
                tools: Some(true),
                vision: Some(true),
                efforts: vec!["Low".into(), "High".into()]
            }
        );
        assert_eq!(d.reported[1].1, Reported::default());
        for fact in [
            json!({"context_length": "lots"}),
            json!({"supported_parameters": true}),
            json!({"architecture": {"input_modalities": [42]}}),
            json!({"reasoning": {"supported_efforts": [42]}}),
        ] {
            let mut model = fact;
            model["id"] = json!("a/b");
            assert!(matches!(
                Fixture::new(vec![(200, json!({"data": [model]}))]).list("OpenRouterChat"),
                Err(ListFailure::Malformed(_))
            ));
        }
    }

    #[test]
    fn anthropic_optional_capabilities_are_provider_facts_without_invented_defaults() {
        let f = Fixture::new(vec![(
            200,
            json!({"data": [
            {"id": "claude-opus-5-5", "capabilities": {"image_input": {"supported": true}, "effort": {"low": {"supported": true}, "high": {"supported": false}, "max": {"supported": true}}}},
            {"id": "claude-haiku-5-5"}
        ], "has_more": false}),
        )]);
        let d = f.list("AnthropicMessages").unwrap();
        assert_eq!(
            d.reported[0].1,
            Reported {
                tools: None,
                vision: Some(true),
                efforts: vec!["Low".into(), "Max".into()]
            }
        );
        assert_eq!(d.reported[1].1, Reported::default());
        for caps in [
            json!(false),
            json!({"image_input": {"supported": "true"}}),
            json!({"effort": {"high": 42}}),
        ] {
            let f = Fixture::new(vec![(
                200,
                json!({"data": [{"id": "claude-opus-5-5", "capabilities": caps}], "has_more": false}),
            )]);
            assert!(matches!(
                f.list("AnthropicMessages"),
                Err(ListFailure::Malformed(_))
            ));
        }
    }

    #[test]
    fn joins_keep_base_prefixes_without_doubling_slashes() {
        assert_eq!(
            join("http://localhost:1234/v1/", "models"),
            "http://localhost:1234/v1/models"
        );
        assert_eq!(
            join("https://api.anthropic.com", "/v1/models"),
            "https://api.anthropic.com/v1/models"
        );
    }
}
