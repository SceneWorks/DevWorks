//! Attach labeled stories, annotations and images to one exact modern Word package.
use super::{
    DocxError, DocxLimits, WordBody, WordCoverageIssue, body, images, markup,
    numbering::Numbering,
    opc::{self, Manifest, Relationships, Target},
    package::Package,
    styles::Styles,
    xml::{self, WORD, WORD_STRICT, Xml},
};
use devworks_core::{
    fingerprint::sha256_hex,
    protocol::{Id, Revision, data::Locator, provider::ArtifactRef},
    services::documents::extraction::{ExtractedBlock, ExtractionControl, ExtractionInput},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WordStoryKind {
    Main,
    Footnotes,
    Endnotes,
    Header,
    Footer,
    Comments,
}

impl WordStoryKind {
    pub(super) fn root(self) -> &'static str {
        match self {
            Self::Main => "document",
            Self::Footnotes => "footnotes",
            Self::Endnotes => "endnotes",
            Self::Header => "hdr",
            Self::Footer => "ftr",
            Self::Comments => "comments",
        }
    }

    pub(super) fn entry_element(self) -> Option<&'static str> {
        match self {
            Self::Footnotes => Some("footnote"),
            Self::Endnotes => Some("endnote"),
            Self::Comments => Some("comment"),
            _ => None,
        }
    }

    fn relationship(self) -> &'static str {
        match self {
            Self::Header => "header",
            Self::Footer => "footer",
            kind => kind.root(),
        }
    }

    fn content_type(self) -> String {
        format!(
            "application/vnd.openxmlformats-officedocument.wordprocessingml.{}+xml",
            self.relationship(),
        )
    }
}

/// An exact source point within the package. Section marks can belong to the part without
/// belonging to a paragraph. Word text has no native pagination locator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WordPoint {
    pub part: String,
    pub paragraph: Option<u32>,
    pub cell: Option<Id>,
}

impl WordPoint {
    pub fn locator(&self) -> Option<Locator> {
        Some(Locator::Docx {
            part: part_id(&self.part),
            paragraph: self.paragraph?,
            table_cell: self.cell.clone(),
        })
    }
}

