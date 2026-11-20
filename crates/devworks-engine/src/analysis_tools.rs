//! Retained analysis tools translate arguments into the same authorized native reads as the UI.
use crate::{tool_turn::frame_as, turn_engine::TurnEngine};
use devworks_core::{
    context::InvocationContext,
    fingerprint::sha256_hex,
    ports::storage::{ReadOptions, StorageError},
    protocol::{
        Id, Revision,
        data::{DataScope, ViewSelection},
        provider::{QualifiedId, SchemaRef, ToolSchema},
        records::{AnalysisClusterPageResult, RepositoryRecord},
        results::{DomainQueryReply, OperationResult},
    },
    providers::ProviderError,
    registry::{self, Mode},
};
use serde_json::{Value, json};

pub(crate) const NAMESPACE: &str = "devworks-analysis/";
const READ_LIMIT: usize = 20;

#[derive(Clone, Copy)]
pub(crate) struct Alias {
    name: &'static str,
    operations: &'static [&'static str],
    fields: &'static [&'static str],
    required: &'static [&'static str],
}

const ALIASES: &[Alias] = &[
    Alias {
        name: "get_repository_health",
        operations: &[
            "analysis.health",
            "repositories.search",
            "code.graph_scope_status",
        ],
        fields: &["repository", "topHotspots"],
        required: &["repository"],
    },
    Alias {
        name: "get_fleet_health",
        operations: &["analysis.fleet_health", "repositories.list"],
        fields: &["maxHealth", "limit"],
        required: &[],
    },
    Alias {
        name: "find_archival_candidates",
        operations: &["analysis.archival_candidates", "repositories.list"],
        fields: &[],
        required: &[],
    },
    Alias {
        name: "get_service_clusters",
        operations: &["analysis.clusters", "repositories.list"],
        fields: &[],
        required: &[],
    },
    Alias {
        name: "get_cluster_detail",
        operations: &[
            "analysis.clusters",
            "analysis.cluster_detail",
            "repositories.list",
        ],
        fields: &["clusterId"],
        required: &["clusterId"],
    },
    Alias {
        name: "file_hotspot",
        operations: &[
            "analysis.file_hotspot",
            "repositories.search",
            "code.graph_scope_status",
        ],
        fields: &["repository", "path"],
        required: &["repository", "path"],
    },
    Alias {
        name: "repository_report",
        operations: &[
            "repositories.search",
            "repositories.list",
            "repositories.get",
            "analysis.health",
            "analysis.fleet_health",
            "analysis.archival_candidates",
            "code.overview",
            "code.architecture",
            "code.graph_scope_status",
            "analysis.cross_repo_links",
        ],
        fields: &[
            "operation",
            "repository",
            "search",
            "group",
            "page",
            "pageSize",
            "topHotspots",
            "maxHealth",
            "limit",
        ],
        required: &[],
    },
    Alias {
        name: "graph_cluster",
        operations: &[
            "analysis.clusters",
            "analysis.cluster_detail",
            "repositories.list",
        ],
        fields: &["operation", "clusterId"],
        required: &[],
    },
];

pub(crate) fn selected(builtins: &[ToolSchema]) -> impl Iterator<Item = Alias> + '_ {
    ALIASES.iter().copied().filter(|alias| {
        alias.operations.iter().all(|op| {
            builtins.iter().any(|tool| {
                crate::providers::schemas::builtin_operation(tool.capability_id.as_str())
                    .is_some_and(|(name, mode)| name == *op && mode == Some(Mode::Query))
            })
        })
    })
}

