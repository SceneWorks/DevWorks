//! The subscription OAuth protocols, ported from the simple-router reference without a Codex or
//! Claude CLI: the Codex device-code flow (`Codex/CodexOAuthClient.cs`) and the Claude PKCE
//! authorization-code flow (`Claude/ClaudeOAuthClient.cs`), each with code exchange and token
//! refresh. Every request goes through [`HttpTransport`]; a response body is never kept in an
//! error because it may hold credentials.
use super::profiles::{AUTH_USER_AGENT, AuthEndpoints};
use crate::ports::http::{
    HttpError, HttpRequest, HttpResponse, HttpTransport, base64url, base64url_decode, form, percent,
};
use crate::ports::secrets::Secret;
use serde_json::{Value, json};
use sha2::Digest;

/// Access tokens without a reported lifetime are assumed to last an hour (reference
/// `DefaultLifetime`).
const DEFAULT_LIFETIME_MS: i64 = 60 * 60 * 1000;
/// A cached access token is refreshed this long before it expires (reference `RefreshSkew`).
pub const REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;
/// Device polling interval bounds (reference `DefaultInterval`, `MaxInterval`).
const DEFAULT_INTERVAL_MS: i64 = 5_000;
const MAX_INTERVAL_MS: i64 = 30_000;
/// How long a device code may wait to be entered (reference `DeviceCodeTimeout`).
pub const DEVICE_LIFETIME_MS: i64 = 15 * 60 * 1000;
/// How long a browser sign-in waits for its pasted code (a bound of the in-memory verifier).
pub const BROWSER_LIFETIME_MS: i64 = 15 * 60 * 1000;

/// OAuth tokens of one subscription account. Plaintext only in memory and the secret store.
#[derive(Clone, PartialEq, Eq)]
pub struct Tokens {
    pub access: Secret,
    pub refresh: Secret,
    pub expires_at_ms: i64,
    /// The provider account the tokens act for (Codex: the ChatGPT account ID the backend needs;
    /// Claude: the account UUID when the token response names one).
    pub account: Option<String>,
}
impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field("expires_at_ms", &self.expires_at_ms)
            .finish_non_exhaustive()
    }
}

/// What a secret-store handle holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Credential {
    ApiKey(Secret),
    OAuth(Tokens),
}
impl Credential {
    /// The stored form (JSON). Only the secret store ever receives it.
    pub fn to_secret(&self) -> Secret {
        Secret::new(match self {
            Self::ApiKey(k) => json!({"kind": "apiKey", "key": k.expose()}).to_string(),
            Self::OAuth(t) => json!({
                "kind": "oauth",
                "access": t.access.expose(),
                "refresh": t.refresh.expose(),
                "expiresAtMs": t.expires_at_ms,
                "account": t.account,
            })
            .to_string(),
        })
    }
    pub fn parse(secret: &Secret) -> Option<Self> {
        let v: Value = serde_json::from_str(secret.expose()).ok()?;
        match v["kind"].as_str()? {
            "apiKey" => Some(Self::ApiKey(Secret::new(v["key"].as_str()?))),
            "oauth" => Some(Self::OAuth(Tokens {
                access: Secret::new(v["access"].as_str()?),
                refresh: Secret::new(v["refresh"].as_str()?),
                expires_at_ms: v["expiresAtMs"].as_i64()?,
                account: v["account"].as_str().map(str::to_owned),
            })),
            _ => None,
        }
    }
}

/// Why an auth request did not produce what was asked. Never carries a body or a secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthFailure {
    /// The provider refused the grant or credential (400/401): sign in again.
    Rejected(u16),
    /// Any other HTTP status (rate limits, outages, a redirect).
    Status(u16),
    /// The response was not the documented shape.
    Malformed(&'static str),
    /// No response (an availability event).
    Transport(HttpError),
}
impl AuthFailure {
    pub fn reason(&self) -> String {
        match self {
            Self::Rejected(s) => {
                format!("the provider rejected the credentials (HTTP {s}); sign in again")
            }
            Self::Status(s) => format!("the provider answered HTTP {s}"),
            Self::Malformed(what) => format!("the provider sent a malformed {what}"),
            Self::Transport(e) => e.to_string(),
        }
    }
}

