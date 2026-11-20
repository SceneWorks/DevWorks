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
use unicode_normalization::{
    UnicodeNormalization,
    char::{canonical_combining_class, decompose_canonical},
};

const TEXT_ENGINE: &str = "devworks-text-v1";
const MAX_HEADING_BYTES: usize = 256;
const MAX_CANONICAL_NONSTARTERS: usize = 1024;

#[derive(Debug, Clone, Copy)]
pub struct ExtractionLimits {
    /// Bytes in the immutable artifact.
    pub input_bytes: usize,
    /// Bytes in the whole NFC text before splitting.
    pub normalized_bytes: usize,
    /// UTF-8 bytes per block, including source line endings.
    pub chunk_bytes: usize,
    /// Maximum returned blocks and maximum top-level headings.
    pub blocks: usize,
    /// Limit structural markers before CommonMark allocates its block tree.
    pub markdown_markers: usize,
}

impl Default for ExtractionLimits {
    fn default() -> Self {
        Self {
            input_bytes: 32 * 1024 * 1024,
            normalized_bytes: 16 * 1024 * 1024,
            chunk_bytes: 4096,
            blocks: 16_384,
            markdown_markers: 1024 * 1024,
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
    UnicodeStructureLimit,
    MarkdownStructureLimit,
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

/// Extracts bytes the caller has already authorized for the exact managed version.
/// The caller supplies the extraction revision and must recheck authority and cancellation
/// at publication. An error, including cancellation or deadline expiry, yields no blocks.
pub trait DocumentExtractor: Send + Sync {
    fn extract(
        &self,
        input: ExtractionInput<'_>,
        control: &ExtractionControl<'_>,
    ) -> Result<Vec<ExtractedBlock>, ExtractionError>;
}

/// Extracts UTF-8 and BOM-marked UTF-16. The source survives as inert NFC text with LF line ends.
/// CommonMark source offsets give the top-level section hierarchy without rendering markup.
/// Checks cancellation between bounded steps and on both sides of the synchronous parser setup.
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
            || !(1..=defaults.markdown_markers).contains(&limits.markdown_markers)
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
            check_markdown_work(&decoded, self.limits.markdown_markers, control)?;
            markdown_headings(&decoded, self.limits.blocks, control)?
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
        let (units, trailing) = rest.as_chunks::<2>();
        if !trailing.is_empty() {
            return Err(InvalidEncoding);
        }
        let units = units.iter().copied().map(from_bytes);
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
    let mut nonstarters = 0;
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
        }
        if scalar.is_ascii() {
            nonstarters = 0;
        } else {
            // NFC buffers an entire nonstarter run before emitting it. Count canonical
            // decomposition here so expansion cannot exceed the work bound inside NFC.
            decompose_canonical(scalar, |part| {
                if canonical_combining_class(part) == 0 {
                    nonstarters = 0;
                } else {
                    nonstarters += 1;
                    if nonstarters > MAX_CANONICAL_NONSTARTERS {
                        stopped = Err(ExtractionError::UnicodeStructureLimit);
                    }
                }
            });
            if stopped.is_err() {
                return None;
            }
        }
        Some(if scalar == '\r' { '\n' } else { scalar })
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

// The CommonMark parser builds a synchronous block tree before yielding events. Count all
// possible ASCII punctuation and line boundaries conservatively before that pass.
fn check_markdown_work(
    source: &str,
    max_markers: usize,
    control: &ExtractionControl<'_>,
) -> Result<(), ExtractionError> {
    let mut markers = 0;
    for window in source.as_bytes().chunks(4096) {
        control.check()?;
        markers += window
            .iter()
            .filter(|byte| byte.is_ascii_punctuation() || matches!(byte, b'\n' | b'\r'))
            .count();
        if markers > max_markers {
            return Err(ExtractionError::MarkdownStructureLimit);
        }
    }
    Ok(())
}

fn markdown_headings(
    text: &str,
    max_headings: usize,
    control: &ExtractionControl<'_>,
) -> Result<Vec<(u32, Heading)>, ExtractionError> {
    control.check()?;
    let mut headings = Vec::new();
    let mut active: Option<(usize, Heading)> = None;
    let mut title_clipped = false;
    let mut depth = 0usize;
    let events = Parser::new(text).into_offset_iter();
    control.check()?;
    for (event, offsets) in events {
        control.check()?;
        match event {
            Event::Start(tag) => {
                if depth == 0
                    && let Tag::Heading { level, .. } = tag
                {
                    if headings.len() == max_headings {
                        return Err(ExtractionError::BlockLimit);
                    }
                    title_clipped = false;
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
                    && let Some((offset, mut heading)) = active.take()
                {
                    heading.title = heading.title.nfc().collect();
                    heading
                        .title
                        .truncate(heading.title.floor_char_boundary(MAX_HEADING_BYTES));
                    headings.push((offset, heading));
                }
            }
            Event::Text(fragment) | Event::Code(fragment) => {
                if !title_clipped && let Some((_, heading)) = &mut active {
                    let remaining = MAX_HEADING_BYTES - heading.title.len();
                    let end = fragment.floor_char_boundary(remaining.min(fragment.len()));
                    heading.title.push_str(&fragment[..end]);
                    title_clipped = end < fragment.len();
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if !title_clipped
                    && let Some((_, heading)) = &mut active
                    && heading.title.len() < MAX_HEADING_BYTES
                {
                    heading.title.push(' ');
                }
            }
            _ => {}
        }
    }
    source_heading_lines(text, headings, control)
}

// The parser sees the original Unicode spelling. NFC can turn non-ASCII scalar values into
// Markdown punctuation, so its byte offsets must be converted before chunking normalized text.
fn source_heading_lines(
    text: &str,
    headings: Vec<(usize, Heading)>,
    control: &ExtractionControl<'_>,
) -> Result<Vec<(u32, Heading)>, ExtractionError> {
    let mut result = Vec::with_capacity(headings.len());
    let mut headings = headings.into_iter().peekable();
    let mut offset = 0;
    let mut line = 0u32;
    let mut prior_cr = false;
    for part in text
        .as_bytes()
        .split_inclusive(|byte| matches!(byte, b'\r' | b'\n'))
    {
        if headings.peek().is_none() {
            break;
        }
        offset += part.len();
        if prior_cr && part == b"\n" {
            prior_cr = false;
            continue;
        }
        prior_cr = part.ends_with(b"\r");
        line += 1;
        if line.is_multiple_of(4096) {
            control.check()?;
        }
        if let Some((_, heading)) = headings.next_if(|(start, _)| *start < offset) {
            result.push((line, heading));
        }
    }
    control.check()?;
    Ok(result)
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
    headings: Vec<(u32, Heading)>,
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
    for (index, line) in text.split_inclusive('\n').enumerate() {
        let line_number = index as u32 + 1;
        if let Some((_, next)) = headings.next_if(|(start, _)| *start == line_number) {
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
        chunker.line(line, line_number, control)?;
    }
    chunker.flush()?;
    control.check()?;
    Ok(chunker.blocks)
}

#[cfg(test)]
mod tests;
