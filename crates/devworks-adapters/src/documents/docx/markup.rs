//! Source annotations and relationships are kept separate from final-view passage text.
use super::{
    DocxError, DocxLimits, WordBody, WordCoverageIssue, WordEdit, WordEditKind, WordEntry,
    WordImagePlacement, WordPoint, WordReference, WordReferenceKind, WordStoryKind,
    xml::{self, Element, REL, REL_STRICT, WORD, WORD_STRICT, Xml},
};
use devworks_core::services::documents::extraction::{ExtractionControl, normalize_text};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub(super) struct Markup {
    pub entries: Vec<WordEntry>,
    pub references: Vec<WordReference>,
    pub edits: Vec<WordEdit>,
    pub images: Vec<ImageRef>,
    pub issues: BTreeSet<WordCoverageIssue>,
}

pub(super) struct ImageRef {
    pub relationship: String,
    pub placement: WordImagePlacement,
}

struct Edit {
    depth: usize,
    record: WordEdit,
}

struct Picture {
    depth: usize,
    addressed: bool,
    supported_layout: bool,
    refs: Vec<ImageRef>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ContextElement {
    Table,
    TableProperties,
    Row,
    RowProperties,
    Cell,
    CellProperties,
    ParagraphProperties,
    RunProperties,
    Other,
}

struct Scanner<'a> {
    kind: WordStoryKind,
    body: &'a WordBody,
    limits: &'a mut DocxLimits,
    control: &'a ExtractionControl<'a>,
    output: Markup,
    word: &'static str,
    inside_body: bool,
    paragraph: u32,
    paragraph_stack: Vec<(usize, u32)>,
    context: Vec<ContextElement>,
    section: u32,
    even_headers: bool,
    title_page: bool,
    section_ref_start: usize,
    section_depth: Option<usize>,
    section_links: BTreeMap<(WordStoryKind, String), String>,
    section_explicit: BTreeSet<(WordStoryKind, String)>,
    suppressed: Option<usize>,
    entry: Option<WordEntry>,
    edits: Vec<Edit>,
    pictures: Vec<Picture>,
    image_count: u32,
    edit_text_depth: Option<usize>,
    raw_edit_bytes: usize,
}

pub(super) fn read(
    raw: &[u8],
    config: (WordStoryKind, bool),
    body: &WordBody,
    limits: &mut DocxLimits,
    control: &ExtractionControl<'_>,
) -> Result<Markup, DocxError> {
    let mut reader = Scanner {
        kind: config.0,
        body,
        limits,
        control,
        output: Markup::default(),
        word: WORD,
        inside_body: false,
        paragraph: 0,
        paragraph_stack: Vec::new(),
        context: Vec::new(),
        section: 0,
        even_headers: config.1,
        title_page: false,
        section_ref_start: 0,
        section_depth: None,
        section_links: BTreeMap::new(),
        section_explicit: BTreeSet::new(),
        suppressed: None,
        entry: None,
        edits: Vec::new(),
        pictures: Vec::new(),
        image_count: 0,
        edit_text_depth: None,
        raw_edit_bytes: 0,
    };
    xml::walk(
        raw,
        reader.limits.xml_depth,
        control,
        |depth, node| match node {
            Xml::Start(element) => {
                reader.start(depth, &element)?;
                reader.context.push(context(&element, reader.word));
                Ok(())
            }
            Xml::End => {
                reader.context.pop();
                reader.end(depth)
            }
            Xml::Text(text) => reader.text(&text),
        },
    )?;
    reader.output.entries.sort_by_key(|entry| entry.id);
    if reader
        .output
        .entries
        .windows(2)
        .any(|pair| pair[0].id == pair[1].id)
    {
        return Err(DocxError::InvalidBody);
    }
    Ok(reader.output)
}

