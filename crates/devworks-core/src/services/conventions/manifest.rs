//! A resumable manifest transport. Its cursor is a source locator, so later pages never
//! regenerate or scan the preceding document/resource bytes. No whole bundle is retained.
use super::{
    ConventionService, access, model,
    passages::{DocumentRequest, PreparedDocument},
    queries,
};
use crate::{
    context::InvocationContext,
    ports::{Preparation, storage::*},
    protocol::{
        DomainErrorCode, Id,
        provider::ArtifactRef,
        records::*,
        support::{BoundedText, GraphText},
    },
    services::{
        artifacts::{ConventionSourceLease, ConventionSourceSelection},
        cursor::CursorBinding,
        sessions::{Failure, Refusal, counter},
    },
};
use serde::{Deserialize, Serialize};
fn invalid(e: impl std::fmt::Display) -> StorageError {
    StorageError::InvalidRecord(e.to_string())
}
fn changed() -> Failure {
    Refusal::new(DomainErrorCode::SourceChanged, "convention_manifest", None).into()
}
fn q(v: &impl Serialize) -> Result<String, StorageError> {
    serde_json::to_string(v).map_err(invalid)
}
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum Part {
    Header,
    MdPrefix,
    Md,
    MdEnd,
    RawPrefix,
    Raw,
    RawEnd,
    Resources,
    ResourcePrefix,
    Resource,
    ResourceEnd,
    End,
    Done,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Position {
    part: Part,
    resource: Option<Id>,
    first: bool,
    offset: u64,
    sub: usize,
    chars: u64,
}
impl Default for Position {
    fn default() -> Self {
        Self {
            part: Part::Header,
            resource: None,
            first: true,
            offset: 0,
            sub: 0,
            chars: 0,
        }
    }
}
impl Position {
    fn advance(&mut self, part: Part) {
        self.part = part;
        self.offset = 0;
        self.sub = 0;
    }
}
struct Bytes {
    lease: ConventionSourceLease,
    offset: u64,
    buffer: Vec<u8>,
}
impl Bytes {
    fn at(
        &mut self,
        offset: u64,
        count: usize,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<Vec<u8>, Failure> {
        if offset > self.lease.version.byte_count as u64 {
            return Err(StorageError::CursorExpired.into());
        }
        if self.buffer.is_empty()
            || offset < self.offset
            || offset.saturating_add(count as u64) > self.offset + self.buffer.len() as u64
        {
            self.buffer = self
                .lease
                .read_range_controlled(offset, 65536, stopped)
                .map_err(|e| match e {
                    crate::services::artifacts::lifecycle::ExactReadRefusal::Storage(error) => {
                        Failure::Storage(error)
                    }
                    _ => changed(),
                })?;
            self.offset = offset;
        }
        let start = (offset - self.offset) as usize;
        let end = (start + count).min(self.buffer.len());
        Ok(self.buffer[start..end].to_vec())
    }
}
struct Resource {
    row: StoredConventionResource,
    reference: ArtifactRef,
}
impl ConventionService {
    fn manifest_parent_path(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        owner: &StoredConvention,
    ) -> Result<Vec<String>, Failure> {
        let mut parent = owner.parent_id.clone();
        let mut path = Vec::new();
        let mut seen = std::collections::HashSet::new();
        while let Some(id) = parent {
            if path.len() >= model::MAX_DEPTH || !seen.insert(id.clone()) {
                return Err(changed());
            }
            let row = access::owner(read, ctx, &id, None, Some(&model::scope(&owner.scope)))?;
            parent = row.parent_id;
            path.push(row.slug);
        }
        path.reverse();
        Ok(path)
    }
    fn manifest_resource(
        &self,
        env: &Preparation<'_>,
        ctx: &InvocationContext,
        request: &DocumentRequest<'_>,
        id: Option<&Id>,
        after: bool,
    ) -> Result<Option<Resource>, Failure> {
        env.read(|read| {
            Ok((|| {
                let scoped = access::scope(read, ctx, request.scope, false)?;
                let row = if after || id.is_none() {
                    read.conventions()
                        .convention_resources(
                            &ConventionResourceQuery {
                                scopes: scoped.readable,
                                convention_id: Some(request.owner.id.clone()),
                                convention_revision: Some(request.owner.revision),
                                lifecycles: vec!["Active".into(), "Retired".into()],
                            },
                            id,
                            1,
                        )?
                        .into_iter()
                        .next()
                } else {
                    read.conventions().convention_resource(
                        &request.owner.id,
                        id.expect("provided"),
                        Some(request.owner.revision),
                        &scoped.readable,
                    )?
                };
                row.map(|row| {
                    Ok::<_, Failure>(Resource {
                        reference: queries::parse(row.content_ref.clone())?,
                        row,
                    })
                })
                .transpose()
            })())
        })?
    }
    pub(super) fn prepare_manifest(
        &self,
        ctx: &InvocationContext,
        env: &Preparation<'_>,
        request: DocumentRequest<'_>,
    ) -> Result<PreparedDocument, Failure> {
        let (revision, parent_path) = env.read(|read| {
            Ok((|| {
                let scoped = access::scope(read, ctx, request.scope, false)?;
                Ok::<_, Failure>((
                    queries::scope_revision(read, &scoped.readable)?,
                    self.manifest_parent_path(read, ctx, &request.owner)?,
                ))
            })())
        })??;
        if request.owner.lifecycle == "Deleted" || request.owner.content_ref.is_none() {
            return Ok(PreparedDocument {
                owner: request.owner,
                resource: None,
                scope_revision: revision,
                format: ConventionFormat::Manifest,
                passage: None,
                binary: None,
                reason: Some("The selected revision has no available document body.".into()),
            });
        }
        let md: ArtifactRef =
            queries::parse(request.owner.content_ref.clone().ok_or_else(changed)?)?;
        let raw: Option<ArtifactRef> = request
            .owner
            .raw_content_ref
            .clone()
            .map(queries::parse)
            .transpose()?;
        let intent = q(&request.intent)?;
        let owner_revision = request.owner.revision.to_string();
        let cursor = CursorBinding::new(
            ctx,
            "conventions.manifest",
            &[&intent, &owner_revision],
            revision,
        );
        let mut state = request
            .cursor
            .map(|c| cursor.decode(c))
            .transpose()?
            .map(|s| serde_json::from_str::<Position>(&s).map_err(|_| StorageError::CursorExpired))
            .transpose()?
            .unwrap_or_default();
        let first_char = state.chars;
        let mut text = String::new();
        let mut emitted = 0_usize;
        let mut resource = None;
        let mut bytes = None;
        loop {
            if self.is_stopped() || env.stopped(self.clock.now_ms().get()) {
                return Err(StorageError::Cancelled.into());
            }
            if state.part == Part::Done {
                break;
            }
            let literal = match state.part {
                Part::Header => Some(format!(
                    "{{\"format\":\"devworks-convention\",\"version\":1,\"slug\":{},\"title\":{},\"parentPath\":{},\"sortOrder\":{},\"lifecycle\":{},\"applicability\":{},\"markdown\":",
                    q(&request.owner.slug)?,
                    q(&request.owner.title)?,
                    q(&parent_path)?,
                    request.owner.sort_order,
                    q(&request.owner.lifecycle)?,
                    q(&request.owner.applicability)?
                )),
                Part::MdPrefix => Some(source_prefix(&md)?),
                Part::MdEnd | Part::RawEnd => Some("\"}".into()),
                Part::RawPrefix => Some(format!(
                    ",\"rawContent\":{}",
                    source_prefix(raw.as_ref().ok_or_else(changed)?)?
                )),
                Part::Resources => Some(",\"resources\":[".into()),
                Part::ResourcePrefix => {
                    if resource.is_none() {
                        resource = self.manifest_resource(
                            env,
                            ctx,
                            &request,
                            state.resource.as_ref(),
                            false,
                        )?;
                    }
                    let Some(r) = resource.as_ref() else {
                        state.advance(Part::End);
                        continue;
                    };
                    state.resource = Some(r.row.id.clone());
                    Some(format!(
                        "{}{{\"key\":{},\"name\":{},\"mediaType\":{},\"lifecycle\":{},\"sha256\":{},\"byteLength\":{},\"base64\":\"",
                        if state.first { "" } else { "," },
                        q(&format!("resource-{}", r.row.id.as_str()))?,
                        q(&r.row.name)?,
                        q(&r.row.media_type)?,
                        q(&r.row.lifecycle)?,
                        q(&r.reference.sha256)?,
                        q(&r.reference.byte_length.get().to_string())?
                    ))
                }
                Part::ResourceEnd => Some("\"}".into()),
                Part::End => Some("]}".into()),
                _ => None,
            };
            if let Some(literal) = literal {
                let start =
                    usize::try_from(state.offset).map_err(|_| StorageError::CursorExpired)?;
                let remaining = literal.get(start..).ok_or(StorageError::CursorExpired)?;
                let mut complete = true;
                for ch in remaining.chars() {
                    if emitted >= request.max_chars || text.len() + ch.len_utf8() > 65536 {
                        complete = false;
                        break;
                    }
                    text.push(ch);
                    emitted += 1;
                    state.offset += ch.len_utf8() as u64;
                    state.chars += 1;
                }
                if !complete {
                    break;
                }
                let next = match state.part {
                    Part::Header => Part::MdPrefix,
                    Part::MdPrefix => Part::Md,
                    Part::MdEnd => {
                        if raw.is_some() {
                            Part::RawPrefix
                        } else {
                            Part::Resources
                        }
                    }
                    Part::RawPrefix => Part::Raw,
                    Part::RawEnd => Part::Resources,
                    Part::Resources => Part::ResourcePrefix,
                    Part::ResourcePrefix => Part::Resource,
                    Part::ResourceEnd => {
                        let current = state.resource.as_ref().ok_or_else(changed)?;
                        resource =
                            self.manifest_resource(env, ctx, &request, Some(current), true)?;
                        state.resource = resource.as_ref().map(|r| r.row.id.clone());
                        state.first = false;
                        if resource.is_some() {
                            Part::ResourcePrefix
                        } else {
                            Part::End
                        }
                    }
                    Part::End => Part::Done,
                    _ => return Err(changed()),
                };
                state.advance(next);
                bytes = None;
                continue;
            }
            let selection = match state.part {
                Part::Md => ConventionSourceSelection::Markdown,
                Part::Raw => ConventionSourceSelection::Raw,
                Part::Resource => {
                    if resource.is_none() {
                        resource = self.manifest_resource(
                            env,
                            ctx,
                            &request,
                            state.resource.as_ref(),
                            false,
                        )?;
                    }
                    ConventionSourceSelection::Resource(&resource.as_ref().ok_or_else(changed)?.row)
                }
                _ => return Err(changed()),
            };
            if bytes.is_none() {
                let lease = env.read(|read| {
                    Ok(self.artifacts.resolve_convention_source(
                        read,
                        ctx,
                        request.scope,
                        &request.owner,
                        selection,
                    ))
                })??;
                bytes = Some(Bytes {
                    lease,
                    offset: 0,
                    buffer: Vec::new(),
                });
            }
            let source = bytes.as_mut().expect("resolved");
            if state.offset > source.lease.version.byte_count as u64 {
                return Err(StorageError::CursorExpired.into());
            }
            if state.offset == source.lease.version.byte_count as u64 {
                if state.sub != 0 {
                    return Err(StorageError::CursorExpired.into());
                }
                let next = match state.part {
                    Part::Md => Part::MdEnd,
                    Part::Raw => Part::RawEnd,
                    _ => Part::ResourceEnd,
                };
                state.advance(next);
                bytes = None;
                continue;
            }
            let mut stopped = || self.is_stopped() || env.stopped(self.clock.now_ms().get());
            let (encoded, consumed) = if state.part == Part::Resource {
                use base64::Engine;
                let raw = source.at(state.offset, 3, &mut stopped)?;
                if raw.is_empty() {
                    return Err(changed());
                }
                (
                    base64::engine::general_purpose::STANDARD.encode(&raw),
                    raw.len(),
                )
            } else {
                let raw = source.at(state.offset, 4, &mut stopped)?;
                let n = match raw.first() {
                    Some(0..=0x7f) => 1,
                    Some(0xc2..=0xdf) => 2,
                    Some(0xe0..=0xef) => 3,
                    Some(0xf0..=0xf4) => 4,
                    _ => return Err(changed()),
                };
                let value =
                    std::str::from_utf8(raw.get(..n).ok_or_else(changed)?).map_err(invalid)?;
                let ch = value.chars().next().ok_or_else(changed)?;
                let encoded = match ch {
                    '"' => "\\\"".into(),
                    '\\' => "\\\\".into(),
                    c if (c as u32) < 32 => format!("\\u{:04x}", c as u32),
                    _ => value.to_owned(),
                };
                (encoded, n)
            };
            let remaining = encoded
                .get(state.sub..)
                .ok_or(StorageError::CursorExpired)?;
            let mut complete = true;
            for ch in remaining.chars() {
                if emitted >= request.max_chars || text.len() + ch.len_utf8() > 65536 {
                    complete = false;
                    break;
                }
                text.push(ch);
                emitted += 1;
                state.sub += ch.len_utf8();
                state.chars += 1;
            }
            if !complete {
                break;
            }
            state.offset += consumed as u64;
            state.sub = 0;
        }
        let more = state.part != Part::Done;
        let next_cursor = if more {
            Some(BoundedText(cursor.encode(&q(&state)?)))
        } else {
            None
        };
        Ok(PreparedDocument {
            owner: request.owner,
            resource: None,
            scope_revision: revision,
            format: ConventionFormat::Manifest,
            passage: Some(ConventionPassage {
                text: GraphText(text),
                offset: counter(i64::try_from(first_char).map_err(invalid)?),
                next_cursor,
                has_more: more,
            }),
            binary: None,
            reason: None,
        })
    }
}
fn source_prefix(reference: &ArtifactRef) -> Result<String, StorageError> {
    Ok(format!(
        "{{\"sha256\":{},\"byteLength\":{},\"text\":\"",
        q(&reference.sha256)?,
        q(&reference.byte_length.get().to_string())?
    ))
}