impl Alias {
    pub(crate) fn document(self) -> Value {
        let mut properties = serde_json::Map::new();
        for &field in self.fields {
            let value = match field {
                "operation" if self.name == "repository_report" => json!({
                    "type": "string", "enum": ["search","summary","architecture","health","fleet_health","archival_candidates"]
                }),
                "operation" => json!({"type": "string", "enum": ["list", "detail"]}),
                "clusterId" => json!({"oneOf": [
                    {"type": "integer", "minimum": 0},
                    {"type": "string", "minLength": 1, "maxLength": 128}
                ]}),
                "maxHealth" => json!({"type": "number", "minimum": 0, "maximum": 10}),
                "limit" | "pageSize" => json!({"type": "integer", "minimum": 1, "maximum": 128}),
                "page" => json!({"type": "integer", "minimum": 1, "maximum": 20}),
                "topHotspots" => json!({"type": "integer", "minimum": 1, "maximum": 100}),
                "path" => json!({"type": "string", "minLength": 1, "maxLength": 4096}),
                _ => json!({"type": "string", "maxLength": 256}),
            };
            properties.insert(field.to_owned(), value);
        }
        json!({
            "title": self.name,
            "description": "Read repository analysis for the current authorized source views.",
            "type": "object", "properties": properties, "required": self.required,
            "additionalProperties": false,
        })
    }

    pub(crate) fn tool(self) -> Result<ToolSchema, ProviderError> {
        let document = self.document();
        let bytes = devworks_core::jcs::canonicalize(&document)
            .map_err(|_| ProviderError::MaterialUnavailable)?;
        let name = format!("{NAMESPACE}{}", self.name);
        Ok(ToolSchema {
            wire_name: self.name.into(),
            capability_id: QualifiedId::try_from(name.clone())
                .map_err(ProviderError::InvalidInput)?,
            input_schema: SchemaRef {
                schema_id: QualifiedId::try_from(format!("{name}.input"))
                    .map_err(ProviderError::InvalidInput)?,
                revision: Revision::new(1).expect("positive alias revision"),
                sha256: sha256_hex(bytes.as_bytes())
                    .try_into()
                    .map_err(ProviderError::InvalidInput)?,
            },
        })
    }

    pub(crate) fn from_capability(id: &str) -> Option<Self> {
        let name = id.strip_prefix(NAMESPACE)?;
        ALIASES.iter().copied().find(|alias| alias.name == name)
    }

    pub(crate) fn from_schema(id: &str) -> Option<Self> {
        Self::from_capability(id.strip_suffix(".input")?)
    }

