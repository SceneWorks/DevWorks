//! Native read routes for the analysis family.
mod measurements;
mod project;
use super::super::model;
use super::{
    communities::CommunityReport,
    links::LinkReport,
    observation::{self, Observation},
    phase_store::PhaseStore,
    read::{ReadPlan, Report},
    security::SecurityReport,
    selection,
    vitals::VitalsReport,
};
use crate::context::InvocationContext;
use crate::gateway::AdmittedQuery;
use crate::operations::{OperationInput, analysis as ops};
use crate::ports::storage::*;
use crate::ports::{IdKind, IdSource, Preparation, Prepared, QueryAnswer, QueryHandler};
use crate::protocol::data::{DataScope, ViewSelection};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::*;
use crate::protocol::results::OperationResult;
use crate::protocol::support::{BoundedList, Finite, Label, RelativePath};
use crate::protocol::{CorrelationId, DomainErrorCode, DomainFault, Id};
use crate::services::cursor::{self, CursorBinding};
use crate::services::kernel::{Failure, Refusal, counter, rev};
use serde_json::json;
use std::{collections::BTreeSet, sync::Arc};

pub(super) struct Queries {
    pub reports: Arc<PhaseStore>,
    pub ids: Arc<dyn IdSource>,
    pub indexing: Arc<std::sync::OnceLock<Arc<crate::services::indexing::IndexEnv>>>,
}

