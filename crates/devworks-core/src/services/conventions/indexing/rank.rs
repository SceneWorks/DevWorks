//! Retained ASCII BM25 statistics and excerpt semantics. SQL owns corpus-wide statistics and
//! final fusion so neither corpus size nor a prematurely truncated candidate pool changes rank.
use std::collections::BTreeMap;

/// CodeGraph's [a-z0-9][a-z0-9_-]* tokenizer, independently of SQLite unicode61 rules.
pub fn terms(text: &str) -> BTreeMap<String, u32> {
    let mut terms = BTreeMap::new();
    let lower = text.to_lowercase();
    let bytes = lower.as_bytes();
    let mut offset = 0;
    while offset < bytes.len() {
        if !bytes[offset].is_ascii_alphanumeric() {
            offset += 1;
            continue;
        }
        let start = offset;
        offset += 1;
        while offset < bytes.len()
            && (bytes[offset].is_ascii_alphanumeric() || matches!(bytes[offset], b'_' | b'-'))
        {
            offset += 1;
        }
        *terms.entry(lower[start..offset].to_owned()).or_default() += 1;
    }
    terms
}
pub fn inverse_document_frequency(documents: u64, frequency: u64) -> f64 {
    (1.0 + ((documents as f64 - frequency as f64 + 0.5) / (frequency as f64 + 0.5))).ln()
}
pub fn bm25(frequency: u32, length: u64, average_length: f64, idf: f64) -> f64 {
    let frequency = f64::from(frequency);
    if frequency == 0.0 {
        return 0.0;
    }
    idf * ((frequency * 2.2)
        / (frequency + 1.2 * (0.25 + 0.75 * (length as f64 / average_length.max(1.0)))))
}
/// A display excerpt, never a substitute for the complete immutable source artifact.
pub fn excerpt(text: &str, query: &str) -> String {
    let lower = text.to_lowercase();
    let hit = terms(query)
        .keys()
        .filter_map(|term| lower.find(term))
        .min()
        .unwrap_or(0);
    // Map the case-folded hit back to an original Unicode boundary instead of slicing a byte
    // offset from the folded string into potentially different-length source characters.
    let mut folded = 0;
    let mut hit_char = 0;
    for (n, c) in text.chars().enumerate() {
        if folded >= hit {
            hit_char = n;
            break;
        }
        folded += c.to_lowercase().map(char::len_utf8).sum::<usize>();
        hit_char = n + 1;
    }
    text.chars()
        .skip(hit_char.saturating_sub(120))
        .take(360)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_tokens_include_breadcrumbs_and_full_corpus_idf() {
        let terms =
            terms("Messaging > # Consumers\n\nIDEMPOTENT consumers foo_bar foo-bar _skip-Ünicode");
        assert_eq!(terms["consumers"], 2);
        assert_eq!(terms["foo_bar"], 1);
        assert_eq!(terms["foo-bar"], 1);
        assert_eq!(terms["skip-"], 1);
        assert_eq!(terms["nicode"], 1);
        let idf = inverse_document_frequency(10, 2);
        assert!((idf - (1.0_f64 + 8.5 / 2.5).ln()).abs() < 1e-12);
        assert!(bm25(2, 20, 20.0, idf) > bm25(1, 20, 20.0, idf));
        assert!(bm25(1, 10, 20.0, idf) > bm25(1, 40, 20.0, idf));
        assert_eq!(bm25(0, 10, 20.0, idf), 0.0);
    }
    #[test]
    fn excerpt_is_unicode_safe_and_collapses_whitespace_without_changing_source() {
        let source = format!(
            "{}\n\nIDEMPOTENT  consumers\t{}",
            "éİ".repeat(200),
            "末".repeat(400)
        );
        let excerpt = excerpt(&source, "idempotent");
        assert!(excerpt.contains("IDEMPOTENT consumers"));
        assert!(!excerpt.contains('\n'));
        assert!(excerpt.chars().count() <= 360);
    }
}