impl Scanner<'_> {
    fn start(&mut self, depth: usize, element: &Element<'_>) -> Result<(), DocxError> {
        let name = element.name;
        if depth == 1 {
            self.word = match name.ns {
                WORD => WORD,
                WORD_STRICT => WORD_STRICT,
                _ => return Err(DocxError::InvalidBody),
            };
            self.inside_body = self.kind != WordStoryKind::Main;
            return Ok(());
        }
        if depth == 2 && name.is(self.word, "body") && self.kind == WordStoryKind::Main {
            self.inside_body = true;
            return Ok(());
        }
        if !self.inside_body {
            return Ok(());
        }

        if name.is(self.word, "p") {
            self.paragraph += 1;
            self.paragraph_stack.push((depth, self.paragraph));
            if let Some(entry) = &mut self.entry {
                entry.first_paragraph.get_or_insert(self.paragraph);
                entry.last_paragraph = Some(self.paragraph);
            }
            for edit in &mut self.edits {
                edit.record.source.paragraph.get_or_insert(self.paragraph);
                edit.record.last_paragraph = Some(self.paragraph);
            }
        }
        if self.suppressed.is_some() {
            return Ok(());
        }
        if !self.hidden_from_final_text() && !self.pictures.is_empty() {
            self.picture_node(element)?;
        }
        if name.ns != self.word {
            if name.is(
                "http://schemas.openxmlformats.org/markup-compatibility/2006",
                "AlternateContent",
            ) {
                self.suppressed = Some(depth);
            }
            return Ok(());
        }

        if depth == 2 && self.kind.entry_element() == Some(name.local) {
            let separator = matches!(
                self.kind,
                WordStoryKind::Footnotes | WordStoryKind::Endnotes
            ) && element
                .attr(self.word, "type")
                .is_some_and(|kind| kind != "normal");
            if separator {
                self.suppressed = Some(depth);
            } else {
                self.consume_record()?;
                self.entry = Some(WordEntry {
                    id: id(element, self.word)?,
                    first_paragraph: None,
                    last_paragraph: None,
                    author: bounded(element.attr(self.word, "author"), 512)?,
                    date: bounded(element.attr(self.word, "date"), 128)?,
                    referenced: false,
                });
            }
            return Ok(());
        }

        match name.local {
            "drawing" | "pict"
                if self.paragraph_stack.last().is_some() && !self.hidden_from_final_text() =>
            {
                self.pictures.push(Picture {
                    depth,
                    addressed: false,
                    supported_layout: name.local == "drawing",
                    refs: Vec::new(),
                });
            }
            "txbxContent" | "object" | "altChunk" | "subDoc" => {
                self.suppressed = Some(depth);
                if name.local == "txbxContent" {
                    self.output.issues.insert(WordCoverageIssue::TextBox);
                }
            }
            "ins" | "del" | "moveTo" | "moveFrom" | "pPrChange" | "rPrChange" | "tblPrChange"
            | "trPrChange" | "tcPrChange" | "sectPrChange" | "cellIns" | "cellDel" => {
                self.start_edit(depth, element)?;
            }
            "cellMerge"
            | "numberingChange"
            | "customXmlInsRangeStart"
            | "customXmlDelRangeStart"
            | "customXmlMoveFromRangeStart"
            | "customXmlMoveToRangeStart" => {
                self.output
                    .issues
                    .insert(WordCoverageIssue::UnsupportedRevision);
            }
            "t" | "delText" if !self.edits.is_empty() => self.edit_text_depth = Some(depth),
            "tab" | "br" | "cr" if !self.edits.is_empty() => {
                self.append_edit_text(if name.local == "tab" { "\t" } else { "\n" })?;
            }
            "sectPr" if self.kind == WordStoryKind::Main && self.edits.is_empty() => {
                self.section += 1;
                self.section_depth = Some(depth);
                self.section_explicit.clear();
                self.title_page = false;
                self.section_ref_start = self.output.references.len();
            }
            "titlePg" if self.section_depth == depth.checked_sub(1) => {
                self.title_page = xml::on_off(element.attr(self.word, "val"))?;
            }
            _ => {}
        }

        if self.hidden_from_final_text() {
            return Ok(());
        }
        let reference = match name.local {
            "footnoteReference" => WordReferenceKind::Footnote(id(element, self.word)?),
            "endnoteReference" => WordReferenceKind::Endnote(id(element, self.word)?),
            "commentRangeStart" => WordReferenceKind::CommentStart(id(element, self.word)?),
            "commentRangeEnd" => WordReferenceKind::CommentEnd(id(element, self.word)?),
            "commentReference" => WordReferenceKind::CommentMark(id(element, self.word)?),
            "headerReference" | "footerReference" if self.section_depth == depth.checked_sub(1) => {
                let variant = element
                    .attr(self.word, "type")
                    .filter(|mode| matches!(*mode, "default" | "first" | "even"))
                    .ok_or(DocxError::InvalidBody)?
                    .to_owned();
                let relationship =
                    relationship(element, "id")?.ok_or(DocxError::InvalidRelationships)?;
                let kind = if name.local == "headerReference" {
                    WordStoryKind::Header
                } else {
                    WordStoryKind::Footer
                };
                let key = (kind, variant.clone());
                if !self.section_explicit.insert(key.clone()) {
                    return Err(DocxError::InvalidBody);
                }
                self.section_links.insert(key, relationship.clone());
                section_reference(kind, self.section, variant, relationship, false)
            }
            _ => return Ok(()),
        };
        self.consume_record()?;
        self.output.references.push(WordReference {
            source: self.point(),
            kind: reference,
            target_part: None,
        });
        Ok(())
    }

    fn end(&mut self, depth: usize) -> Result<(), DocxError> {
        if self
            .paragraph_stack
            .last()
            .is_some_and(|point| point.0 == depth)
        {
            self.paragraph_stack.pop();
        }
        if self.edit_text_depth == Some(depth) {
            self.edit_text_depth = None;
        }
        if self.suppressed == Some(depth) {
            self.suppressed = None;
        }
        if self.section_depth == Some(depth) {
            self.section_depth = None;
            let inherited: Vec<_> = self
                .section_links
                .iter()
                .filter(|(key, _)| !self.section_explicit.contains(*key))
                .map(|((kind, variant), relation)| {
                    section_reference(*kind, self.section, variant.clone(), relation.clone(), true)
                })
                .collect();
            for kind in inherited {
                self.consume_record()?;
                self.output.references.push(WordReference {
                    source: self.point(),
                    kind,
                    target_part: None,
                });
            }
            for reference in &mut self.output.references[self.section_ref_start..] {
                match &mut reference.kind {
                    WordReferenceKind::Header {
                        variant, active, ..
                    }
                    | WordReferenceKind::Footer {
                        variant, active, ..
                    } => {
                        *active = match variant.as_str() {
                            "first" => self.title_page,
                            "even" => self.even_headers,
                            _ => true,
                        };
                    }
                    _ => {}
                }
            }
        }
        if let Some(mut picture) = self.pictures.pop_if(|picture| picture.depth == depth) {
            if !picture.addressed {
                self.output
                    .issues
                    .insert(WordCoverageIssue::UnsupportedImage);
            }
            if !picture.refs.is_empty() && (!picture.supported_layout || picture.refs.len() > 1) {
                self.output
                    .issues
                    .insert(WordCoverageIssue::UnsupportedImageLayout);
                for image in &mut picture.refs {
                    image.placement.rendition_supported = false;
                }
            }
            self.output.images.extend(picture.refs);
        }
        while let Some(edit) = self.edits.pop_if(|edit| edit.depth == depth) {
            let mut edit = edit.record;
            if edit.source.paragraph.is_none() {
                self.output
                    .issues
                    .insert(WordCoverageIssue::UnsupportedRevision);
            }
            if edit.source.cell.is_none() {
                edit.source.cell = self.cell_for(edit.source.paragraph);
            }
            let kept = normalize_text(
                &edit.text,
                self.limits.extraction.normalized_bytes,
                self.control,
            )?;
            self.limits.extraction.normalized_bytes -= kept.len();
            edit.text = kept;
            self.output.edits.push(edit);
        }
        if depth == 2 {
            if let Some(entry) = self.entry.take() {
                self.output.entries.push(entry);
            }
            if self.kind == WordStoryKind::Main {
                self.inside_body = false;
            }
        }
        Ok(())
    }

    fn picture_node(&mut self, element: &Element<'_>) -> Result<(), DocxError> {
        let name = element.name;
        let drawing = matches!(
            name.ns,
            "http://schemas.openxmlformats.org/drawingml/2006/main"
                | "http://purl.oclc.org/ooxml/drawingml/main"
        );
        let vml = name.ns == "urn:schemas-microsoft-com:vml";
        if layout_requires_rendering(element, drawing, vml) {
            self.pictures
                .last_mut()
                .expect("picture scope")
                .supported_layout = false;
        }
        if !(drawing && name.local == "blip" || vml && name.local == "imagedata") {
            return Ok(());
        }
        self.pictures.last_mut().expect("picture scope").addressed = true;
        let external = element
            .attr(REL, "link")
            .or_else(|| element.attr(REL_STRICT, "link"))
            .is_some();
        if external {
            self.output.issues.insert(WordCoverageIssue::ExternalImage);
        }
        let field = if vml { "id" } else { "embed" };
        if let Some(id) = relationship(element, field)? {
            self.consume_record()?;
            self.image_count += 1;
            let placement = WordImagePlacement {
                source: self.point(),
                ordinal: self.image_count,
                rendition_supported: true,
            };
            self.pictures
                .last_mut()
                .expect("picture scope")
                .refs
                .push(ImageRef {
                    relationship: id,
                    placement,
                });
        } else if !external {
            self.output
                .issues
                .insert(WordCoverageIssue::UnsupportedImage);
        }
        Ok(())
    }

    fn start_edit(&mut self, depth: usize, element: &Element<'_>) -> Result<(), DocxError> {
        use ContextElement::*;
        let mut kind = match element.name.local {
            "ins" | "cellIns" => WordEditKind::Insertion,
            "del" | "cellDel" => WordEditKind::Deletion,
            "moveTo" => WordEditKind::MoveIn,
            "moveFrom" => WordEditKind::MoveOut,
            _ => WordEditKind::Properties,
        };
        let scope = match (element.name.local, self.context.as_slice()) {
            ("ins" | "del", [.., Row, RowProperties]) => depth - 2,
            ("cellIns" | "cellDel", [.., Cell, CellProperties])
            | ("tblPrChange", [.., Table, TableProperties])
            | ("trPrChange", [.., Row, RowProperties])
            | ("tcPrChange", [.., Cell, CellProperties]) => depth - 2,
            ("ins", [.., ParagraphProperties, RunProperties]) => {
                kind = WordEditKind::ParagraphBoundaryInsertion;
                depth
            }
            ("del", [.., ParagraphProperties, RunProperties]) => {
                self.output
                    .issues
                    .insert(WordCoverageIssue::UnsupportedRevision);
                kind = WordEditKind::ParagraphBoundaryDeletion;
                depth
            }
            ("cellIns" | "cellDel", _) => {
                self.output
                    .issues
                    .insert(WordCoverageIssue::UnsupportedRevision);
                depth
            }
            _ => depth,
        };
        self.consume_record()?;
        let source = self.point();
        if kind == WordEditKind::Properties {
            self.suppressed = Some(depth);
        }
        self.edits.push(Edit {
            depth: scope,
            record: WordEdit {
                last_paragraph: source.paragraph,
                source,
                kind,
                id: element
                    .attr(self.word, "id")
                    .map(|id| id.parse::<i32>())
                    .transpose()
                    .map_err(|_| DocxError::InvalidBody)?,
                author: bounded(element.attr(self.word, "author"), 512)?,
                date: bounded(element.attr(self.word, "date"), 128)?,
                text: String::new(),
            },
        });
        Ok(())
    }

    fn hidden_from_final_text(&self) -> bool {
        self.edits.iter().any(|edit| {
            matches!(
                edit.record.kind,
                WordEditKind::Deletion | WordEditKind::MoveOut
            )
        })
    }

    fn text(&mut self, text: &str) -> Result<(), DocxError> {
        if self.edit_text_depth.is_some() && self.suppressed.is_none() {
            self.append_edit_text(text)?;
        }
        Ok(())
    }

    fn append_edit_text(&mut self, text: &str) -> Result<(), DocxError> {
        let textual = |edit: &&mut Edit| {
            matches!(
                edit.record.kind,
                WordEditKind::Insertion
                    | WordEditKind::Deletion
                    | WordEditKind::MoveIn
                    | WordEditKind::MoveOut
            )
        };
        let added = self.edits.iter_mut().filter(textual).count() * text.len();
        self.raw_edit_bytes += added;
        if self.raw_edit_bytes > self.limits.part_bytes {
            return Err(DocxError::StructureLimit);
        }
        for edit in self.edits.iter_mut().filter(textual) {
            edit.record.text.push_str(text);
        }
        Ok(())
    }

    fn point(&self) -> WordPoint {
        let paragraph = self.paragraph_stack.last().map(|location| location.1);
        WordPoint {
            part: self.body.part.clone(),
            paragraph,
            cell: self.cell_for(paragraph),
        }
    }

    fn cell_for(&self, paragraph: Option<u32>) -> Option<devworks_core::protocol::Id> {
        let index = self
            .body
            .paragraphs
            .binary_search_by_key(&paragraph?, |item| item.number)
            .ok()?;
        self.body.paragraphs[index].cell.clone()
    }

    fn consume_record(&mut self) -> Result<(), DocxError> {
        self.limits.extraction.blocks = self
            .limits
            .extraction
            .blocks
            .checked_sub(1)
            .ok_or(DocxError::StructureLimit)?;
        Ok(())
    }
}