enum Kind<'a> {
    Status,
    Health { top: usize },
    Links,
    Clusters { graph: bool },
    Cluster { id: &'a Id, ordinal: Option<i64> },
    Security,
    Metrics { project: Option<&'a str> },
    Hotspots,
    File { path: &'a RelativePath },
    Fleet { maximum: Option<&'a Finite> },
    Archival,
}

struct Request<'a> {
    scope: &'a DataScope,
    views: &'a ViewSelection,
    page: &'a PageRequest,
    filters: &'a [Label],
    run_id: Option<&'a Id>,
    kind: Kind<'a>,
}

impl<'a> Request<'a> {
    fn from(input: &'a OperationInput) -> Result<Self, Failure> {
        let OperationInput::Analysis(input) = input else {
            return Err(unsupported());
        };
        macro_rules! query {
            ($input:ident, $kind:expr) => {
                Self {
                    scope: &$input.scope,
                    views: &$input.views,
                    page: &$input.page,
                    filters: &$input.filters.0,
                    run_id: None,
                    kind: $kind,
                }
            };
        }
        Ok(match input {
            ops::Input::Status(i) => Self {
                run_id: i.run_id.as_ref(),
                ..query!(i, Kind::Status)
            },
            ops::Input::Health(i) => Self {
                run_id: i.run_id.as_ref(),
                ..query!(
                    i,
                    Kind::Health {
                        top: i.top_hotspots.as_ref().map_or(10, |n| n.get()) as usize
                    }
                )
            },
            ops::Input::CrossRepoLinks(i) => query!(i, Kind::Links),
            ops::Input::Clusters(i) => query!(
                i,
                Kind::Clusters {
                    graph: matches!(i.projection, Some(AnalysisClusterProjection::Graph)),
                }
            ),
            ops::Input::ClusterDetail(i) => query!(
                i,
                Kind::Cluster {
                    id: &i.cluster_id,
                    ordinal: i.legacy_ordinal.as_ref().map(|n| n.get()),
                }
            ),
            ops::Input::Security(i) => Self {
                run_id: i.run_id.as_ref(),
                ..query!(i, Kind::Security)
            },
            ops::Input::Metrics(i) => query!(
                i,
                Kind::Metrics {
                    project: i.dotnet_project.as_ref().map(|text| text.0.as_str())
                }
            ),
            ops::Input::Hotspots(i) => query!(i, Kind::Hotspots),
            ops::Input::FileHotspot(i) => query!(i, Kind::File { path: &i.path }),
            ops::Input::FleetHealth(i) => query!(
                i,
                Kind::Fleet {
                    maximum: i.max_health.as_ref()
                }
            ),
            ops::Input::ArchivalCandidates(i) => query!(i, Kind::Archival),
            _ => return Err(unsupported()),
        })
    }

    fn phase(&self) -> Option<AnalysisPhase> {
        Some(match self.kind {
            Kind::Status => return None,
            Kind::Links | Kind::Archival => AnalysisPhase::Links,
            Kind::Clusters { .. } | Kind::Cluster { .. } => AnalysisPhase::Communities,
            Kind::Security => AnalysisPhase::Security,
            _ => AnalysisPhase::Vitals,
        })
    }

    fn repository_projection(&self) -> bool {
        matches!(
            self.kind,
            Kind::Health { .. }
                | Kind::File { .. }
                | Kind::Security
                | Kind::Metrics { .. }
                | Kind::Hotspots
                | Kind::Fleet { .. }
        )
    }

    fn history(&self) -> bool {
        matches!(
            self.kind,
            Kind::Health { .. } | Kind::File { .. } | Kind::Security
        )
    }

    fn filter(&self, read: &dyn ReadSnapshot, plan: &ReadPlan) -> Result<Filters, Failure> {
        let mut filters = Filters::default();
        for label in self.filters {
            let Some((name, value)) = label.0.split_once(':') else {
                return Err(invalid("analysis_filter"));
            };
            if value.is_empty() {
                return Err(invalid("analysis_filter"));
            }
            match name {
                "repository" => {
                    let id = Id::new(value).map_err(|_| invalid("analysis_repository_filter"))?;
                    if !plan
                        .selected
                        .views
                        .iter()
                        .any(|view| view.repository_id == id)
                        && !plan
                            .selected
                            .unavailable
                            .iter()
                            .any(|view| view.repository_id == id)
                    {
                        return Err(selection::unavailable());
                    }
                    filters.repositories.insert(id.as_str().to_owned());
                }
                "role"
                    if matches!(
                        self.kind,
                        Kind::Metrics { .. }
                            | Kind::Hotspots
                            | Kind::File { .. }
                            | Kind::Health { .. }
                    ) && matches!(value, "core" | "test") =>
                {
                    filters.roles.insert(value.to_owned());
                }
                "relationship"
                    if matches!(self.kind, Kind::Links | Kind::Clusters { graph: true }) =>
                {
                    if value.len() > 48
                        || !value
                            .bytes()
                            .all(|byte| byte.is_ascii_uppercase() || byte == b'_')
                    {
                        return Err(invalid("analysis_relationship_filter"));
                    }
                    filters.relationships.insert(value.to_owned());
                }
                _ => return Err(invalid("analysis_filter")),
            }
        }
        // An archival assertion requires the complete, current workspace inventory.
        if matches!(self.kind, Kind::Archival) && matches!(self.scope, DataScope::Workspace) {
            let active = read.projects().repositories(
                &RepositoryFilter {
                    project: None,
                    text: None,
                    languages: Vec::new(),
                    include_deleted: false,
                },
                None,
                256,
            )?;
            filters.complete_workspace = active.len() < 256 && plan.selected.unavailable.is_empty();
            for repository in active.iter().filter(|repo| repo.lifecycle == "Active") {
                let selected = plan
                    .selected
                    .views
                    .iter()
                    .find(|view| view.repository_id == repository.id);
                if let Some(view) = selected {
                    if read
                        .indexing()
                        .published(&repository.id)?
                        .is_none_or(|pointer| pointer.view_id != view.view_id)
                    {
                        filters.workspace_snapshots_stale = true;
                    }
                } else {
                    filters.complete_workspace = false;
                }
            }
            filters.complete_workspace &= !filters.workspace_snapshots_stale;
        }
        Ok(filters)
    }

    fn cursor(&self, ctx: &InvocationContext, operation: &str, plan: &ReadPlan) -> CursorBinding {
        let selectors = match self.kind {
            Kind::Health { top } => json!({"topHotspots": top}),
            Kind::Clusters { graph } => json!({"graph": graph}),
            Kind::Cluster { id, ordinal } => json!({"id": id, "ordinal": ordinal}),
            Kind::Metrics { project } => json!({"project": project}),
            Kind::File { path } => json!({"path": path}),
            Kind::Fleet { maximum } => json!({"maxHealth": maximum}),
            _ => serde_json::Value::Null,
        };
        let criteria = selection::digest(&json!({
            "filters": self.filters, "selectors": selectors, "run": self.run_id,
            "reader": super::super::lease::owner(ctx), "policy": ctx.policy_revision(),
        }));
        CursorBinding::new(
            ctx,
            operation,
            &[&plan.fence(), &criteria],
            plan.selected.configuration_revision,
        )
    }
}

#[derive(Default)]
struct Filters {
    repositories: BTreeSet<String>,
    roles: BTreeSet<String>,
    relationships: BTreeSet<String>,
    complete_workspace: bool,
    workspace_snapshots_stale: bool,
}
impl Filters {
    fn repository(&self, id: &Id) -> bool {
        self.repositories.is_empty() || self.repositories.contains(id.as_str())
    }
    fn role(&self, role: &str) -> bool {
        self.roles.is_empty() || self.roles.contains(role)
    }
    fn relationship(&self, value: &str) -> bool {
        self.relationships.is_empty() || self.relationships.contains(value)
    }
}

#[derive(Default)]
struct Documents {
    references: Vec<ArtifactRef>,
    links: Option<LinkReport>,
    communities: Option<CommunityReport>,
    vitals: Option<VitalsReport>,
    security: Option<SecurityReport>,
    trends: Vec<AnalysisTrendPoint>,
    narratives: Vec<AnalysisNarrative>,
    fleet: Vec<measurements::FleetReading>,
    measurement_gaps: Vec<String>,
}

struct Captured {
    fence: String,
    documents: Documents,
}

impl QueryHandler for Queries {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, DomainFault> {
        let request = Request::from(query.input()).map_err(|error| self.fault(error))?;
        let result = env
            .read(|read| {
                Ok(ReadPlan::load(
                    read,
                    ctx,
                    request.scope,
                    request.views,
                    request.run_id,
                    request.history(),
                    request.repository_projection(),
                ))
            })
            .map_err(|error| self.fault(error.into()))?;
        let plan = result.map_err(|error| self.fault(error))?;
        let documents = self
            .load(ctx, &request, &plan)
            .map_err(|error| self.fault(error))?;
        Ok(Some(Prepared::new(Captured {
            fence: plan.fence(),
            documents,
        })))
    }

    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        read: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        self.answer(ctx, query, read)
            .map_err(|error| self.fault(error))
    }
}

impl Queries {
    fn load(
        &self,
        ctx: &InvocationContext,
        request: &Request<'_>,
        plan: &ReadPlan,
    ) -> Result<Documents, Failure> {
        let mut docs = Documents::default();
        let report = request.phase().and_then(|phase| {
            plan.primary(phase, request.history(), request.repository_projection())
        });
        if matches!(request.kind, Kind::Fleet { .. }) {
            return measurements::fleet(self, ctx, plan);
        }
        match request.phase() {
            Some(AnalysisPhase::Links) => {
                docs.links = self.document(ctx, report.as_ref(), &mut docs.references)?
            }
            Some(AnalysisPhase::Communities) => {
                docs.communities = self.document(ctx, report.as_ref(), &mut docs.references)?
            }
            Some(AnalysisPhase::Security) => {
                docs.security = self.document(ctx, report.as_ref(), &mut docs.references)?
            }
            Some(AnalysisPhase::Vitals) => {
                docs.vitals = self.document(ctx, report.as_ref(), &mut docs.references)?;
                if matches!(request.kind, Kind::Health { .. } | Kind::Fleet { .. }) {
                    let companion = report.as_ref().and_then(|report| {
                        plan.phase_for_binding(AnalysisPhase::Security, &report.run.binding_key)
                            .or_else(|| match plan.selected.views.as_slice() {
                                [view] => plan
                                    .repository_phases(AnalysisPhase::Security, view, false)
                                    .next()
                                    .map(|(_, report)| report),
                                _ => None,
                            })
                    });
                    docs.security = self.document(ctx, companion.as_ref(), &mut docs.references)?;
                }
            }
            _ => {}
        }
        if request.repository_projection() {
            let selected = |id: &Id| {
                plan.selected
                    .views
                    .iter()
                    .any(|view| &view.repository_id == id)
            };
            if let Some(vitals) = &mut docs.vitals {
                vitals
                    .repositories
                    .retain(|repo| selected(&repo.repository));
            }
            if let Some(security) = &mut docs.security {
                security
                    .repositories
                    .retain(|repo| selected(&repo.repository));
            }
        }
        if matches!(request.kind, Kind::Health { .. }) {
            measurements::trends(self, ctx, request, plan, &mut docs)?;
            if let Some(report) = &report {
                measurements::narratives(self, ctx, plan, report, &mut docs)?;
            }
        }
        Ok(docs)
    }

    fn document<T: serde::de::DeserializeOwned>(
        &self,
        ctx: &InvocationContext,
        report: Option<&Report<'_>>,
        refs: &mut Vec<ArtifactRef>,
    ) -> Result<Option<T>, Failure> {
        let Some(reference) = report.and_then(Report::reference) else {
            return Ok(None);
        };
        let loaded = self.reports.read(ctx, reference)?;
        refs.push(reference.clone());
        Ok(Some(loaded))
    }

    fn answer(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        read: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, Failure> {
        let request = Request::from(query.input())?;
        let plan = ReadPlan::load(
            read,
            ctx,
            request.scope,
            request.views,
            request.run_id,
            request.history(),
            request.repository_projection(),
        )?;
        let captured = query
            .prepared::<Captured>()
            .ok_or_else(selection::unavailable)?;
        if plan.fence() != captured.fence {
            return Err(if request.page.cursor.is_some() {
                StorageError::CursorExpired.into()
            } else {
                selection::unavailable()
            });
        }
        for reference in &captured.documents.references {
            if !observation::readable(read, ctx, reference)? {
                return Err(selection::unavailable());
            }
        }
        let filters = request.filter(read, &plan)?;
        let binding = request.cursor(ctx, query.descriptor().name, &plan);
        let report = request.phase().and_then(|phase| {
            plan.primary(phase, request.history(), request.repository_projection())
        });
        if let Some(report) = &report
            && !plan.readable(read, ctx, report)?
        {
            return Err(selection::unavailable());
        }
        let result = project::answer(project::Input {
            ctx,
            read,
            plan: &plan,
            request: &request,
            filters: &filters,
            cursor: &binding,
            report: report.as_ref(),
            documents: &captured.documents,
            parser: self.indexing.get().map(|env| env.parser.as_ref()),
        })?;
        Ok(QueryAnswer {
            stamp: read.stamp().read_stamp(
                ctx.policy_revision().clone(),
                rev(plan.selected.configuration_revision),
            ),
            result,
        })
    }

    fn fault(&self, error: Failure) -> DomainFault {
        error.query_fault(
            "repository_analysis",
            CorrelationId(self.ids.next_id(IdKind::Correlation)),
        )
    }
}

fn invalid(target: &'static str) -> Failure {
    Refusal::new(DomainErrorCode::InvalidArgument, target, None).into()
}
fn unsupported() -> Failure {
    Refusal::new(DomainErrorCode::UnsupportedCapability, "analysis", None).into()
}
