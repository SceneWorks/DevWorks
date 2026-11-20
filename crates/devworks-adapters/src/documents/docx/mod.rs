//! Bounded WordprocessingML extraction, with no external relationship resolution.
mod body;
mod fields;
mod numbering;
mod opc;
mod package;
mod styles;
mod xml;

use devworks_core::{
    protocol::Id,
    services::documents::extraction::{
        ExtractedBlock, ExtractionControl, ExtractionError, ExtractionInput, ExtractionLimits,
    },
};
use std::collections::BTreeSet;

const DOCX_MEDIA: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";

#[derive(Clone, Copy, Debug)]
pub struct DocxLimits {
    pub extraction: ExtractionLimits,
    pub parts: usize,
    pub expanded_bytes: usize,
    pub part_bytes: usize,
    pub xml_depth: usize,
}

impl Default for DocxLimits {
    fn default() -> Self {
        Self {
            extraction: ExtractionLimits::default(),
            parts: 512,
            expanded_bytes: 128 * 1024 * 1024,
            part_bytes: 16 * 1024 * 1024,
            xml_depth: 128,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocxError {
    Extraction(ExtractionError),
    UnsupportedFormat,
    UnsupportedVariant,
    UnsupportedCompression,
    CompoundContainer,
    Encrypted,
    MalformedPackage,
    MalformedXml,
    ForbiddenXml,
    InvalidContentTypes,
    InvalidRelationships,
    InvalidBody,
    InvalidStyles,
    InvalidNumbering,
    UnsafePartName,
    DuplicatePart,
    MissingPart,
    PartCountLimit,
    PartSizeLimit,
    ExpandedLimit,
    XmlDepthLimit,
    XmlAttributeLimit,
    StructureLimit,
}

impl From<ExtractionError> for DocxError {
    fn from(error: ExtractionError) -> Self {
        Self::Extraction(error)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WordCoverageIssue {
    EmbeddedObject,
    ExternalContent,
    TextBox,
    Equation,
    AlternateContent,
    Drawing,
    RevisionMarkup,
    FieldCode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordMerge {
    Start,
    Continue,
}

#[derive(Clone, Debug)]
pub struct WordCell {
    pub id: Id,
    pub enclosing_cell: Option<Id>,
    pub table: u32,
    pub row: u32,
    pub column: u32,
    pub column_span: u32,
    pub vertical: Option<WordMerge>,
    pub horizontal: Option<WordMerge>,
    pub vertical_origin: Option<Id>,
    pub horizontal_origin: Option<Id>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WordList {
    pub numbering_id: u32,
    /// Zero-based level from WordprocessingML.
    pub level: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WordLinkTarget {
    External(String),
    Part(String),
    Bookmark(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WordLink {
    pub target: WordLinkTarget,
    pub anchor: Option<String>,
}

#[derive(Debug)]
pub struct WordParagraph {
    pub number: u32,
    pub style: Option<String>,
    pub heading_level: Option<u8>,
    pub list: Option<WordList>,
    pub cell: Option<Id>,
    pub hyperlinks: Vec<WordLink>,
    pub blocks: Vec<ExtractedBlock>,
}

#[derive(Debug)]
pub struct WordBody {
    /// Canonical OPC name; the citation uses a stable opaque ID derived from it.
    pub part: String,
    pub paragraphs: Vec<WordParagraph>,
    pub cells: Vec<WordCell>,
    pub coverage_issues: BTreeSet<WordCoverageIssue>,
}

/// Reads only the managed bytes and returns immutable passage citations. Trips any resource or
/// cancellation limit before returning output. Body figures and unsupported content are diagnosed.
#[derive(Default)]
pub struct DocxExtractor {
    limits: DocxLimits,
}

impl DocxExtractor {
    pub fn with_limits(limits: DocxLimits) -> Result<Self, DocxError> {
        use devworks_core::services::documents::extraction::TextExtractor;
        TextExtractor::with_limits(limits.extraction)?;
        let ceiling = DocxLimits::default();
        if !(1..=ceiling.parts).contains(&limits.parts)
            || !(1..=ceiling.expanded_bytes).contains(&limits.expanded_bytes)
            || !(1..=ceiling.part_bytes).contains(&limits.part_bytes)
            || !(1..=ceiling.xml_depth).contains(&limits.xml_depth)
        {
            return Err(ExtractionError::InvalidLimits.into());
        }
        Ok(Self { limits })
    }

    pub fn extract(
        &self,
        source: ExtractionInput<'_>,
        control: &ExtractionControl<'_>,
    ) -> Result<WordBody, DocxError> {
        control.check()?;
        source.verify(self.limits.extraction.input_bytes)?;
        let media = source.version.media_type.split(';').next().unwrap().trim();
        if !media.eq_ignore_ascii_case(DOCX_MEDIA) {
            return Err(DocxError::UnsupportedFormat);
        }
        let mut package = package::Package::open(source.bytes, &self.limits, control)?;
        let manifest = opc::Manifest::read(&mut package, &self.limits, control)?;
        let root = opc::read_relationships(&mut package, &manifest, "", &self.limits, control)?;
        let part = opc::main_part(&manifest, &root)?;
        let relations =
            opc::read_relationships(&mut package, &manifest, part, &self.limits, control)?;
        let styles = if let Some(name) = opc::styles_part(&manifest, &relations)? {
            styles::read(&package.read(name, control)?, &self.limits, control)?
        } else {
            styles::Styles::default()
        };
        let numbering = if let Some(name) = opc::numbering_part(&manifest, &relations)? {
            numbering::read(&package.read(name, control)?, &self.limits, control)?
        } else {
            numbering::Numbering::default()
        };
        let xml = package.read(part, control)?;
        let result = body::read(
            &xml,
            part,
            &source,
            &relations,
            (&styles, &numbering),
            &self.limits,
            control,
        )?;
        control.check()?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
