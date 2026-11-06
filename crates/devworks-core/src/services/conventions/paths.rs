//! Source-backed breadcrumbs: at most three original heading spans, never copied per chunk.
use super::{
    ConventionService, access, model,
    passages::{DocumentRequest, PreparedDocument},
    queries,
};
use crate::{
    context::InvocationContext,
    ports::{Preparation, storage::*},
    protocol::{
        DomainErrorCode,
        provider::ArtifactRef,
        records::*,
        support::{BoundedList, BoundedText, Check, Content, GraphText},
    },
    services::{
        artifacts::{ConventionSourceLease, ConventionSourceSelection},
        cursor::CursorBinding,
        sessions::{Failure, Refusal, counter},
    },
};
use std::{
    collections::HashSet,
    io::{self, Read},
    sync::Arc,
};
fn changed() -> Failure {
    Refusal::new(DomainErrorCode::SourceChanged, "convention_path", None).into()
}
fn invalid(error: impl std::fmt::Display) -> StorageError {
    StorageError::InvalidRecord(error.to_string())
}
struct Path {
    owner: StoredConvention,
    source: ConventionPathSource,
    inline: Option<Content>,
}
fn resolve(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    owner: &StoredConvention,
    path: &ConventionIndexedPath,
) -> Result<Path, Failure> {
    if access::owner(read, ctx, &owner.id, Some(owner.revision), None)? != *owner {
        return Err(changed());
    }
    let header = read
        .conventions()
        .convention_index_generation(&path.generation_id)?
        .filter(|i| i.state == "Published" && i.convention_id == owner.id)
        .ok_or_else(changed)?;
    let chunk = read
        .conventions()
        .convention_chunk(&path.generation_id, &path.chunk_id)?
        .filter(|c| {
            c.convention_id == owner.id && c.convention_revision == header.convention_revision
        })
        .ok_or_else(changed)?;
    let captured = access::owner(read, ctx, &owner.id, Some(header.convention_revision), None)?;
    if captured.content_ref.as_ref() != Some(&header.source_ref)
        || owner.content_ref != captured.content_ref
        || owner.title != captured.title
        || owner.scope != captured.scope
    {
        return Err(changed());
    }
    let artifact: ArtifactRef = queries::parse(header.source_ref)?;
    let mut next = Some(chunk.section_id);
    let mut seen = HashSet::new();
    let mut sections = Vec::new();
    while let Some(id) = next {
        if sections.len() >= 4 || !seen.insert(id.clone()) {
            return Err(changed());
        }
        let section = read
            .conventions()
            .convention_section(&path.generation_id, &id)?
            .ok_or_else(changed)?;
        next = section.parent_id.clone();
        sections.push(section);
    }
    sections.reverse();
    let root = sections
        .first()
        .filter(|s| {
            s.level == 0 && s.parent_id.is_none() && s.heading.as_deref() == Some(&captured.title)
        })
        .ok_or_else(changed)?;
    if root.source_offset.is_some() || root.source_length.is_some() {
        return Err(changed());
    }
    let mut previous = 0;
    let mut spans = Vec::new();
    let mut inline = Some(captured.title.clone());
    for section in sections.iter().skip(1) {
        if section.level <= previous || section.level > 3 {
            return Err(changed());
        }
        previous = section.level;
        let first = section
            .source_offset
            .filter(|n| *n >= 0)
            .ok_or_else(changed)?;
        let length = section
            .source_length
            .filter(|n| *n >= 0)
            .ok_or_else(changed)?;
        if first
            .checked_add(length)
            .is_none_or(|n| n > artifact.byte_length.get())
        {
            return Err(changed());
        }
        spans.push(ConventionHeadingSpan {
            level: counter(i64::from(section.level)),
            first_byte: counter(first),
            byte_length: counter(length),
        });
        inline = match (inline, section.heading.as_ref()) {
            (Some(mut text), Some(heading))
                if text.len() + heading.len() + usize::from(section.level) + 4 <= 65536 =>
            {
                text.push_str(" > ");
                for _ in 0..section.level {
                    text.push('#');
                }
                text.push(' ');
                text.push_str(heading);
                Some(text)
            }
            _ => None,
        };
    }
    let source = ConventionPathSource {
        artifact,
        convention_revision: crate::services::sessions::rev(captured.revision),
        headings: BoundedList(spans),
    };
    source.check().map_err(invalid)?;
    let inline =
        inline.and_then(|s| serde_json::from_value::<Content>(serde_json::Value::String(s)).ok());
    Ok(Path {
        owner: captured,
        source,
        inline,
    })
}
/// Same exact source association for ranked results and explicit SectionPath reads.
pub(crate) fn path_source(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    owner: &StoredConvention,
    path: &ConventionIndexedPath,
) -> Result<ConventionSectionPath, Failure> {
    let result = resolve(read, ctx, owner, path)?;
    Ok(if let Some(text) = result.inline {
        ConventionSectionPath {
            text: Some(text),
            source: None,
        }
    } else {
        ConventionSectionPath {
            text: None,
            source: Some(result.source),
        }
    })
}
enum Piece {
    Literal(Vec<u8>),
    Source { first: u64, length: u64 },
}
struct PathReader<'a> {
    lease: Arc<ConventionSourceLease>,
    pieces: Vec<Piece>,
    piece: usize,
    offset: u64,
    stopped: &'a mut dyn FnMut() -> bool,
    cache: Vec<u8>,
    cache_piece: usize,
    cache_offset: u64,
}
impl Read for PathReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if (self.stopped)() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "path read cancelled",
            ));
        }
        loop {
            let Some(piece) = self.pieces.get(self.piece) else {
                return Ok(0);
            };
            let n = match piece {
                Piece::Literal(bytes) => {
                    let left = bytes.len().saturating_sub(self.offset as usize);
                    let n = buffer.len().min(left);
                    buffer[..n]
                        .copy_from_slice(&bytes[self.offset as usize..self.offset as usize + n]);
                    n
                }
                Piece::Source { first, length } => {
                    let left = length.saturating_sub(self.offset) as usize;
                    if left == 0 {
                        0
                    } else {
                        if self.cache_piece != self.piece
                            || self.offset < self.cache_offset
                            || self.offset >= self.cache_offset + self.cache.len() as u64
                        {
                            let count = left.min(65536);
                            self.cache = self
                                .lease
                                .read_range_controlled(first + self.offset, count, self.stopped)
                                .map_err(|_| io::Error::other("path source unavailable"))?;
                            if self.cache.len() != count {
                                return Err(io::Error::other("path source shortened"));
                            }
                            self.cache_piece = self.piece;
                            self.cache_offset = self.offset;
                        }
                        let start = (self.offset - self.cache_offset) as usize;
                        let count = buffer.len().min(self.cache.len() - start);
                        buffer[..count].copy_from_slice(&self.cache[start..start + count]);
                        count
                    }
                }
            };
            if n > 0 {
                self.offset += n as u64;
                return Ok(n);
            }
            self.piece += 1;
            self.offset = 0;
        }
    }
}
impl ConventionService {
    pub(super) fn prepare_path(
        &self,
        ctx: &InvocationContext,
        env: &Preparation<'_>,
        request: DocumentRequest<'_>,
        selected: &ConventionIndexedPath,
    ) -> Result<PreparedDocument, Failure> {
        let (path, lease, revision) = env.read(|read| {
            Ok((|| {
                let path = resolve(read, ctx, &request.owner, selected)?;
                let scoped = access::scope(read, ctx, request.scope, false)?;
                let revision = queries::scope_revision(read, &scoped.readable)?;
                let lease = self.artifacts.resolve_convention_source(
                    read,
                    ctx,
                    request.scope,
                    &path.owner,
                    ConventionSourceSelection::Markdown,
                )?;
                Ok::<_, Failure>((path, lease, revision))
            })())
        })??;
        let intent = serde_json::to_string(&request.intent).map_err(invalid)?;
        let generation = selected.generation_id.as_str();
        let chunk = selected.chunk_id.as_str();
        let cursor = CursorBinding::new(
            ctx,
            "conventions.path",
            &[&intent, generation, chunk],
            revision,
        );
        let position = request.cursor.map(|c| cursor.decode(c)).transpose()?;
        let (chars, piece, offset) = if let Some(position) = position {
            let mut fields = position.split(':');
            let chars = fields
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or(StorageError::CursorExpired)?;
            let piece = fields
                .next()
                .and_then(|s| s.parse::<usize>().ok())
                .ok_or(StorageError::CursorExpired)?;
            let offset = fields
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or(StorageError::CursorExpired)?;
            if fields.next().is_some() {
                return Err(StorageError::CursorExpired.into());
            }
            (chars, piece, offset)
        } else {
            (0, 0, 0)
        };
        let mut pieces = vec![Piece::Literal(path.owner.title.as_bytes().to_vec())];
        for span in &path.source.headings.0 {
            pieces.push(Piece::Literal(
                format!(" > {} ", "#".repeat(span.level.get() as usize)).into_bytes(),
            ));
            pieces.push(Piece::Source {
                first: span.first_byte.get() as u64,
                length: span.byte_length.get() as u64,
            });
        }
        if piece >= pieces.len() {
            return Err(StorageError::CursorExpired.into());
        }
        let mut stopped = || self.is_stopped() || env.stopped(self.clock.now_ms().get());
        let mut reader = PathReader {
            lease: Arc::new(lease),
            pieces,
            piece,
            offset,
            stopped: &mut stopped,
            cache: Vec::new(),
            cache_piece: usize::MAX,
            cache_offset: 0,
        };
        let mut text = String::new();
        let mut count = 0_u64;
        let more = loop {
            if count >= request.max_chars as u64 {
                break reader
                    .pieces
                    .iter()
                    .enumerate()
                    .skip(reader.piece)
                    .any(|(i, p)| {
                        let length = match p {
                            Piece::Literal(v) => v.len() as u64,
                            Piece::Source { length, .. } => *length,
                        };
                        length > if i == reader.piece { reader.offset } else { 0 }
                    });
            }
            let before = (reader.piece, reader.offset);
            let mut raw = [0_u8; 4];
            let n = reader.read(&mut raw[..1]).map_err(invalid)?;
            if n == 0 {
                break false;
            }
            let length = match raw[0] {
                0..=0x7f => 1,
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => return Err(changed()),
            };
            reader.read_exact(&mut raw[1..length]).map_err(invalid)?;
            let value = std::str::from_utf8(&raw[..length]).map_err(invalid)?;
            if text.len() + length > 65536 {
                reader.piece = before.0;
                reader.offset = before.1;
                break true;
            }
            text.push_str(value);
            count += 1;
        };
        let next = chars + count;
        let continuation = more.then(|| {
            BoundedText(cursor.encode(&format!("{next}:{}:{}", reader.piece, reader.offset)))
        });
        Ok(PreparedDocument {
            owner: path.owner,
            resource: None,
            scope_revision: revision,
            format: ConventionFormat::SectionPath,
            passage: Some(ConventionPassage {
                text: GraphText(text),
                offset: counter(i64::try_from(chars).map_err(invalid)?),
                next_cursor: continuation,
                has_more: more,
            }),
            binary: None,
            reason: None,
        })
    }
}
