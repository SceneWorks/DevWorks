//! Bounded UTF-8 and binary source reads, performed outside SQL with actual reader leases.
use super::{ConventionService, access, model, queries};
use crate::{
    context::InvocationContext,
    ports::{Preparation, storage::*},
    protocol::{
        DomainErrorCode,
        data::DataScope,
        records::*,
        support::{BoundedText, GraphText},
    },
    services::{
        artifacts::ConventionSourceSelection,
        cursor::CursorBinding,
        sessions::{Failure, Refusal, counter},
    },
};
use base64::Engine;
use std::io::{self, Read};

pub(super) struct DocumentRequest<'a> {
    pub owner: StoredConvention,
    pub scope: Option<&'a DataScope>,
    pub format: ConventionFormat,
    pub resource: Option<StoredConventionResource>,
    pub cursor: Option<&'a str>,
    pub max_chars: usize,
    pub max_bytes: usize,
    pub intent: serde_json::Value,
}
pub(super) struct PreparedDocument {
    pub owner: StoredConvention,
    pub resource: Option<StoredConventionResource>,
    pub scope_revision: i64,
    pub format: ConventionFormat,
    pub passage: Option<ConventionPassage>,
    pub binary: Option<ConventionBinaryPassage>,
    pub reason: Option<String>,
}
fn invalid(error: impl std::fmt::Display) -> StorageError {
    StorageError::InvalidRecord(error.to_string())
}
fn unavailable() -> Failure {
    Refusal::new(DomainErrorCode::SourceChanged, "convention_source", None).into()
}
impl ConventionService {
    pub(super) fn prepare_document(
        &self,
        ctx: &InvocationContext,
        env: &Preparation<'_>,
        request: DocumentRequest<'_>,
    ) -> Result<PreparedDocument, Failure> {
        if matches!(
            request.format,
            ConventionFormat::Manifest | ConventionFormat::SectionPath
        ) {
            return Err(Refusal::new(
                DomainErrorCode::UnsupportedCapability,
                "convention_format",
                None,
            )
            .into());
        }
        if self.is_stopped() || env.stopped(self.clock.now_ms().get()) {
            return Err(StorageError::Cancelled.into());
        }
        let (revision, lease) = env.read(|read| {
            Ok((|| {
                let scoped = access::scope(read, ctx, request.scope, false)?;
                let revision = queries::scope_revision(read, &scoped.readable)?;
                let selection = match request.format {
                    ConventionFormat::Raw => ConventionSourceSelection::Raw,
                    ConventionFormat::Resource => ConventionSourceSelection::Resource(
                        request.resource.as_ref().ok_or_else(unavailable)?,
                    ),
                    _ => ConventionSourceSelection::Markdown,
                };
                let absent = match request.format {
                    ConventionFormat::Raw => request.owner.raw_content_ref.is_none(),
                    _ => request.owner.content_ref.is_none(),
                };
                let lease = if absent || request.owner.lifecycle == "Deleted" {
                    None
                } else {
                    Some(self.artifacts.resolve_convention_source(
                        read,
                        ctx,
                        request.scope,
                        &request.owner,
                        selection,
                    )?)
                };
                Ok::<_, Failure>((revision, lease))
            })())
        })??;
        let mut prepared = PreparedDocument {
            owner: request.owner,
            resource: request.resource,
            scope_revision: revision,
            format: request.format,
            passage: None,
            binary: None,
            reason: None,
        };
        let Some(lease) = lease else {
            prepared.reason = Some(
                "This recorded revision has no available source for the selected format.".into(),
            );
            return Ok(prepared);
        };
        let source_revision = prepared.owner.revision.to_string();
        let intent = serde_json::to_string(&request.intent).map_err(invalid)?;
        let cursor = CursorBinding::new(
            ctx,
            "conventions.source",
            &[&intent, &source_revision],
            revision,
        );
        let position = request.cursor.map(|c| cursor.decode(c)).transpose()?;
        if matches!(prepared.format, ConventionFormat::Resource) {
            let offset = position
                .as_deref()
                .map(|s| s.parse::<u64>().map_err(|_| StorageError::CursorExpired))
                .transpose()?
                .unwrap_or(0);
            if offset > lease.version.byte_count as u64 {
                return Err(StorageError::CursorExpired.into());
            }
            let mut bytes = lease
                .read_range(offset, request.max_bytes + 1)
                .map_err(|_| unavailable())?;
            let more = bytes.len() > request.max_bytes;
            bytes.truncate(request.max_bytes);
            let end = offset
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| invalid("resource offset overflow"))?;
            prepared.binary = Some(ConventionBinaryPassage {
                base64: BoundedText(base64::engine::general_purpose::STANDARD.encode(&bytes)),
                first_byte: counter(i64::try_from(offset).map_err(invalid)?),
                byte_length: counter(bytes.len() as i64),
                next_cursor: more.then(|| BoundedText(cursor.encode(&end.to_string()))),
                has_more: more,
            });
        } else {
            let (chars, offset) = position
                .as_deref()
                .map(|s| {
                    s.split_once(':')
                        .ok_or(StorageError::CursorExpired)
                        .and_then(|(a, b)| {
                            Ok((
                                a.parse::<u64>().map_err(|_| StorageError::CursorExpired)?,
                                b.parse::<u64>().map_err(|_| StorageError::CursorExpired)?,
                            ))
                        })
                })
                .transpose()?
                .unwrap_or((0, 0));
            if offset > lease.version.byte_count as u64 {
                return Err(StorageError::CursorExpired.into());
            }
            let bytes = lease.read_range(offset, 65540).map_err(|_| unavailable())?;
            let valid = match std::str::from_utf8(&bytes) {
                Ok(s) => s,
                Err(e)
                    if e.error_len().is_none()
                        && offset + (bytes.len() as u64) < lease.version.byte_count as u64 =>
                {
                    std::str::from_utf8(&bytes[..e.valid_up_to()]).map_err(invalid)?
                }
                Err(e) => return Err(invalid(e).into()),
            };
            let mut count = 0_u64;
            let mut end = 0;
            for (byte, ch) in valid.char_indices() {
                if count >= request.max_chars as u64 || byte + ch.len_utf8() > 65536 {
                    break;
                }
                count += 1;
                end = byte + ch.len_utf8();
            }
            let next_byte = offset + end as u64;
            let next_char = chars
                .checked_add(count)
                .ok_or_else(|| invalid("source offset overflow"))?;
            let more = next_byte < lease.version.byte_count as u64;
            prepared.passage = Some(ConventionPassage {
                text: GraphText(valid[..end].to_owned()),
                offset: counter(i64::try_from(chars).map_err(invalid)?),
                next_cursor: more
                    .then(|| BoundedText(cursor.encode(&format!("{next_char}:{next_byte}")))),
                has_more: more,
            });
        }
        Ok(prepared)
    }
    pub(super) fn publish_document(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        scope: Option<&DataScope>,
        prepared: &PreparedDocument,
    ) -> Result<ConventionReadOutcome, Failure> {
        let scoped = access::scope(read, ctx, scope, false)?;
        if queries::scope_revision(read, &scoped.readable)? != prepared.scope_revision {
            return Err(StorageError::CursorExpired.into());
        }
        let owner = access::owner(
            read,
            ctx,
            &prepared.owner.id,
            Some(prepared.owner.revision),
            scope,
        )?;
        if owner != prepared.owner {
            return Err(unavailable());
        }
        if prepared.reason.is_none() {
            let selection = match prepared.format {
                ConventionFormat::Raw => ConventionSourceSelection::Raw,
                ConventionFormat::Resource => ConventionSourceSelection::Resource(
                    prepared.resource.as_ref().ok_or_else(unavailable)?,
                ),
                _ => ConventionSourceSelection::Markdown,
            };
            let _lease = self
                .artifacts
                .resolve_convention_source(read, ctx, scope, &owner, selection)?;
        }
        let record = model::record(&owner, self.index.status(read, &owner)?)?;
        if let Some(resource) = prepared.resource.as_ref() {
            if read
                .conventions()
                .convention_resource(
                    &owner.id,
                    &resource.id,
                    Some(owner.revision),
                    &scoped.readable,
                )?
                .as_ref()
                != Some(resource)
            {
                return Err(unavailable());
            }
            return Ok(ConventionReadOutcome::Resource {
                convention: record,
                resource: model::resource(resource, &owner)?,
                passage: prepared.binary.clone().ok_or_else(unavailable)?,
            });
        }
        Ok(ConventionReadOutcome::Document {
            view: ConventionReadView {
                convention: record,
                format: prepared.format.clone(),
                passage: prepared.passage.clone(),
                availability: if prepared.reason.is_some() {
                    ConventionAvailability::Unavailable
                } else {
                    ConventionAvailability::Available
                },
                reason: prepared.reason.clone().map(BoundedText),
                manifest_schema: model::manifest_schema(),
            },
        })
    }
}
fn scalar(reader: &mut dyn Read) -> io::Result<Option<char>> {
    let mut raw = [0_u8; 4];
    if reader.read(&mut raw[..1])? == 0 {
        return Ok(None);
    }
    let n = match raw[0] {
        0..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "convention source is not UTF-8",
            ));
        }
    };
    reader.read_exact(&mut raw[1..n])?;
    Ok(Some(
        std::str::from_utf8(&raw[..n])
            .map_err(io::Error::other)?
            .chars()
            .next()
            .expect("scalar"),
    ))
}
pub(super) fn text_page(
    reader: &mut dyn Read,
    offset: u64,
    max_chars: usize,
    cursor: &CursorBinding,
    stopped: impl Fn() -> bool,
) -> Result<ConventionPassage, Failure> {
    for n in 0..offset {
        if n % 4096 == 0 && stopped() {
            return Err(StorageError::Cancelled.into());
        }
        if scalar(reader).map_err(invalid)?.is_none() {
            return Err(StorageError::CursorExpired.into());
        }
    }
    let mut text = String::new();
    let mut count = 0_u64;
    let more = loop {
        if count % 4096 == 0 && stopped() {
            return Err(StorageError::Cancelled.into());
        }
        let Some(ch) = scalar(reader).map_err(invalid)? else {
            break false;
        };
        if count >= max_chars as u64 || text.len() + ch.len_utf8() > 65536 {
            break true;
        }
        text.push(ch);
        count += 1;
    };
    let end = offset
        .checked_add(count)
        .ok_or_else(|| invalid("source offset overflow"))?;
    Ok(ConventionPassage {
        text: GraphText(text),
        offset: counter(i64::try_from(offset).map_err(invalid)?),
        next_cursor: more.then(|| BoundedText(cursor.encode(&end.to_string()))),
        has_more: more,
    })
}
