//! Secret references and redaction for logs, diagnostics, errors and exports (SEC-01).
//!
//! Application records hold only opaque [`SecretRef`]s in the settings format
//! (`dwsecret-` and an opaque id, minted by [`mint_secret_ref`]); values are resolved by the
//! owning provider transport (the OS secret store adapter is its own item). [`Redactor`] removes
//! known secret values, credential headers, credential-shaped tokens (the settings
//! [`looks_like_credential`] heuristic plus long mixed-case opaque runs) and typed sensitive JSON
//! fields before text is persisted or exported, and leaves secret references intact. It cannot
//! prove arbitrary text contains no secret.
use crate::settings::validate::{is_secret_ref, looks_like_credential, mint_secret_ref};
use serde_json::{Map, Value};
use std::fmt;

/// Replacement text for a redacted value.
pub const REDACTED: &str = "[REDACTED]";

/// A host-minted secret-store reference. It is a reference, not a credential, so redaction
/// preserves it; its `Debug` shows only the reference.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct SecretRef(String);
impl SecretRef {
    /// The reference the secret store returns for a stored secret's 128-bit opaque id.
    pub fn mint(opaque_id: u128) -> Self {
        Self(mint_secret_ref(opaque_id))
    }
    /// Accepts exactly the settings reference format ([`is_secret_ref`]).
    pub fn parse(reference: &str) -> Option<Self> {
        is_secret_ref(reference).then(|| Self(reference.to_owned()))
    }
    /// Reads a credential setting value `{"secretRef": "<reference>"}`.
    pub fn from_setting(value: &Value) -> Option<Self> {
        let map = value.as_object().filter(|m| m.len() == 1)?;
        Self::parse(map.get("secretRef")?.as_str()?)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretRef({})", self.0)
    }
}

/// Field-name fragments that mark a value as sensitive wherever they appear in the normalized
/// name (case-insensitive, `_` and `-` ignored): `aws_secret_access_key`, `client_secret_id`, ...
const SENSITIVE_PARTS: [&str; 8] = [
    "password",
    "passwd",
    "passphrase",
    "secret",
    "apikey",
    "accesskey",
    "privatekey",
    "credential",
];
/// Field-name suffixes that mark a value as sensitive (`accessToken`, `Authorization`, ...).
const SENSITIVE_SUFFIXES: [&str; 3] = ["token", "authorization", "cookie"];

/// Redacts known values and recognizable credential shapes.
#[derive(Clone, Debug, Default)]
pub struct Redactor {
    known: Vec<String>,
}
impl Redactor {
    pub fn new() -> Self {
        Self::default()
    }
    /// Registers a resolved secret value so every occurrence is removed. Values shorter than 4
    /// bytes are ignored to avoid shredding ordinary text.
    pub fn register(&mut self, value: &str) {
        if value.len() >= 4 && !self.known.iter().any(|k| k == value) {
            self.known.push(value.to_owned());
            self.known.sort_by_key(|k| std::cmp::Reverse(k.len()));
        }
    }

    pub fn redact_text(&self, text: &str) -> String {
        let mut out = redact_pem(text);
        for k in &self.known {
            out = out.replace(k.as_str(), REDACTED);
        }
        let mut result = String::with_capacity(out.len());
        for line in out.split_inclusive('\n') {
            result.push_str(&redact_line(line));
        }
        result
    }

    pub fn redact_json(&self, value: &Value) -> Value {
        match value {
            Value::String(s) => Value::String(self.redact_text(s)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|v| self.redact_json(v)).collect())
            }
            Value::Object(map) => {
                let mut out = Map::new();
                for (k, v) in map {
                    // A secret reference under a credential field is kept: it is not a secret.
                    let reference = SecretRef::from_setting(v).is_some()
                        || v.as_str().is_some_and(is_secret_ref);
                    let redacted = if sensitive(k) && !v.is_null() && !reference {
                        Value::String(REDACTED.into())
                    } else {
                        self.redact_json(v)
                    };
                    out.insert(k.clone(), redacted);
                }
                Value::Object(out)
            }
            other => other.clone(),
        }
    }
}

fn sensitive(field: &str) -> bool {
    let norm: String = field
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .collect::<String>()
        .to_lowercase();
    !norm.is_empty()
        && (SENSITIVE_PARTS.iter().any(|s| norm.contains(s))
            || SENSITIVE_SUFFIXES.iter().any(|s| norm.ends_with(s)))
}

