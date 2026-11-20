use super::*;
use crate::protocol::Counter;
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

fn version(bytes: &[u8], media: &str, id: &str) -> ArtifactRef {
    ArtifactRef {
        artifact_id: Id::new("document").unwrap(),
        version_id: Id::new(id).unwrap(),
        sha256: Digest::try_from(sha256_hex(bytes)).unwrap(),
        byte_length: Counter::new(bytes.len() as i64).unwrap(),
        media_type: media.into(),
    }
}

fn control(cancelled: &(dyn Fn() -> bool + Send + Sync)) -> ExtractionControl<'_> {
    ExtractionControl {
        deadline: Instant::now() + Duration::from_secs(30),
        cancelled,
    }
}

fn extract(
    extractor: &TextExtractor,
    bytes: &[u8],
    media: &str,
    id: &str,
    revision: i64,
) -> Result<Vec<ExtractedBlock>, ExtractionError> {
    extractor.extract(
        ExtractionInput {
            version: &version(bytes, media, id),
            revision: &Revision::new(revision).unwrap(),
            bytes,
        },
        &control(&|| false),
    )
}

fn lines(block: &ExtractedBlock) -> (u32, u32) {
    match block.citation.locator {
        Locator::Text {
            first_line,
            last_line,
        } => (first_line, last_line),
        _ => panic!("expected a source line range"),
    }
}

#[test]
fn markdown_uses_source_lines_and_keeps_a_commonmark_heading_path() {
    let bytes = "\u{feff}# Cafe\u{301} #\r\nintro\r\n\r\n```text\r\n# fenced\r\n```\r\n\r\nSection *one*\r\n-----\r\n## Same level\r<pre>\n# html\n</pre>\n\n> # Quoted\n\n### [Nested](https://example.com)\n\n# Final\nlast"
        .as_bytes();
    let blocks = extract(&TextExtractor::default(), bytes, "text/markdown", "v1", 1).unwrap();
    let expected = [
        ((1, 7), vec![(1, "Café")]),
        ((8, 9), vec![(1, "Café"), (2, "Section one")]),
        ((10, 16), vec![(1, "Café"), (2, "Same level")]),
        (
            (17, 18),
            vec![(1, "Café"), (2, "Same level"), (3, "Nested")],
        ),
        ((19, 20), vec![(1, "Final")]),
    ];
    assert_eq!(blocks.len(), expected.len());
    for (block, (range, path)) in blocks.iter().zip(expected) {
        assert_eq!(lines(block), range);
        assert_eq!(
            block
                .headings
                .iter()
                .map(|h| (h.level, h.title.as_str()))
                .collect::<Vec<_>>(),
            path
        );
        assert_eq!(
            block.citation.content_digest.as_str(),
            sha256_hex(block.text.as_bytes())
        );
        assert!(block.citation.validate().is_ok());
    }
    assert!(blocks[0].text.contains("# fenced"));
    assert!(blocks[2].text.contains("# html"));
    assert!(blocks[3].text.contains("[Nested](https://example.com)"));
    assert!(!blocks.iter().any(|block| block.text.contains('\r')));
}

#[test]
fn utf16_reopens_exact_lines_with_unicode_and_keeps_plain_text_inert() {
    for bom in [[0xff, 0xfe], [0xfe, 0xff]] {
        let mut bytes = bom.to_vec();
        for unit in "# Plain\r\nCafe\u{301}\r\n😀".encode_utf16() {
            let unit = if bom[0] == 0xff {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            };
            bytes.extend(unit);
        }
        let blocks = extract(&TextExtractor::default(), &bytes, "text/plain", "v1", 1).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(lines(&blocks[0]), (1, 3));
        assert_eq!(blocks[0].text, "# Plain\nCafé\n😀");
        assert!(blocks[0].headings.is_empty());
        let CitationSource::Managed {
            version,
            extraction_revision,
        } = &blocks[0].citation.source
        else {
            panic!("expected managed-version citation");
        };
        assert_eq!(version.version_id.as_str(), "v1");
        assert_eq!(version.sha256.as_str(), sha256_hex(&bytes));
        assert_eq!(extraction_revision.get(), 1);
    }
}

