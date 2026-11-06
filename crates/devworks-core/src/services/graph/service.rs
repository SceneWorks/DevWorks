//! Shared operators and gateway lifecycle; leases are internal retention, never authority.
use super::{
    analysis,
    budget::Budget,
    cursor::Cursors,
    impact, lease, model, operators,
    read::{Metadata, Position, Reader},
    scope, search, source,
};
use crate::{
    context::InvocationContext,
    gateway::{AdmittedCommand, AdmittedQuery},
    operations::{self, OperationInput},
    ports::{storage::*, *},
    protocol::{data::*, records::*, results::*, *},
    services::{
        artifacts::ArtifactService,
        indexing::{IndexEnv, views},
        sessions::{self, Failure, Refusal, rev},
    },
};
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    sync::Arc,
    time::Instant,
};

pub const OPERATIONS: [&str; 20] = [
    "code.search",
    "code.architecture",
    "code.dependency",
    "code.impact",
    "code.source",
    "code.traverse",
    "code.graph_scope_status",
    "code.overview",
    "code.nodes_by_file",
    "code.node_detail",
    "code.set_trust",
    "code.file_impact",
    "code.graph_schema",
    "code.call_path",
    "code.data_lineage",
    "code.consumers",
    "code.publishers",
    "code.snippet",
    "code.repository_dependencies",
    "repositories.nodes",
];

