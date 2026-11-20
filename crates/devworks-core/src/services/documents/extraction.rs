//! Native document extraction from an already authorized, immutable artifact version.
//! The caller owns scope checks and publication. Failed extraction yields no publishable text.

use crate::{
    fingerprint::{canonical_digest, sha256_hex},
    protocol::{
        Id, Revision,
        data::{Citation, CitationSource, Locator},
        provider::{ArtifactRef, Digest},
    },
};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use serde_json::json;
use std::{borrow::Cow, sync::Arc, time::Instant};
use unicode_normalization::UnicodeNormalization;

const TEXT_ENGINE: &str = "devworks-text-v1";
const MAX_HEADING_BYTES: usize = 256;

#[derive(Debug, Clone, Copy)]
pub struct ExtractionLimits {
    pub input_bytes: usize,
    pub normalized_bytes: usize,
    pub chunk_bytes: usize,
    pub blocks: usize,
}

impl Default for ExtractionLimits {
    fn default() -> Self {
        Self {
            input_bytes: 32 * 1024 * 1024,
            normalized_bytes: 16 * 1024 * 1024,
            chunk_bytes: 4096,
            blocks: 16_384,
        }
    }
}

pub struct ExtractionInput<'a> {
    pub version: &'a ArtifactRef,
    pub revision: &'a Revision,
    pub bytes: &'a [u8],
}

pub struct ExtractionControl<'a> {
    pub cancelled: &'a (dyn Fn() -> bool + Send + Sync),
    pub deadline: Instant,
}

impl ExtractionControl<'_> {
    pub fn check(&self) -> Result<(), ExtractionError> {
        if (self.cancelled)() {
            return Err(ExtractionError::Cancelled);
        }
        if Instant::now() >= self.deadline {
            return Err(ExtractionError::Deadline);
        }
        Ok(())
    }
}

/// Stable classes for the document job to map to bounded diagnostics. No source bytes are echoed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractionError {
    Cancelled,
    Deadline,
    InvalidLimits,
    SourceTooLarge,
    SourceMismatch,
    UnsupportedFormat,
    UnsupportedEncoding,
    InvalidEncoding,
    Empty,
    NormalizedLimit,
    BlockLimit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    pub level: u8,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct ExtractedBlock {
    pub citation: Citation,
    pub headings: Arc<[Heading]>,
    pub text: String,
}

impl ExtractedBlock {
    fn text(
        input: &ExtractionInput<'_>,
        line_range: (u32, u32),
        ordinal: usize,
        headings: Arc<[Heading]>,
        text: String,
    ) -> Self {
        let content_digest = sha256_hex(text.as_bytes());
        let locator = Locator::Text {
            first_line: line_range.0,
            last_line: line_range.1,
        };
        let identity = json!([
            TEXT_ENGINE,
            input.version,
            input.revision,
            ordinal,
            locator,
            content_digest
        ]);
        let block_id = canonical_digest(&identity).expect("bounded document block identity");
        Self {
            citation: Citation {
                source: CitationSource::Managed {
                    version: input.version.clone(),
                    extraction_revision: input.revision.clone(),
                },
                block_id: Id::new(block_id).expect("SHA256 is an opaque ID"),
                content_digest: Digest::try_from(content_digest).expect("SHA256 digest"),
                locator,
            },
            headings,
            text,
        }
    }
}

pub trait DocumentExtractor: Send + Sync {
    fn extract(
        &self,
        input: ExtractionInput<'_>,
        control: &ExtractionControl<'_>,
    ) -> Result<Vec<ExtractedBlock>, ExtractionError>;
}

/// Extracts UTF-8 and BOM-marked UTF-16. The source survives as inert NFC text with LF line ends.
/// CommonMark source offsets give the top-level section hierarchy without rendering markup.
#[derive(Default)]
pub struct TextExtractor {
    limits: ExtractionLimits,
}

