//! Pure value validation shared by `settings.validate`, `settings.save_revision`, proposal
//! approval and the native view model, so every surface reports the same issues.
use super::catalog::{SettingDescriptor, ValueKind};
use serde_json::Value;

/// One validation finding. `blocking` issues refuse a save; advisory issues (for example a
/// directory that is currently unavailable) are reported but do not change the configured value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    /// The setting key, or `expectedRevision` / `entries` for request-level findings.
    pub path: String,
    pub code: &'static str,
    pub message: String,
    pub blocking: bool,
}
impl Issue {
    pub fn blocking(
        path: impl Into<String>,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            code,
            message: message.into(),
            blocking: true,
        }
    }
    pub fn advisory(
        path: impl Into<String>,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            code,
            message: message.into(),
            blocking: false,
        }
    }
}

/// Issue codes. Stable identifiers for the UI and agents.
pub mod codes {
    pub const UNKNOWN_KEY: &str = "setting.unknownKey";
    pub const SCOPE_NOT_ALLOWED: &str = "setting.scopeNotAllowed";
    pub const HARNESS_ENFORCED: &str = "policy.harnessEnforced";
    pub const SCHEMA_MISMATCH: &str = "schema.mismatch";
    pub const DUPLICATE_KEY: &str = "entries.duplicateKey";
    pub const STALE_REVISION: &str = "revision.stale";
    pub const TYPE: &str = "value.type";
    pub const RANGE: &str = "value.range";
    pub const CHOICE: &str = "value.choice";
    pub const PATH_INVALID: &str = "path.invalid";
    pub const PATH_NOT_ABSOLUTE: &str = "path.notAbsolute";
    pub const PATH_UNAVAILABLE: &str = "advisory.pathUnavailable";
    pub const RAW_CREDENTIAL: &str = "credential.rawValueRefused";
    pub const STORED_UNKNOWN: &str = "revision.unknownStoredKey";
}

/// Validates a configured (non-null) value against its descriptor's type.
pub fn check_value(d: &SettingDescriptor, v: &Value) -> Vec<Issue> {
    let key = d.key;
    let mut out = Vec::new();
    let ty = |expected: &str| {
        Issue::blocking(key, codes::TYPE, format!("{} expects {expected}", d.label))
    };
    match d.kind {
        ValueKind::Bool => {
            if !v.is_boolean() {
                out.push(ty("true or false"));
            }
        }
        ValueKind::Integer { min, max } => match v.as_i64() {
            Some(n) if (min..=max).contains(&n) => {}
            Some(_) => out.push(Issue::blocking(
                key,
                codes::RANGE,
                format!("{} must be between {min} and {max}", d.label),
            )),
            None => out.push(ty("a whole number")),
        },
        ValueKind::Choice(options) => match v.as_str() {
            Some(s) if options.contains(&s) => {}
            _ => out.push(Issue::blocking(
                key,
                codes::CHOICE,
                format!("{} must be one of {}", d.label, options.join(", ")),
            )),
        },
        ValueKind::Directory => match v.as_str() {
            Some(s) => out.extend(check_directory(key, s)),
            None => out.push(ty("a directory path")),
        },
        ValueKind::Instructions => match v.as_str() {
            Some(s) if s.chars().count() <= 65536 && !s.contains('\0') => {}
            Some(_) => out.push(Issue::blocking(
                key,
                codes::RANGE,
                "Instructions must be at most 65536 characters without NUL",
            )),
            None => out.push(ty("instruction text")),
        },
        ValueKind::ModelList => match v.as_array() {
            Some(items) => {
                let valid_id = |x: &Value| {
                    x.as_str().is_some_and(|s| {
                        !s.is_empty()
                            && s.len() <= 128
                            && s.bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                    })
                };
                let mut seen = std::collections::HashSet::new();
                if items.len() > 32
                    || !items.iter().all(valid_id)
                    || !items.iter().all(|x| seen.insert(x.as_str()))
                {
                    out.push(ty("at most 32 unique model IDs"));
                }
            }
            None => out.push(ty("a list of model IDs")),
        },
        ValueKind::SecretRef => {
            let reference = v.as_object().filter(|o| o.len() == 1).and_then(|o| {
                o.get("secretRef").and_then(Value::as_str).filter(|s| {
                    !s.is_empty()
                        && s.len() <= 128
                        && s.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                })
            });
            if reference.is_none() {
                // Anything but an opaque reference in a credential field is treated as a raw
                // credential attempt and never stored or echoed.
                out.push(Issue::blocking(
                    key,
                    codes::RAW_CREDENTIAL,
                    "Credential fields accept only a secret-store reference; the value was not stored",
                ));
            }
        }
    }
    if d.kind != ValueKind::SecretRef && contains_credential(v) {
        out.push(Issue::blocking(
            key,
            codes::RAW_CREDENTIAL,
            "The value looks like a credential; credentials belong in the secret store, not configuration",
        ));
    }
    out
}

/// Syntactic directory validation. Availability is a separate, advisory runtime check.
pub fn check_directory(key: &str, s: &str) -> Vec<Issue> {
    if s.is_empty() || s.len() > 4096 || s.chars().any(char::is_control) || s.trim() != s {
        return vec![Issue::blocking(
            key,
            codes::PATH_INVALID,
            "Enter a directory path without control characters or surrounding spaces",
        )];
    }
    let bytes = s.as_bytes();
    let windows_drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    let absolute = s.starts_with('/')
        || s == "~"
        || s.starts_with("~/")
        || windows_drive
        || s.starts_with("\\\\");
    if !absolute {
        return vec![Issue::blocking(
            key,
            codes::PATH_NOT_ABSOLUTE,
            "Use an absolute path or one starting with ~/",
        )];
    }
    if s.split(['/', '\\']).any(|c| c == "..") {
        return vec![Issue::blocking(
            key,
            codes::PATH_INVALID,
            "Path components cannot be ..",
        )];
    }
    Vec::new()
}

/// High-confidence credential shapes. Configuration stores only secret references; a value that
/// matches one of these is refused rather than persisted.
pub fn looks_like_credential(s: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "sk-",
        "sk_live_",
        "rk_live_",
        "xoxb-",
        "xoxp-",
        "xapp-",
        "AKIA",
        "ASIA",
        "glpat-",
    ];
    if s.contains("-----BEGIN") && s.contains("PRIVATE KEY") {
        return true;
    }
    s.split(|c: char| c.is_whitespace() || "\"'`,;=()[]{}<>".contains(c))
        .any(|token| {
            token.len() >= 20
                && PREFIXES.iter().any(|p| token.starts_with(p))
                && token[2..]
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        })
}

fn contains_credential(v: &Value) -> bool {
    match v {
        Value::String(s) => looks_like_credential(s),
        Value::Array(a) => a.iter().any(contains_credential),
        Value::Object(o) => o.values().any(contains_credential),
        _ => false,
    }
}