    pub(crate) fn dispatch(
        self,
        engine: &TurnEngine,
        ctx: &InvocationContext,
        options: &ReadOptions,
        args: &Value,
    ) -> Result<Value, StorageError> {
        let reader = Reader {
            engine,
            ctx,
            options,
        };
        if args.as_object().is_none_or(|fields| {
            fields
                .keys()
                .any(|field| !self.fields.contains(&field.as_str()))
                || self
                    .required
                    .iter()
                    .any(|field| !fields.contains_key(*field))
        }) {
            return Err(invalid());
        }
        let mode = match self.name {
            "get_repository_health" => "health",
            "get_fleet_health" => "fleet_health",
            "find_archival_candidates" => "archival_candidates",
            "get_service_clusters" => "clusters",
            "get_cluster_detail" => "detail",
            "file_hotspot" => "file_hotspot",
            "repository_report" => text(args, "operation")?.unwrap_or("search"),
            "graph_cluster" => text(args, "operation")?.unwrap_or("list"),
            _ => return Err(invalid()),
        };
        if (self.name == "graph_cluster" && !matches!(mode, "list" | "detail"))
            || (self.name == "repository_report"
                && !matches!(
                    mode,
                    "search"
                        | "summary"
                        | "architecture"
                        | "health"
                        | "fleet_health"
                        | "archival_candidates"
                ))
        {
            return Err(invalid());
        }
        let result = match mode {
            "search" => return reader.search(args),
            "summary" | "architecture" | "health" | "file_hotspot" => {
                let repository = text(args, "repository")?
                    .filter(|r| !r.is_empty())
                    .ok_or_else(invalid)?;
                let repo = reader.named(repository)?;
                let captured = reader.views(std::slice::from_ref(&repo.id))?;
                match mode {
                    "summary" => {
                        let summary =
                            reader.query("repositories.get", json!({"repositoryId": repo.id}))?;
                        let health =
                            reader.analysis("analysis.health", &captured, 25, json!({}))?;
                        return Ok(json!({"repository": summary, "analysis": health}));
                    }
                    "architecture" => {
                        let graph = reader.graph(
                            "code.architecture",
                            json!({"kind": "Overview", "repositoryId": repo.id}),
                            Some(&captured),
                        )?;
                        let links = reader.analysis(
                            "analysis.cross_repo_links",
                            &reader.fleet()?,
                            50,
                            json!({"filters": [format!("repository:{}", repo.id.as_str())]}),
                        )?;
                        return Ok(json!({"architecture": graph, "links": links}));
                    }
                    "health" => reader.analysis(
                        "analysis.health",
                        &captured,
                        25,
                        json!({"topHotspots": number(args, "topHotspots", 10, 100)?}),
                    )?,
                    _ => reader.analysis(
                        "analysis.file_hotspot",
                        &captured,
                        1,
                        json!({"path": text(args, "path")?.ok_or_else(invalid)?}),
                    )?,
                }
            }
            "fleet_health" => {
                let threshold = match args.get("maxHealth") {
                    None => json!({}),
                    Some(v)
                        if v.as_f64()
                            .is_some_and(|score| (0.0..=10.0).contains(&score)) =>
                    {
                        json!({"maxHealth": v})
                    }
                    _ => return Err(invalid()),
                };
                reader.analysis(
                    "analysis.fleet_health",
                    &reader.fleet()?,
                    number(args, "limit", 50, 128)?,
                    threshold,
                )?
            }
            "archival_candidates" => reader.analysis(
                "analysis.archival_candidates",
                &reader.fleet()?,
                50,
                json!({}),
            )?,
            "clusters" | "list" => {
                reader.analysis("analysis.clusters", &reader.fleet()?, 50, json!({}))?
            }
            "detail" => {
                let views = reader.fleet()?;
                let id = args.get("clusterId").ok_or_else(invalid)?;
                let mut selector = json!({"clusterId": id});
                if let Some(ordinal) = id.as_i64().filter(|n| *n >= 0) {
                    // Freeze the view composition used to translate a legacy ordinal.
                    let OperationResult::AnalysisClusters {
                        value: AnalysisClusterPageResult::Current(page),
                    } = reader.analysis("analysis.clusters", &views, 128, json!({}))?
                    else {
                        return Err(invalid());
                    };
                    let cluster = page
                        .items
                        .0
                        .iter()
                        .find(|item| {
                            item.cluster
                                .details
                                .as_ref()
                                .is_some_and(|detail| detail.legacy_ordinal.get() == ordinal)
                        })
                        .ok_or_else(unavailable)?;
                    selector = json!({"clusterId": cluster.cluster.id, "legacyOrdinal": ordinal.to_string()});
                } else if id.as_str().is_none() {
                    return Err(invalid());
                }
                reader.analysis("analysis.cluster_detail", &views, 50, selector)?
            }
            _ => return Err(invalid()),
        };
        serde_json::to_value(result).map_err(|_| invalid())
    }
}

struct Reader<'a> {
    engine: &'a TurnEngine,
    ctx: &'a InvocationContext,
    options: &'a ReadOptions,
}

