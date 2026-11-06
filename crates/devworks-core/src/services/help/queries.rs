use super::{APP_VERSION, HelpService, current_generation_args};
use crate::context::InvocationContext;
use crate::gateway::AdmittedQuery;
use crate::operations::{OperationInput, help as ops};
use crate::ports::storage::*;
use crate::ports::{AuthorityRequest, IdKind, QueryAnswer, QueryHandler};
use crate::protocol::records::*;
use crate::protocol::support::{BoundedList, BoundedText};
use crate::protocol::{CorrelationId, DomainErrorCode, DomainFault, Id};
use crate::registry::{Mode, SchemaRevision};
use crate::services::cursor::CursorBinding;
use crate::services::sessions::{Failure, Refusal, counter, query_fault, rev};
use serde_json::json;

type R<T> = Result<T, Failure>;
const PAGE_BYTES: usize = 900_000;

struct ListRequest<'a> {
    operation: &'a str,
    filter: HelpFilter,
    limit: u16,
    cursor: Option<&'a str>,
    parts: Vec<String>,
}

impl HelpService {
    fn snapshot(&self, read: &dyn ReadSnapshot, ctx: &InvocationContext) -> R<HelpSnapshot> {
        let head = read.help().help_head()?;
        let args = current_generation_args(read)?;
        let descriptor =
            crate::registry::find("help.generate_capability_docs").expect("Help descriptor");
        let binding = descriptor.binding(Mode::Command).expect("Help command");
        let normalized = serde_json::to_value(&args).expect("typed Help arguments");
        let fingerprint =
            crate::fingerprint::compute(descriptor, Mode::Command, SchemaRevision(1), &normalized)
                .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
        let input = OperationInput::Help(ops::Input::GenerateCapabilityDocs(args.clone()));
        let authority = crate::authority::service::decide_in(
            read,
            ctx,
            &AuthorityRequest {
                descriptor,
                binding,
                effect: binding.effect,
                action: None,
                input: &input,
                fingerprint: &fingerprint,
            },
            self.clock.now_ms().get(),
            &[],
        );
        let generation_operation = head
            .generation_operation_id
            .as_ref()
            .map(|id| read.operations().operation(id))
            .transpose()?
            .flatten()
            .map(|operation| OperationRef {
                operation_id: operation.id,
                state: operation.state,
            });
        Ok(HelpSnapshot {
            app_version: BoundedText(APP_VERSION.into()),
            help_revision: rev(head.revision),
            registry_revision: args.registry_revision,
            can_generate: authority.is_ok(),
            cannot_generate_reason: authority.err().map(|denied| {
                BoundedText(format!(
                    "Current authority refuses documentation regeneration: {}.",
                    denied.reason.as_str()
                ))
            }),
            generation_operation,
        })
    }

    fn cursor_binding(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        operation: &str,
        parts: &[String],
    ) -> R<CursorBinding> {
        let head = read.help().help_head()?;
        let principal = read
            .authority()
            .principal(ctx.principal())?
            .map(|p| p.revision)
            .unwrap_or(0);
        let basis=json!({"query":parts,"generation":head.generation_id,"registry":read.packages().registry_revision()?,
            "source":self.source_digest(read.packages().registry_revision()?),"principalRevision":principal}).to_string();
        Ok(CursorBinding::new(ctx, operation, &[&basis], head.revision))
    }

    fn state(
        &self,
        read: &dyn ReadSnapshot,
        row: &StoredHelpTopic,
    ) -> R<(HelpAvailability, Option<String>)> {
        if row.source_kind == "Package" {
            let Some(package_id) = row.package_id.as_ref() else {
                return Err(
                    StorageError::InvalidRecord("package Help identity missing".into()).into(),
                );
            };
            let current = read.packages().package(package_id.as_str())?;
            if !current.as_ref().is_some_and(|package| {
                package.installed && package.active_digest == row.manifest_digest
            }) {
                return Ok((
                    HelpAvailability::Stale,
                    Some(
                        "This guide does not match the currently installed package digest.".into(),
                    ),
                ));
            }
        } else if row.source_kind == "Bundled" {
            let current = super::bundle::load()?;
            if row.version != APP_VERSION
                || !current
                    .iter()
                    .any(|topic| topic.id == row.id && topic.digest == row.content_digest)
            {
                return Ok((
                    HelpAvailability::Stale,
                    Some(
                        "This topic does not match the running app's bundled documentation.".into(),
                    ),
                ));
            }
        } else if row.source_kind == "Generated" {
            let head = read.help().help_head()?;
            let owner = read
                .help()
                .help_generation_operation(&row.generation_id)?
                .map(|id| read.operations().operation(&id))
                .transpose()?
                .flatten();
            let input = self.source_digest(read.packages().registry_revision()?);
            if head.app_version != APP_VERSION
                || owner.as_ref().is_none_or(|operation| {
                    operation.payload["kind"] != super::JOB_KIND
                        || operation.payload["inputDigest"].as_str() != Some(&input)
                })
            {
                return Ok((HelpAvailability::Stale,Some("Capability documentation was generated from an older app or registry snapshot.".into())));
            }
        }
        let availability = match row.availability.as_str() {
            "Available" => HelpAvailability::Available,
            "Missing" => HelpAvailability::Missing,
            "Stale" => HelpAvailability::Stale,
            _ => HelpAvailability::Unavailable,
        };
        Ok((availability, row.reason.clone()))
    }