pub(super) fn part_id(part: &str) -> Id {
    Id::new(sha256_hex(part.as_bytes())).expect("SHA256 is an opaque ID")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WordReferenceKind {
    Footnote(i32),
    Endnote(i32),
    CommentStart(i32),
    CommentEnd(i32),
    CommentMark(i32),
    Header {
        section: u32,
        variant: String,
        relationship: String,
        inherited: bool,
        active: bool,
    },
    Footer {
        section: u32,
        variant: String,
        relationship: String,
        inherited: bool,
        active: bool,
    },
}

impl WordReferenceKind {
    fn entry(&self) -> Option<(WordStoryKind, i32)> {
        match *self {
            Self::Footnote(id) => Some((WordStoryKind::Footnotes, id)),
            Self::Endnote(id) => Some((WordStoryKind::Endnotes, id)),
            Self::CommentStart(id) | Self::CommentEnd(id) | Self::CommentMark(id) => {
                Some((WordStoryKind::Comments, id))
            }
            Self::Header { .. } | Self::Footer { .. } => None,
        }
    }

    fn section_relationship(&self) -> Option<(WordStoryKind, &str)> {
        match self {
            Self::Header { relationship, .. } => Some((WordStoryKind::Header, relationship)),
            Self::Footer { relationship, .. } => Some((WordStoryKind::Footer, relationship)),
            _ => None,
        }
    }
    fn active_section_reference(&self) -> bool {
        matches!(
            self,
            Self::Header { active: true, .. } | Self::Footer { active: true, .. }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WordReference {
    pub source: WordPoint,
    pub kind: WordReferenceKind,
    /// Canonical target part, populated only when the declared relationship and entry resolve.
    pub target_part: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WordEntry {
    pub id: i32,
    pub first_paragraph: Option<u32>,
    pub last_paragraph: Option<u32>,
    pub author: Option<String>,
    pub date: Option<String>,
    pub referenced: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordEditKind {
    Insertion,
    Deletion,
    MoveIn,
    MoveOut,
    Properties,
    ParagraphBoundaryInsertion,
    ParagraphBoundaryDeletion,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WordEdit {
    pub source: WordPoint,
    pub last_paragraph: Option<u32>,
    pub kind: WordEditKind,
    pub id: Option<i32>,
    pub author: Option<String>,
    pub date: Option<String>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WordImagePlacement {
    pub source: WordPoint,
    /// One-based picture occurrence in its part.
    pub ordinal: u32,
    pub rendition_supported: bool,
}

/// Bounded native raster with independent original and payload identity. Payload bytes can be
/// shared by multiple distinct placements without relabeling their origin.
#[derive(Debug)]
pub struct WordImage {
    pub part: String,
    pub original_digest: String,
    pub original_media_type: String,
    pub payload_digest: String,
    pub payload_media_type: &'static str,
    pub payload: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
    pub placements: Vec<WordImagePlacement>,
    pub vision_placements: Vec<WordImagePlacement>,
}

#[derive(Debug)]
pub struct WordStory {
    pub kind: WordStoryKind,
    pub body: WordBody,
    pub entries: Vec<WordEntry>,
    pub active: bool,
    searchable_paragraphs: BTreeSet<u32>,
}

impl WordStory {
    fn is_searchable(&self, paragraph: u32) -> bool {
        self.active
            && (self.kind.entry_element().is_none()
                || self.searchable_paragraphs.contains(&paragraph))
    }
}

#[derive(Debug)]
pub struct WordDocument {
    pub version: ArtifactRef,
    pub extraction_revision: Revision,
    pub body: WordBody,
    pub stories: Vec<WordStory>,
    /// Comments and tracked change records are source evidence outside searchable final text.
    pub comments: Option<WordStory>,
    pub edits: Vec<WordEdit>,
    pub references: Vec<WordReference>,
    pub images: Vec<WordImage>,
    pub coverage_issues: BTreeSet<WordCoverageIssue>,
}

impl WordDocument {
    pub fn searchable_blocks(&self) -> impl Iterator<Item = &ExtractedBlock> {
        self.body
            .paragraphs
            .iter()
            .chain(self.stories.iter().flat_map(|story| {
                story
                    .body
                    .paragraphs
                    .iter()
                    .filter(|paragraph| story.is_searchable(paragraph.number))
            }))
            .flat_map(|paragraph| &paragraph.blocks)
    }
}

pub(super) fn extract<'a>(
    package: &mut Package<'a>,
    manifest: &Manifest,
    main: (&str, &Relationships),
    source: &ExtractionInput<'a>,
    formats: (&Styles, &Numbering),
    limits: &DocxLimits,
    control: &ExtractionControl<'_>,
) -> Result<WordDocument, DocxError> {
    let even_headers = even_header_setting(package, manifest, main.1, limits, control)?;
    let mut reader = Reader {
        package,
        manifest,
        source,
        formats,
        even_headers,
        limits: *limits,
        control,
    };
    let (body, markup) = reader.part(main.0, WordStoryKind::Main, main.1)?;
    let mut output = WordDocument {
        version: source.version.clone(),
        extraction_revision: source.revision.clone(),
        coverage_issues: body.coverage_issues.clone(),
        body,
        stories: Vec::new(),
        comments: None,
        edits: Vec::new(),
        references: Vec::new(),
        images: Vec::new(),
    };
    let mut image_refs = BTreeMap::new();
    reader.attach(&mut output, markup, main.1, &mut image_refs)?;
    let targets = story_targets(main.1, &output.references, &mut output.coverage_issues)?;
    let active_sections: BTreeSet<_> = output
        .references
        .iter()
        .filter(|reference| reference.kind.active_section_reference())
        .filter_map(|reference| reference.kind.section_relationship())
        .filter_map(|(kind, id)| section_target(main.1, kind, id))
        .map(str::to_owned)
        .collect();
    for (kind, part) in targets {
        control.check()?;
        manifest.expected(&part, &kind.content_type())?;
        let relationships =
            opc::read_relationships(reader.package, manifest, &part, limits, control)?;
        let (body, markup) = reader.part(&part, kind, &relationships)?;
        output.coverage_issues.extend(&body.coverage_issues);
        let entries = reader.attach(&mut output, markup, &relationships, &mut image_refs)?;
        let story = WordStory {
            kind,
            body,
            entries,
            active: kind.entry_element().is_some() || active_sections.contains(&part),
            searchable_paragraphs: BTreeSet::new(),
        };
        if kind == WordStoryKind::Comments {
            output.comments = Some(story);
        } else {
            output.stories.push(story);
        }
    }
    resolve_references(&mut output, main.1, control)?;
    reader.resolve_images(&mut output, image_refs)?;
    control.check()?;
    Ok(output)
}

struct Reader<'a, 'input, 'clock> {
    package: &'a mut Package<'input>,
    manifest: &'a Manifest,
    source: &'a ExtractionInput<'input>,
    formats: (&'a Styles, &'a Numbering),
    even_headers: bool,
    limits: DocxLimits,
    control: &'a ExtractionControl<'clock>,
}

impl Reader<'_, '_, '_> {
    fn part(
        &mut self,
        part: &str,
        kind: WordStoryKind,
        relationships: &Relationships,
    ) -> Result<(WordBody, markup::Markup), DocxError> {
        let raw = self.package.read(part, self.control)?;
        let mut body = body::read(
            &raw,
            (part, kind),
            self.source,
            relationships,
            self.formats,
            &self.limits,
            self.control,
        )?;
        self.limits.extraction.normalized_bytes -= body.normalized_bytes;
        self.limits.extraction.blocks = self
            .limits
            .extraction
            .blocks
            .checked_sub(
                body.paragraphs
                    .iter()
                    .map(|paragraph| paragraph.blocks.len().max(1))
                    .sum::<usize>(),
            )
            .ok_or(DocxError::StructureLimit)?;
        let markup = markup::read(
            &raw,
            (kind, self.even_headers),
            &body,
            &mut self.limits,
            self.control,
        )?;
        if !markup.edits.is_empty() {
            body.coverage_issues
                .remove(&WordCoverageIssue::RevisionMarkup);
        }
        if (!markup.images.is_empty() || markup.issues.contains(&WordCoverageIssue::ExternalImage))
            && !markup.issues.contains(&WordCoverageIssue::UnsupportedImage)
        {
            body.coverage_issues.remove(&WordCoverageIssue::Drawing);
        }
        Ok((body, markup))
    }

    fn attach(
        &self,
        output: &mut WordDocument,
        markup: markup::Markup,
        relationships: &Relationships,
        image_refs: &mut BTreeMap<String, Vec<WordImagePlacement>>,
    ) -> Result<Vec<WordEntry>, DocxError> {
        output.coverage_issues.extend(markup.issues);
        output.edits.extend(markup.edits);
        output.references.extend(markup.references);
        for image in markup.images {
            self.control.check()?;
            match relationships.get(&image.relationship) {
                Some(rel) if opc::is_kind(&rel.kind, "image") => match &rel.target {
                    Target::Internal(part) if self.package.names.contains_key(part) => {
                        image_refs
                            .entry(part.clone())
                            .or_default()
                            .push(image.placement);
                    }
                    Target::External(_) => {
                        output
                            .coverage_issues
                            .insert(WordCoverageIssue::ExternalImage);
                    }
                    _ => {
                        output
                            .coverage_issues
                            .insert(WordCoverageIssue::InvalidImage);
                    }
                },
                _ => {
                    output
                        .coverage_issues
                        .insert(WordCoverageIssue::InvalidImage);
                }
            }
        }
        Ok(markup.entries)
    }

    fn resolve_images(
        &mut self,
        output: &mut WordDocument,
        refs: BTreeMap<String, Vec<WordImagePlacement>>,
    ) -> Result<(), DocxError> {
        let mut remaining = self.limits.expanded_bytes.min(32 * 1024 * 1024);
        for (part, placements) in refs {
            self.control.check()?;
            let Some(media) = self.manifest.media(&part) else {
                output
                    .coverage_issues
                    .insert(WordCoverageIssue::InvalidImage);
                continue;
            };
            let bytes = self.package.read(&part, self.control)?;
            let raster = match images::read(&bytes, media, self.control)? {
                Ok(image) => image,
                Err(issue) => {
                    output.coverage_issues.insert(issue);
                    continue;
                }
            };
            let Some(next) = remaining.checked_sub(raster.payload.len()) else {
                output
                    .coverage_issues
                    .insert(WordCoverageIssue::ImageBounds);
                break;
            };
            remaining = next;
            let vision_placements = placements
                .iter()
                .filter(|placement| {
                    placement.rendition_supported && image_only(output, &placement.source)
                })
                .cloned()
                .collect();
            output.images.push(WordImage {
                part,
                original_digest: sha256_hex(&bytes),
                original_media_type: media.to_owned(),
                payload_digest: sha256_hex(&raster.payload),
                payload_media_type: "image/png",
                payload: Arc::from(raster.payload),
                width: raster.width,
                height: raster.height,
                placements,
                vision_placements,
            });
        }
        Ok(())
    }
}

fn story_targets(
    relationships: &Relationships,
    refs: &[WordReference],
    issues: &mut BTreeSet<WordCoverageIssue>,
) -> Result<BTreeSet<(WordStoryKind, String)>, DocxError> {
    let mut targets = BTreeSet::new();
    let mut used_parts = BTreeMap::<String, WordStoryKind>::new();
    for kind in [
        WordStoryKind::Footnotes,
        WordStoryKind::Endnotes,
        WordStoryKind::Comments,
    ] {
        if let Some(part) = opc::unique_internal(relationships, kind.relationship())? {
            targets.insert((kind, part.to_owned()));
        }
    }
    for reference in refs {
        if let Some((kind, id)) = reference.kind.section_relationship() {
            match section_target(relationships, kind, id) {
                Some(part) => {
                    targets.insert((kind, part.to_owned()));
                }
                None => {
                    issues.insert(WordCoverageIssue::BrokenStoryReference);
                }
            }
        }
    }
    for (kind, part) in &targets {
        if used_parts.insert(part.clone(), *kind).is_some() {
            return Err(DocxError::InvalidRelationships);
        }
    }
    Ok(targets)
}

fn section_target<'a>(
    relationships: &'a Relationships,
    kind: WordStoryKind,
    id: &str,
) -> Option<&'a str> {
    let relationship = relationships
        .get(id)
        .filter(|item| opc::is_kind(&item.kind, kind.relationship()))?;
    match &relationship.target {
        Target::Internal(part) => Some(part.as_str()),
        Target::External(_) => None,
    }
}

fn resolve_references(
    document: &mut WordDocument,
    main: &Relationships,
    control: &ExtractionControl<'_>,
) -> Result<(), DocxError> {
    let mut searchable_parts = BTreeSet::from([document.body.part.clone()]);
    searchable_parts.extend(
        document
            .stories
            .iter()
            .filter(|story| {
                story.active && matches!(story.kind, WordStoryKind::Header | WordStoryKind::Footer)
            })
            .map(|story| story.body.part.clone()),
    );
    for reference in &mut document.references {
        control.check()?;
        if let Some((kind, entry_id)) = reference.kind.entry() {
            let story = document
                .stories
                .iter_mut()
                .chain(document.comments.iter_mut())
                .find(|story| story.kind == kind);
            if let Some(story) = story
                && let Ok(index) = story
                    .entries
                    .binary_search_by_key(&entry_id, |entry| entry.id)
            {
                story.entries[index].referenced |=
                    searchable_parts.contains(&reference.source.part);
                reference.target_part = Some(story.body.part.clone());
            }
        } else if let Some((kind, id)) = reference.kind.section_relationship() {
            reference.target_part = section_target(main, kind, id).map(str::to_owned);
        }
        if reference.target_part.is_none() {
            document
                .coverage_issues
                .insert(WordCoverageIssue::BrokenStoryReference);
        }
    }
    for story in &mut document.stories {
        for entry in story.entries.iter().filter(|entry| entry.referenced) {
            control.check()?;
            if let Some((first, last)) = entry.first_paragraph.zip(entry.last_paragraph) {
                story.searchable_paragraphs.extend(first..=last);
            }
        }
    }
    Ok(())
}

fn image_only(document: &WordDocument, point: &WordPoint) -> bool {
    let has_empty_paragraph = |body: &WordBody| {
        point.paragraph.is_some_and(|number| {
            body.paragraphs
                .binary_search_by_key(&number, |para| para.number)
                .is_ok_and(|index| body.paragraphs[index].blocks.is_empty())
        })
    };
    if point.part == document.body.part {
        return has_empty_paragraph(&document.body);
    }
    document
        .stories
        .iter()
        .find(|story| story.body.part == point.part)
        .is_some_and(|story| {
            has_empty_paragraph(&story.body)
                && point
                    .paragraph
                    .is_some_and(|number| story.is_searchable(number))
        })
}

fn even_header_setting(
    package: &mut Package<'_>,
    manifest: &Manifest,
    relationships: &Relationships,
    limits: &DocxLimits,
    control: &ExtractionControl<'_>,
) -> Result<bool, DocxError> {
    let Some(part) = opc::unique_internal(relationships, "settings")? else {
        return Ok(false);
    };
    manifest.expected(
        part,
        "application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml",
    )?;
    let raw = package.read(part, control)?;
    let mut word = WORD;
    let mut enabled = None;
    xml::walk(&raw, limits.xml_depth, control, |depth, event| {
        if let Xml::Start(element) = event {
            if depth == 1 {
                word = match element.name.ns {
                    WORD => WORD,
                    WORD_STRICT => WORD_STRICT,
                    _ => return Err(DocxError::InvalidBody),
                };
                if element.name.local != "settings" {
                    return Err(DocxError::InvalidBody);
                }
            } else if depth == 2 && element.name.is(word, "evenAndOddHeaders") {
                if enabled.is_some() {
                    return Err(DocxError::InvalidBody);
                }
                enabled = Some(xml::on_off(element.attr(word, "val"))?);
            }
        }
        Ok(())
    })?;
    Ok(enabled.unwrap_or(false))
}