impl Reader<'_> {
    fn scope(&self) -> DataScope {
        self.ctx
            .project_id()
            .map_or(DataScope::Workspace, |id| DataScope::Project {
                project_id: id.clone(),
            })
    }

    fn query(&self, operation: &str, input: Value) -> Result<OperationResult, StorageError> {
        let descriptor = registry::find(operation).ok_or_else(unavailable)?;
        let revision = *descriptor.revisions.last().ok_or_else(unavailable)?;
        let bytes = frame_as(
            "retained-analysis-tool".into(),
            descriptor,
            Mode::Query,
            revision,
            input,
        )
        .map_err(|_| invalid())?;
        match self
            .engine
            .query_with(self.ctx, operation, &bytes, self.options)
        {
            Ok(DomainQueryReply::Result { result, .. }) => Ok(result),
            Ok(DomainQueryReply::Rejected { fault, .. }) => Err(StorageError::InvalidRecord(
                format!("domain_{:?}", fault.code),
            )),
            Err(_) => Err(unavailable()),
        }
    }

    fn graph(
        &self,
        operation: &str,
        query: Value,
        view: Option<&ViewSelection>,
    ) -> Result<OperationResult, StorageError> {
        let mut body = json!({
            "scope": self.scope(), "query": query,
            "limits": {"maxResults": 50, "maxVisited": 20000, "deadlineMs": 10000}
        });
        if let Some(view) = view {
            body["view"] = json!(view);
        } else {
            body["current"] = json!(true);
        }
        self.query(operation, body)
    }

    fn analysis(
        &self,
        operation: &str,
        views: &ViewSelection,
        limit: u64,
        selectors: Value,
    ) -> Result<OperationResult, StorageError> {
        let mut input =
            json!({"scope": self.scope(), "views": views, "page": {"limit": limit}, "filters": []});
        input
            .as_object_mut()
            .expect("object")
            .extend(selectors.as_object().ok_or_else(invalid)?.clone());
        self.query(operation, input)
    }

    fn repositories(&self, term: Option<&str>) -> Result<Vec<RepositoryRecord>, StorageError> {
        let mut rows = Vec::new();
        let mut cursor = None;
        for _ in 0..READ_LIMIT {
            let mut body = json!({"scope": self.scope(), "page": {"limit": 128}});
            if let Some(cursor) = cursor {
                body["page"]["cursor"] = cursor;
            }
            let result = if let Some(term) = term {
                body["query"] = json!(term);
                body["languageIds"] = json!([]);
                self.query("repositories.search", body)?
            } else {
                self.query("repositories.list", body)?
            };
            let (OperationResult::RepositoriesList { value }
            | OperationResult::RepositoriesSearch { value }) = result
            else {
                return Err(invalid());
            };
            rows.extend(value.items.0);
            cursor = value.next_cursor.map(|value| json!(value));
            if cursor.is_none() {
                return Ok(rows);
            }
        }
        Err(StorageError::InvalidRecord(
            "Repository page limit reached".into(),
        ))
    }

    fn named(&self, name: &str) -> Result<RepositoryRecord, StorageError> {
        let mut candidates = self
            .repositories(Some(name))?
            .into_iter()
            .filter(|repo| repo.name.0.eq_ignore_ascii_case(name) || repo.id.as_str() == name);
        let record = candidates.next().ok_or_else(unavailable)?;
        if candidates.next().is_some() {
            return Err(invalid());
        }
        Ok(record)
    }

    fn search(&self, args: &Value) -> Result<Value, StorageError> {
        let search = text(args, "search")?.filter(|text| !text.is_empty());
        let group = text(args, "group")?.filter(|text| !text.is_empty());
        let term = search
            .and_then(|pattern| pattern.split(['*', '?']).max_by_key(|part| part.len()))
            .filter(|s| !s.is_empty());
        let mut rows = self.repositories(term)?;
        if let Some(pattern) = search {
            rows.retain(|repo| wildcard(pattern, &repo.name.0));
        }
        if let Some(group) = group {
            let ids = self.repository_group(group)?;
            rows.retain(|row| ids.contains(row.id.as_str()));
        }
        let size = number(args, "pageSize", 25, 128)? as usize;
        let page = number(args, "page", 1, READ_LIMIT as u64)? as usize;
        let total = rows.len();
        let rows = rows
            .into_iter()
            .skip((page - 1) * size)
            .take(size)
            .collect::<Vec<_>>();
        Ok(json!({"items": rows, "page": page, "pageSize": size, "total": total}))
    }

    fn repository_group(
        &self,
        group: &str,
    ) -> Result<std::collections::BTreeSet<String>, StorageError> {
        use devworks_core::protocol::data::GraphResult;
        let mut ids = std::collections::BTreeSet::new();
        let mut pin = None;
        let mut cursor = None;
        for _ in 0..READ_LIMIT {
            let mut input = json!({
                "scope": self.scope(), "query": {"kind":"Overview"},
                "limits": {"maxResults": 50, "maxVisited": 20000, "deadlineMs": 10000}
            });
            if let Some(pin) = &pin {
                input["view"] = json!(pin);
            } else {
                input["current"] = json!(true);
            }
            if let Some(cursor) = cursor.take() {
                input["cursor"] = Value::String(cursor);
            }
            let OperationResult::CodeOverview { value: page } =
                self.query("code.overview", input)?
            else {
                return Err(invalid());
            };
            if pin.is_none() {
                pin = page
                    .view
                    .map(|set| ViewSelection::Pinned { pin_id: set.pin_id });
            }
            let GraphResult::RepositoryOverview { value } = page.result else {
                return Err(unavailable());
            };
            ids.extend(
                value
                    .repositories
                    .0
                    .into_iter()
                    .filter(|repo| {
                        repo.source_group
                            .as_ref()
                            .is_some_and(|value| value.0.eq_ignore_ascii_case(group))
                    })
                    .map(|repo| repo.repository_id.as_str().to_owned()),
            );
            cursor = page.next_cursor;
            if cursor.is_none() {
                if page
                    .scope_coverage
                    .is_some_and(|coverage| coverage.coverage.more_gaps)
                {
                    return Err(StorageError::InvalidRecord(
                        "Source group coverage is incomplete".into(),
                    ));
                }
                return Ok(ids);
            }
        }
        Err(StorageError::InvalidRecord(
            "Source group page limit reached".into(),
        ))
    }

    fn views(&self, ids: &[Id]) -> Result<ViewSelection, StorageError> {
        if let Some(owned) = self.ctx.worktree()
            && ids.iter().any(|id| id == owned.repository_id())
        {
            let result = self.graph(
                "code.graph_scope_status",
                json!({"kind": "ScopeStatus"}),
                None,
            )?;
            let OperationResult::CodeGraphScopeStatus { value } = result else {
                return Err(unavailable());
            };
            let view = value.view.ok_or_else(unavailable)?;
            if view.views.len() != ids.len() {
                return Err(unavailable());
            }
            return Ok(ViewSelection::Pinned {
                pin_id: view.pin_id,
            });
        }
        if ids.is_empty() || ids.len() > 256 {
            return Err(unavailable());
        }
        Ok(ViewSelection::Published {
            repository_ids: ids.to_vec(),
        })
    }

    fn fleet(&self) -> Result<ViewSelection, StorageError> {
        let ids = self
            .repositories(None)?
            .into_iter()
            .map(|repo| repo.id)
            .collect::<Vec<_>>();
        self.views(&ids)
    }
}

