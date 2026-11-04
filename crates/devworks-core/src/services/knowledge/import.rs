//! User data import (`knowledge.import_user_data`): the user's own Knowledge, exported from a
//! harness memory or written by hand, in the built-in `devworks/knowledge-import` format. This is
//! not the excluded migration of legacy CodeGraph Knowledge: it reads one immutable artifact the
//! user saved, never a CodeGraph database or service.
//!
//! The retained CodeGraph `import_knowledge` behavior is preserved: each candidate carries the
//! importing agent's verification (`verified`, `stale`, `unverifiable`, `personal`); only
//! verified and unverifiable candidates are stored; an active entry of the same kind with the
//! same folded summary (or an earlier candidate of the batch) is a duplicate; the dry run is the
//! `knowledge.diagnostics` preview, and the import stores exactly the previewed candidates, all
//! or none, when its preview digest still matches.
use super::model::{fold, fold_tags, preview_digest};
use crate::fingerprint::sha256_hex;
use serde::Deserialize;

/// The import format's schema identity.
pub const IMPORT_SCHEMA_ID: &str = "devworks/knowledge-import";
pub const IMPORT_SCHEMA_REVISION: i64 = 1;
/// Largest import file read.
pub const MAX_IMPORT_BYTES: u64 = 4 * 1024 * 1024;
/// Most candidates in one import: every stored candidate is named in the write receipt, whose
/// identifier lists hold 128.
pub const MAX_CANDIDATES: usize = 128;

/// The JSON Schema of the import format (its digest is the `SchemaRef` digest).
pub const IMPORT_SCHEMA: &str = r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","$id":"devworks/knowledge-import/1","type":"object","additionalProperties":false,"required":["format","candidates"],"properties":{"format":{"const":"devworks/knowledge-import"},"candidates":{"type":"array","minItems":1,"maxItems":128,"items":{"type":"object","additionalProperties":false,"required":["entry","sourceFile","verification"],"properties":{"sourceFile":{"type":"string","minLength":1,"maxLength":4096},"verification":{"enum":["verified","stale","unverifiable","personal"]},"verificationNote":{"type":"string","maxLength":4096},"entry":{"type":"object","additionalProperties":false,"required":["kind","summary"],"properties":{"kind":{"enum":["goal","decision","constraint","fact","gotcha","attempt","architecture","lesson"]},"summary":{"type":"string","minLength":1,"maxLength":500},"body":{"type":"string","maxLength":16000},"subject":{"type":"string","minLength":1,"maxLength":256},"confidence":{"enum":["high","medium","low"]},"tags":{"type":"array","maxItems":32,"items":{"type":"string","minLength":1,"maxLength":64}},"activities":{"type":"array","maxItems":16,"items":{"type":"string","minLength":1,"maxLength":64}}}}}}}}}"#;

/// The digest a `SchemaRef` must carry for this format.
pub fn import_schema_digest() -> String {
    sha256_hex(IMPORT_SCHEMA.as_bytes())
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ImportFile {
    format: String,
    candidates: Vec<RawCandidate>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawCandidate {
    entry: RawEntry,
    source_file: String,
    verification: String,
    #[serde(default)]
    verification_note: Option<String>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawEntry {
    kind: String,
    summary: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    confidence: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    activities: Vec<String>,
}

/// One validated candidate.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    /// Its index in the file (the identity-map key with the artifact version).
    pub index: usize,
    /// `KnowledgeEntryKind` wire name.
    pub entry_kind: String,
    pub summary: String,
    pub body: Option<String>,
    pub subject: String,
    pub confidence: f64,
    pub tags: Vec<String>,
    pub activities: Vec<String>,
    pub source_file: String,
    pub verification: String,
    pub verification_note: Option<String>,
}

/// What importing a candidate would do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    WouldStore,
    Duplicate,
    AlreadyImported,
    Stale,
    Personal,
}
impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WouldStore => "would_store",
            Self::Duplicate => "duplicate",
            Self::AlreadyImported => "already_imported",
            Self::Stale => "stale",
            Self::Personal => "personal",
        }
    }
}

