use super::*;

const DRAWING_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";

fn chapter(markup: &str) -> String {
    format!(
        r#"<w:document xmlns:w="{WORD}" xmlns:r="{REL}" xmlns:a="{DRAWING_NS}">
      <w:body>{markup}</w:body></w:document>"#
    )
}

fn package(body: &str, relations: &str, extra_types: &str, parts: &[(&str, &[u8])]) -> Vec<u8> {
    let types = format!(
        r#"<Types xmlns="{}">
          <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
          <Default Extension="png" ContentType="image/png"/>
          <Default Extension="gif" ContentType="image/gif"/>
          <Default Extension="jpg" ContentType="image/jpeg"/>
          <Default Extension="webp" ContentType="image/webp"/>
          <Default Extension="svg" ContentType="image/svg+xml"/>
          <Override PartName="/word/document.xml" ContentType="{MAIN_TYPE}"/>
          {extra_types}
        </Types>"#,
        xml::TYPES
    );
    let root = relationships(&format!(
        r#"<Relationship Id="doc" Type="{REL}/officeDocument" Target="word/document.xml"/>"#
    ));
    let rels = relationships(relations);
    let mut entries = vec![
        ("[Content_Types].xml", types.as_bytes()),
        ("_rels/.rels", root.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/_rels/document.xml.rels", rels.as_bytes()),
    ];
    entries.extend_from_slice(parts);
    archive(&entries)
}

fn run_stories(bytes: &[u8], edition: &str) -> Result<WordDocument, DocxError> {
    DocxExtractor::default().extract(
        ExtractionInput {
            version: &version(bytes, DOCX_MEDIA, edition),
            revision: &Revision::new(7).unwrap(),
            bytes,
        },
        &ExtractionControl {
            deadline: Instant::now() + Duration::from_secs(30),
            cancelled: &|| false,
        },
    )
}

#[test]
fn auxiliary_stories_preserve_exact_entries_and_repeated_section_references() {
    let main = chapter(
        r#"
      <w:p><w:pPr><w:sectPr>
        <w:headerReference w:type="default" r:id="head"/>
        <w:footerReference w:type="default" r:id="foot"/>
      </w:sectPr></w:pPr><w:r><w:t>chapter one</w:t></w:r></w:p>
      <w:p><w:commentRangeStart w:id="8"/><w:r><w:t>chapter two</w:t>
        <w:footnoteReference w:id="2"/><w:endnoteReference w:id="3"/>
      </w:r><w:commentRangeEnd w:id="8"/><w:r><w:commentReference w:id="8"/></w:r></w:p>
      <w:sectPr/>
    "#,
    );
    let word = |markup: &str| markup.replace("WORD_NS", WORD);
    let footer =
        word(r#"<w:ftr xmlns:w="WORD_NS"><w:p><w:r><w:t>footer</w:t></w:r></w:p></w:ftr>"#);
    let header =
        word(r#"<w:hdr xmlns:w="WORD_NS"><w:p><w:r><w:t>header</w:t></w:r></w:p></w:hdr>"#);
    let footnotes = word(
        r#"<w:footnotes xmlns:w="WORD_NS">
      <w:footnote w:id="-1" w:type="separator"><w:p><w:r><w:t>separator</w:t></w:r></w:p></w:footnote>
      <w:footnote w:id="2"><w:p><w:r><w:t>footnote</w:t></w:r></w:p></w:footnote>
      <w:footnote w:id="4"><w:p><w:r><w:t>unused note</w:t></w:r></w:p></w:footnote>
    </w:footnotes>"#,
    );
    let endnotes = word(
        r#"<w:endnotes xmlns:w="WORD_NS">
      <w:endnote w:id="3"><w:p><w:r><w:t>endnote</w:t></w:r></w:p></w:endnote>
    </w:endnotes>"#,
    );
    let comments = word(
        r#"<w:comments xmlns:w="WORD_NS">
      <w:comment w:id="8" w:author="Reviewer" w:date="2026-11-20T06:00:00Z">
        <w:p><w:r><w:t>comment text</w:t></w:r></w:p>
      </w:comment>
    </w:comments>"#,
    );
    let rels = format!(
        r#"
      <Relationship Id="head" Type="{REL}/header" Target="sections/h.xml"/>
      <Relationship Id="foot" Type="{REL}/footer" Target="sections/f.xml"/>
      <Relationship Id="notes" Type="{REL}/footnotes" Target="notes/fn.xml"/>
      <Relationship Id="ends" Type="{REL}/endnotes" Target="notes/en.xml"/>
      <Relationship Id="comments" Type="{REL}/comments" Target="review/comments.xml"/>
    "#
    );
    let types = [
        ("sections/h.xml", "header"),
        ("sections/f.xml", "footer"),
        ("notes/fn.xml", "footnotes"),
        ("notes/en.xml", "endnotes"),
        ("review/comments.xml", "comments"),
    ].iter().map(|(part, kind)| {
        format!(r#"<Override PartName="/word/{part}" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.{kind}+xml"/>"#)
    }).collect::<String>();
    let bytes = package(
        &main,
        &rels,
        &types,
        &[
            ("word/sections/h.xml", header.as_bytes()),
            ("word/sections/f.xml", footer.as_bytes()),
            ("word/notes/fn.xml", footnotes.as_bytes()),
            ("word/notes/en.xml", endnotes.as_bytes()),
            ("word/review/comments.xml", comments.as_bytes()),
        ],
    );
    let doc = run_stories(&bytes, "original").unwrap();
    assert!(doc.coverage_issues.is_empty());
    assert_eq!(doc.version.version_id.as_str(), "original");
    assert_eq!(doc.version.sha256.as_str(), sha256_hex(&bytes));
    let searchable = doc
        .searchable_blocks()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        searchable,
        [
            "chapter one",
            "chapter two",
            "footnote",
            "endnote",
            "header",
            "footer"
        ]
    );
    let comments = doc.comments.as_ref().unwrap();
    assert_eq!(comments.entries[0].author.as_deref(), Some("Reviewer"));
    assert!(comments.entries[0].referenced);
    assert_eq!(comments.body.paragraphs[0].blocks[0].text, "comment text");
    assert!(matches!(
        &comments.body.paragraphs[0].blocks[0].citation.locator,
        Locator::Docx { part, paragraph: 1, .. } if part.as_str() == sha256_hex(b"word/review/comments.xml")
    ));
    let notes = &doc.stories[0];
    assert_eq!(notes.kind, WordStoryKind::Footnotes);
    assert_eq!(
        notes
            .entries
            .iter()
            .map(|entry| (entry.id, entry.referenced))
            .collect::<Vec<_>>(),
        [(2, true), (4, false)]
    );
    assert!(matches!(
        &notes.body.paragraphs[0].blocks[0].citation.locator,
        Locator::Docx { part, paragraph: 2, .. } if part.as_str() == sha256_hex(b"word/notes/fn.xml")
    ));
    let sections = doc
        .references
        .iter()
        .filter_map(|reference| match reference.kind {
            WordReferenceKind::Header {
                section, inherited, ..
            }
            | WordReferenceKind::Footer {
                section, inherited, ..
            } => Some((
                section,
                inherited,
                reference.source.paragraph,
                reference.target_part.as_deref(),
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        sections,
        [
            (1, false, Some(1), Some("word/sections/h.xml")),
            (1, false, Some(1), Some("word/sections/f.xml")),
            (2, true, None, Some("word/sections/h.xml")),
            (2, true, None, Some("word/sections/f.xml")),
        ]
    );
    assert_eq!(doc.references.len(), 9);
}

#[test]
fn final_view_tracks_insertions_deletions_moves_and_partial_markup() {
    let main = chapter(
        r#"
      <w:p>
        <w:ins w:id="2" w:author="A"><w:r><w:t>inserted </w:t></w:r></w:ins>
        <w:del w:id="3" w:author="B"><w:r><w:delText>deleted</w:delText></w:r></w:del>
        <w:moveFrom w:id="4"><w:r><w:t>old place</w:t></w:r></w:moveFrom>
        <w:moveTo w:id="5"><w:r><w:t>new place</w:t></w:r></w:moveTo>
      </w:p>
      <w:tbl><w:tr><w:tc><w:tcPr><w:cellMerge w:id="7"/></w:tcPr>
        <w:p><w:r><w:object/><w:t>retained cell</w:t></w:r></w:p>
      </w:tc></w:tr></w:tbl>
      <w:p><w:r><w:pict><w:txbxContent><w:p><w:r><w:t>textbox</w:t></w:r></w:p></w:txbxContent></w:pict></w:r></w:p>
    "#,
    );
    let bytes = package(&main, "", "", &[]);
    let doc = run_stories(&bytes, "r1").unwrap();
    assert_eq!(
        doc.searchable_blocks()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>(),
        ["inserted new place", "retained cell"]
    );
    assert_eq!(
        doc.edits
            .iter()
            .map(|edit| (edit.kind, edit.id, edit.text.as_str()))
            .collect::<Vec<_>>(),
        [
            (WordEditKind::Insertion, Some(2), "inserted "),
            (WordEditKind::Deletion, Some(3), "deleted"),
            (WordEditKind::MoveOut, Some(4), "old place"),
            (WordEditKind::MoveIn, Some(5), "new place"),
        ]
    );
    assert!(doc.edits.iter().all(|edit| matches!(
        edit.source.locator(),
        Some(Locator::Docx { paragraph: 1, .. })
    )));
    assert!(
        doc.coverage_issues
            .contains(&WordCoverageIssue::UnsupportedRevision)
    );
    assert!(
        doc.coverage_issues
            .contains(&WordCoverageIssue::EmbeddedObject)
    );
    assert!(doc.coverage_issues.contains(&WordCoverageIssue::TextBox));
}

fn image_bytes(width: u32, height: u32, kind: image::ImageFormat) -> Vec<u8> {
    let pixels = image::RgbImage::from_pixel(width, height, image::Rgb([160, 20, 40]));
    let mut bytes = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(pixels)
        .write_to(&mut bytes, kind)
        .unwrap();
    bytes.into_inner()
}

#[test]
fn embedded_rasters_are_statically_bounded_and_bound_to_actual_placements() {
    use image::ImageFormat::*;
    let png = image_bytes(11, 7, Png);
    let jpeg = image_bytes(9, 8, Jpeg);
    let gif = image_bytes(3, 4, Gif);
    let webp = image_bytes(2, 2, WebP);
    let huge = image_bytes(1500, 1500, Png);
    let main = chapter(
        r#"
      <w:p><w:r><w:drawing><a:blip r:embed="png"/></w:drawing></w:r></w:p>
      <w:p><w:r><w:t>mixed</w:t><w:drawing><a:blip r:embed="png"/></w:drawing></w:r></w:p>
      <w:p><w:r><w:drawing><a:blip r:embed="jpeg"/></w:drawing></w:r></w:p>
      <w:p><w:r><w:drawing><a:blip r:embed="gif"/></w:drawing></w:r></w:p>
      <w:p><w:r><w:drawing><a:blip r:embed="webp"/></w:drawing></w:r></w:p>
      <w:p><w:r><w:drawing><a:blip r:embed="large"/></w:drawing></w:r></w:p>
      <w:p><w:r><w:drawing><a:blip r:embed="broken"/></w:drawing></w:r></w:p>
      <w:p><w:r><w:drawing><a:blip r:embed="vector"/></w:drawing></w:r></w:p>
      <w:p><w:r><w:drawing><a:blip r:link="external"/></w:drawing></w:r></w:p>
      <w:p><w:del w:id="5"><w:r><w:drawing><a:blip r:embed="png"/></w:drawing></w:r></w:del></w:p>
    "#,
    );
    let rels = [
        ("png", "shared.png"),
        ("jpeg", "small.jpg"),
        ("gif", "small.gif"),
        ("webp", "small.webp"),
        ("large", "large.png"),
        ("broken", "bad.png"),
        ("vector", "vector.svg"),
    ]
    .iter()
    .map(|(id, part)| {
        format!(r#"<Relationship Id="{id}" Type="{REL}/image" Target="media/{part}"/>"#)
    })
    .collect::<String>()
        + &format!(
            r#"<Relationship Id="external" Type="{REL}/image" Target="https://example.invalid/image" TargetMode="External"/>"#
        );
    let bytes = package(
        &main,
        &rels,
        "",
        &[
            ("word/media/shared.png", png.as_slice()),
            ("word/media/small.jpg", jpeg.as_slice()),
            ("word/media/small.gif", gif.as_slice()),
            ("word/media/small.webp", webp.as_slice()),
            ("word/media/large.png", huge.as_slice()),
            ("word/media/bad.png", b"\x89PNG\r\n\x1a\n"),
            ("word/media/vector.svg", b"<svg/>"),
        ],
    );
    let doc = run_stories(&bytes, "v1").unwrap();
    let picture = doc
        .images
        .iter()
        .find(|image| image.part.ends_with("shared.png"))
        .unwrap();
    assert_eq!(
        picture
            .placements
            .iter()
            .map(|point| point.paragraph)
            .collect::<Vec<_>>(),
        [Some(1), Some(2)]
    );
    assert_eq!(
        picture
            .vision_placements
            .iter()
            .map(|point| point.paragraph)
            .collect::<Vec<_>>(),
        [Some(1)]
    );
    assert_eq!((picture.width, picture.height), (11, 7));
    assert_eq!(picture.original_digest, sha256_hex(&png));
    assert_eq!(picture.payload_digest, sha256_hex(&picture.payload));
    assert_eq!(doc.images.len(), 4);
    assert!(
        doc.images
            .iter()
            .all(|image| image.payload_media_type == "image/png"
                && image.payload.len() <= 2 * 1024 * 1024
                && image::guess_format(&image.payload).unwrap() == Png)
    );
    assert_eq!(
        doc.coverage_issues,
        BTreeSet::from([
            WordCoverageIssue::ImageBounds,
            WordCoverageIssue::InvalidImage,
            WordCoverageIssue::UnsupportedImage,
            WordCoverageIssue::ExternalImage,
        ])
    );
}
