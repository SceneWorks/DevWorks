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
            Ok(Discovery {
                models: models
                    .iter()
                    .filter_map(|m| m["slug"].as_str())
                    .map(|id| DiscoveredModel {
                        id: id.to_owned(),
                        context_tokens: None,
                    })
                    .collect(),
                reported: Vec::new(),
            })
        }
        "ClaudeSubscriptionMessages" | "AnthropicMessages" => {
            let mut models = Vec::new();
            let mut after: Option<String> = None;
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
                    if let Some(id) = m["id"].as_str() {
                        models.push(DiscoveredModel {
                            id: id.to_owned(),
                            context_tokens: positive(&m["max_input_tokens"]),
                        });
                    }
                }
                if body["has_more"].as_bool() != Some(true) {
                    break;
                }
                let last = body["last_id"].as_str().filter(|s| !s.is_empty());
                match last {
                    Some(l) if Some(l) != after.as_deref() && page < MAX_PAGES => {
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
                reported: Vec::new(),
            })
        }
        "OpenRouterChat" => {
            let body = get(http, join(endpoint, "models"), auth)?;
            let data = body["data"]
                .as_array()
                .ok_or(ListFailure::Malformed("model list"))?;
            let mut out = Discovery::default();
            for m in data {
                let Some(id) = m["id"].as_str().filter(|s| !s.is_empty()) else {
                    return Err(ListFailure::Malformed("model list"));
                };
                // Auto routers pick a different model per request: no fixed facts.
                if id.starts_with("openrouter/auto") {
                    out.models.push(DiscoveredModel {
                        id: id.to_owned(),
                        context_tokens: None,
                    });
                    continue;
                }
                let context = &m["context_length"];
                if !context.is_null() && positive(context).is_none() {
                    return Err(ListFailure::Malformed("context length"));
                }
                let has = |list: &Value, name: &str| {
                    list.as_array()
                        .map(|a| a.iter().any(|v| v.as_str() == Some(name)))
                };
                let efforts = m["reasoning"]["supported_efforts"]
                    .as_array()
                    .map(|a| {
                        let mut out: Vec<String> = Vec::new();
                        for e in a.iter().filter_map(Value::as_str) {
                            if let Some(name) = effort_name(e)
                                && !out.iter().any(|o| o == name)
                            {
                                out.push(name.to_owned());
                            }
                        }
                        out.sort_by_key(|e| EFFORT_ORDER.iter().position(|o| o == e));
                        out
                    })
                    .unwrap_or_default();
                out.models.push(DiscoveredModel {
                    id: id.to_owned(),
                    context_tokens: positive(context),
                });
                out.reported.push((
                    id.to_owned(),
                    Reported {
                        tools: has(&m["supported_parameters"], "tools"),
                        vision: has(&m["architecture"]["input_modalities"], "image"),
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
            Ok(Discovery {
                models: data
                    .iter()
                    .filter_map(|m| m["id"].as_str())
                    .map(|id| DiscoveredModel {
                        id: id.to_owned(),
                        context_tokens: None,
                    })
                    .collect(),
                reported: Vec::new(),
            })
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