fn layout_requires_rendering(element: &Element<'_>, drawing: bool, vml: bool) -> bool {
    let name = element.name;
    let changes = |fields: &[&str]| {
        fields.iter().any(|field| {
            element
                .attr("", field)
                .is_some_and(|value| !matches!(value, "0" | "0%" | "false" | "off"))
        })
    };
    if drawing {
        return match name.local {
            "srcRect" => changes(&["l", "t", "r", "b"]),
            "xfrm" => changes(&["rot", "flipH", "flipV"]),
            "prstGeom" => element
                .attr("", "prst")
                .is_some_and(|shape| shape != "rect"),
            "custGeom" | "tile" | "clrChange" | "duotone" | "alphaMod" | "lum" | "grayscl" => true,
            _ => false,
        };
    }
    vml || matches!(name.local, "wgp" | "grpSp" | "sp")
}

fn context(element: &Element<'_>, word: &str) -> ContextElement {
    use ContextElement::*;
    if element.name.ns != word {
        return Other;
    }
    match element.name.local {
        "tbl" => Table,
        "tblPr" => TableProperties,
        "tr" => Row,
        "trPr" => RowProperties,
        "tc" => Cell,
        "tcPr" => CellProperties,
        "pPr" => ParagraphProperties,
        "rPr" => RunProperties,
        _ => Other,
    }
}

fn section_reference(
    kind: WordStoryKind,
    section: u32,
    variant: String,
    relationship: String,
    inherited: bool,
) -> WordReferenceKind {
    if kind == WordStoryKind::Header {
        WordReferenceKind::Header {
            section,
            variant,
            relationship,
            inherited,
            active: false,
        }
    } else {
        WordReferenceKind::Footer {
            section,
            variant,
            relationship,
            inherited,
            active: false,
        }
    }
}

fn relationship(element: &Element<'_>, attribute: &str) -> Result<Option<String>, DocxError> {
    bounded(
        element
            .attr(REL, attribute)
            .or_else(|| element.attr(REL_STRICT, attribute)),
        128,
    )
}

fn bounded(value: Option<&str>, maximum: usize) -> Result<Option<String>, DocxError> {
    match value {
        Some(value) if value.is_empty() || value.len() > maximum => Err(DocxError::InvalidBody),
        value => Ok(value.map(str::to_owned)),
    }
}

fn id(element: &Element<'_>, word: &str) -> Result<i32, DocxError> {
    element
        .attr(word, "id")
        .and_then(|id| id.parse().ok())
        .ok_or(DocxError::InvalidBody)
}
