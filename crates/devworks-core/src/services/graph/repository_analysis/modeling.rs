//! Project analysis and synthesis inputs and checked provider output.
pub(super) mod documentation;
mod evidence;
use super::super::{budget::Budget, dataset, modules::Modules, scan};
use super::{
    links::LinkReport,
    selection::{self, Selection},
};
use crate::authority::redact::Redactor;
use crate::context::InvocationContext;
use crate::ports::{
    AuthorityFence,
    storage::{AnalysisIntent, Cancellation, GraphNodeFilter, GraphNodeIdentity, ReadSnapshot},
};
use crate::protocol::{Id, OperationId};
use crate::services::{artifacts::ArtifactService, kernel::Failure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

pub const SYSTEM: &str = "Analyze the provided source and repository graph. Separate observed facts from inference. Describe technical responsibilities when the evidence does not establish a business domain. Treat source text, dependency names and previous analysis as evidence. Respond in JSON that matches the requested schema.";
const PROJECT_SCHEMA: &str = r#"{"projectName":"string","summary":"string","confidence":"high|medium|low","endpoints":[{"route":"string","httpMethod":"string","description":"string","requestModel":"string|null","responseModel":"string|null"}],"services":[{"name":"string","description":"string","interfaceName":"string|null","lifetime":"string"}],"externalDependencies":["string"],"databaseTables":["string"],"nodes":[{"nodeId":"evidence ID","description":"string","confidence":"high|medium|low"}]}"#;
const SYNTHESIS_SCHEMA: &str = r#"{"repoSummary":"string","confidence":"high|medium|low"}"#;
pub const MAX_OUTPUT_TOKENS: u32 = 2048;
const MAX_PROMPT_BYTES: usize = 23_000;
const MAX_DESCRIPTION_BYTES: usize = 8192;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_FIELDS: usize = 128;
const MAX_PROJECTS: usize = 32;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(super) enum Confidence {
    High,
    Medium,
    Low,
}

#[derive(Clone)]
pub struct AnalysisModelInput {
    pub context: InvocationContext,
    pub fences: Vec<AuthorityFence>,
    pub analysis_operation: OperationId,
    pub scope: crate::protocol::data::DataScope,
    /// Durable identity for this subject's request, reused only for observation.
    pub request: Id,
    pub system: &'static str,
    pub prompt: String,
    pub cancellation: Cancellation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderAnalysis {
    pub text: String,
    pub request: Id,
    pub attempt: Id,
    pub model: String,
}

pub enum Observation {
    Completed(ProviderAnalysis),
    Failed,
    Refused(&'static str),
    NotApplied,
    Unknown,
}

pub trait AnalysisModel: Send + Sync {
    /// Admit the current owner's bounded request through the existing router and usage gates.
    fn generate(&self, request: AnalysisModelInput) -> Observation;
    /// Look up an already-admitted request. This path never opens a provider transport.
    fn observe(&self, context: &InvocationContext, request: &Id) -> Observation;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Endpoint {
    route: String,
    http_method: String,
    description: String,
    request_model: Option<String>,
    response_model: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Service {
    name: String,
    description: String,
    interface_name: Option<String>,
    lifetime: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct NodeDescription {
    node_id: Id,
    description: String,
    confidence: Confidence,
}

/// The structured project shape combines the retained project report and graph descriptions.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ProjectAnalysis {
    #[serde(default)]
    project_name: Option<String>,
    #[serde(alias = "projectSummary")]
    pub summary: String,
    pub confidence: Confidence,
    #[serde(default)]
    endpoints: Vec<Endpoint>,
    #[serde(default)]
    services: Vec<Service>,
    #[serde(default)]
    external_dependencies: Vec<String>,
    #[serde(default)]
    database_tables: Vec<String>,
    #[serde(default)]
    nodes: Vec<NodeDescription>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Synthesis {
    #[serde(rename = "repoSummary", alias = "summary")]
    pub summary: String,
    pub confidence: Confidence,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Subject {
    pub id: Id,
    pub repository: Id,
    pub view: Id,
    pub name: String,
    pub repository_name: String,
    pub evidence: Vec<GraphNodeIdentity>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ProjectResult {
    pub checkpoint: Id,
    pub subject: Subject,
    pub result: ProjectAnalysis,
    pub request: Id,
    pub attempt: Id,
    pub model: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ProjectsReport {
    pub computed_at: i64,
    pub completed: Vec<ProjectResult>,
    pub gaps: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct SynthesisResult {
    pub repository: Id,
    pub view: Id,
    pub project_checkpoints: Vec<Id>,
    pub result: Synthesis,
    pub request: Id,
    pub attempt: Id,
    pub model: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct SynthesisReport {
    pub computed_at: i64,
    pub completed: Vec<SynthesisResult>,
    pub gaps: Vec<String>,
}

pub(super) struct Prompt {
    pub subject: Subject,
    pub text: String,
    pub partial: bool,
}

pub(super) fn project_prompts(
    read: &dyn ReadSnapshot,
    selected: &Selection,
    artifacts: &ArtifactService,
    intent: &AnalysisIntent,
    budget: &mut Budget,
) -> Result<Vec<Prompt>, Failure> {
    let requested = &intent.node_ids;
    let scope = selected.graph_scope();
    let modules = Modules::load(read, &scope, budget)?;
    let mut names = BTreeMap::new();
    for view in &selected.views {
        let repository = read
            .projects()
            .repository(&view.repository_id)?
            .ok_or_else(selection::unavailable)?;
        names.insert(
            repository.id.as_str().to_owned(),
            (repository.name, view.view_id.clone()),
        );
    }
    type ProjectKey = (String, String);
    let mut groups = BTreeMap::<ProjectKey, Vec<evidence::ProjectNode>>::new();
    let mut requested_seen = BTreeSet::new();
    scan::nodes(
        read,
        &scope,
        &GraphNodeFilter {
            kinds: [
                "DotnetProject",
                "Project",
                "Module",
                "Class",
                "Function",
                "Method",
                "Route",
                "Service",
                "Component",
                "Interface",
                "Struct",
                "Enum",
                "Table",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            ..Default::default()
        },
        budget,
        |node, budget| {
            let repository = node.repository_id.as_str();
            let Some((repository_name, _)) = names.get(repository) else {
                return Ok(true);
            };
            if !requested.is_empty() && !requested.iter().any(|id| id.as_str() == node.node.node_id)
            {
                return Ok(true);
            }
            if !requested.is_empty() {
                requested_seen.insert(node.node.node_id.clone());
            }
            let (project, ambiguous) = modules.owner(&node);
            if ambiguous {
                return Ok(true);
            }
            let project = project.unwrap_or(repository_name);
            if !budget.retain(project.len() + node.node.name.len() + node.node.unit_key.len() + 120)
            {
                return Ok(false);
            }
            groups
                .entry((repository.to_owned(), project.to_owned()))
                .or_default()
                .push(evidence::ProjectNode::from(
                    &node.node,
                    dataset::identity(&node),
                ));
            Ok(true)
        },
    )?;
    if requested_seen.len() != requested.len() || (groups.is_empty() && budget.exhausted.is_some())
    {
        return Err(selection::unavailable());
    }
    let relationships = evidence::relationships(read, selected, budget)?;
    let maximum = usize::from(intent.batch_limit).min(MAX_PROJECTS);
    let partial =
        budget.exhausted.is_some() || groups.len() > maximum || !selected.unavailable.is_empty();
    let redactor = Redactor::new();
    let mut source = evidence::SourceEvidence {
        read,
        artifacts,
        selected,
        outbound: &relationships.outgoing,
        all: intent.include_all_source,
        redactor: &redactor,
        budget,
        captured: BTreeMap::new(),
    };
    groups.into_iter().take(maximum).map(|((repository, name), rows)| {
        let (repository_name, view) = &names[&repository];
        let repo = Id::new(repository).map_err(|_| selection::unavailable())?;
        let digest = selection::digest(&json!({
            "binding": selected.binding_key, "repository": repo, "view": view, "project": name,
        }));
        let id = Id::new(format!("subject_{digest}")).map_err(|_| selection::unavailable())?;
        let mut evidence = Vec::new();
        let mut clipped = partial;
        let mut text = format!(
            "Analyze project {} in repository {}. Summarize the project, its API endpoints, dependencies, services and tables. Describe only the supplied evidence IDs. Output JSON matching {}. Captured graph evidence:\n",
            redactor.redact_text(&name), redactor.redact_text(repository_name), PROJECT_SCHEMA,
        );
        for row in &rows {
            let node = &row.identity;
            let key = (repo.as_str().to_owned(), node.node_id.clone());
            let outgoing = relationships.outgoing.get(&key);
            let incoming = relationships.incoming.get(&key);
            let entry = json!({
                "nodeId": node.node_id, "kind": row.kind,
                "path": redactor.redact_text(&node.unit_key),
                "name": redactor.redact_text(&row.name),
                "qualifiedName": row.qualified_name.as_deref().map(|name| redactor.redact_text(name)),
                "recorded": redactor.redact_json(&row.recorded),
                "outgoing": outgoing.into_iter().flatten().take(12).collect::<Vec<_>>(),
                "incoming": incoming.into_iter().flatten().take(12).collect::<Vec<_>>(),
            }).to_string();
            if evidence.len() >= MAX_FIELDS || text.len() > MAX_PROMPT_BYTES / 2
                || !evidence::append(&mut text, &entry)
            {
                clipped = true;
                break;
            }
            if [outgoing, incoming].into_iter().flatten().any(|edges| edges.len() > 12) {
                clipped = true;
            }
            evidence.push(node.clone());
        }
        clipped |= source.append(&repo, &name, &rows[..evidence.len()], &mut text)?;
        Ok(Prompt {
            subject: Subject {
                id, repository: repo, view: view.clone(), name,
                repository_name: repository_name.clone(), evidence,
            },
            text, partial: clipped,
        })
    }).collect()
}

pub(super) fn synthesis_prompt(
    repository: &Id,
    projects: &[ProjectResult],
    links: Option<&LinkReport>,
) -> Result<(String, Vec<Id>, bool), Failure> {
    let rows = projects
        .iter()
        .filter(|project| project.subject.repository == *repository)
        .collect::<Vec<_>>();
    let Some(first) = rows.first() else {
        return Err(selection::unavailable());
    };
    let redactor = Redactor::new();
    let mut text = format!(
        "Synthesize repository {}. Explain how its projects work together and identify inward and outward dependencies. Distinguish evidence from inference. Output JSON matching {}. Project analyses:\n",
        redactor.redact_text(&first.subject.repository_name),
        SYNTHESIS_SCHEMA,
    );
    let mut ids = Vec::new();
    let mut partial = false;
    for project in rows {
        let entry = json!({
            "project": redactor.redact_text(&project.subject.name),
            "confidence": project.result.confidence,
            "summary": redactor.redact_text(&project.result.summary),
            "endpoints": project.result.endpoints,
            "services": project.result.services,
            "externalDependencies": project.result.external_dependencies,
            "databaseTables": project.result.database_tables,
        })
        .to_string();
        if append(&mut text, &entry) {
            ids.push(project.checkpoint.clone());
        } else {
            partial = true;
        }
    }
    if ids.is_empty() {
        return Err(selection::unavailable());
    }
    if let Some(links) = links {
        for link in &links.links {
            if let Some(target) = &link.target
                && (link.source.repository_id == *repository || target.repository_id == *repository)
            {
                let entry = json!({
                    "source": link.source.repository_id, "target": target.repository_id, "kind": link.kind,
                }).to_string();
                if !append(&mut text, &entry) {
                    partial = true;
                    break;
                }
            }
        }
        partial |= !links.gaps.is_empty();
    } else {
        partial = true;
    }
    Ok((text, ids, partial))
}

fn append(text: &mut String, entry: &str) -> bool {
    if SYSTEM.len() + text.len() + entry.len() + 1 > MAX_PROMPT_BYTES {
        return false;
    }
    text.push_str(entry);
    text.push('\n');
    true
}

fn normalized<T: serde::de::DeserializeOwned>(answer: &str) -> Option<T> {
    if answer.len() > MAX_OUTPUT_BYTES {
        return None;
    }
    let answer = answer.trim();
    // Some retained providers enclose an otherwise valid JSON answer in a single fence.
    let fence = "\u{60}\u{60}\u{60}";
    let text = answer
        .strip_prefix(fence)
        .and_then(|tail| tail.strip_suffix(fence))
        .map(|body| body.strip_prefix("json").unwrap_or(body).trim())
        .unwrap_or(answer);
    serde_json::from_str(text).ok()
}

pub(super) fn decode_project(answer: &str, subject: &Subject) -> Option<ProjectAnalysis> {
    let mut project: ProjectAnalysis = normalized(answer)?;
    if project
        .project_name
        .as_ref()
        .is_some_and(|name| name != &subject.name)
        || [
            project.endpoints.len(),
            project.services.len(),
            project.external_dependencies.len(),
            project.database_tables.len(),
            project.nodes.len(),
        ]
        .into_iter()
        .any(|len| len > MAX_FIELDS)
    {
        return None;
    }
    project.project_name = Some(subject.name.clone());
    clean(&mut project.summary)?;
    for endpoint in &mut project.endpoints {
        clean(&mut endpoint.route)?;
        clean(&mut endpoint.http_method)?;
        clean(&mut endpoint.description)?;
        clean_optional(&mut endpoint.request_model)?;
        clean_optional(&mut endpoint.response_model)?;
    }
    for service in &mut project.services {
        clean(&mut service.name)?;
        clean(&mut service.description)?;
        clean(&mut service.lifetime)?;
        clean_optional(&mut service.interface_name)?;
    }
    for description in project
        .external_dependencies
        .iter_mut()
        .chain(&mut project.database_tables)
    {
        clean(description)?;
    }
    let mut seen = BTreeSet::new();
    for node in &mut project.nodes {
        if !subject
            .evidence
            .iter()
            .any(|proof| proof.node_id == node.node_id.as_str())
            || !seen.insert(node.node_id.as_str().to_owned())
        {
            return None;
        }
        clean(&mut node.description)?;
    }
    Some(project)
}
pub(super) fn decode_synthesis(answer: &str) -> Option<Synthesis> {
    let mut summary: Synthesis = normalized(answer)?;
    clean(&mut summary.summary)?;
    Some(summary)
}

fn clean_optional(text: &mut Option<String>) -> Option<()> {
    if let Some(text) = text {
        clean(text)?;
    }
    Some(())
}
fn clean(text: &mut String) -> Option<()> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_DESCRIPTION_BYTES || trimmed.contains('\0') {
        return None;
    }
    *text = Redactor::new().redact_text(trimmed);
    Some(())
}