pub struct GraphService {
    store: Arc<dyn ApplicationStore>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdSource>,
    artifacts: Arc<ArtifactService>,
    indexing: Option<Arc<IndexEnv>>,
}
impl GraphService {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
        artifacts: Arc<ArtifactService>,
    ) -> Self {
        Self {
            store,
            clock,
            ids,
            artifacts,
            indexing: None,
        }
    }
    pub fn with_indexing(mut self, indexing: Option<Arc<IndexEnv>>) -> Self {
        self.indexing = indexing;
        self
    }
    fn fault(&self, failure: Failure) -> DomainFault {
        sessions::query_fault(
            failure,
            "graph",
            CorrelationId(self.ids.next_id(IdKind::Correlation)),
        )
    }
}
struct ReadPrepared {
    request: GraphReadRequest,
    selected: Option<lease::Selected>,
    started: Instant,
    deadline: i64,
    cancellation: Option<Cancellation>,
    source: Option<source::Source>,
    source_budget: Option<Budget>,
}
fn request(ctx: &InvocationContext, query: &AdmittedQuery) -> Result<GraphReadRequest, Failure> {
    use operations::code::Input::*;
    let request = match query.input() {
        OperationInput::Code(
            Search(i)
            | Architecture(i)
            | Dependency(i)
            | Impact(i)
            | Source(i)
            | Traverse(i)
            | GraphScopeStatus(i)
            | Overview(i)
            | NodesByFile(i)
            | NodeDetail(i)
            | FileImpact(i)
            | GraphSchema(i)
            | CallPath(i)
            | DataLineage(i)
            | Consumers(i)
            | Publishers(i)
            | Snippet(i)
            | RepositoryDependencies(i),
        ) => i.clone(),
        OperationInput::Repositories(operations::repositories::Input::Nodes(i)) => {
            let scope = i.scope.clone().unwrap_or_else(|| {
                ctx.project_id()
                    .map(|id| DataScope::Project {
                        project_id: id.clone(),
                    })
                    .unwrap_or(DataScope::Workspace)
            });
            let mut value = json!({"protocol":"DevWorks/1","queryId":query.query_id(),"scope":scope,"query":{"kind":"Search","selection":{"mode":"NodeList","text":"","repositoryId":i.repository_id,"nodeKind":i.node_kind}},"limits":{"maxResults":i.page.limit.get(),"maxVisited":100000,"deadlineMs":30000}});
            if i.node_kind.is_none() {
                value["query"]["selection"]
                    .as_object_mut()
                    .expect("selection")
                    .remove("nodeKind");
            }
            if let Some(module) = &i.internal_module {
                value["query"]["selection"]["internalModule"] = json!(module);
            }
            if let Some(views) = &i.views {
                value["view"] = json!(views);
            } else {
                value["current"] = json!(true);
            }
            if let Some(cursor) = &i.page.cursor {
                value["cursor"] = json!(cursor);
            }
            // Keep the admitted protocol, whose actual version string is validated by admission.
            value["protocol"] =
                serde_json::to_value(Protocol::Draft1).map_err(|_| StorageError::CursorExpired)?;
            model::wire(value)?
        }
        _ => {
            return Err(Refusal::new(
                DomainErrorCode::UnsupportedCapability,
                "graph_operation",
                None,
            )
            .into());
        }
    };
    Ok(request)
}
fn source_selection(query: &GraphQuery) -> Option<CodeSourceSelection> {
    match query {
        GraphQuery::Source {
            selection: Some(s), ..
        } => Some(s.clone()),
        GraphQuery::Source {
            node_id: Some(node),
            ..
        } => Some(CodeSourceSelection::Node {
            node_id: node.clone(),
            mode: CodeSourceMode::NodeWindow,
        }),
        _ => None,
    }
}
fn metadata_query(query: &GraphQuery) -> bool {
    matches!(
        query,
        GraphQuery::Overview { .. } | GraphQuery::Schema | GraphQuery::ScopeStatus
    )
}
impl QueryHandler for GraphService {
    fn read_deadline_ms(&self, q: &AdmittedQuery) -> Option<i64> {
        q.prepared::<ReadPrepared>().map(|p| p.deadline)
    }
    fn prepare(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, DomainFault> {
        let result = (|| {
            let started = Instant::now();
            let request = request(ctx, query)?;
            let deadline = self
                .clock
                .now_ms()
                .get()
                .saturating_add(request.limits.deadline_ms as i64)
                .min(env.deadline_ms.unwrap_or(i64::MAX));
            if env.stopped(self.clock.now_ms().get()) {
                return Err(StorageError::Cancelled.into());
            }
            let mut requested = request.view.clone();
            if let Some(cursor) = &request.cursor
                && let Some(pin_id) = Cursors::pin(cursor)?
            {
                if let Some(ViewSelection::Pinned { pin_id: provided }) = &requested
                    && provided != &pin_id
                {
                    return Err(StorageError::CursorExpired.into());
                }
                requested = Some(ViewSelection::Pinned { pin_id });
            }
            let selected = match lease::select(
                lease::Host {
                    store: &self.store,
                    clock: &*self.clock,
                    ids: &*self.ids,
                },
                ctx,
                query,
                &request.scope,
                requested.as_ref(),
                &ReadOptions {
                    deadline_ms: Some(deadline),
                    cancellation: env.cancellation.cloned(),
                },
            ) {
                Ok(selected) => {
                    if request.cursor.is_some() {
                        match &request.view {
                            Some(ViewSelection::Published { repository_ids }) => {
                                let expected: std::collections::BTreeSet<_> =
                                    repository_ids.iter().map(|id| id.as_str()).collect();
                                let actual: std::collections::BTreeSet<_> = selected
                                    .pin
                                    .scope
                                    .views
                                    .iter()
                                    .map(|v| v.repository_id.as_str())
                                    .collect();
                                if expected != actual
                                    || !selected.pin.unavailable_repositories.is_empty()
                                {
                                    return Err(StorageError::CursorExpired.into());
                                }
                            }
                            Some(ViewSelection::Exact { view_ids }) => {
                                let expected: std::collections::BTreeSet<_> =
                                    view_ids.iter().map(|id| id.as_str()).collect();
                                let actual: std::collections::BTreeSet<_> = selected
                                    .pin
                                    .scope
                                    .views
                                    .iter()
                                    .map(|v| v.view_id.as_str())
                                    .collect();
                                if expected != actual {
                                    return Err(StorageError::CursorExpired.into());
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(selected)
                }
                Err(Failure::Refused(r))
                    if r.code == DomainErrorCode::ViewUnavailable
                        && request.current == Some(true)
                        && metadata_query(&request.query)
                        && ctx.worktree().is_none() =>
                {
                    None
                }
                Err(e) => return Err(e),
            };
            let source_and_budget = if let (Some(selected), Some(selection)) =
                (&selected, source_selection(&request.query))
            {
                let mut source_bound = None;
                let scanned = read_snapshot(
                    &*self.store,
                    &ReadOptions {
                        deadline_ms: Some(deadline),
                        cancellation: env.cancellation.cloned(),
                    },
                    |read| {
                        let mut budget = Budget::new(&request.limits);
                        budget.start_at(started, env.cancellation.cloned());
                        let prepared_source = (|| {
                            let cursors =
                                bound_cursors(ctx, query, &request, Some(selected), read)?;
                            let position = request
                                .cursor
                                .as_ref()
                                .map(|c| cursors.decode(c))
                                .transpose()?
                                .unwrap_or_default();
                            let mut reader = Reader::new(
                                read,
                                selected,
                                budget,
                                position,
                                request.limits.max_results as usize,
                            )
                            .with_indexing(self.indexing.as_deref());
                            let source = source::prepare(
                                &mut reader,
                                &self.artifacts,
                                &selection,
                                match &request.query {
                                    GraphQuery::Source {
                                        selection: None,
                                        max_lines,
                                        ..
                                    } => *max_lines,
                                    _ => None,
                                },
                            )?;
                            Ok::<_, Failure>((source, reader.budget))
                        })();
                        if let Err(Failure::Refused(refusal)) = &prepared_source
                            && matches!(
                                refusal.target_kind,
                                "graph_deadline" | "graph_visited_limit" | "graph_memory_limit"
                            )
                        {
                            source_bound = Some(refusal.clone());
                        }
                        Ok(prepared_source)
                    },
                );
                let scanned = match scanned {
                    Err(StorageError::Cancelled)
                        if !env.cancellation.is_some_and(Cancellation::is_cancelled)
                            && source_bound.is_some() =>
                    {
                        return Err(source_bound.expect("known source bound").into());
                    }
                    other => other,
                };
                Some(scanned??)
            } else {
                None
            };
            let (source, source_budget) = match source_and_budget {
                Some((source, budget)) => (Some(source), Some(budget)),
                None => (None, None),
            };
            Ok(Some(Prepared::new(ReadPrepared {
                request,
                selected,
                started,
                deadline,
                cancellation: env.cancellation.cloned(),
                source,
                source_budget,
            })))
        })();
        result.map_err(|e| self.fault(e))
    }
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        read: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        self.answer(ctx, query, read).map_err(|e| self.fault(e))
    }
}
impl GraphService {
    fn answer(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        read: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, Failure> {
        let prepared = query
            .prepared::<ReadPrepared>()
            .ok_or_else(|| scope::unavailable(None))?;
        let request = &prepared.request;
        let now = self.clock.now_ms().get();
        if now > prepared.deadline
            || prepared
                .cancellation
                .as_ref()
                .is_some_and(Cancellation::is_cancelled)
        {
            return Err(StorageError::Cancelled.into());
        }
        let selected = prepared.selected.as_ref();
        if let Some(selected) = selected {
            let pin = read
                .graph()
                .graph_pin(&selected.pin.id)?
                .ok_or_else(|| scope::unavailable(Some(selected.pin.id.clone())))?;
            lease::check_pin(&pin, ctx, &request.scope, now)?;
            if pin.intent_digest != selected.pin.intent_digest {
                return Err(scope::unavailable(Some(pin.id)));
            }
            for missing in &pin.unavailable_repositories {
                scope::authorize(read, ctx, &request.scope, &missing.repository_id)?;
            }
            for view in &pin.scope.views {
                scope::retained(read, ctx, &request.scope, &pin, view, now)?;
            }
        }
        let scope_revision = if let Some(s) = selected {
            read.graph().graph_trust_revision(&s.pin.scope)?
        } else {
            read.stamp().committed_sequence.max(1)
        };
        let stamp = read
            .stamp()
            .read_stamp(ctx.policy_revision().clone(), rev(scope_revision));
        let cursors = bound_cursors(ctx, query, request, selected, read)?;
        let position = request
            .cursor
            .as_ref()
            .map(|c| cursors.decode(c))
            .transpose()?
            .unwrap_or_default();
        let mut budget = Budget::new(&request.limits);
        budget.start_at(prepared.started, prepared.cancellation.clone());
        let metadata = Metadata {
            read,
            context: ctx,
            scope: &request.scope,
            stamp: stamp.clone(),
            position: position.clone(),
            limit: request.limits.max_results as usize,
            next: RefCell::new(None),
            budget: RefCell::new(budget),
            encoded_bytes: Cell::new(0),
        };
        let mut reader = selected.map(|s| {
            let mut b = Budget::new(&request.limits);
            b.start_at(prepared.started, prepared.cancellation.clone());
            if let Some(prior) = &prepared.source_budget {
                b.absorb_usage(prior);
            }
            Reader::new(read, s, b, position, request.limits.max_results as usize)
                .with_indexing(self.indexing.as_deref())
                .with_cursors(&cursors)
        });
        let result = match &request.query {
            GraphQuery::Overview { repository_id }
                if query.descriptor().name == "code.architecture" =>
            {
                analysis::architecture(
                    reader.as_mut().ok_or_else(|| scope::unavailable(None))?,
                    repository_id.as_ref().ok_or_else(|| {
                        Refusal::new(DomainErrorCode::InvalidArgument, "repository", None)
                    })?,
                )?
            }
            GraphQuery::Overview { repository_id } => {
                analysis::overview(&metadata, reader.as_mut(), repository_id.as_ref())?
            }
            GraphQuery::Schema => analysis::schema(&metadata, reader.as_mut())?,
            GraphQuery::ScopeStatus => {
                self.status(&metadata, reader.as_mut(), request.current == Some(true))?
            }
            _ => self.operate(
                reader.as_mut().ok_or_else(|| scope::unavailable(None))?,
                &request.query,
                prepared.source.as_ref(),
                query.descriptor().name,
            )?,
        };
        let mut gaps = Vec::new();
        let exhausted;
        let mut next = metadata.next.borrow().clone();
        if let Some(reader) = &reader {
            gaps = reader.gaps.clone();
            exhausted = reader.budget.exhausted;
            next = reader.next.clone();
        } else {
            exhausted = metadata.budget.borrow().exhausted;
        }
        if let Some(reason) = exhausted {
            gaps.push(reason);
        }
        if let Some(selected) = selected {
            for layer in &selected.layers {
                for id in &layer.layers {
                    let generation = read
                        .indexing()
                        .generation(id)?
                        .ok_or_else(|| scope::unavailable(Some(id.clone())))?;
                    let (state, _) =
                        crate::services::indexing::status::manifest_coverage(&generation.manifest);
                    if !matches!(state, CoverageState::Complete)
                        && !gaps.contains(&"index_coverage_partial")
                    {
                        gaps.push("index_coverage_partial");
                    }
                }
            }
        }
        let missing = selected
            .map(|s| s.pin.unavailable_repositories.clone())
            .unwrap_or_default();
        let partial = !gaps.is_empty() || !missing.is_empty();
        let unavailable = selected.is_none();
        let coverage = coverage(
            &gaps,
            if unavailable {
                CoverageState::Unavailable
            } else if partial {
                CoverageState::Partial
            } else {
                CoverageState::Complete
            },
        );
        let unavailable_repositories = missing
            .iter()
            .map(|m| model::wire(json!({"repositoryId":m.repository_id,"reason":m.reason})))
            .collect::<Result<Vec<CodeUnavailableRepository>, _>>()?;
        let scope_coverage = Some(CodeReadCoverage {
            coverage: coverage.clone(),
            unavailable_repositories: crate::protocol::support::BoundedList(
                unavailable_repositories,
            ),
        });
        let next_cursor = next.as_ref().map(|p| cursors.encode(p)).transpose()?;
        let completion = match exhausted {
            Some("graph_cancelled") => GraphCompletion::Cancelled,
            Some("graph_deadline") => GraphCompletion::Deadline,
            Some(_) => GraphCompletion::WorkLimit,
            None if next_cursor.is_some() => GraphCompletion::ResultLimit,
            None if partial => GraphCompletion::Partial,
            None => GraphCompletion::Complete,
        };
        let resolved = selected
            .map(|selected| resolved(read, selected, stamp.clone()))
            .transpose()?;
        let result = if query.schema_revision().0 < 3 {
            legacy(result)?
        } else {
            result
        };
        let value = crate::protocol::data::GraphPage {
            protocol: request.protocol.clone(),
            query_id: request.query_id.clone(),
            view: resolved,
            completion,
            result,
            scope_coverage: if query.schema_revision().0 < 3 {
                None
            } else {
                scope_coverage
            },
            next_cursor,
        };
        value.validate().map_err(|_| scope::unavailable(None))?;
        Ok(QueryAnswer {
            stamp,
            result: operation_result(query.descriptor().name, value)?,
        })
    }
    fn operate(
        &self,
        reader: &mut Reader<'_>,
        query: &GraphQuery,
        captured: Option<&source::Source>,
        operation: &str,
    ) -> Result<GraphResult, Failure> {
        let choose = |selection: &Option<CodeNodeSelection>, node: &Option<Id>| {
            selection
                .clone()
                .or_else(|| {
                    node.clone()
                        .map(|node_id| CodeNodeSelection::Node { node_id })
                })
                .ok_or_else(|| {
                    Refusal::new(DomainErrorCode::InvalidArgument, "node_selection", None)
                })
        };
        match query {
            GraphQuery::Search {
                text,
                node_kinds,
                selection,
            } => {
                let (mode, text, repo, kinds, module) = if let Some(s) = selection {
                    (
                        s.mode.clone(),
                        s.text.0.as_str(),
                        s.repository_id.as_ref(),
                        s.node_kind.iter().map(|v| v.0.clone()).collect(),
                        s.internal_module.as_ref().map(|s| s.0.as_str()),
                    )
                } else {
                    (
                        CodeSearchMode::RankedNodes,
                        text.as_deref().unwrap_or(""),
                        None,
                        node_kinds
                            .as_ref()
                            .map(|v| v.iter().map(|i| i.as_str().to_owned()).collect())
                            .unwrap_or_default(),
                        None,
                    )
                };
                search::query(
                    reader,
                    search::Search {
                        mode,
                        text,
                        repository: repo,
                        kinds,
                        module,
                    },
                    operation == "repositories.nodes",
                )
            }
            GraphQuery::Node { node_id } => operators::detail(reader, node_id),
            GraphQuery::Source { .. } => {
                source::query(reader, captured.ok_or_else(|| scope::unavailable(None))?)
            }
            GraphQuery::Traverse {
                start_ids,
                direction,
                edge_kinds,
                max_depth,
            } => operators::traverse(reader, start_ids, direction.clone(), edge_kinds, *max_depth),
            GraphQuery::Path {
                selection: Some(s), ..
            } => operators::calls(reader, s),
            GraphQuery::Path {
                from_id: Some(from),
                to_id: Some(to),
                max_depth,
                ..
            } => operators::shortest(reader, from, to, max_depth.unwrap_or(3)),
            GraphQuery::NodeImpact { node_id, selection } => {
                if let Some(s) = selection {
                    impact::node_impact(reader, &s.target, s.depth.get() as u16)
                } else {
                    impact::node_impact(reader, &choose(&None, node_id)?, 3)
                }
            }
            GraphQuery::FileImpact {
                repository_id,
                unit_key,
                selection,
            }
            | GraphQuery::NodesByFile {
                repository_id,
                unit_key,
                selection,
            } => {
                let s = if let Some(s) = selection {
                    s.clone()
                } else {
                    model::wire(json!({"repositoryId":repository_id,"path":unit_key}))?
                };
                if matches!(query, GraphQuery::FileImpact { .. }) {
                    impact::file_impact(
                        reader,
                        &s.repository_id,
                        &s.path.0,
                        s.depth.as_ref().map_or(3, |d| d.get() as u16),
                    )
                } else {
                    operators::by_file(reader, &s)
                }
            }
            GraphQuery::RepositoryDependencies { repository_id } => {
                analysis::dependencies(reader, repository_id)
            }
            GraphQuery::Consumers { node_id, selection } => {
                operators::related(reader, &choose(selection, node_id)?, false)
            }
            GraphQuery::Publishers { node_id, selection } => {
                operators::related(reader, &choose(selection, node_id)?, true)
            }
            GraphQuery::DataLineage { node_id, selection } => {
                operators::lineage(reader, &choose(selection, node_id)?)
            }
            _ => Err(Refusal::new(DomainErrorCode::InvalidArgument, "graph_query", None).into()),
        }
    }
    fn status(
        &self,
        metadata: &Metadata<'_>,
        mut reader: Option<&mut Reader<'_>>,
        current: bool,
    ) -> Result<GraphResult, Failure> {
        let mut repositories = Vec::new();
        metadata.repositories(|row|{if let Some(env)=&self.indexing{let status=crate::services::indexing::status::status(env,metadata.read,metadata.context,&row.id,3,None)?;let value=serde_json::to_value(status).map_err(|_|scope::unavailable(None))?;let record:CodeRepositoryStatus=model::wire(json!({"repositoryId":row.id,"freshness":value["freshness"],"languages":value["languages"],"semanticPacks":value["semanticPacks"],"overlays":value["overlays"]}))?;if repositories.len()>=metadata.limit||!metadata.retain_wire(&record)?{*metadata.next.borrow_mut()=Some(Position{offset:repositories.len(),..Default::default()});return Ok(false);}repositories.push(record);}Ok(true)})?;
        if let Some(reader) = &mut reader {
            reader.budget.absorb_usage(&metadata.budget.borrow());
            reader.next = metadata.next.borrow().clone();
        }
        let available = reader.is_some();
        let mut value = json!({"stamp":metadata.stamp,"coverage":coverage(&[],if available{CoverageState::Complete}else{CoverageState::Unavailable}),"current":current,"repositories":repositories});
        if !available {
            value["unavailableReason"] = json!("NotIndexed");
        }
        Ok(GraphResult::ScopeDetails {
            value: model::wire(value)?,
        })
    }
}
fn coverage(gaps: &[&str], state: CoverageState) -> Coverage {
    let mut unique = std::collections::BTreeSet::new();
    let mut ids = Vec::new();
    for gap in gaps {
        if unique.insert(*gap) && ids.len() < 128 {
            ids.push(Id::new(*gap).unwrap_or_else(|_| {
                Id::new(format!(
                    "graph_gap_{}",
                    crate::fingerprint::sha256_hex(gap.as_bytes())
                ))
                .expect("digest identifier")
            }));
        }
    }
    Coverage {
        state,
        gaps: ids,
        more_gaps: unique.len() > 128,
    }
}
fn resolved(
    read: &dyn ReadSnapshot,
    selected: &lease::Selected,
    stamp: ReadStamp,
) -> Result<ResolvedViewSet, Failure> {
    let mut views = Vec::new();
    for layer in &selected.layers {
        let mut refs = Vec::new();
        for id in [
            &layer.view.baseline_id,
            layer
                .view
                .branch_id
                .as_ref()
                .unwrap_or(&layer.view.baseline_id),
            layer
                .view
                .worktree_id
                .as_ref()
                .unwrap_or(&layer.view.baseline_id),
        ] {
            if refs.iter().any(|r: &LayerRef| r.generation_id == *id) {
                continue;
            }
            let generation = read
                .indexing()
                .generation(id)?
                .ok_or_else(|| scope::unavailable(Some(id.clone())))?;
            let parent = refs.last().map(|r: &LayerRef| r.generation_id.clone());
            refs.push(LayerRef {
                generation_id: id.clone(),
                manifest_digest: model::wire(json!(crate::fingerprint::sha256_hex(
                    generation.manifest.to_string().as_bytes()
                )))?,
                parent_generation_id: parent,
            });
        }
        let witness = selected
            .pin
            .view_witnesses
            .iter()
            .find(|w| w.view_id == layer.view.view_id)
            .ok_or_else(|| scope::unavailable(Some(layer.view.view_id.clone())))?;
        let mut gaps = Vec::new();
        for id in &layer.layers {
            let generation = read
                .indexing()
                .generation(id)?
                .ok_or_else(|| scope::unavailable(Some(id.clone())))?;
            let (_, recorded) =
                crate::services::indexing::status::manifest_coverage(&generation.manifest);
            gaps.extend(recorded);
        }
        let strings: Vec<_> = gaps.iter().map(|g| g.as_str()).collect();
        let coverage = coverage(
            &strings,
            if gaps.is_empty() {
                CoverageState::Complete
            } else {
                CoverageState::Partial
            },
        );
        views.push(RepositoryView {
            view_id: layer.view.view_id.clone(),
            snapshot: model::snapshot(&layer.view)?,
            source_revision: rev(witness.source_revision),
            baseline: refs.remove(0),
            branch: if layer.view.branch_id.is_some() {
                Some(refs.remove(0))
            } else {
                None
            },
            worktree: if layer.view.worktree_id.is_some() {
                Some(refs.remove(0))
            } else {
                None
            },
            coverage,
        });
    }
    Ok(ResolvedViewSet {
        schema_views: Vec::new(),
        pin_id: selected.pin.id.clone(),
        stamp,
        expires_at_ms: Counter::new(selected.pin.expires_at_ms)
            .map_err(|_| scope::unavailable(None))?,
        views,
    })
}
fn legacy(result: GraphResult) -> Result<GraphResult, Failure> {
    let node = |n: CodeNode| -> Result<NodeView, Failure> {
        let citation = n
            .source
            .and_then(|s| s.legacy_citation)
            .ok_or_else(|| scope::unavailable(None))?;
        Ok(NodeView {
            node_id: n.node_id,
            unit_key: model::wire(json!(n.unit_key))?,
            kind: model::wire(json!(n.node_kind))?,
            name: n.name.0,
            citation,
        })
    };
    match result {
        GraphResult::CodeNodes { value } => Ok(GraphResult::Nodes {
            nodes: value
                .nodes
                .0
                .into_iter()
                .map(node)
                .collect::<Result<_, _>>()?,
        }),
        GraphResult::CodeSearch { value } => {
            let mut nodes = Vec::new();
            for hit in value.items.0 {
                if let CodeSearchHit::Node { node: n } = hit {
                    nodes.push(node(n)?);
                } else {
                    return Err(scope::unavailable(None));
                }
            }
            Ok(GraphResult::Nodes { nodes })
        }
        GraphResult::SourceText { value } => Ok(GraphResult::Source {
            citation: value
                .source
                .legacy_citation
                .ok_or_else(|| scope::unavailable(None))?,
            text: value.text.0,
        }),
        GraphResult::ScopeDetails { value } => Ok(GraphResult::ScopeStatus {
            coverage: value.coverage,
        }),
        _ => Err(scope::unavailable(None)),
    }
}
fn operation_result(
    name: &str,
    value: crate::protocol::data::GraphPage,
) -> Result<OperationResult, Failure> {
    Ok(match name {
        "code.search" => OperationResult::CodeSearch { value },
        "code.architecture" => OperationResult::CodeArchitecture { value },
        "code.dependency" => OperationResult::CodeDependency { value },
        "code.impact" => OperationResult::CodeImpact { value },
        "code.source" => OperationResult::CodeSource { value },
        "code.traverse" => OperationResult::CodeTraverse { value },
        "code.graph_scope_status" => OperationResult::CodeGraphScopeStatus { value },
        "code.overview" => OperationResult::CodeOverview { value },
        "code.nodes_by_file" => OperationResult::CodeNodesByFile { value },
        "code.node_detail" => OperationResult::CodeNodeDetail { value },
        "code.file_impact" => OperationResult::CodeFileImpact { value },
        "code.graph_schema" => OperationResult::CodeGraphSchema { value },
        "code.call_path" => OperationResult::CodeCallPath { value },
        "code.data_lineage" => OperationResult::CodeDataLineage { value },
        "code.consumers" => OperationResult::CodeConsumers { value },
        "code.publishers" => OperationResult::CodePublishers { value },
        "code.snippet" => OperationResult::CodeSnippet { value },
        "code.repository_dependencies" => OperationResult::CodeRepositoryDependencies { value },
        "repositories.nodes" => OperationResult::RepositoriesNodes { value },
        _ => {
            return Err(Refusal::new(
                DomainErrorCode::UnsupportedCapability,
                "graph_operation",
                None,
            )
            .into());
        }
    })
}

impl CommandHandler for GraphService {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let outcome = (|| {
            let OperationInput::Code(operations::code::Input::SetTrust(input)) = command.input()
            else {
                return Err(Refusal::new(
                    DomainErrorCode::UnsupportedCapability,
                    "graph_command",
                    None,
                )
                .into());
            };
            let IndexCommand::SetTrust {
                repository_id,
                node_id,
                expected_revision,
                view_id,
                expected_content_digest,
                trusted,
                unit_key,
                expected_view_digest,
                expected_content_identity,
                scope: data_scope,
                pin_id,
            } = &input.command
            else {
                return Err(Refusal::new(DomainErrorCode::InvalidArgument, "trust", None).into());
            };
            let data_scope = data_scope.clone().unwrap_or_else(|| {
                ctx.project_id()
                    .map(|id| DataScope::Project {
                        project_id: id.clone(),
                    })
                    .unwrap_or(DataScope::Workspace)
            });
            let pin = pin_id
                .as_ref()
                .map(|id| tx.graph().graph_pin(id))
                .transpose()?
                .flatten();
            if pin_id.is_some() && pin.is_none() {
                return Err(scope::unavailable(pin_id.clone()));
            }
            if let Some(pin) = &pin {
                lease::check_pin(pin, ctx, &data_scope, tx.now_ms())?;
                for missing in &pin.unavailable_repositories {
                    crate::services::projects::visible_repository(
                        tx.projects(),
                        ctx,
                        &missing.repository_id,
                    )?;
                    if let DataScope::Project { project_id } = &data_scope
                        && !tx
                            .projects()
                            .members(project_id)?
                            .contains(&missing.repository_id)
                    {
                        return Err(Refusal::new(
                            DomainErrorCode::PermissionDenied,
                            "graph_scope",
                            None,
                        )
                        .into());
                    }
                }
            }
            let selected = if let Some(pin) = &pin {
                let mut selected = Vec::new();
                for binding in &pin.scope.views {
                    let view = views::layers(tx.indexing(), &binding.view_id)?;
                    if view.view.repository_id != binding.repository_id
                        || view.view.view_digest != binding.view_digest
                    {
                        return Err(scope::unavailable(Some(view_id.clone())));
                    }
                    selected.push(view);
                }
                selected
            } else {
                vec![views::layers(tx.indexing(), view_id)?]
            };
            scope::validate_in(
                tx,
                ctx,
                &data_scope,
                &selected,
                false,
                pin.as_ref().map(|p| p.view_witnesses.as_slice()),
            )?;
            let target = selected
                .iter()
                .find(|v| v.view.view_id == *view_id && v.view.repository_id == *repository_id)
                .ok_or_else(|| scope::unavailable(Some(view_id.clone())))?;
            if expected_view_digest
                .as_ref()
                .is_some_and(|d| d.0 != target.view.view_digest)
            {
                return Err(Refusal::new(
                    DomainErrorCode::SourceChanged,
                    "graph_view",
                    Some(view_id.clone()),
                )
                .into());
            }
            let graph_scope = GraphScope {
                views: selected
                    .iter()
                    .map(|v| GraphViewBinding {
                        repository_id: v.view.repository_id.clone(),
                        view_id: v.view.view_id.clone(),
                        view_digest: v.view.view_digest.clone(),
                    })
                    .collect(),
            };
            let rows = tx.graph().graph_nodes(
                &GraphScope {
                    views: vec![GraphViewBinding {
                        repository_id: repository_id.clone(),
                        view_id: view_id.clone(),
                        view_digest: target.view.view_digest.clone(),
                    }],
                },
                &GraphNodeFilter {
                    node_id: Some(node_id.as_str().into()),
                    unit_key: unit_key.as_ref().map(|u| u.0.clone()),
                    ..Default::default()
                },
                None,
                2,
            )?;
            if rows.items.len() != 1 || rows.has_more {
                return Err(Refusal::new(
                    DomainErrorCode::InvalidArgument,
                    "ambiguous_node_identity",
                    Some(node_id.clone()),
                )
                .into());
            }
            let node = &rows.items[0];
            let identity = dataset_identity(node);
            if expected_content_identity
                .as_ref()
                .is_some_and(|d| d.0 != node.content_digest)
            {
                return Err(Refusal::new(
                    DomainErrorCode::SourceChanged,
                    "graph_content",
                    Some(node_id.clone()),
                )
                .into());
            }
            if let Some(digest) = expected_content_digest {
                let unit = crate::services::indexing::views::resolve_unit(
                    tx.indexing(),
                    target,
                    &node.node.unit_key,
                )?
                .ok_or_else(|| scope::unavailable(Some(node_id.clone())))?
                .1;
                let capture_id: Id = model::wire(unit.payload["sourceCapture"]["id"].clone())?;
                let captured = tx
                    .graph()
                    .graph_source_capture(&capture_id)?
                    .ok_or_else(|| scope::unavailable(Some(node_id.clone())))?;
                if captured.repository_id != *repository_id
                    || captured.unit_key != node.node.unit_key
                    || captured.source_digest != node.content_digest
                    || captured.blob_digest != digest.0
                {
                    return Err(Refusal::new(
                        DomainErrorCode::SourceChanged,
                        "graph_content",
                        Some(node_id.clone()),
                    )
                    .into());
                }
            }
            let existing = tx
                .graph()
                .graph_trust(&graph_scope, &identity, &node.content_digest)?;
            let revision = existing.as_ref().map_or(1, |r| r.revision);
            if expected_revision.get() != revision {
                return Err(Refusal::new(
                    DomainErrorCode::Conflict,
                    "graph_trust",
                    Some(node_id.clone()),
                )
                .into());
            }
            if existing.as_ref().is_some_and(|r| r.trusted == *trusted) {
                return Ok(DomainDecision::NoChange {
                    result: OperationResult::CodeSetTrust {
                        value: MutationSummary {
                            id: node_id.clone(),
                            revision: rev(revision),
                            changed: false,
                        },
                    },
                });
            }
            let row = StoredGraphTrust {
                identity,
                view_id: view_id.clone(),
                view_digest: target.view.view_digest.clone(),
                content_digest: node.content_digest.clone(),
                revision: revision + 1,
                trusted: *trusted,
                updated_by: ctx.principal().clone(),
                updated_at_ms: tx.now_ms(),
            };
            tx.graph().put_graph_trust(&graph_scope, &row, revision)?;
            let event = EventId(tx.new_id(IdKind::Event));
            tx.history().append(NewEvent{id:event,actor:ctx.principal().clone(),project_id:ctx.project_id().cloned(),root_id:None,operation_id:None,aggregate_kind:"graph_trust".into(),aggregate_id:node_id.as_str().into(),aggregate_revision:row.revision,kind:"GraphTrustChanged".into(),payload:json!({"repositoryId":repository_id,"viewId":view_id,"unitKey":node.node.unit_key,"nodeId":node_id,"trusted":trusted,"contentIdentity":node.content_digest})})?;
            Ok(DomainDecision::Applied {
                result: OperationResult::CodeSetTrust {
                    value: MutationSummary {
                        id: node_id.clone(),
                        revision: rev(row.revision),
                        changed: true,
                    },
                },
            })
        })();
        sessions::decided(tx, outcome)
    }
}
fn dataset_identity(node: &GraphNode) -> GraphNodeIdentity {
    super::dataset::identity(node)
}

fn bound_cursors(
    ctx: &InvocationContext,
    query: &AdmittedQuery,
    request: &GraphReadRequest,
    selected: Option<&lease::Selected>,
    read: &dyn ReadSnapshot,
) -> Result<Cursors, Failure> {
    let revision = if let Some(selected) = selected {
        read.graph().graph_trust_revision(&selected.pin.scope)?
    } else {
        read.stamp().committed_sequence.max(1)
    };
    let mut canonical =
        json!({"scope":request.scope,"query":request.query,"limit":request.limits.max_results});
    if let Some(selected) = selected {
        canonical["selection"] = json!(selected.pin.intent_digest);
        let mut registrations = Vec::new();
        for view in &selected.pin.scope.views {
            let repository = read
                .projects()
                .repository(&view.repository_id)?
                .ok_or_else(|| scope::unavailable(Some(view.repository_id.clone())))?;
            registrations.push(json!({"id":repository.id,"revision":repository.revision,"name":repository.name,"remote":repository.remote_url}));
        }
        canonical["registrations"] = json!(registrations);
    }
    let binding = crate::services::cursor::CursorBinding::new(
        ctx,
        query.descriptor().name,
        &[&canonical.to_string()],
        revision,
    );
    Ok(Cursors::new(binding, selected.map(|s| s.pin.id.clone())))
}