#[test]
fn overlong_line_chunks_are_utf8_bounded_and_identity_is_exact() {
    let extractor = TextExtractor::with_limits(ExtractionLimits {
        chunk_bytes: 5,
        ..ExtractionLimits::default()
    })
    .unwrap();
    let source = "😀😀\nalpha\nend".as_bytes();
    let run =
        |version, revision| extract(&extractor, source, "text/plain", version, revision).unwrap();
    let first = run("v1", 1);
    let blocks = first
        .iter()
        .map(|b| (lines(b), b.text.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        blocks,
        [
            ((1, 1), "😀"),
            ((1, 1), "😀\n"),
            ((2, 2), "alpha"),
            ((3, 3), "end"),
        ]
    );

    let ids = |blocks: &[ExtractedBlock]| {
        blocks
            .iter()
            .map(|b| b.citation.block_id.clone())
            .collect::<Vec<_>>()
    };
    let original_ids = ids(&first);
    assert_eq!(original_ids, ids(&run("v1", 1)));
    assert_eq!(
        original_ids
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        4
    );
    for changed in [run("v2", 1), run("v1", 2)] {
        assert!(
            original_ids
                .iter()
                .zip(changed)
                .all(|(original, other)| original != &other.citation.block_id)
        );
    }
}

#[test]
fn refusals_have_typed_outcomes_and_return_no_blocks() {
    use ExtractionError::*;
    let extractor = TextExtractor::default();
    for (bytes, expected) in [
        (&[0xff][..], InvalidEncoding),
        (&[0xff, 0xfe, 1][..], InvalidEncoding),
        (&[0xff, 0xfe, 0, 0xd8][..], InvalidEncoding),
        (&[0, 0, 0xfe, 0xff, 0, 0, 0, 65][..], UnsupportedEncoding),
        (b"a\0b", InvalidEncoding),
        (b" \t\r\n", Empty),
        (&[0xef, 0xbb, 0xbf][..], Empty),
    ] {
        assert_eq!(
            extract(&extractor, bytes, "text/plain", "v1", 1).unwrap_err(),
            expected
        );
    }
    assert_eq!(
        extract(&extractor, b"%PDF", "application/pdf", "v1", 1).unwrap_err(),
        UnsupportedFormat
    );

    let limits = TextExtractor::with_limits(ExtractionLimits {
        input_bytes: 12,
        normalized_bytes: 10,
        chunk_bytes: 4,
        blocks: 2,
    })
    .unwrap();
    assert_eq!(
        extract(&limits, b"abcdefghijkl!", "text/plain", "v1", 1).unwrap_err(),
        SourceTooLarge
    );
    assert_eq!(
        extract(&limits, b"abcdefghijk", "text/plain", "v1", 1).unwrap_err(),
        NormalizedLimit
    );
    assert_eq!(
        extract(&limits, b"a\nbc\ndef", "text/plain", "v1", 1).unwrap_err(),
        BlockLimit
    );
    assert_eq!(
        extract(&limits, b"abcdefgh", "text/plain", "v1", 1)
            .unwrap()
            .len(),
        2
    );

    let mut wrong = version(b"text", "text/plain", "v1");
    let attempt = |version: &ArtifactRef, bytes: &[u8]| {
        extractor.extract(
            ExtractionInput {
                version,
                revision: &Revision::new(1).unwrap(),
                bytes,
            },
            &control(&|| false),
        )
    };
    assert_eq!(attempt(&wrong, b"test").unwrap_err(), SourceMismatch);
    wrong.byte_length = Counter::new(5).unwrap();
    assert_eq!(attempt(&wrong, b"text").unwrap_err(), SourceMismatch);
}

#[test]
fn cancellation_and_deadline_discard_in_progress_output() {
    let bytes = "abc\n".repeat(20_000);
    let edition = version(bytes.as_bytes(), "text/plain", "v1");
    let revision = Revision::new(1).unwrap();
    let input = || ExtractionInput {
        version: &edition,
        revision: &revision,
        bytes: bytes.as_bytes(),
    };
    let checks = AtomicUsize::new(0);
    let cancel = || checks.fetch_add(1, Ordering::Relaxed) >= 30;
    let extractor = TextExtractor::default();
    assert_eq!(
        extractor.extract(input(), &control(&cancel)).unwrap_err(),
        ExtractionError::Cancelled
    );
    assert_eq!(checks.load(Ordering::Relaxed), 31);
    let deadline = ExtractionControl {
        deadline: Instant::now(),
        cancelled: &|| false,
    };
    assert_eq!(
        extractor.extract(input(), &deadline).unwrap_err(),
        ExtractionError::Deadline
    );
}