fn wildcard(pattern: &str, name: &str) -> bool {
    let pattern = pattern.to_lowercase();
    let name = name.to_lowercase();
    if !pattern.contains(['*', '?']) {
        return name.contains(&pattern);
    }
    let chars = name.chars().collect::<Vec<_>>();
    let mut positions = vec![false; chars.len() + 1];
    positions[0] = true;
    for token in pattern.chars() {
        let mut next = vec![false; positions.len()];
        if token == '*' {
            next[0] = positions[0];
            for index in 1..positions.len() {
                next[index] = positions[index] || next[index - 1];
            }
        } else {
            for (index, name) in chars.iter().enumerate() {
                next[index + 1] = positions[index] && (token == '?' || token == *name);
            }
        }
        positions = next;
    }
    positions[chars.len()]
}

fn text<'a>(args: &'a Value, field: &str) -> Result<Option<&'a str>, StorageError> {
    let maximum = if field == "path" { 4096 } else { 256 };
    args.get(field)
        .map(|v| {
            v.as_str()
                .filter(|value| value.len() <= maximum)
                .ok_or_else(invalid)
        })
        .transpose()
}
fn number(args: &Value, field: &str, default: u64, max: u64) -> Result<u64, StorageError> {
    args.get(field).map_or(Ok(default), |v| {
        v.as_u64()
            .filter(|n| (1..=max).contains(n))
            .ok_or_else(invalid)
    })
}
fn invalid() -> StorageError {
    StorageError::InvalidRecord("Invalid retained analysis tool arguments".into())
}
fn unavailable() -> StorageError {
    StorageError::InvalidRecord("ViewUnavailable".into())
}
