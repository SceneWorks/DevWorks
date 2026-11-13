//! Secret references and redaction for logs, diagnostics, errors and exports (SEC-01).
//!
//! Application records hold only opaque [`SecretRef`]s in the settings format
//! (`dwsecret-` and an opaque id, minted by [`mint_secret_ref`]); values are resolved by the
//! owning provider transport (the OS secret store adapter is its own item).
//!
//! # What [`Redactor`] removes
//!
//! * **Known values.** Every occurrence of a value registered on the redactor
//!   ([`Redactor::register`]) or in the process-wide registry of resolved secrets
//!   ([`register_resolved_secret`], which the production secret resolver and the provider
//!   credential reader call). A redactor ignores values shorter than 4 bytes; the registry,
//!   which every redactor in the process applies, ignores values shorter than
//!   [`RESOLVED_VALUE_MIN`] bytes, because a short value would shred identifiers everywhere
//!   (such a value is still redacted beside a sensitive name). The registry keeps the
//!   [`RESOLVED_LIMIT`] most recently registered values of at most [`RESOLVED_VALUE_MAX`] bytes
//!   each; the oldest is evicted (its buffer cleared) when a new one arrives.
//! * **Private-key PEM blocks** (`-----BEGIN ... PRIVATE KEY-----` or `ENCRYPTED`), including
//!   an unterminated one. Certificates and other public blocks are kept.
//! * **Sensitive names.** A name is sensitive when, lowercased with `_` and `-` removed, it
//!   contains `password`, `passwd`, `passphrase`, `secret`, `apikey`, `accesskey`,
//!   `privatekey` or `credential`, or ends with `token`, `authorization` or `cookie`, and does
//!   not end with a metadata suffix (`id`, `ref`, `revision`, `count`, `hash`, ...). So
//!   `apiKey`, `api_key`, `API-KEY` and `credentials` are sensitive; `credentialRef` and
//!   `tokenCount` are not. Under a sensitive JSON key the whole value is redacted: a string or
//!   number becomes `[REDACTED]`, and every string and number inside an object or array does
//!   too. Secret references, `null`, booleans and empty strings are kept.
//! * **JSON embedded in text.** An object or array that parses as JSON anywhere in the text is
//!   redacted as above (and written back compactly) when the text mentions a sensitive name or
//!   holds a `\u` escape (which can spell one).
//! * **Per line**: the value after a credential header (`Authorization:`,
//!   `Proxy-Authorization:`, `Cookie:`, `X-Api-Key:`); `name=value`, `name:value`,
//!   `name: value`, `"name":"value"`, `"name": value`, `--name value` and `--name=value` for a
//!   sensitive name (a value inside JSON the embedded-JSON pass parsed is left to that pass,
//!   which keeps objects, booleans and nulls); URL userinfo passwords (`scheme://user:[REDACTED]@host`); `-pVALUE` with
//!   a mixed letter-and-digit value of at least 6 bytes.
//! * **Credential-shaped words**: the settings [`looks_like_credential`] prefixes (`ghp_`,
//!   `github_pat_`, `sk-`, `xoxb-`, `AKIA`, `glpat-`, ...), JSON Web Tokens (two or three
//!   `.`-separated base64url segments whose first two start with `eyJ`), Google OAuth access
//!   tokens (`ya29.`), and 40-byte base64 runs with upper case, lower case and a digit (the
//!   shape of an AWS secret access key).
//!
//! Secret references are always kept. File paths, URLs without userinfo, version strings and
//! lowercase hex digests are not credential-shaped. Anything else, for example an unregistered
//! value with no recognisable shape and no sensitive name beside it, or JSON cut off before
//! it closes, is not caught: the redactor cannot prove arbitrary text contains no secret.
use crate::settings::validate::{is_secret_ref, looks_like_credential, mint_secret_ref};
use serde_json::{Map, Value};
use std::collections::VecDeque;
use std::fmt;
use std::sync::RwLock;

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
/// Field-name suffixes of references and metadata about a secret, never the secret itself
/// (`credentialRevision`, `credentialRef`, `secretId`, ...).
const METADATA_SUFFIXES: [&str; 12] = [
    "revision",
    "ref",
    "refs",
    "generation",
    "id",
    "ids",
    "count",
    "digest",
    "hash",
    "kind",
    "version",
    "estimate",
];

