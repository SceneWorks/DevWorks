//! Pure Knowledge rules shared by every entry point: folding, kind and tag classification,
//! lesson origin and the recurrence rule, and preview digests. Nothing here reads the store.
use crate::fingerprint::sha256_hex;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Reserved category tags (KNOW-08): one Knowledge system, told apart by tags.
pub const TAG_LESSON: &str = "lesson";
pub const TAG_KNOWLEDGE: &str = "knowledge";
pub const TAG_MEMORY: &str = "memory";

/// Lesson relation predicates with lesson semantics in `knowledge.relate`.
pub const SAME_AS: &str = "same_as";
pub const CONTRADICTS: &str = "contradicts";
pub const UNMERGE: &str = "unmerge";

/// Most words a search uses (longer queries are refused rather than silently cut).
pub const MAX_WORDS: usize = 32;

/// The wire name of a unit enum value.
pub fn wire<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}
/// The enum value of a stored wire name.
pub fn parse<T: DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value(Value::String(s.to_owned())).ok()
}

/// Unicode case folding with whitespace collapsed: the same function folds what is stored and
/// what is asked for, so matching is symmetric beyond ASCII (SQLite's `lower()` is ASCII-only).
pub fn fold(s: &str) -> String {
    s.split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// A folded, sorted, unique tag set (empty labels dropped).
pub fn fold_tags<'a>(tags: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = tags
        .into_iter()
        .map(fold)
        .filter(|t| !t.is_empty())
        .map(|t| t.chars().take(256).collect())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Applicability with its activities, tools and harness folded (filters fold the same way).
pub fn fold_applicability(
    a: &crate::protocol::records::Applicability,
) -> crate::protocol::records::Applicability {
    use crate::protocol::support::{BoundedList, BoundedText};
    let labels = |items: &[BoundedText<1, 256>]| {
        let mut out: Vec<BoundedText<1, 256>> = Vec::new();
        for t in items {
            let f = fold(&t.0);
            if !f.is_empty() && !out.iter().any(|x| x.0 == f) {
                out.push(BoundedText(f));
            }
        }
        BoundedList(out)
    };
    crate::protocol::records::Applicability {
        activities: labels(&a.activities.0),
        paths: a.paths.clone(),
        tools: labels(&a.tools.0),
        harness: a
            .harness
            .as_ref()
            .map(|h| BoundedText(fold(&h.0)))
            .filter(|h| !h.0.is_empty()),
        repository_ids: a.repository_ids.clone(),
    }
}

/// The search words of `text`: maximal runs of letters and digits, folded and unique, in order.
pub fn words(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in text.split(|c: char| !c.is_alphanumeric()) {
        let w = fold(w);
        if !w.is_empty() && !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

/// The reuse key of an entity: folded type and name.
pub fn entity_key(entity_type: &str, name: &str) -> String {
    format!("{}|{}", fold(entity_type), fold(name))
}

/// The revision-1 `KnowledgeKind` an entry kind projects onto.
pub fn kind_of(entry_kind: &str) -> &'static str {
    match entry_kind {
        "Decision" => "Decision",
        "Lesson" => "Lesson",
        "Constraint" => "Constraint",
        "Gotcha" | "Attempt" | "Finding" => "Finding",
        "Proposal" => "Proposal",
        // Goal, Architecture and Fact are statements about the work.
        _ => "Fact",
    }
}
/// The entry kind a revision-1 kind records when no entry kind is given.
pub fn default_entry(kind: &str) -> &'static str {
    match kind {
        "Decision" => "Decision",
        "Lesson" => "Lesson",
        "Constraint" => "Constraint",
        "Finding" => "Finding",
        "Proposal" => "Proposal",
        _ => "Fact",
    }
}
/// The category tag of an entry kind (KNOW-08): lessons; project knowledge entries; memory
/// observations.
pub fn category_tag(entry_kind: &str) -> &'static str {
    match entry_kind {
        "Lesson" => TAG_LESSON,
        "Goal" | "Decision" | "Constraint" | "Gotcha" | "Attempt" | "Architecture" => TAG_KNOWLEDGE,
        _ => TAG_MEMORY,
    }
}

