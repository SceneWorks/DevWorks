//! Provider disclosure records (SEC-01): what project content an outbound provider request
//! carries, exposed as data for the request preview and request history. Secrets and content
//! the invoking principal is not authorized to send are excluded before context construction
//! and listed as exclusions, never silently dropped.
use super::redact::Redactor;
use crate::fingerprint::sha256_hex;
use crate::protocol::{Id, ProjectId};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum ContentKind {
    File,
    Snippet,
    Attachment,
    Transcript,
    Embedding,
    Vision,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Exclusion {
    /// Outside the principal's authorized scope.
    Unauthorized,
    /// Removed because it contained a known credential value or shape.
    SecretRedacted,
}

/// One item that would be sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisclosedItem {
    pub kind: ContentKind,
    pub project_id: Option<ProjectId>,
    pub source: String,
    pub revision: Option<String>,
    pub bytes: u64,
    pub sha256: String,
    /// The item was sent with credential-shaped spans redacted.
    pub redacted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExcludedItem {
    pub kind: ContentKind,
    pub source: String,
    pub reason: Exclusion,
}

/// The disclosure of one outbound provider request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDisclosure {
    pub destination: String,
    pub model: String,
    pub connection_id: Option<Id>,
    pub items: Vec<DisclosedItem>,
    pub excluded: Vec<ExcludedItem>,
}

/// One candidate item offered for a provider context.
pub struct Candidate<'a> {
    pub kind: ContentKind,
    pub project_id: Option<ProjectId>,
    pub source: &'a str,
    pub revision: Option<&'a str>,
    pub text: &'a str,
    /// Whether the invoking principal may send this item (decided by authority/scope).
    pub authorized: bool,
}

/// Builds the payload actually sent and its disclosure record together, so the record cannot
/// diverge from the content: unauthorized items are excluded, the rest are redacted.
pub fn disclose(
    destination: &str,
    model: &str,
    connection_id: Option<Id>,
    redactor: &Redactor,
    candidates: &[Candidate<'_>],
) -> (Vec<String>, ProviderDisclosure) {
    let mut sent = Vec::new();
    let mut items = Vec::new();
    let mut excluded = Vec::new();
    for c in candidates {
        if !c.authorized {
            excluded.push(ExcludedItem {
                kind: c.kind,
                source: c.source.to_owned(),
                reason: Exclusion::Unauthorized,
            });
            continue;
        }
        let text = redactor.redact_text(c.text);
        let redacted = text != c.text;
        if redacted {
            excluded.push(ExcludedItem {
                kind: c.kind,
                source: c.source.to_owned(),
                reason: Exclusion::SecretRedacted,
            });
        }
        items.push(DisclosedItem {
            kind: c.kind,
            project_id: c.project_id.clone(),
            source: c.source.to_owned(),
            revision: c.revision.map(str::to_owned),
            bytes: text.len() as u64,
            sha256: sha256_hex(text.as_bytes()),
            redacted,
        });
        sent.push(text);
    }
    (
        sent,
        ProviderDisclosure {
            destination: destination.to_owned(),
            model: model.to_owned(),
            connection_id,
            items,
            excluded,
        },
    )
}