/// The most resolved secret values the process-wide registry keeps.
pub const RESOLVED_LIMIT: usize = 256;
/// The longest resolved value the registry keeps, in bytes (longer values are not registered).
pub const RESOLVED_VALUE_MAX: usize = 16 * 1024;
/// The shortest resolved value the registry keeps, in bytes: the registry applies to all text
/// in the process, so a shorter value would shred identifiers and ordinary words.
pub const RESOLVED_VALUE_MIN: usize = 8;
/// The shortest value a single redactor registers: shorter ones would shred ordinary text.
const KNOWN_VALUE_MIN: usize = 4;

/// Resolved secret values, oldest first, shared by every [`Redactor`] in the process.
static RESOLVED: RwLock<VecDeque<String>> = RwLock::new(VecDeque::new());

/// Registers a secret value the host resolved (a settings secret reference, a provider
/// credential) so every [`Redactor`] in the process removes it from later text. Bounded: the
/// [`RESOLVED_LIMIT`] most recent values of [`RESOLVED_VALUE_MIN`] to [`RESOLVED_VALUE_MAX`]
/// bytes are kept and an evicted value's buffer is cleared. Registering never logs or persists
/// the value.
pub fn register_resolved_secret(value: &str) {
    if !(RESOLVED_VALUE_MIN..=RESOLVED_VALUE_MAX).contains(&value.len()) {
        return;
    }
    let mut known = RESOLVED.write().unwrap_or_else(|p| p.into_inner());
    if let Some(i) = known.iter().position(|k| k == value) {
        // Most recently resolved last, so a value in use is evicted last.
        let again = known.remove(i).expect("position is in range");
        known.push_back(again);
        return;
    }
    if known.len() >= RESOLVED_LIMIT
        && let Some(evicted) = known.pop_front()
    {
        clear(evicted);
    }
    known.push_back(value.to_owned());
}

/// Overwrites a dropped secret's buffer (best effort, as [`crate::ports::secrets::Secret`]).
fn clear(value: String) {
    let mut bytes = value.into_bytes();
    bytes.iter_mut().for_each(|b| *b = 0);
    std::hint::black_box(&bytes);
}

/// Redacts known values and recognizable credential shapes (see the module documentation for
/// exactly what).
#[derive(Clone, Debug, Default)]
pub struct Redactor {
    known: Vec<String>,
}
impl Redactor {
    pub fn new() -> Self {
        Self::default()
    }
    /// Registers a secret value on this redactor only, so every occurrence is removed. Values
    /// shorter than 4 bytes are ignored to avoid shredding ordinary text.
    pub fn register(&mut self, value: &str) {
        if value.len() >= KNOWN_VALUE_MIN && !self.known.iter().any(|k| k == value) {
            self.known.push(value.to_owned());
        }
    }