impl TextExtractor {
    /// Tightens the native bounds, for example to test every refusal with small fixtures.
    pub fn with_limits(limits: ExtractionLimits) -> Result<Self, ExtractionError> {
        let defaults = ExtractionLimits::default();
        if limits.input_bytes == 0
            || limits.input_bytes > defaults.input_bytes
            || limits.normalized_bytes == 0
            || limits.normalized_bytes > defaults.normalized_bytes
            || !(4..=defaults.chunk_bytes).contains(&limits.chunk_bytes)
            || !(1..=defaults.blocks).contains(&limits.blocks)
        {
            return Err(ExtractionError::InvalidLimits);
        }
        Ok(Self { limits })
    }
}

impl DocumentExtractor for TextExtractor {
    fn extract(
        &self,
        input: ExtractionInput<'_>,
        control: &ExtractionControl<'_>,
    ) -> Result<Vec<ExtractedBlock>, ExtractionError> {
        use ExtractionError::*;
        control.check()?;
        if input.bytes.len() > self.limits.input_bytes {
            return Err(SourceTooLarge);
        }
        if input.version.validate().is_err()
            || input.version.byte_length.get() != input.bytes.len() as i64
            || input.version.sha256.as_str() != sha256_hex(input.bytes)
        {
            return Err(SourceMismatch);
        }

        let media = input.version.media_type.split(';').next().unwrap().trim();
        let markdown = match media.to_ascii_lowercase().as_str() {
            "text/plain" => false,
            "text/markdown" | "text/x-markdown" => true,
            _ => return Err(UnsupportedFormat),
        };
        let decoded = decode(input.bytes, control)?;
        let text = normalize(&decoded, self.limits.normalized_bytes, control)?;
        if text.trim().is_empty() {
            return Err(Empty);
        }
        let headings = if markdown {
            markdown_headings(&text, self.limits.blocks, control)?
        } else {
            Vec::new()
        };
        chunk_text(&input, &text, headings, &self.limits, control)
    }
}

fn decode<'a>(
    bytes: &'a [u8],
    control: &ExtractionControl<'_>,
) -> Result<Cow<'a, str>, ExtractionError> {
    use ExtractionError::*;
    if bytes.starts_with(&[0xff, 0xfe, 0, 0]) || bytes.starts_with(&[0, 0, 0xfe, 0xff]) {
        return Err(UnsupportedEncoding);
    }
    let utf16 = bytes
        .strip_prefix(&[0xff, 0xfe])
        .map(|rest| (rest, u16::from_le_bytes as fn([u8; 2]) -> u16))
        .or_else(|| {
            bytes
                .strip_prefix(&[0xfe, 0xff])
                .map(|rest| (rest, u16::from_be_bytes as _))
        });

    if let Some((rest, from_bytes)) = utf16 {
        if rest.len() % 2 != 0 {
            return Err(InvalidEncoding);
        }
        let units = rest
            .chunks_exact(2)
            .map(|pair| from_bytes([pair[0], pair[1]]));
        let mut decoded = String::new();
        for (index, scalar) in char::decode_utf16(units).enumerate() {
            if index % 4096 == 0 {
                control.check()?;
            }
            decoded.push(scalar.map_err(|_| InvalidEncoding)?);
        }
        return Ok(Cow::Owned(decoded));
    }

    let without_bom = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    std::str::from_utf8(without_bom)
        .map(Cow::Borrowed)
        .map_err(|_| InvalidEncoding)
}

fn normalize(
    decoded: &str,
    max_bytes: usize,
    control: &ExtractionControl<'_>,
) -> Result<String, ExtractionError> {
    let mut source = decoded.chars().peekable();
    let mut until_check = 0;
    let mut stopped = Ok(());
    let lf = std::iter::from_fn(|| {
        let scalar = source.next()?;
        if until_check == 0 {
            until_check = 4096;
            stopped = control.check();
            if stopped.is_err() {
                return None;
            }
        }
        until_check -= 1;
        if scalar == '\0' {
            stopped = Err(ExtractionError::InvalidEncoding);
            return None;
        }
        if scalar == '\r' {
            source.next_if_eq(&'\n');
            return Some('\n');
        }
        Some(scalar)
    })
    .fuse();

    let mut normalized = String::with_capacity(decoded.len().min(max_bytes));
    for scalar in lf.nfc() {
        if normalized.len() + scalar.len_utf8() > max_bytes {
            return Err(ExtractionError::NormalizedLimit);
        }
        normalized.push(scalar);
    }
    stopped?;
    control.check()?;
    Ok(normalized)
}