    fn summary(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        row: &StoredHelpTopic,
    ) -> R<HelpTopicSummary> {
        let (availability, mut reason) = self.state(read, row)?;
        if row.source_kind == "Package"
            && let (Some(owner), Some(package_id)) = (&self.packages, &row.package_id)
            && let Some(package) = read.packages().package(package_id.as_str())?
        {
            let (state, detail) = owner.help_availability(
                read.packages(),
                &package,
                ctx.project_id().map(|id| id.0.as_str()),
            )?;
            let current = format!(
                "Current package availability: {state}. {detail} Documentation is an installed declaration and does not prove activation or grant authority."
            );
            reason = Some(reason.map_or(current.clone(), |reason| format!("{reason} {current}")));
        }
        let content = row
            .content_ref
            .as_ref()
            .map(|reference| {
                serde_json::from_value::<crate::protocol::provider::ArtifactRef>(reference.clone())
            })
            .transpose()
            .map_err(|_| StorageError::InvalidRecord("Help artifact reference is invalid".into()))?
            .filter(|reference| {
                crate::services::approvals::owner_preview::artifact_binding(
                    read.artifacts(),
                    reference,
                )
                .is_ok()
            });
        let provenance =
            if row.provenance.is_null() {
                None
            } else {
                Some(serde_json::from_value(row.provenance.clone()).map_err(|_| {
                    StorageError::InvalidRecord("Help provenance is invalid".into())
                })?)
            };
        let capabilities = row
            .related_operations
            .iter()
            .flat_map(|operation| self.capability_metadata(operation))
            .collect();
        Ok(HelpTopicSummary {
            id: row.id.clone(),
            version: BoundedText(row.version.clone()),
            title: BoundedText(row.title.clone()),
            content,
            related_capability_ids: BoundedList(Vec::new()),
            availability,
            reason: reason.map(BoundedText),
            provenance,
            capabilities: BoundedList(capabilities),
            related_capabilities: BoundedList(
                row.related_operations
                    .iter()
                    .cloned()
                    .map(BoundedText)
                    .collect(),
            ),
        })
    }

