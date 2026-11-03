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

/// JSON member names whose values are always redacted (compared case-insensitively, ignoring
/// `_` and `-`).
const SENSITIVE_FIELDS: [&str; 12] = [
    "password",
    "passphrase",
    "secret",
    "clientsecret",
    "token",
    "accesstoken",
    "refreshtoken",
    "apikey",
    "authorization",
    "cookie",
    "privatekey",
    "credential",
];

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
        let mut out = text.to_owned();
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
                    let reference = SecretRef::from_setting(v).is_some();
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
    SENSITIVE_FIELDS
        .iter()
        .any(|s| norm == *s || norm.ends_with(s))
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
            pending = word
                .strip_suffix([':', '='])
                .is_some_and(|k| sensitive(k.trim_start_matches(['-', '?'])));
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

/// Long unbroken base64/hex-like runs with mixed character classes (likely opaque credentials).
fn looks_random(word: &str) -> bool {
    word.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+' | b'/' | b'='))
        && word.bytes().any(|b| b.is_ascii_digit())
        && word.bytes().any(|b| b.is_ascii_uppercase())
        && word.bytes().any(|b| b.is_ascii_lowercase())
}