/// Private-key PEM blocks, however short, including an unterminated one.
fn redact_pem(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(begin) = rest.find("-----BEGIN ") {
        let header_end = rest[begin..].find('\n').map_or(rest.len(), |n| begin + n);
        let header = &rest[begin..header_end];
        if !(header.contains("PRIVATE KEY") || header.contains("ENCRYPTED")) {
            out.push_str(&rest[..header_end]);
            rest = &rest[header_end..];
            continue;
        }
        out.push_str(&rest[..begin]);
        out.push_str(REDACTED);
        rest = match rest[begin..].find("-----END ") {
            Some(n) => {
                let after = begin + n + "-----END ".len();
                match rest[after..].find("-----") {
                    Some(m) => &rest[after + m + 5..],
                    None => "",
                }
            }
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// Credential headers, `key=value` assignments of sensitive names and token-shaped words.
fn redact_line(line: &str) -> String {
    // ASCII lowercasing keeps byte offsets aligned with `line`.
    let lower = line.to_ascii_lowercase();
    for header in [
        "authorization:",
        "proxy-authorization:",
        "cookie:",
        "x-api-key:",
    ] {
        if let Some(i) = lower.find(header) {
            let end = line.len() - line.trim_end_matches(['\r', '\n']).len();
            let start = i + header.len();
            return format!(
                "{} {}{}",
                &line[..start],
                REDACTED,
                &line[line.len() - end..]
            );
        }
    }
    let mut out = String::with_capacity(line.len());
    let mut word = String::new();
    // `token: value` with a space: the value is the next word.
    let mut pending = false;
    let mut flush = |word: &mut String, out: &mut String| {
        if word.is_empty() {
            return;
        }
        if pending {
            out.push_str(REDACTED);
            pending = false;
        } else {
            out.push_str(&redact_word(word));
            // `token: value`, `password = value` and `--password value` / `--token value`.
            pending = word
                .strip_suffix([':', '='])
                .is_some_and(|k| sensitive(k.trim_start_matches(['-', '?'])))
                || (word.starts_with('-')
                    && !word.contains('=')
                    && sensitive(word.trim_start_matches('-')));
        }
        word.clear();
    };
    for c in line.chars() {
        if c.is_whitespace() || c == '"' || c == '\'' || c == ',' || c == '&' {
            flush(&mut word, &mut out);
            out.push(c);
        } else {
            word.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

fn redact_word(word: &str) -> String {
    // URL userinfo: `scheme://user:secret@host` keeps the scheme, user and host.
    if let Some(scheme_end) = word.find("://") {
        let authority_start = scheme_end + 3;
        let authority_end = word[authority_start..]
            .find(['/', '?', '#'])
            .map_or(word.len(), |n| authority_start + n);
        if let Some(at) = word[authority_start..authority_end].rfind('@') {
            let userinfo = &word[authority_start..authority_start + at];
            let user = userinfo.split(':').next().unwrap_or("");
            let kept = if userinfo.contains(':') {
                format!("{user}:{REDACTED}")
            } else {
                REDACTED.to_owned()
            };
            return format!(
                "{}{kept}{}",
                &word[..authority_start],
                redact_word_tail(&word[authority_start + at..])
            );
        }
    }
    // `-pSECRET` style short flags with an attached value that looks like a secret.
    if let Some(value) = word.strip_prefix("-p")
        && value.len() >= 6
        && value.bytes().any(|b| b.is_ascii_digit())
        && value.bytes().any(|b| b.is_ascii_alphabetic())
        && !value.contains('=')
    {
        return format!("-p{REDACTED}");
    }
    if let Some((k, v)) = word.split_once(['=', ':'])
        && !v.is_empty()
        && sensitive(k.trim_start_matches(['-', '?']))
    {
        let sep = &word[k.len()..k.len() + 1];
        return format!("{k}{sep}{REDACTED}");
    }
    if is_secret_ref(word) {
        return word.to_owned();
    }
    if looks_like_credential(word) || (word.len() >= 32 && looks_random(word)) {
        return REDACTED.into();
    }
    word.to_owned()
}

fn redact_word_tail(tail: &str) -> String {
    if looks_like_credential(tail) {
        REDACTED.to_owned()
    } else {
        tail.to_owned()
    }
}

/// A domain decision with credential-shaped content redacted, for receipts (persisted and
/// returned). A decision that would no longer decode after redaction is kept as it was; its
/// use case is responsible for not echoing secrets (settings refuse credentials outright).
pub fn redact_decision(
    decision: crate::protocol::results::DomainDecision,
) -> crate::protocol::results::DomainDecision {
    let Ok(value) = serde_json::to_value(&decision) else {
        return decision;
    };
    let redacted = Redactor::new().redact_json(&value);
    if redacted == value {
        return decision;
    }
    serde_json::from_value(redacted).unwrap_or(decision)
}

/// Long unbroken base64/hex-like runs with mixed character classes (likely opaque credentials).
fn looks_random(word: &str) -> bool {
    word.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+' | b'/' | b'='))
        && word.bytes().any(|b| b.is_ascii_digit())
        && word.bytes().any(|b| b.is_ascii_uppercase())
        && word.bytes().any(|b| b.is_ascii_lowercase())
}