    fn list(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        request: ListRequest<'_>,
    ) -> R<HelpTopicPage> {
        let ListRequest {
            operation,
            filter,
            limit,
            cursor,
            parts,
        } = request;
        let snapshot = self.snapshot(read, ctx)?;
        let head = read.help().help_head()?;
        let Some(generation) = head.generation_id.as_ref() else {
            if cursor.is_some() {
                return Err(StorageError::CursorExpired.into());
            }
            return Ok(HelpTopicPage {
                items: BoundedList(Vec::new()),
                next_cursor: None,
                has_more: false,
                availability: HelpAvailability::Unavailable,
                reason: Some(BoundedText(
                    "The offline Help catalog has not been published yet.".into(),
                )),
                snapshot,
            });
        };
        let binding = self.cursor_binding(read, ctx, operation, &parts)?;
        let mut after = cursor
            .map(|cursor| binding.decode(cursor))
            .transpose()?
            .map(Id::new)
            .transpose()
            .map_err(|_| StorageError::CursorExpired)?;
        let mut items = Vec::new();
        let mut last = None;
        let mut has_more = false;
        'pages: loop {
            let rows = read
                .help()
                .help_topics(generation, &filter, after.as_ref(), 100)?;
            let exhausted = rows.len() < 100;
            for row in &rows {
                if row.source_kind == "Package"
                    && let Some(id) = row.package_id.as_ref()
                    && read
                        .packages()
                        .package(id.as_str())?
                        .is_none_or(|package| !package.installed)
                {
                    continue;
                }
                let summary = self.summary(read, ctx, row)?;
                if items.len() >= usize::from(limit) {
                    has_more = true;
                    break 'pages;
                }
                items.push(summary);
                if serde_json::to_vec(&items)
                    .map_err(|e| StorageError::InvalidRecord(e.to_string()))?
                    .len()
                    > PAGE_BYTES
                {
                    items.pop();
                    if items.is_empty() {
                        return Err(Refusal::new(
                            DomainErrorCode::ViewUnavailable,
                            "help_summary",
                            Some(row.id.clone()),
                        )
                        .into());
                    }
                    has_more = true;
                    break 'pages;
                }
                last = Some(row.id.clone());
            }
            if exhausted {
                break;
            }
            after = rows.last().map(|row| row.id.clone());
        }
        let next_cursor = has_more
            .then(|| BoundedText(binding.encode(last.as_ref().expect("nonempty page").as_str())));
        Ok(HelpTopicPage {
            items: BoundedList(items),
            next_cursor,
            has_more,
            availability: HelpAvailability::Available,
            reason: None,
            snapshot,
        })
    }

    fn read(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::ReadInput,
        revision: u32,
    ) -> R<HelpReadView> {
        let snapshot = self.snapshot(read, ctx)?;
        let head = read.help().help_head()?;
        let row = head
            .generation_id
            .as_ref()
            .map(|generation| read.help().help_topic(generation, &input.topic_id))
            .transpose()?
            .flatten();
        let unavailable = |availability, reason: String, current_version: Option<String>| {
            HelpReadView::Unavailable {
                topic_id: input.topic_id.clone(),
                requested_version: input.version.clone(),
                availability,
                reason: BoundedText(reason),
                current_version: current_version.map(BoundedText),
                snapshot: snapshot.clone(),
            }
        };
        let Some(row) = row else {
            return Ok(unavailable(
                HelpAvailability::Missing,
                "This topic is not in the current offline catalog.".into(),
                None,
            ));
        };
        if row.version != input.version.0 {
            return Ok(unavailable(
                HelpAvailability::Stale,
                "The requested topic version is not the current app or installed package version."
                    .into(),
                Some(row.version),
            ));
        }
        let summary = self.summary(read, ctx, &row)?;
        if !matches!(summary.availability, HelpAvailability::Available) {
            return Ok(unavailable(
                summary.availability,
                summary
                    .reason
                    .map(|r| r.0)
                    .unwrap_or_else(|| "This guide is unavailable.".into()),
                Some(row.version),
            ));
        }
        let requested = input
            .max_chars
            .as_ref()
            .map_or("default", |value| value.0.as_str());
        let chars = input
            .max_chars
            .as_ref()
            .map(|value| {
                value.0.parse::<u64>().map_err(|_| {
                    Refusal::new(DomainErrorCode::InvalidArgument, "help_passage_limit", None)
                })
            })
            .transpose()?
            .unwrap_or(8192)
            .min(65536) as u32;
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
            .map(|cursor| binding.decode(&cursor.0))
            .transpose()?
            .map(|position| {
                position
                    .parse::<i64>()
                    .map_err(|_| StorageError::CursorExpired)
            })
            .transpose()?
            .unwrap_or(0);
        if offset < 0 || offset > row.character_count {
            return Err(StorageError::CursorExpired.into());
        }
        let text = read
            .help()
            .help_passage(&row.generation_id, &row.id, offset, chars)?
            .ok_or_else(|| {
                StorageError::InvalidRecord("published Help passage is missing".into())
            })?;
        let next = offset + text.chars().count() as i64;
        let has_more = next < row.character_count;
        Ok(HelpReadView::Available {
            topic: summary,
            passage: HelpPassage {
                text: BoundedText(text),
                offset: counter(offset),
                next_cursor: has_more.then(|| BoundedText(binding.encode(&next.to_string()))),
                has_more,
            },
            snapshot,
        })
    }

    fn package_guides(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::PackageGuidesInput,
        revision: u32,
    ) -> R<HelpTopicPage> {
        let current = read.packages().package(input.package_id.as_str())?;
        let version = current
            .filter(|package| package.installed)
            .and_then(|package| package.active_digest)
            .map(|digest| {
                read.packages()
                    .package_version(input.package_id.as_str(), &digest)
            })
            .transpose()?
            .flatten();
        let unavailable = match version {
            None => Some((
                HelpAvailability::Missing,
                "This package has no currently installed active guide.".into(),
            )),
            Some(version) if version.version != input.version.0 => Some((
                HelpAvailability::Stale,
                format!(
                    "The installed package version is {}; the requested guide version is stale.",
                    version.version
                ),
            )),
            _ => None,
        };
        if let Some((availability, reason)) = unavailable {
            return Ok(HelpTopicPage {
                items: BoundedList(Vec::new()),
                next_cursor: None,
                has_more: false,
                availability,
                reason: Some(BoundedText(reason)),
                snapshot: self.snapshot(read, ctx)?,
            });
        }
        self.list(
            read,
            ctx,
            ListRequest {
                operation: "help.package_guides",
                filter: HelpFilter {
                    package_id: Some(input.package_id.clone()),
                    ..HelpFilter::default()
                },
                limit: input.page.limit.get() as u16,
                cursor: input.page.cursor.as_ref().map(|cursor| cursor.0.as_str()),
                parts: vec![
                    input.package_id.as_str().into(),
                    input.version.0.clone(),
                    revision.to_string(),
                ],
            },
        )
    }
}