    pub fn redact_text(&self, text: &str) -> String {
        let mut out = redact_pem(text);
        {
            let resolved = RESOLVED.read().unwrap_or_else(|p| p.into_inner());
            let mut known: Vec<&str> = self
                .known
                .iter()
                .chain(resolved.iter())
                .map(String::as_str)
                .collect();
            // Longest first, so a value containing another is removed whole.
            known.sort_by_key(|k| std::cmp::Reverse(k.len()));
            known.dedup();
            for k in known {
                if out.contains(k) {
                    out = out.replace(k, REDACTED);
                }
            }
        }
        let (out, parsed) = self.redact_embedded_json(&out);
        let mut result = String::with_capacity(out.len());
        let mut offset = 0;
        for line in out.split_inclusive('\n') {
            result.push_str(&redact_line(line, offset, &parsed));
            offset += line.len();
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
                    let redacted = if sensitive(k) {
                        redact_subtree(v)
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

    /// JSON objects and arrays embedded in `text`, redacted as [`Self::redact_json`] does and
    /// written back compactly; text that does not parse, or needs no redaction, is kept as is.
    /// Also returns the byte spans of the output that parsed as JSON and were handled
    /// structurally, which the line pass's name-then-value rule leaves alone.
    fn redact_embedded_json(&self, text: &str) -> (String, Vec<(usize, usize)>) {
        let mut parsed = Vec::new();
        // A `\u` escape can spell a sensitive key the raw text does not show.
        if !text.contains("\\u") && !mentions_sensitive_name(text) {
            return (text.to_owned(), parsed);
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(i) = rest.find(['{', '[']) {
            let mut values = serde_json::Deserializer::from_str(&rest[i..]).into_iter::<Value>();
            match values.next() {
                Some(Ok(v)) if v.is_object() || v.is_array() => {
                    let end = i + values.byte_offset();
                    out.push_str(&rest[..i]);
                    let start = out.len();
                    let redacted = self.redact_json(&v);
                    match serde_json::to_string(&redacted) {
                        Ok(s) if redacted != v => out.push_str(&s),
                        _ => out.push_str(&rest[i..end]),
                    }
                    parsed.push((start, out.len()));
                    rest = &rest[end..];
                }
                _ => {
                    out.push_str(&rest[..=i]);
                    rest = &rest[i + 1..];
                }
            }
        }
        out.push_str(rest);
        (out, parsed)
    }
}

/// A value under a sensitive key: every string and number in it is redacted; secret
/// references, `null`, booleans and empty strings are kept, as is the shape of objects and
/// arrays (so a `{"secretRef": ...}` credential setting survives).
fn redact_subtree(value: &Value) -> Value {
    match value {
        Value::String(s) if s.is_empty() || is_secret_ref(s) => value.clone(),
        Value::String(_) | Value::Number(_) => Value::String(REDACTED.into()),
        Value::Array(items) => Value::Array(items.iter().map(redact_subtree).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), redact_subtree(v)))
                .collect(),
        ),
        Value::Null | Value::Bool(_) => value.clone(),
    }
}

/// Whether `text` mentions a sensitive name fragment at all (the cheap gate before parsing
/// embedded JSON).
fn mentions_sensitive_name(text: &str) -> bool {
    let norm: String = text
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .collect::<String>()
        .to_lowercase();
    SENSITIVE_PARTS
        .iter()
        .chain(SENSITIVE_SUFFIXES.iter())
        .any(|s| norm.contains(s))
}

/// Whether a field, parameter or header name marks its value as a secret (the same rule the
/// JSON and `key=value` redaction applies).
pub fn sensitive_name(name: &str) -> bool {
    sensitive(name)
}

fn sensitive(field: &str) -> bool {
    let norm: String = field
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .collect::<String>()
        .to_lowercase();
    !norm.is_empty()
        && !METADATA_SUFFIXES.iter().any(|s| norm.ends_with(s))
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
/// `offset` is the line's byte position in the text and `parsed` the spans of that text the
/// embedded-JSON pass handled structurally: a value word starting inside one is kept.
fn redact_line(line: &str, offset: usize, parsed: &[(usize, usize)]) -> String {
    let structured = |at: usize| parsed.iter().any(|&(s, e)| (s..e).contains(&(offset + at)));
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
    // The previous word was a bare sensitive name (`"password"` before `:` in JSON).
    let mut after_name = false;
    let mut flush = |word: &mut String, start: usize, out: &mut String| {
        if word.is_empty() {
            return;
        }
        // JSON escapes inside a logged string (`{\"password\":\"x\"}`) leave a `\` on words.
        let bare = word.trim_matches('\\');
        if pending {
            // A secret reference is not a secret, and a value inside JSON the embedded-JSON
            // pass parsed was already redacted structurally (objects, booleans and nulls kept).
            if is_secret_ref(bare) || structured(start) {
                out.push_str(word);
            } else {
                out.push_str(REDACTED);
            }
            pending = false;
            after_name = false;
        } else if after_name && (bare == ":" || bare == "=") {
            // `"password":"value"` and `"password": value`.
            out.push_str(word);
            pending = true;
            after_name = false;
        } else {
            out.push_str(&redact_word(word));
            // `token: value`, `password = value` and `--password value` / `--token value`.
            pending = word
                .strip_suffix([':', '='])
                .is_some_and(|k| sensitive(k.trim_start_matches(['-', '?'])))
                || (word.starts_with('-')
                    && !word.contains('=')
                    && sensitive(word.trim_start_matches('-')));
            after_name = !pending && !bare.contains([':', '=']) && sensitive(bare);
        }
        word.clear();
    };
    let mut start = 0;
    for (at, c) in line.char_indices() {
        if c.is_whitespace() || c == '"' || c == '\'' || c == ',' || c == '&' {
            flush(&mut word, start, &mut out);
            out.push(c);
        } else {
            if word.is_empty() {
                start = at;
            }
            word.push(c);
        }
    }
    flush(&mut word, start, &mut out);
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
    // Known secret shapes only (token prefixes, JWTs, private keys): no general shape guessing,
    // which rewrote paths, branch names and digests.
    if looks_like_credential(word) || credential_shaped(word) {
        return REDACTED.into();
    }
    word.to_owned()
}

/// Credential shapes that may contain `.`, `/` or `+`, which the settings prefix heuristic
/// rejects: JSON Web Tokens, Google OAuth access tokens and AWS secret access keys. Surrounding
/// punctuation (`(`, `)`, `;`, `<`, `>`, a trailing `.`) is ignored.
fn credential_shaped(word: &str) -> bool {
    let token = word.trim_matches(|c: char| "()[]{}<>;`".contains(c));
    let base64url = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    };
    let segments: Vec<&str> = token.trim_end_matches('.').split('.').collect();
    if (2..=3).contains(&segments.len())
        && segments[0].starts_with("eyJ")
        && segments[1].starts_with("eyJ")
        && token.len() >= 20
        && segments.iter().all(|s| base64url(s))
    {
        return true;
    }
    if let Some(rest) = token.strip_prefix("ya29.")
        && rest.len() >= 20
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
    {
        return true;
    }
    token.len() == 40
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'/' || b == b'+')
        && token.bytes().any(|b| b.is_ascii_uppercase())
        && token.bytes().any(|b| b.is_ascii_lowercase())
        && token.bytes().any(|b| b.is_ascii_digit())
        && !token.starts_with('/')
        && token.matches('/').count() <= 3
}