/// Parses an import file. An invalid file is refused as a whole (`Err` names the first
/// problem); nothing is guessed.
pub fn parse(bytes: &[u8]) -> Result<Vec<Candidate>, String> {
    let file: ImportFile =
        serde_json::from_slice(bytes).map_err(|e| format!("not a knowledge import: {e}"))?;
    if file.format != IMPORT_SCHEMA_ID {
        return Err(format!("format must be {IMPORT_SCHEMA_ID}"));
    }
    if file.candidates.is_empty() || file.candidates.len() > MAX_CANDIDATES {
        return Err(format!("1 to {MAX_CANDIDATES} candidates"));
    }
    let mut out = Vec::new();
    for (index, c) in file.candidates.into_iter().enumerate() {
        let bad = |m: &str| format!("candidate {index}: {m}");
        let entry_kind = match c.entry.kind.as_str() {
            "goal" => "Goal",
            "decision" => "Decision",
            "constraint" => "Constraint",
            "fact" => "Fact",
            "gotcha" => "Gotcha",
            "attempt" => "Attempt",
            "architecture" => "Architecture",
            "lesson" => "Lesson",
            _ => return Err(bad("unknown kind")),
        };
        let summary = c.entry.summary.trim().to_owned();
        if summary.is_empty() || summary.chars().count() > 500 || summary.contains('\0') {
            return Err(bad("summary must be 1 to 500 characters"));
        }
        if c.entry
            .body
            .as_ref()
            .is_some_and(|b| b.chars().count() > 16000 || b.contains('\0'))
        {
            return Err(bad("body exceeds 16000 characters"));
        }
        let subject = c
            .entry
            .subject
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("General")
            .to_owned();
        if subject.chars().count() > 256 || subject.contains('\0') {
            return Err(bad("subject exceeds 256 characters"));
        }
        let confidence = match c.entry.confidence.as_deref() {
            None | Some("medium") => 0.6,
            Some("high") => 0.9,
            Some("low") => 0.3,
            Some(_) => return Err(bad("confidence must be high, medium or low")),
        };
        if c.entry.tags.len() > 32 || c.entry.tags.iter().any(|t| t.is_empty() || t.len() > 64) {
            return Err(bad("at most 32 tags of 1 to 64 characters"));
        }
        if c.entry.activities.len() > 16
            || c.entry
                .activities
                .iter()
                .any(|t| t.is_empty() || t.len() > 64)
        {
            return Err(bad("at most 16 activities of 1 to 64 characters"));
        }
        if !matches!(
            c.verification.as_str(),
            "verified" | "stale" | "unverifiable" | "personal"
        ) {
            return Err(bad(
                "verification must be verified, stale, unverifiable or personal",
            ));
        }
        if c.source_file.is_empty() || c.source_file.len() > 4096 || c.source_file.contains('\0') {
            return Err(bad("sourceFile must be 1 to 4096 bytes"));
        }
        if c.verification_note
            .as_ref()
            .is_some_and(|n| n.len() > 4096 || n.contains('\0'))
        {
            return Err(bad("verificationNote exceeds 4096 bytes"));
        }
        out.push(Candidate {
            index,
            entry_kind: entry_kind.to_owned(),
            summary,
            body: c.entry.body,
            subject,
            confidence,
            tags: fold_tags(c.entry.tags.iter().map(String::as_str)),
            activities: fold_tags(c.entry.activities.iter().map(String::as_str)),
            source_file: c.source_file,
            verification: c.verification,
            verification_note: c.verification_note,
        });
    }
    Ok(out)
}

/// The outcome of every candidate: `duplicate_of_active(entry_kind, folded summary)` says
/// whether an active entry of the scope already says it; `imported(index)` whether this
/// artifact version's candidate was already imported.
pub fn evaluate(
    candidates: &[Candidate],
    mut duplicate_of_active: impl FnMut(&str, &str) -> bool,
    mut imported: impl FnMut(usize) -> bool,
) -> Vec<Outcome> {
    let mut seen: Vec<(String, String)> = Vec::new();
    candidates
        .iter()
        .map(|c| {
            let key = (c.entry_kind.clone(), fold(&c.summary));
            let outcome = if imported(c.index) {
                Outcome::AlreadyImported
            } else if c.verification == "personal" {
                Outcome::Personal
            } else if c.verification == "stale" {
                Outcome::Stale
            } else if seen.contains(&key) || duplicate_of_active(&key.0, &key.1) {
                Outcome::Duplicate
            } else {
                Outcome::WouldStore
            };
            seen.push(key);
            outcome
        })
        .collect()
}