impl QueryHandler for HelpService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        read: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let revision = query.schema_revision().0;
        let result: R<crate::protocol::results::OperationResult> = match query.input() {
            OperationInput::Help(ops::Input::Search(input)) => self
                .list(
                    read,
                    ctx,
                    ListRequest {
                        operation: "help.search",
                        filter: HelpFilter {
                            query: (input.query.0.trim() != "*")
                                .then(|| input.query.0.trim().to_lowercase()),
                            ..HelpFilter::default()
                        },
                        limit: input.page.limit.get() as u16,
                        cursor: input.page.cursor.as_ref().map(|cursor| cursor.0.as_str()),
                        parts: vec![input.query.0.clone(), revision.to_string()],
                    },
                )
                .map(
                    |value| crate::protocol::results::OperationResult::HelpSearch {
                        value: HelpTopicPageResult::Current(value),
                    },
                ),
            OperationInput::Help(ops::Input::Read(input)) => {
                self.read(read, ctx, input, revision).map(|value| {
                    crate::protocol::results::OperationResult::HelpRead {
                        value: HelpReadResult::Current(value),
                    }
                })
            }
            OperationInput::Help(ops::Input::TopicForScreen(input)) => {
                let mut page = self.list(
                    read,
                    ctx,
                    ListRequest {
                        operation: "help.topic_for_screen",
                        filter: HelpFilter {
                            screen_id: Some(input.screen_id.as_str().into()),
                            ..HelpFilter::default()
                        },
                        limit: input
                            .page
                            .as_ref()
                            .map_or(20, |page| page.limit.get() as u16),
                        cursor: input
                            .page
                            .as_ref()
                            .and_then(|page| page.cursor.as_ref())
                            .map(|cursor| cursor.0.as_str()),
                        parts: vec![
                            input.screen_id.as_str().into(),
                            revision.to_string(),
                            input
                                .capability_revision
                                .as_ref()
                                .map_or("current".into(), |revision| revision.get().to_string()),
                        ],
                    },
                );
                if let Ok(page) = &mut page {
                    if input.capability_revision.as_ref().is_some_and(|version| {
                        version.get() != page.snapshot.registry_revision.get()
                    }) {
                        page.items.0.clear();
                        page.next_cursor = None;
                        page.has_more = false;
                        page.availability = HelpAvailability::Stale;
                        page.reason = Some(BoundedText(
                            "The requested screen capability revision is stale.".into(),
                        ));
                    } else if page.items.0.is_empty()
                        && matches!(page.availability, HelpAvailability::Available)
                    {
                        page.availability = HelpAvailability::Missing;
                        page.reason = Some(BoundedText(
                            "No local Help topic is bound to this screen.".into(),
                        ));
                    }
                }
                page.map(
                    |value| crate::protocol::results::OperationResult::HelpTopicForScreen {
                        value: HelpTopicPageResult::Current(value),
                    },
                )
            }
            OperationInput::Help(ops::Input::PackageGuides(input)) => self
                .package_guides(read, ctx, input, revision)
                .map(
                    |value| crate::protocol::results::OperationResult::HelpPackageGuides {
                        value: HelpTopicPageResult::Current(value),
                    },
                ),
            _ => Err(Refusal::new(
                DomainErrorCode::UnsupportedCapability,
                "help_operation",
                None,
            )
            .into()),
        };
        let result = result.map_err(|failure| {
            query_fault(
                failure,
                "help",
                CorrelationId(self.ids.next_id(IdKind::Correlation)),
            )
        })?;
        let head = read.help().help_head().map_err(|error| {
            query_fault(
                error.into(),
                "help",
                CorrelationId(self.ids.next_id(IdKind::Correlation)),
            )
        })?;
        Ok(QueryAnswer {
            stamp: read
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(head.revision)),
            result,
        })
    }
}