fn redact_word_tail(tail: &str) -> String {
    if looks_like_credential(tail) || credential_shaped(tail) {
        REDACTED.to_owned()
    } else {
        tail.to_owned()
    }
}

/// A domain decision with secrets redacted, for receipts (persisted and returned): typed
/// secret-valued fields and known secret patterns only. A decision whose redacted form no
/// longer decodes is refused (`Err`), never stored unredacted.
pub fn redact_decision(
    decision: crate::protocol::results::DomainDecision,
) -> Result<crate::protocol::results::DomainDecision, String> {
    let value = serde_json::to_value(&decision).map_err(|e| e.to_string())?;
    let redacted = Redactor::new().redact_json(&value);
    if redacted == value {
        return Ok(decision);
    }
    serde_json::from_value(redacted)
        .map_err(|_| "the decision carries a secret in a field that cannot hold a redaction".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn values_under_sensitive_json_keys_are_redacted_in_text_and_json() {
        let r = Redactor::new();
        // (value, secret, flat): a flat `"name":"value"` pair is also caught when the JSON is
        // escaped inside another string, where it no longer parses.
        let cases = [
            (json!({"password": "hunter2"}), "hunter2", true),
            (
                json!({"credentials": {"value": "x7-cred"}}),
                "x7-cred",
                false,
            ),
            (json!({"apiKeys": ["x7-key"]}), "x7-key", false),
            (json!({"apiKey": "x7-camel"}), "x7-camel", true),
            (json!({"api_key": 918273}), "918273", false),
            (json!({"API-KEY": "x7-kebab"}), "x7-kebab", true),
        ];
        for (value, secret, flat) in cases {
            // JSON form.
            let redacted = r.redact_json(&value).to_string();
            assert!(!redacted.contains(secret), "{redacted}");
            assert!(redacted.contains(REDACTED), "{redacted}");
            // Line forms: compact, pretty, embedded in a log line, escaped inside a string.
            let compact = value.to_string();
            let pretty = serde_json::to_string_pretty(&value).unwrap();
            let embedded = format!("provider said {compact} (status 401)\n");
            let escaped = serde_json::to_string(&compact).unwrap();
            let mut texts = vec![&compact, &pretty, &embedded];
            if flat {
                texts.push(&escaped);
            }
            for text in texts {
                let out = r.redact_text(text);
                assert!(!out.contains(secret), "{text} -> {out}");
            }
        }
        // A cut-off JSON body still loses the value after a sensitive key.
        let out = r.redact_text(r#"{"error":"denied","password":"hunter2","#);
        assert!(!out.contains("hunter2"), "{out}");
        assert!(out.contains(r#""error":"denied""#), "{out}");
        // Metadata, references and ordinary fields are kept.
        let kept = json!({
            "credential": {"secretRef": "dwsecret-0123456789abcdef0123456789abcdef"},
            "credentialRevision": 3,
            "tokenCount": 12,
            "passwordSet": true,
            "user": "me",
        });
        assert_eq!(r.redact_json(&kept), kept);
        // Compact and pretty: the line pass keeps the objects, booleans and nulls the
        // structural pass kept.
        let pretty = serde_json::to_string_pretty(&kept).unwrap();
        for line in [kept.to_string(), pretty, r#"{"token": null}"#.to_owned()] {
            assert_eq!(r.redact_text(&line), line);
        }
        // A nested object under a sensitive key stays valid JSON, its value redacted.
        let nested = r.redact_text("{\n  \"credentials\": {\n    \"value\": \"x7-cred\"\n  }\n}");
        assert_eq!(
            serde_json::from_str::<Value>(&nested).unwrap(),
            json!({"credentials": {"value": REDACTED}})
        );
        // A value that only looks structured, outside parsed JSON, is still redacted.
        for line in ["cmd --password {orchard7}", "cmd --password [orchard7]"] {
            assert_eq!(r.redact_text(line), format!("cmd --password {REDACTED}"));
        }
        assert_eq!(
            r.redact_text("cmd --password true"),
            format!("cmd --password {REDACTED}")
        );
        // A sensitive key spelled with a JSON escape is still a sensitive key.
        let escaped_key = r#"{"password":"hunter2"}"#;
        let out = r.redact_text(escaped_key);
        assert!(!out.contains("hunter2"), "{out}");
        let value: Value = serde_json::from_str(escaped_key).unwrap();
        assert_eq!(r.redact_json(&value), json!({"password": REDACTED}));
    }

    #[test]
    fn tokens_with_dots_slashes_or_plus_signs_are_credential_shaped() {
        let r = Redactor::new();
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        let google = "ya29.a0AfH6SMBx7qLz9kR2Vt4Wd8Jp0Ny";
        let aws = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let text = format!("jwt {jwt}\ngoogle ({google})\naws {aws}\n");
        let out = r.redact_text(&text);
        for leaked in [jwt, google, aws] {
            assert!(!out.contains(leaked), "{leaked} in {out}");
        }
        // Paths, URLs without credentials, versions and digests are not credential-shaped.
        for ordinary in [
            "/Users/dev/projects/devworks/crates/devworks-core/src/authority/redact.rs",
            "src/services/diagnostics/HealthCheckReportBuilder2.rs",
            "https://example.com/api/v1/items?page=2&sort=Name",
            "https://github.com/SceneWorks/DevWorks/pull/12345/files",
            "1.2.3-beta.4+build.5",
            "v10.24.1",
            "0123456789abcdef0123456789abcdef01234567",
            "sha256:4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945",
        ] {
            let line = format!("at {ordinary} done\n");
            assert_eq!(r.redact_text(&line), line, "{ordinary}");
        }
    }

    #[test]
    fn resolved_secrets_are_redacted_by_every_redactor_and_the_registry_is_bounded() {
        // A value with no shape and no sensitive name beside it.
        let value = "orchard lantern qz5 resolved";
        let before = Redactor::new();
        register_resolved_secret(value);
        register_resolved_secret("rt-1abc"); // shorter than RESOLVED_VALUE_MIN: ignored
        let line = format!("upstream echoed {value} and part-rt-1abc");
        // Redactors made before and after registration both remove it.
        for r in [before, Redactor::new()] {
            assert_eq!(
                r.redact_text(&line),
                "upstream echoed [REDACTED] and part-rt-1abc"
            );
        }
        let too_long = "y".repeat(RESOLVED_VALUE_MAX + 1);
        register_resolved_secret(&too_long);
        assert!(!RESOLVED.read().unwrap().iter().any(|k| k == &too_long));
        // Filling the registry evicts the oldest value; re-resolving one keeps it newest.
        for i in 0..RESOLVED_LIMIT - 1 {
            register_resolved_secret(&format!("bulk-resolved-{i:04}"));
        }
        register_resolved_secret(value);
        register_resolved_secret("bulk-resolved-final");
        let known = RESOLVED.read().unwrap();
        assert_eq!(known.len(), RESOLVED_LIMIT);
        assert!(known.iter().any(|k| k == value));
        assert!(!known.iter().any(|k| k == "bulk-resolved-0000"));
    }
}