fn markdown_headings(
    text: &str,
    max_headings: usize,
    control: &ExtractionControl<'_>,
) -> Result<Vec<(usize, Heading)>, ExtractionError> {
    control.check()?;
    let mut headings = Vec::new();
    let mut active: Option<(usize, Heading)> = None;
    let mut depth = 0usize;
    for (event, offsets) in Parser::new(text).into_offset_iter() {
        control.check()?;
        match event {
            Event::Start(tag) => {
                if depth == 0
                    && let Tag::Heading { level, .. } = tag
                {
                    if headings.len() == max_headings {
                        return Err(ExtractionError::BlockLimit);
                    }
                    active = Some((
                        offsets.start,
                        Heading {
                            level: level as u8,
                            title: String::new(),
                        },
                    ));
                }
                depth += 1;
            }
            Event::End(tag) => {
                depth -= 1;
                if depth == 0
                    && matches!(tag, TagEnd::Heading(_))
                    && let Some(heading) = active.take()
                {
                    headings.push(heading);
                }
            }
            Event::Text(fragment) | Event::Code(fragment) => {
                if let Some((_, heading)) = &mut active {
                    let remaining = MAX_HEADING_BYTES - heading.title.len();
                    let end = fragment.floor_char_boundary(remaining.min(fragment.len()));
                    heading.title.push_str(&fragment[..end]);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some((_, heading)) = &mut active
                    && heading.title.len() < MAX_HEADING_BYTES
                {
                    heading.title.push(' ');
                }
            }
            _ => {}
        }
    }
    Ok(headings)
}

struct TextChunker<'a> {
    input: &'a ExtractionInput<'a>,
    limits: &'a ExtractionLimits,
    blocks: Vec<ExtractedBlock>,
    headings: Arc<[Heading]>,
    text: String,
    first_line: u32,
    last_line: u32,
}

impl TextChunker<'_> {
    fn flush(&mut self) -> Result<(), ExtractionError> {
        let text = std::mem::take(&mut self.text);
        if text.trim().is_empty() {
            return Ok(());
        }
        if self.blocks.len() == self.limits.blocks {
            return Err(ExtractionError::BlockLimit);
        }
        self.blocks.push(ExtractedBlock::text(
            self.input,
            (self.first_line, self.last_line),
            self.blocks.len(),
            self.headings.clone(),
            text,
        ));
        Ok(())
    }

    fn line(
        &mut self,
        mut text: &str,
        number: u32,
        control: &ExtractionControl<'_>,
    ) -> Result<(), ExtractionError> {
        if self.text.len() + text.len() > self.limits.chunk_bytes {
            self.flush()?;
        }
        while !text.is_empty() {
            control.check()?;
            let end = text.floor_char_boundary(self.limits.chunk_bytes.min(text.len()));
            if self.text.is_empty() {
                self.first_line = number;
            }
            self.last_line = number;
            self.text.push_str(&text[..end]);
            text = &text[end..];
            if !text.is_empty() {
                self.flush()?;
            }
        }
        Ok(())
    }
}

fn chunk_text(
    input: &ExtractionInput<'_>,
    text: &str,
    headings: Vec<(usize, Heading)>,
    limits: &ExtractionLimits,
    control: &ExtractionControl<'_>,
) -> Result<Vec<ExtractedBlock>, ExtractionError> {
    let mut chunker = TextChunker {
        input,
        limits,
        blocks: Vec::new(),
        headings: Arc::from([]),
        text: String::new(),
        first_line: 0,
        last_line: 0,
    };
    let mut headings = headings.into_iter().peekable();
    let mut offset = 0;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        offset += line.len();
        if let Some((_, next)) = headings.next_if(|(start, _)| *start < offset) {
            chunker.flush()?;
            let mut path: Vec<_> = chunker
                .headings
                .iter()
                .take_while(|heading| heading.level < next.level)
                .cloned()
                .collect();
            path.push(next);
            chunker.headings = Arc::from(path);
        }
        chunker.line(line, index as u32 + 1, control)?;
    }
    chunker.flush()?;
    control.check()?;
    Ok(chunker.blocks)
}

#[cfg(test)]
mod tests;
