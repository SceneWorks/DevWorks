//! Endpoint profiles (ROUTE-09, ROUTE-10): how each connection kind authenticates, which
//! endpoint it defaults to and what it may be configured with. The subscription OAuth endpoints
//! and public client identities are the simple-router reference's (`RouterOptions.cs`:
//! `ClaudeOptions`, `CodexOptions`), overridable through [`AuthEndpoints`] so tests point them at
//! a local fake.
use crate::fingerprint::sha256_hex;

/// How a profile obtains its credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Auth {
    /// Codex subscription: OpenAI device-code sign-in (the Codex CLI's public client).
    DeviceCode,
    /// Claude subscription: browser authorization-code sign-in with PKCE.
    AuthorizationCode,
    /// An API key; `required` is false for compatible endpoints (Ollama, LM Studio).
    ApiKey { required: bool },
}

#[derive(Clone, Copy, Debug)]
pub struct Profile {
    /// `EndpointKind` wire name.
    pub name: &'static str,
    pub label: &'static str,
    pub auth: Auth,
    /// Subscription billing (never API billing).
    pub subscription: bool,
    /// Disabled until a human accepts [`RISK_NOTICE`].
    pub risk_gated: bool,
    /// Plain HTTP is an explicit-configuration exception for local/compatible endpoints only.
    pub allows_http: bool,
    pub default_endpoint: &'static str,
}

pub const PROFILES: [Profile; 8] = [
    Profile {
        name: "CodexSubscriptionResponses",
        label: "Codex subscription",
        auth: Auth::DeviceCode,
        subscription: true,
        risk_gated: false,
        allows_http: false,
        default_endpoint: "https://chatgpt.com/backend-api/codex",
    },
    Profile {
        name: "ClaudeSubscriptionMessages",
        label: "Claude subscription",
        auth: Auth::AuthorizationCode,
        subscription: true,
        risk_gated: true,
        allows_http: false,
        default_endpoint: "https://api.anthropic.com",
    },
    Profile {
        name: "AnthropicMessages",
        label: "Anthropic API key",
        auth: Auth::ApiKey { required: true },
        subscription: false,
        risk_gated: false,
        allows_http: false,
        default_endpoint: "https://api.anthropic.com",
    },
    Profile {
        name: "OpenRouterChat",
        label: "OpenRouter",
        auth: Auth::ApiKey { required: true },
        subscription: false,
        risk_gated: false,
        allows_http: false,
        default_endpoint: "https://openrouter.ai/api/v1",
    },
    Profile {
        name: "OpenAiResponses",
        label: "OpenAI API key",
        auth: Auth::ApiKey { required: true },
        subscription: false,
        risk_gated: false,
        allows_http: false,
        default_endpoint: "https://api.openai.com/v1",
    },
    Profile {
        name: "CompatibleChat",
        label: "OpenAI-compatible endpoint",
        auth: Auth::ApiKey { required: false },
        subscription: false,
        risk_gated: false,
        allows_http: true,
        default_endpoint: "http://localhost:11434/v1",
    },
    Profile {
        name: "Embedding",
        label: "Compatible embedding endpoint",
        auth: Auth::ApiKey { required: false },
        subscription: false,
        risk_gated: false,
        allows_http: true,
        default_endpoint: "http://localhost:11434/v1",
    },
    Profile {
        name: "SystemOne",
        label: "SystemOne decision endpoint",
        auth: Auth::ApiKey { required: false },
        subscription: false,
        risk_gated: false,
        allows_http: true,
        default_endpoint: "http://localhost:8000/v1",
    },
];

pub fn profile(name: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|p| p.name == name)
}

impl Profile {
    pub fn oauth(&self) -> bool {
        matches!(self.auth, Auth::DeviceCode | Auth::AuthorizationCode)
    }
    pub fn key_required(&self) -> bool {
        matches!(self.auth, Auth::ApiKey { required: true })
    }
    /// Whether a connection of this profile needs a credential to be usable.
    pub fn needs_credential(&self) -> bool {
        self.oauth() || self.key_required()
    }
}

/// The exact notice a human accepts before a Claude subscription connection is enabled
/// (ROUTE-10). Its SHA-256 is what `SubscriptionRisk` must name; any change to this text
/// revokes every earlier acceptance.
pub const RISK_NOTICE: &str = "Claude subscription access in DevWorks is not supported by \
Anthropic for third-party applications. DevWorks signs in to your Claude account directly, as \
the simple-router reference does, without a Claude CLI. Anthropic may refuse, limit or stop this \
access at any time, it may conflict with Anthropic's terms for your account, and compatibility is \
not guaranteed. Requests through this connection use your Claude subscription, never Anthropic \
API billing: DevWorks never switches this connection to an API key, or an API-key connection to \
your subscription. You use this connection at your own risk.";

