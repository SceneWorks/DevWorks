//! File work runs outside snapshots. Results are rebound to current Help/authority at query time.
use super::HelpService;
use crate::{
    context::InvocationContext,
    gateway::AdmittedQuery,
    operations::{OperationInput, help as ops},
    ports::{Preparation, Prepared, storage::*},
    protocol::{DomainErrorCode, Id, records::HelpAvailability},
    services::{
        artifacts::host::HelpFile,
        sessions::{Failure, Refusal},
    },
};
use std::collections::BTreeSet;
#[derive(Clone, PartialEq)]
pub(super) struct ReadKey {
    pub span: StoredHelpPassage,
    pub offset: i64,
    pub chars: u32,
}
struct ReadPlan {
    key: ReadKey,
    file: HelpFile,
}
pub(super) struct PreparedHelp {
    fence: String,
    pub matches: BTreeSet<String>,
    pub read: Option<(ReadKey, String)>,
}
impl HelpService {
    fn preparation_fence(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        operation: &str,
    ) -> Result<String, StorageError> {
        self.cursor_binding(read, ctx, operation, &[])
            .map(|c| c.encode("prepared"))
            .map_err(|e| match e {
                Failure::Storage(e) => e,
                Failure::Refused(_) => StorageError::ViewUnavailable,
            })
    }
    pub(super) fn validate_prepared(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
    ) -> Result<(), StorageError> {
        if let Some(prepared) = query.prepared::<PreparedHelp>()
            && prepared.fence != self.preparation_fence(read, ctx, query.descriptor().name)?
        {
            return Err(StorageError::CursorExpired);
        }
        Ok(())
    }
    pub(super) fn passage_count(input: &ops::ReadInput) -> Result<u32, Refusal> {
        input
            .max_chars
            .as_ref()
            .map(|v| {
                v.0.parse::<u64>().map_err(|_| {
                    Refusal::new(DomainErrorCode::InvalidArgument, "help_passage_limit", None)
                })
            })
            .transpose()
            .map(|n| n.unwrap_or(8192).min(65536) as u32)
    }
    pub(super) fn passage_position(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::ReadInput,
        revision: u32,
    ) -> Result<(i64, u32, crate::services::cursor::CursorBinding), Failure> {
        let requested = input.max_chars.as_ref().map_or("default", |v| v.0.as_str());
        let chars = Self::passage_count(input)?;
        let binding = self.cursor_binding(
            read,
            ctx,
            "help.read",
            &[
                input.topic_id.as_str().into(),
                input.version.0.clone(),
                requested.into(),
                revision.to_string(),
            ],
        )?;
        let offset = input
            .cursor
            .as_ref()
            .map(|c| binding.decode(&c.0))
            .transpose()?
            .map(|s| s.parse::<i64>().map_err(|_| StorageError::CursorExpired))
            .transpose()?
            .unwrap_or(0);
        Ok((offset, chars, binding))
    }
    fn read_plan(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::ReadInput,
        revision: u32,
    ) -> Result<Option<ReadPlan>, StorageError> {
        let map = |e: Failure| match e {
            Failure::Storage(e) => e,
            Failure::Refused(_) => StorageError::ViewUnavailable,
        };
        let head = read.help().help_head()?;
        let row = head
            .generation_id
            .as_ref()
            .map(|g| read.help().help_topic(g, &input.topic_id))
            .transpose()?
            .flatten();
        let Some(row) = row else {
            return Ok(None);
        };
        if row.version != input.version.0
            || !matches!(
                self.state(read, &row).map_err(map)?.0,
                HelpAvailability::Available
            )
        {
            return Ok(None);
        }
        let (offset, chars, _) = self
            .passage_position(read, ctx, input, revision)
            .map_err(map)?;
        if offset < 0 || offset > row.character_count {
            return Err(StorageError::CursorExpired);
        }
        let span = read
            .help()
            .help_passage_source(&row.generation_id, &row.id, offset, chars)?
            .ok_or(StorageError::ViewUnavailable)?;
        let reference = serde_json::from_value(span.content_ref.clone())
            .map_err(|_| StorageError::ViewUnavailable)?;
        let file = self
            .artifacts
            .capture_help_file(read, &self.store.info(), &reference)?;
        Ok(Some(ReadPlan {
            key: ReadKey {
                span,
                offset,
                chars,
            },
            file,
        }))
    }
    pub(super) fn read_key(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::ReadInput,
        revision: u32,
    ) -> Result<Option<ReadKey>, StorageError> {
        Ok(self.read_plan(read, ctx, input, revision)?.map(|p| p.key))
    }
    pub(super) fn prepare_io(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        let operation = query.descriptor().name;
        let fence = env.read(|r| self.preparation_fence(r, ctx, operation))?;
        let mut result = PreparedHelp {
            fence,
            matches: BTreeSet::new(),
            read: None,
        };
        let mut stopped = || self.is_stopped() || env.stopped(self.clock.now_ms().get());
        if stopped() {
            return Err(StorageError::Cancelled);
        }
        match query.input() {
            OperationInput::Help(ops::Input::Read(input)) => {
                if let Some(plan) =
                    env.read(|r| self.read_plan(r, ctx, input, query.schema_revision().0))?
                {
                    let span = &plan.key.span;
                    let max = (HELP_CHECKPOINT_BYTES + 4 + u64::from(plan.key.chars) * 4)
                        .min(span.byte_count.saturating_sub(span.checkpoint.byte_offset))
                        as usize;
                    let bytes = self.artifacts.read_help_range(
                        &plan.file,
                        span.checkpoint.byte_offset,
                        max,
                        &mut stopped,
                    )?;
                    let valid = match std::str::from_utf8(&bytes) {
                        Ok(s) => s,
                        Err(e) if e.error_len().is_none() => {
                            std::str::from_utf8(&bytes[..e.valid_up_to()]).expect("valid prefix")
                        }
                        Err(_) => return Err(StorageError::ViewUnavailable),
                    };
                    let skip = (plan.key.offset - span.checkpoint.character_offset) as usize;
                    let text = valid
                        .chars()
                        .skip(skip)
                        .take(plan.key.chars as usize)
                        .collect::<String>();
                    let expected =
                        i64::from(plan.key.chars).min(span.character_count - plan.key.offset);
                    if text.chars().count() as i64 != expected {
                        return Err(StorageError::ViewUnavailable);
                    }
                    result.read = Some((plan.key, text));
                }
            }
            OperationInput::Help(ops::Input::Search(input)) if input.query.0.trim() != "*" => {
                let query_text = input.query.0.trim().to_lowercase();
                if query_text.is_empty() {
                    return Err(StorageError::InvalidRecord("empty Help query".into()));
                }
                let (generation, mut after) = env.read(|r| {
                    let head = r.help().help_head()?;
                    let binding = self
                        .cursor_binding(
                            r,
                            ctx,
                            "help.search",
                            &[input.query.0.clone(), query.schema_revision().0.to_string()],
                        )
                        .map_err(|e| match e {
                            Failure::Storage(e) => e,
                            Failure::Refused(_) => StorageError::ViewUnavailable,
                        })?;
                    let after = input
                        .page
                        .cursor
                        .as_ref()
                        .map(|c| binding.decode(&c.0))
                        .transpose()?
                        .map(Id::new)
                        .transpose()
                        .map_err(|_| StorageError::CursorExpired)?;
                    Ok((head.generation_id, after))
                })?;
                let Some(generation) = generation else {
                    return Ok(Some(Prepared::new(result)));
                };
                'pages: loop {
                    if stopped() {
                        return Err(StorageError::Cancelled);
                    }
                    let rows = env.read(|r| {
                        r.help().help_topics(
                            &generation,
                            &HelpFilter::default(),
                            after.as_ref(),
                            100,
                        )
                    })?;
                    let exhausted = rows.len() < 100;
                    for row in &rows {
                        if stopped() {
                            return Err(StorageError::Cancelled);
                        }
                        let metadata = env.read(|r| {
                            if row.source_kind == "Package"
                                && row
                                    .package_id
                                    .as_ref()
                                    .map(|id| r.packages().package(id.as_str()))
                                    .transpose()?
                                    .flatten()
                                    .is_none_or(|p| !p.installed)
                            {
                                return Ok(None);
                            }
                            Ok(Some(r.help().help_content(&row.content_digest)?))
                        })?;
                        let Some(content) = metadata else {
                            continue;
                        };
                        let indexed = content.as_ref().map_or("", |c| c.search_text.as_str());
                        let joined = format!("{}\n{indexed}", row.title.to_lowercase());
                        let exact_projection = content.as_ref().is_none_or(|c| c.index_complete)
                            || !query_text.contains(['σ', 'ς']);
                        let mut matches = exact_projection && joined.contains(&query_text);
                        if !matches && content.as_ref().is_some_and(|c| !c.index_complete) {
                            let reference = serde_json::from_value(
                                row.content_ref
                                    .clone()
                                    .ok_or(StorageError::ViewUnavailable)?,
                            )
                            .map_err(|_| StorageError::ViewUnavailable)?;
                            let file = env.read(|r| {
                                self.artifacts
                                    .capture_help_file(r, &self.store.info(), &reference)
                            })?;
                            let mut reader = self.artifacts.open_help_file(&file, &mut stopped)?;
                            matches = super::text::contains(
                                &row.title,
                                reader.as_mut(),
                                &query_text,
                                content.as_ref().expect("partial content").byte_count,
                                &mut stopped,
                            )?;
                        }
                        if matches {
                            result.matches.insert(row.id.as_str().into());
                            if result.matches.len() > input.page.limit.get() as usize {
                                break 'pages;
                            }
                        }
                    }
                    if exhausted {
                        break;
                    }
                    after = rows.last().map(|r| r.id.clone());
                }
            }
            _ => return Ok(None),
        }
        if stopped() {
            return Err(StorageError::Cancelled);
        }
        Ok(Some(Prepared::new(result)))
    }
}