fn send(http: &dyn HttpTransport, request: HttpRequest) -> Result<HttpResponse, AuthFailure> {
    http.send(&request.header("User-Agent", AUTH_USER_AGENT))
        .map_err(AuthFailure::Transport)
}
fn object(r: &HttpResponse, what: &'static str) -> Result<Value, AuthFailure> {
    serde_json::from_slice::<Value>(&r.body)
        .ok()
        .filter(Value::is_object)
        .ok_or(AuthFailure::Malformed(what))
}
fn text(v: &Value, key: &str) -> Option<String> {
    v[key].as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

/// The JWT claims of a token (unverified: only used to read the account and expiry, as the
/// reference does).
fn claims(jwt: Option<&str>) -> Option<Value> {
    let parts: Vec<&str> = jwt?.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    serde_json::from_slice(&base64url_decode(parts[1])?).ok()
}
/// The ChatGPT account ID from an id or access token's claims, as the Codex CLI reads it.
pub fn codex_account(jwt: Option<&str>) -> Option<String> {
    let c = claims(jwt)?;
    c["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .or_else(|| c["chatgpt_account_id"].as_str())
        .or_else(|| c["organizations"][0]["id"].as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The token-endpoint outcome shared by both providers.
fn tokens(
    r: &HttpResponse,
    previous_refresh: Option<&Secret>,
    now_ms: i64,
    account: impl FnOnce(&Value) -> Option<String>,
) -> Result<Tokens, AuthFailure> {
    if !r.success() {
        // A refused refresh means the credentials expired or were revoked (reference).
        return Err(match r.status {
            400 | 401 if previous_refresh.is_some() => AuthFailure::Rejected(r.status),
            400 | 401 | 403 => AuthFailure::Rejected(r.status),
            s => AuthFailure::Status(s),
        });
    }
    let body = object(r, "token response")?;
    let access = text(&body, "access_token").ok_or(AuthFailure::Malformed("token response"))?;
    // A refresh that does not rotate keeps the previous refresh token.
    let refresh = text(&body, "refresh_token")
        .map(Secret::new)
        .or_else(|| previous_refresh.cloned())
        .ok_or(AuthFailure::Malformed("token response"))?;
    let lifetime = body["expires_in"]
        .as_i64()
        .or_else(|| body["expires_in"].as_str().and_then(|s| s.parse().ok()))
        .filter(|s| *s > 0)
        .map(|s| s.saturating_mul(1000));
    let expiry = lifetime.map(|l| now_ms.saturating_add(l)).or_else(|| {
        claims(Some(&access))
            .and_then(|c| c["exp"].as_i64())
            .map(|e| e.saturating_mul(1000))
    });
    let account = account(&body);
    Ok(Tokens {
        access: Secret::new(access),
        refresh,
        expires_at_ms: expiry.unwrap_or(now_ms + DEFAULT_LIFETIME_MS),
        account,
    })
}

// ---------------------------------------------------------------------------------------------
// Codex device code

/// A started device authorization: what the user enters, and how often to poll.
#[derive(Clone)]
pub struct DeviceCode {
    /// The poll credential; never shown.
    pub device_auth_id: Secret,
    pub user_code: String,
    pub interval_ms: i64,
    pub verification_url: String,
}

fn issuer(ep: &AuthEndpoints) -> &str {
    ep.codex_issuer.trim_end_matches('/')
}

pub fn codex_start(
    http: &dyn HttpTransport,
    ep: &AuthEndpoints,
) -> Result<DeviceCode, AuthFailure> {
    let r = send(
        http,
        HttpRequest::post(
            format!("{}/api/accounts/deviceauth/usercode", issuer(ep)),
            "application/json",
            json!({"client_id": ep.codex_client_id})
                .to_string()
                .into_bytes(),
        ),
    )?;
    if !r.success() {
        return Err(AuthFailure::Status(r.status));
    }
    let body = object(&r, "device authorization")?;
    let id = text(&body, "device_auth_id").ok_or(AuthFailure::Malformed("device authorization"))?;
    let user_code = text(&body, "user_code")
        .or_else(|| text(&body, "usercode"))
        .ok_or(AuthFailure::Malformed("device authorization"))?;
    // The issuer sends the interval as a string; clamp it so a bad value never hammers or
    // stalls the poll.
    let seconds = body["interval"]
        .as_i64()
        .or_else(|| body["interval"].as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(-1);
    let interval_ms = if seconds < 1 {
        DEFAULT_INTERVAL_MS
    } else {
        (seconds * 1000).min(MAX_INTERVAL_MS)
    };
    Ok(DeviceCode {
        device_auth_id: Secret::new(id),
        user_code,
        interval_ms,
        verification_url: format!("{}/codex/device", issuer(ep)),
    })
}

/// The approved authorization (code and verifier), or `None` while the code is not entered yet.
pub fn codex_poll(
    http: &dyn HttpTransport,
    ep: &AuthEndpoints,
    device: &DeviceCode,
) -> Result<Option<(Secret, Secret)>, AuthFailure> {
    let r = send(
        http,
        HttpRequest::post(
            format!("{}/api/accounts/deviceauth/token", issuer(ep)),
            "application/json",
            json!({"device_auth_id": device.device_auth_id.expose(), "user_code": device.user_code})
                .to_string()
                .into_bytes(),
        ),
    )?;
    // The issuer answers 403/404 until the code is entered, as the Codex CLI expects.
    if matches!(r.status, 403 | 404) {
        return Ok(None);
    }
    if !r.success() {
        return Err(AuthFailure::Rejected(r.status));
    }
    let body = object(&r, "device authorization")?;
    match (
        text(&body, "authorization_code"),
        text(&body, "code_verifier"),
    ) {
        (Some(code), Some(verifier)) => Ok(Some((Secret::new(code), Secret::new(verifier)))),
        _ => Err(AuthFailure::Malformed("device authorization")),
    }
}

fn codex_token(
    http: &dyn HttpTransport,
    ep: &AuthEndpoints,
    pairs: &[(&str, &str)],
    previous: Option<&Tokens>,
    now_ms: i64,
) -> Result<Tokens, AuthFailure> {
    let r = send(
        http,
        HttpRequest::post(
            format!("{}/oauth/token", issuer(ep)),
            "application/x-www-form-urlencoded",
            form(pairs),
        ),
    )?;
    let t = tokens(&r, previous.map(|p| &p.refresh), now_ms, |body| {
        codex_account(body["id_token"].as_str())
            .or_else(|| codex_account(body["access_token"].as_str()))
    })?;
    // The backend needs the account ID beside every access token.
    let account = t
        .account
        .clone()
        .or_else(|| previous.and_then(|p| p.account.clone()))
        .ok_or(AuthFailure::Malformed("token response"))?;
    Ok(Tokens {
        account: Some(account),
        ..t
    })
}

pub fn codex_exchange(
    http: &dyn HttpTransport,
    ep: &AuthEndpoints,
    code: &Secret,
    verifier: &Secret,
    now_ms: i64,
) -> Result<Tokens, AuthFailure> {
    let redirect = format!("{}/deviceauth/callback", issuer(ep));
    codex_token(
        http,
        ep,
        &[
            ("grant_type", "authorization_code"),
            ("code", code.expose()),
            ("redirect_uri", &redirect),
            ("client_id", &ep.codex_client_id),
            ("code_verifier", verifier.expose()),
        ],
        None,
        now_ms,
    )
}

pub fn codex_refresh(
    http: &dyn HttpTransport,
    ep: &AuthEndpoints,
    previous: &Tokens,
    now_ms: i64,
) -> Result<Tokens, AuthFailure> {
    codex_token(
        http,
        ep,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", previous.refresh.expose()),
            ("client_id", &ep.codex_client_id),
        ],
        Some(previous),
        now_ms,
    )
}

// ---------------------------------------------------------------------------------------------
// Claude PKCE

/// The PKCE S256 challenge of `verifier`.
pub fn challenge(verifier: &str) -> String {
    base64url(sha2::Sha256::digest(verifier.as_bytes()).as_slice())
}

/// The browser authorization URL (reference `StartLogin`).
pub fn claude_authorize_url(ep: &AuthEndpoints, verifier: &Secret, state: &Secret) -> String {
    let query = [
        ("code", "true"),
        ("client_id", ep.claude_client_id.as_str()),
        ("response_type", "code"),
        ("redirect_uri", ep.claude_redirect_uri.as_str()),
        ("scope", ep.claude_scope.as_str()),
        ("code_challenge", &challenge(verifier.expose())),
        ("code_challenge_method", "S256"),
        ("state", state.expose()),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={}", percent(v)))
    .collect::<Vec<_>>()
    .join("&");
    format!("{}?{query}", ep.claude_authorize_url)
}

/// Splits the pasted `code#state` value and checks the state is this sign-in's (reference
/// `ClaudeLoginCommand.CompleteAsync`). A model-generated or foreign value cannot pass.
pub fn claude_pasted_code(pasted: &Secret, state: &Secret) -> Result<Secret, &'static str> {
    let trimmed = pasted.expose().trim();
    let Some((code, got)) = trimmed.split_once('#') else {
        return Err("expected the code#state value Claude shows");
    };
    if code.is_empty() || got.is_empty() {
        return Err("expected the code#state value Claude shows");
    }
    // Constant-time comparison of the state.
    let (a, b) = (got.as_bytes(), state.expose().as_bytes());
    let mut diff = a.len() ^ b.len();
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= usize::from(x ^ y);
    }
    if diff != 0 {
        return Err("the authorization state did not match this sign-in");
    }
    Ok(Secret::new(code))
}

fn claude_token(
    http: &dyn HttpTransport,
    ep: &AuthEndpoints,
    body: Value,
    previous: Option<&Tokens>,
    now_ms: i64,
) -> Result<Tokens, AuthFailure> {
    let r = send(
        http,
        HttpRequest::post(
            ep.claude_token_url.clone(),
            "application/json",
            body.to_string().into_bytes(),
        ),
    )?;
    let t = tokens(&r, previous.map(|p| &p.refresh), now_ms, |body| {
        body["account"]["uuid"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    })?;
    Ok(Tokens {
        account: t
            .account
            .clone()
            .or_else(|| previous.and_then(|p| p.account.clone())),
        ..t
    })
}

pub fn claude_exchange(
    http: &dyn HttpTransport,
    ep: &AuthEndpoints,
    code: &Secret,
    state: &Secret,
    verifier: &Secret,
    now_ms: i64,
) -> Result<Tokens, AuthFailure> {
    claude_token(
        http,
        ep,
        json!({
            "grant_type": "authorization_code",
            "code": code.expose(),
            "state": state.expose(),
            "client_id": ep.claude_client_id,
            "redirect_uri": ep.claude_redirect_uri,
            "code_verifier": verifier.expose(),
        }),
        None,
        now_ms,
    )
}

pub fn claude_refresh(
    http: &dyn HttpTransport,
    ep: &AuthEndpoints,
    previous: &Tokens,
    now_ms: i64,
) -> Result<Tokens, AuthFailure> {
    claude_token(
        http,
        ep,
        json!({
            "grant_type": "refresh_token",
            "refresh_token": previous.refresh.expose(),
            "client_id": ep.claude_client_id,
        }),
        Some(previous),
        now_ms,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(claims: Value) -> String {
        format!("e30.{}.sig", base64url(claims.to_string().as_bytes()))
    }

    #[test]
    fn codex_account_is_read_like_the_codex_cli() {
        let a = jwt(json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct-1"}}));
        assert_eq!(codex_account(Some(&a)).as_deref(), Some("acct-1"));
        let b = jwt(json!({"organizations": [{"id": "org-2"}]}));
        assert_eq!(codex_account(Some(&b)).as_deref(), Some("org-2"));
        assert_eq!(codex_account(Some("not-a-jwt")), None);
    }

    #[test]
    fn the_pasted_code_must_carry_this_sign_ins_state() {
        let state = Secret::new("s-123");
        assert_eq!(
            claude_pasted_code(&Secret::new(" abc#s-123 \n"), &state).unwrap(),
            Secret::new("abc")
        );
        assert!(claude_pasted_code(&Secret::new("abc#s-999"), &state).is_err());
        assert!(claude_pasted_code(&Secret::new("abc"), &state).is_err());
        assert!(claude_pasted_code(&Secret::new("#s-123"), &state).is_err());
    }

    #[test]
    fn credentials_round_trip_through_their_stored_form() {
        let c = Credential::OAuth(Tokens {
            access: Secret::new("at"),
            refresh: Secret::new("rt"),
            expires_at_ms: 42,
            account: Some("acct".into()),
        });
        assert_eq!(Credential::parse(&c.to_secret()), Some(c.clone()));
        let k = Credential::ApiKey(Secret::new("sk"));
        assert_eq!(Credential::parse(&k.to_secret()), Some(k));
        assert!(!format!("{c:?}").contains("rt"));
    }
}