pub fn risk_notice_sha256() -> String {
    sha256_hex(RISK_NOTICE.as_bytes())
}

/// What a risk acceptance covers: the profile, the exact endpoint and the account (empty before
/// the first sign-in). A change to any of them revokes the acceptance.
pub fn risk_binding(profile: &str, endpoint: &str, account_digest: Option<&str>) -> String {
    sha256_hex(format!("{profile}\n{endpoint}\n{}", account_digest.unwrap_or("")).as_bytes())
}

/// The digest an account is stored under (never the account identifier itself).
pub fn account_digest(account: &str) -> String {
    sha256_hex(format!("devworks-account\n{account}").as_bytes())
}

/// The origin (`scheme://host[:port]`) of an endpoint, lowercased.
pub fn origin(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    format!(
        "{}://{}",
        scheme.to_ascii_lowercase(),
        host.to_ascii_lowercase()
    )
}

/// Validates an endpoint for `profile`: an absolute http(s) URL without credentials, query or
/// fragment; HTTPS unless the profile allows an explicitly configured plain-HTTP endpoint.
pub fn check_endpoint(profile: &Profile, url: &str) -> Result<(), &'static str> {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err("endpoint");
    };
    match scheme {
        "https" => {}
        "http" if profile.allows_http => {}
        _ => return Err("endpoint_scheme"),
    }
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty()
        || authority.contains('@')
        || url.contains(['?', '#'])
        || url.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err("endpoint");
    }
    Ok(())
}

/// The OAuth endpoints and public client identities of the subscription profiles.
#[derive(Clone, Debug)]
pub struct AuthEndpoints {
    pub claude_authorize_url: String,
    pub claude_token_url: String,
    pub claude_client_id: String,
    pub claude_redirect_uri: String,
    pub claude_scope: String,
    pub codex_issuer: String,
    pub codex_client_id: String,
    /// Sent as `client_version` on the Codex model list (the backend hides models a client is
    /// too old for, so the reference sits far ahead of real releases).
    pub codex_client_version: String,
}
impl Default for AuthEndpoints {
    fn default() -> Self {
        Self {
            claude_authorize_url: "https://claude.ai/oauth/authorize".into(),
            claude_token_url: "https://console.anthropic.com/v1/oauth/token".into(),
            claude_client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e".into(),
            claude_redirect_uri: "https://console.anthropic.com/oauth/code/callback".into(),
            claude_scope: "org:create_api_key user:profile user:inference".into(),
            codex_issuer: "https://auth.openai.com".into(),
            codex_client_id: "app_EMoamEEZ73f0CkXaXp7hrann".into(),
            codex_client_version: "99.0.0".into(),
        }
    }
}

/// The token endpoints rate-limit generic client user agents (reference `UserAgent`).
pub const AUTH_USER_AGENT: &str = "simple-router/1.0";
/// The Codex backend's originator and user agent (reference `CodexUpstream.Originator`).
pub const CODEX_ORIGINATOR: &str = "codex_cli_rs";
/// The subscription OAuth beta flag (reference `ClaudeUpstream.OAuthBeta`); never sent on the
/// API-key profile.
pub const CLAUDE_OAUTH_BETA: &str = "oauth-2025-04-20";
/// The pinned Messages API version (reference `DefaultAnthropicVersion`).
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_checked_per_profile() {
        let anthropic = profile("AnthropicMessages").unwrap();
        let compatible = profile("CompatibleChat").unwrap();
        assert!(check_endpoint(anthropic, "https://api.anthropic.com").is_ok());
        assert_eq!(
            check_endpoint(anthropic, "http://api.anthropic.com"),
            Err("endpoint_scheme")
        );
        assert!(check_endpoint(compatible, "http://localhost:1234/v1").is_ok());
        assert!(check_endpoint(compatible, "http://user:pw@host/v1").is_err());
        assert!(check_endpoint(compatible, "http://host/v1?key=x").is_err());
        assert_eq!(
            origin("HTTPS://API.Example.com:8443/v1"),
            "https://api.example.com:8443"
        );
    }

    #[test]
    fn the_risk_binding_changes_with_profile_endpoint_and_account() {
        let base = risk_binding(
            "ClaudeSubscriptionMessages",
            "https://api.anthropic.com",
            None,
        );
        assert_ne!(
            base,
            risk_binding("ClaudeSubscriptionMessages", "https://proxy.example", None)
        );
        assert_ne!(
            base,
            risk_binding(
                "ClaudeSubscriptionMessages",
                "https://api.anthropic.com",
                Some("a")
            )
        );
        assert_eq!(risk_notice_sha256().len(), 64);
    }
}