/// The exact preview digest of an import: the source version and bytes, the scope, the schema
/// and every candidate's outcome.
pub fn digest(
    scope_key: &str,
    version_id: &str,
    sha256: &str,
    candidates: &[Candidate],
    outcomes: &[Outcome],
) -> String {
    let mut parts = vec![
        "knowledge.import_user_data".to_owned(),
        scope_key.to_owned(),
        version_id.to_owned(),
        sha256.to_owned(),
        import_schema_digest(),
    ];
    for (c, o) in candidates.iter().zip(outcomes) {
        parts.push(format!(
            "{}|{}|{}|{}",
            c.index,
            o.as_str(),
            c.entry_kind,
            fold(&c.summary)
        ));
    }
    preview_digest(&parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(candidates: &str) -> Vec<u8> {
        format!(r#"{{"format":"devworks/knowledge-import","candidates":[{candidates}]}}"#)
            .into_bytes()
    }

    #[test]
    fn parses_and_classifies_candidates_like_the_retained_import() {
        let bytes = file(
            r#"{"entry":{"kind":"decision","summary":"Use SQLite"},"sourceFile":"a.md","verification":"verified"},
               {"entry":{"kind":"fact","summary":"use sqlite"},"sourceFile":"b.md","verification":"unverifiable"},
               {"entry":{"kind":"gotcha","summary":"Old path"},"sourceFile":"c.md","verification":"stale"},
               {"entry":{"kind":"fact","summary":"My editor"},"sourceFile":"d.md","verification":"personal"},
               {"entry":{"kind":"decision","summary":"USE  sqlite"},"sourceFile":"e.md","verification":"verified"},
               {"entry":{"kind":"lesson","summary":"Run fmt","activities":["implementing"]},"sourceFile":"f.md","verification":"verified"}"#,
        );
        let c = parse(&bytes).unwrap();
        assert_eq!(c.len(), 6);
        let outcomes = evaluate(&c, |k, s| k == "Lesson" && s == "run fmt", |i| i == 1);
        assert_eq!(
            outcomes,
            [
                Outcome::WouldStore,
                Outcome::AlreadyImported,
                Outcome::Stale,
                Outcome::Personal,
                Outcome::Duplicate,
                Outcome::Duplicate,
            ]
        );
        let d = digest("workspace", "v1", "00", &c, &outcomes);
        assert_ne!(
            d,
            digest(
                "workspace",
                "v1",
                "00",
                &c,
                &evaluate(&c, |_, _| false, |_| false)
            )
        );
    }

    #[test]
    fn an_import_never_exceeds_what_its_receipt_names() {
        let one =
            r#"{"entry":{"kind":"fact","summary":"x"},"sourceFile":"a","verification":"verified"}"#;
        let many = |n: usize| file(&vec![one; n].join(","));
        assert_eq!(parse(&many(MAX_CANDIDATES)).unwrap().len(), 128);
        assert!(parse(&many(MAX_CANDIDATES + 1)).is_err());
        assert!(IMPORT_SCHEMA.contains(r#""maxItems":128,"items":{"type":"object""#));
    }

    #[test]
    fn refuses_malformed_files_whole() {
        assert!(parse(b"{}").is_err());
        assert!(parse(&file(r#"{"entry":{"kind":"note","summary":"x"},"sourceFile":"a","verification":"verified"}"#)).is_err());
        assert!(parse(&file(r#"{"entry":{"kind":"fact","summary":""},"sourceFile":"a","verification":"verified"}"#)).is_err());
        assert!(parse(&file(r#"{"entry":{"kind":"fact","summary":"x","extra":1},"sourceFile":"a","verification":"verified"}"#)).is_err());
        assert!(parse(br#"{"format":"other","candidates":[]}"#).is_err());
    }
}
