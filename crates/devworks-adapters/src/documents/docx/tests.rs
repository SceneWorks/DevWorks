use super::*;
use devworks_core::{
    fingerprint::sha256_hex,
    protocol::{
        Counter, Revision,
        data::{CitationSource, Locator},
        provider::{ArtifactRef, Digest},
    },
};
use std::{
    io::{Cursor, Write},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

const WORD: &str = xml::WORD;
const WORD_STRICT: &str = xml::WORD_STRICT;
const REL: &str = xml::REL;

fn document(content: &str, word: &str) -> String {
    format!(
        r#"<w:document xmlns:w="{word}" xmlns:r="{REL}"><w:body>{content}</w:body></w:document>"#
    )
}

fn relationships(content: &str) -> String {
    format!(
        r#"<Relationships xmlns="{}">{content}</Relationships>"#,
        xml::OPC_RELS
    )
}

fn content_types(main_type: &str) -> String {
    format!(
        r#"<Types xmlns="{}">
      <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
      <Default Extension="xml" ContentType="application/xml"/>
      <Override PartName="/docs/main.xml" ContentType="{main_type}"/>
      <Override PartName="/docs/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
    </Types>"#,
        xml::TYPES
    )
}

fn archive(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    for (part, bytes) in parts {
        zip.start_file(
            *part,
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )
        .unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

const MAIN_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml";

fn bundle(main: &[u8], main_type: &str, root_target: &str, extra: &[(&str, &[u8])]) -> Vec<u8> {
    let styles = format!(
        r#"<w:styles xmlns:w="{WORD}">
      <w:style w:type="paragraph" w:styleId="Base"><w:pPr><w:outlineLvl w:val="1"/></w:pPr></w:style>
      <w:style w:type="paragraph" w:styleId="Custom"><w:basedOn w:val="Base"/></w:style>
    </w:styles>"#
    );
    let types = content_types(main_type);
    let root = relationships(&format!(
        r#"<Relationship Id="root" Type="{REL}/officeDocument" Target="{root_target}"/>"#
    ));
    let main_rels = relationships(&format!(
        r#"
       <Relationship Id="style" Type="{REL}/styles" Target="./styles.xml"/>
       <Relationship Id="link" Type="{REL}/hyperlink" Target="file:///fixture/inert-link" TargetMode="External"/>
       <Relationship Id="web" Type="{REL}/hyperlink" Target="https://example.invalid/a?x=1&amp;y=2" TargetMode="External"/>
    "#
    ));
    let mut parts = vec![
        ("[Content_Types].xml", types.as_bytes()),
        ("_rels/.rels", root.as_bytes()),
        ("docs/main.xml", main),
        ("docs/_rels/main.xml.rels", main_rels.as_bytes()),
        ("docs/styles.xml", styles.as_bytes()),
    ];
    parts.extend_from_slice(extra);
    archive(&parts)
}

fn version(bytes: &[u8], media: &str, edition: &str) -> ArtifactRef {
    ArtifactRef {
        artifact_id: Id::new("document").unwrap(),
        version_id: Id::new(edition).unwrap(),
        byte_length: Counter::new(bytes.len() as i64).unwrap(),
        sha256: Digest::try_from(sha256_hex(bytes)).unwrap(),
        media_type: media.into(),
    }
}

fn extraction(
    engine: &DocxExtractor,
    bytes: &[u8],
    media: &str,
    edition: &str,
    cancelled: &(dyn Fn() -> bool + Send + Sync),
) -> Result<WordBody, DocxError> {
    engine.extract(
        ExtractionInput {
            version: &version(bytes, media, edition),
            revision: &Revision::new(7).unwrap(),
            bytes,
        },
        &ExtractionControl {
            deadline: Instant::now() + Duration::from_secs(30),
            cancelled,
        },
    )
}

fn run(bytes: &[u8]) -> Result<WordBody, DocxError> {
    extraction(&DocxExtractor::default(), bytes, DOCX_MEDIA, "v1", &|| {
        false
    })
}

fn body(text: &str) -> Vec<u8> {
    let xml = document(text, WORD);
    bundle(xml.as_bytes(), MAIN_TYPE, "/docs/main.xml", &[])
}

#[test]
fn declared_body_preserves_runs_headings_lists_and_inert_links() {
    let data = body(
        r#"
     <w:p><w:pPr><w:pStyle w:val="Custom"/></w:pPr>
       <w:r><w:t>Cafe</w:t></w:r><w:r><w:t>&#x301;&amp; &#x1f600;</w:t></w:r>
     </w:p>
     <w:p><w:pPr><w:numPr><w:ilvl w:val="3"/><w:numId w:val="42"/></w:numPr></w:pPr>
       <w:hyperlink r:id="link"><w:r><w:t>file</w:t></w:r></w:hyperlink>
       <w:r><w:tab/></w:r>
       <w:hyperlink r:id="web"><w:r><w:t>web</w:t></w:r></w:hyperlink>
       <w:r><w:br/></w:r>
       <w:hyperlink w:anchor="somewhere"><w:r><w:t>bookmark</w:t></w:r></w:hyperlink>
     </w:p>
    "#,
    );
    let result = run(&data).unwrap();
    assert_eq!(result.part, "docs/main.xml");
    assert!(result.coverage_issues.is_empty());
    assert_eq!(result.paragraphs.len(), 2);
    let first = &result.paragraphs[0];
    assert_eq!(first.heading_level, Some(2));
    assert_eq!(first.style.as_deref(), Some("Custom"));
    assert_eq!(first.blocks[0].text, "Café& 😀");
    assert_eq!(first.blocks[0].headings[0].title, "Café& 😀");
    let second = &result.paragraphs[1];
    assert_eq!(second.blocks[0].text, "file\tweb\nbookmark");
    assert_eq!(
        second.blocks[0].headings.as_ref(),
        first.blocks[0].headings.as_ref()
    );
    assert_eq!(
        second.list,
        Some(WordList {
            numbering_id: 42,
            level: 3
        })
    );
    assert_eq!(
        second
            .hyperlinks
            .iter()
            .map(|link| &link.target)
            .collect::<Vec<_>>(),
        [
            &WordLinkTarget::External("file:///fixture/inert-link".into()),
            &WordLinkTarget::External("https://example.invalid/a?x=1&y=2".into()),
            &WordLinkTarget::Bookmark("somewhere".into()),
        ]
    );
    let first_block = &first.blocks[0];
    let second_block = &second.blocks[0];
    for (number, block) in [(1, first_block), (2, second_block)] {
        assert!(matches!(
            &block.citation.locator,
            Locator::Docx { part, paragraph, table_cell: None }
                if part.as_str() == sha256_hex(b"docs/main.xml") && *paragraph == number
        ));
        let CitationSource::Managed {
            version,
            extraction_revision,
        } = &block.citation.source
        else {
            panic!("Word blocks are managed-version citations");
        };
        assert_eq!(version.version_id.as_str(), "v1");
        assert_eq!(extraction_revision.get(), 7);
        assert_eq!(version.sha256.as_str(), sha256_hex(&data));
        block.citation.validate().unwrap();
    }
    assert_eq!(
        run(&data).unwrap().paragraphs[0].blocks[0]
            .citation
            .block_id,
        first_block.citation.block_id
    );
    let replacement = extraction(&DocxExtractor::default(), &data, DOCX_MEDIA, "v2", &|| {
        false
    })
    .unwrap();
    assert_ne!(
        replacement.paragraphs[0].blocks[0].citation.block_id,
        first_block.citation.block_id
    );
}

#[test]
fn nested_and_merged_cells_bind_to_real_table_coordinates() {
    let data = body(
        r#"
      <w:tbl>
        <w:tr><w:trPr><w:gridBefore w:val="1"/></w:trPr>
          <w:tc><w:tcPr><w:gridSpan w:val="2"/><w:vMerge w:val="restart"/></w:tcPr>
            <w:p><w:r><w:t>outer</w:t></w:r></w:p>
            <w:tbl><w:tr><w:tc><w:p><w:r><w:t>inner</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
            <w:p/>
          </w:tc>
          <w:tc><w:tcPr><w:hMerge w:val="restart"/></w:tcPr><w:p/></w:tc>
          <w:tc><w:tcPr><w:hMerge/></w:tcPr><w:p/></w:tc>
        </w:tr>
        <w:tr><w:trPr><w:gridBefore w:val="1"/></w:trPr>
          <w:tc><w:tcPr><w:gridSpan w:val="2"/><w:vMerge/></w:tcPr>
            <w:p><w:r><w:t>continuation</w:t></w:r></w:p>
          </w:tc>
        </w:tr>
      </w:tbl>
    "#,
    );
    let result = run(&data).unwrap();
    assert_eq!(result.cells.len(), 5);
    let outer = &result.cells[0];
    let h_first = &result.cells[1];
    let h_next = &result.cells[2];
    let next = &result.cells[3];
    let inner = &result.cells[4];
    assert_eq!(
        (outer.table, outer.row, outer.column, outer.column_span),
        (1, 1, 2, 2)
    );
    assert_eq!(
        (next.table, next.row, next.column, next.column_span),
        (1, 2, 2, 2)
    );
    assert_eq!(next.vertical_origin.as_ref(), Some(&outer.id));
    assert_eq!(h_next.horizontal_origin.as_ref(), Some(&h_first.id));
    assert_eq!(inner.enclosing_cell.as_ref(), Some(&outer.id));
    assert_eq!((inner.table, inner.row, inner.column), (2, 1, 1));
    for (number, expected_cell) in [(1, outer), (2, inner), (6, next)] {
        let block = &result.paragraphs[number - 1].blocks[0];
        assert!(matches!(
            &block.citation.locator,
            Locator::Docx { paragraph, table_cell: Some(cell), .. }
                if *paragraph == number as u32 && *cell == expected_cell.id
        ));
    }
}

#[test]
fn strict_body_and_bom_marked_xml_are_read_without_a_word_runtime() {
    let strict = document("<w:p><w:r><w:t>中</w:t></w:r></w:p>", WORD_STRICT);
    let mut utf16 = vec![0xff, 0xfe];
    for unit in strict.encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    let bytes = bundle(&utf16, MAIN_TYPE, "docs/main.xml", &[]);
    let result = run(&bytes).unwrap();
    assert_eq!(result.paragraphs[0].blocks[0].text, "中");
}

#[test]
fn tracked_or_unsupported_source_keeps_original_paragraph_numbers() {
    let data = body(
        r#"
     <w:del><w:p><w:r><w:t>deleted</w:t></w:r></w:p></w:del>
     <w:p><w:ins><w:r><w:t>inserted</w:t></w:r></w:ins>
       <w:r><w:object><w:p><w:r><w:t>object text</w:t></w:r></w:p></w:object></w:r>
     </w:p>
     <w:p><w:r><w:t>tail</w:t></w:r></w:p>
    "#,
    );
    let result = run(&data).unwrap();
    assert_eq!(
        result
            .paragraphs
            .iter()
            .map(|p| p.number)
            .collect::<Vec<_>>(),
        [2, 4]
    );
    assert_eq!(result.paragraphs[0].blocks[0].text, "inserted");
    assert_eq!(
        result.coverage_issues,
        BTreeSet::from([
            WordCoverageIssue::EmbeddedObject,
            WordCoverageIssue::RevisionMarkup,
        ])
    );
}

#[test]
fn malformed_namespace_unsafe_name_and_invalid_manifest_are_typed() {
    use DocxError::*;
    let good = document("<w:p><w:r><w:t>ok</w:t></w:r></w:p>", WORD);
    let bytes = |target, media, extras| bundle(good.as_bytes(), media, target, extras);
    assert_eq!(
        run(&bytes("../docs/main.xml", MAIN_TYPE, &[])).unwrap_err(),
        UnsafePartName
    );
    assert_eq!(
        run(&bytes(
            "/docs/main.xml",
            MAIN_TYPE,
            &[("docs/MAIN.xml", b"alias")]
        ))
        .unwrap_err(),
        DuplicatePart
    );
    assert_eq!(
        run(&bytes(
            "/docs/main.xml",
            MAIN_TYPE,
            &[("docs/%6dain.xml", b"alias")]
        ))
        .unwrap_err(),
        UnsafePartName
    );
    assert_eq!(
        run(&bytes(
            "/docs/main.xml",
            MAIN_TYPE,
            &[("docs/../escape.xml", b"escape")]
        ))
        .unwrap_err(),
        UnsafePartName
    );
    assert_eq!(
        run(&bytes("/docs/main.xml", "application/xml", &[])).unwrap_err(),
        InvalidContentTypes
    );
    let foreign = document("<w:p/>", "urn:not-word");
    assert_eq!(
        run(&bundle(
            foreign.as_bytes(),
            MAIN_TYPE,
            "/docs/main.xml",
            &[]
        ))
        .unwrap_err(),
        InvalidBody
    );
    let malformed = bundle(
        b"<document><p></document>",
        MAIN_TYPE,
        "/docs/main.xml",
        &[],
    );
    assert_eq!(run(&malformed).unwrap_err(), InvalidBody);
    let unclosed = bundle(
        format!(r#"<w:document xmlns:w="{WORD}"><w:body>"#).as_bytes(),
        MAIN_TYPE,
        "/docs/main.xml",
        &[],
    );
    assert_eq!(run(&unclosed).unwrap_err(), MalformedXml);
    let dtd = format!("<!DOCTYPE w:document []>{good}");
    assert_eq!(
        run(&bundle(dtd.as_bytes(), MAIN_TYPE, "/docs/main.xml", &[])).unwrap_err(),
        ForbiddenXml
    );
    let mut truncated = bytes("/docs/main.xml", MAIN_TYPE, &[]);
    truncated.truncate(truncated.len() - 1);
    assert_eq!(run(&truncated).unwrap_err(), MalformedPackage);
}

#[test]
fn encrypted_legacy_and_macro_packages_do_not_return_passages() {
    use DocxError::*;
    let mut file = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
    file.create_stream("/EncryptedPackage")
        .unwrap()
        .write_all(&[0; 16])
        .unwrap();
    file.create_stream("/EncryptionInfo")
        .unwrap()
        .write_all(&[0; 8])
        .unwrap();
    let encrypted = file.into_inner().into_inner();
    assert_eq!(run(&encrypted).unwrap_err(), Encrypted);

    let mut file = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
    file.create_stream("/WordDocument")
        .unwrap()
        .write_all(b"legacy")
        .unwrap();
    let old = file.into_inner().into_inner();
    assert_eq!(run(&old).unwrap_err(), CompoundContainer);
    assert_eq!(
        extraction(
            &DocxExtractor::default(),
            &old,
            "application/msword",
            "v1",
            &|| false
        )
        .unwrap_err(),
        UnsupportedFormat
    );

    let macros = bundle(
        document("<w:p/>", WORD).as_bytes(),
        "application/vnd.ms-word.document.macroEnabled.main+xml",
        "/docs/main.xml",
        &[],
    );
    assert_eq!(run(&macros).unwrap_err(), UnsupportedVariant);
}

#[test]
fn declared_and_actual_expansion_limits_and_xml_depth_are_enforced() {
    let xml = format!("<w:p><w:r><w:t>{}</w:t></w:r></w:p>", "a".repeat(32_768));
    let bytes = body(&xml);
    let direct = |limits| {
        extraction(
            &DocxExtractor::with_limits(limits).unwrap(),
            &bytes,
            DOCX_MEDIA,
            "v1",
            &|| false,
        )
        .unwrap_err()
    };
    assert_eq!(
        direct(DocxLimits {
            parts: 4,
            ..DocxLimits::default()
        }),
        DocxError::PartCountLimit
    );
    assert_eq!(
        direct(DocxLimits {
            part_bytes: 4096,
            ..DocxLimits::default()
        }),
        DocxError::PartSizeLimit
    );
    assert_eq!(
        direct(DocxLimits {
            expanded_bytes: 16_384,
            ..DocxLimits::default()
        }),
        DocxError::ExpandedLimit
    );
    assert_eq!(
        direct(DocxLimits {
            xml_depth: 4,
            ..DocxLimits::default()
        }),
        DocxError::XmlDepthLimit
    );
    assert_eq!(
        direct(DocxLimits {
            extraction: ExtractionLimits {
                normalized_bytes: 1024,
                ..ExtractionLimits::default()
            },
            ..DocxLimits::default()
        }),
        DocxError::Extraction(ExtractionError::NormalizedLimit)
    );
    assert_eq!(run(&bytes).unwrap().paragraphs[0].blocks.len(), 8);
}

#[test]
fn cancellation_interrupts_native_part_and_xml_work() {
    let text = "<w:p><w:r><w:t>body</w:t></w:r></w:p>".repeat(256);
    let bytes = body(&text);
    let calls = AtomicUsize::new(0);
    let cancel = || calls.fetch_add(1, Ordering::Relaxed) == 300;
    assert_eq!(
        extraction(&DocxExtractor::default(), &bytes, DOCX_MEDIA, "v1", &cancel).unwrap_err(),
        DocxError::Extraction(ExtractionError::Cancelled)
    );
    assert!(calls.load(Ordering::Relaxed) >= 301);
}