/// Whether a lesson source carries the user's own words (KNOW-05): such a rule takes effect at
/// once. Every other source is inferred and stays candidate evidence until it recurs.
pub fn user_source(source: &str) -> bool {
    matches!(
        source,
        "Prompt" | "Review" | "RelayedPrompt" | "RelayedReview"
    )
}
/// Sources only the user's own ingress may state; an agent relays them instead.
pub fn direct_user_source(source: &str) -> bool {
    matches!(source, "Prompt" | "Review")
}

/// A lesson's rule state as read: a stored `Candidate` or `Proposed` lesson whose distinct
/// source events reach `threshold` is `Effective` by recurrence. A `Rejected` or `Retired` rule
/// never becomes effective by recurrence. Taking effect is not proof that a rule works.
pub fn effective_rule_state(
    stored: &str,
    stored_effect: Option<&str>,
    events: i64,
    threshold: i64,
) -> (String, Option<String>) {
    match stored {
        "Candidate" | "Proposed" if events >= threshold => {
            ("Effective".into(), Some("Recurrence".into()))
        }
        s => (s.to_owned(), stored_effect.map(str::to_owned)),
    }
}

/// The distinct-event key of a lesson event: the same reference from the same source is one
/// event however often it is recorded; an event without a reference is its own write.
pub fn event_key(source: &str, source_ref: Option<&str>, write_id: &str) -> String {
    match source_ref {
        Some(r) => format!("{source}|{r}"),
        None => format!("write|{write_id}"),
    }
}

/// The exact preview digest of a deletion or import: SHA-256 over its canonical description.
pub fn preview_digest(parts: &[String]) -> String {
    sha256_hex(parts.join("\n").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folding_is_unicode_and_symmetric() {
        assert_eq!(fold("  Ärger   ÜBER  "), "ärger über");
        assert_eq!(fold("STRASSE"), fold("strasse"));
        assert_eq!(fold("ΑΘΉΝΑ"), "αθήνα");
        assert_eq!(
            fold_tags(["Lesson", "lesson", " Rust  Tips", ""]),
            vec!["lesson".to_owned(), "rust tips".to_owned()]
        );
        assert_eq!(
            words("Use `cargo fmt`, ÉCOLE école!"),
            ["use", "cargo", "fmt", "école"]
        );
        assert_eq!(
            entity_key("Crate", " DevWorks  Core"),
            "crate|devworks core"
        );
    }

    #[test]
    fn entry_kinds_project_onto_the_recorded_kinds() {
        for (entry, kind) in [
            ("Goal", "Fact"),
            ("Architecture", "Fact"),
            ("Gotcha", "Finding"),
            ("Attempt", "Finding"),
            ("Decision", "Decision"),
            ("Lesson", "Lesson"),
        ] {
            assert_eq!(kind_of(entry), kind);
        }
        assert_eq!(category_tag("Lesson"), TAG_LESSON);
        assert_eq!(category_tag("Gotcha"), TAG_KNOWLEDGE);
        assert_eq!(category_tag("Fact"), TAG_MEMORY);
    }

    #[test]
    fn recurrence_makes_an_inferred_lesson_effective_but_never_a_refused_one() {
        assert_eq!(
            effective_rule_state("Candidate", None, 2, 3).0,
            "Candidate".to_owned()
        );
        assert_eq!(
            effective_rule_state("Candidate", None, 3, 3),
            ("Effective".into(), Some("Recurrence".into()))
        );
        assert_eq!(effective_rule_state("Proposed", None, 4, 3).0, "Effective");
        assert_eq!(effective_rule_state("Rejected", None, 9, 3).0, "Rejected");
        assert_eq!(effective_rule_state("Retired", None, 9, 3).0, "Retired");
        assert!(user_source("RelayedReview") && !user_source("BotReview"));
        assert_eq!(event_key("Agent", Some("pr#1"), "w1"), "Agent|pr#1");
        assert_eq!(event_key("Agent", None, "w1"), "write|w1");
    }
}
