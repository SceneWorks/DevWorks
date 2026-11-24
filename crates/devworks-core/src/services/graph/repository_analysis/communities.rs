//! Overview, details and graph use one immutable weighted community result.
use super::super::{budget::Budget, model};
use super::{
    community,
    links::{self, Link, LinkReport},
    selection::{self, Selection},
};
use crate::ports::storage::ReadSnapshot;
use crate::protocol::Id;
use crate::protocol::records::{
    AnalysisClusterConnection, AnalysisClusterDetails, AnalysisClusterMember, ClusterRecord,
};
use crate::protocol::support::{BoundedList, BoundedText};
use crate::services::kernel::{Failure, counter, label};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct CommunityReport {
    pub computed_at: i64,
    pub modularity: Option<f64>,
    pub repositories: Vec<Member>,
    pub communities: Vec<Community>,
    pub links: Vec<Link>,
    pub gaps: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Member {
    pub repository: Id,
    pub view: Id,
    pub name: String,
    pub language: Option<String>,
    pub source_group: Option<String>,
    pub framework: Option<String>,
    pub foundational: Option<bool>,
    pub assigned: Option<usize>,
    pub centrality: f64,
    pub internal_edges: usize,
    pub external_edges: usize,
}

impl Member {
    pub(super) fn wire(&self, cluster_id: Option<Id>) -> Result<AnalysisClusterMember, Failure> {
        Ok(AnalysisClusterMember {
            repository_id: self.repository.clone(),
            view_id: self.view.clone(),
            name: label(&self.name, self.repository.as_str()),
            centrality: model::wire(json!(self.centrality))?,
            internal_edges: counter(self.internal_edges as i64),
            external_edges: counter(self.external_edges as i64),
            foundational: self.foundational,
            source_group: self.source_group.as_deref().map(|s| label(s, "repository")),
            language: self.language.as_deref().map(|s| label(s, "unknown")),
            framework: self.framework.as_deref().map(|s| label(s, "unknown")),
            cluster_id,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Community {
    pub id: Id,
    pub ordinal: usize,
    pub members: Vec<usize>,
    pub bridges: Vec<usize>,
    pub density: f64,
    pub internal_edges: usize,
    pub external_edges: usize,
    connections: Vec<Connection>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Connection {
    group: usize,
    count: usize,
    kinds: BTreeSet<String>,
}

impl CommunityReport {
    pub(super) fn cluster(&self, group: &Community) -> Result<ClusterRecord, Failure> {
        let first = group
            .members
            .first()
            .and_then(|index| self.repositories.get(*index))
            .ok_or_else(selection::unavailable)?;
        let members = group
            .members
            .iter()
            .map(|&index| {
                self.repositories
                    .get(index)
                    .ok_or_else(selection::unavailable)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let connections = group
            .connections
            .iter()
            .map(|tag| {
                let target = self
                    .communities
                    .get(tag.group)
                    .ok_or_else(selection::unavailable)?;
                Ok(AnalysisClusterConnection {
                    cluster_id: target.id.clone(),
                    edge_count: counter(tag.count as i64),
                    types: BoundedList(
                        tag.kinds
                            .iter()
                            .map(|kind| label(kind, "relationship"))
                            .collect(),
                    ),
                })
            })
            .collect::<Result<Vec<_>, Failure>>()?;
        Ok(ClusterRecord {
            id: group.id.clone(),
            view_id: first.view.clone(),
            name: BoundedText(format!("Community {}", group.ordinal + 1)),
            member_node_ids: BoundedList(members.iter().map(|m| m.repository.clone()).collect()),
            score: model::wire(json!(self.modularity.ok_or_else(selection::unavailable)?))?,
            details: Some(AnalysisClusterDetails {
                legacy_ordinal: counter(group.ordinal as i64),
                internal_edges: counter(group.internal_edges as i64),
                external_edges: counter(group.external_edges as i64),
                density: model::wire(json!(group.density))?,
                members: BoundedList(
                    members
                        .iter()
                        .map(|member| member.wire(Some(group.id.clone())))
                        .collect::<Result<_, _>>()?,
                ),
                bridges: BoundedList(
                    group
                        .bridges
                        .iter()
                        .map(|&index| {
                            self.repositories
                                .get(index)
                                .map(|member| member.repository.clone())
                                .ok_or_else(selection::unavailable)
                        })
                        .collect::<Result<_, _>>()?,
                ),
                connections: BoundedList(connections),
                next_cursor: None,
                has_more: None,
            }),
        })
    }
}

pub(super) fn build(
    read: &dyn ReadSnapshot,
    selected: &Selection,
    links: LinkReport,
    computed_at: i64,
    budget: &mut Budget,
) -> Result<CommunityReport, Failure> {
    let mut repositories = Vec::new();
    let scope = selected.graph_scope();
    let mut metadata = BTreeMap::new();
    super::super::scan::nodes(
        read,
        &scope,
        &crate::ports::storage::GraphNodeFilter {
            kinds: vec!["Repository".into()],
            ..Default::default()
        },
        budget,
        |node, _| {
            metadata
                .entry(node.repository_id.as_str().to_owned())
                .or_insert_with(|| node.node.payload["properties"].clone());
            Ok(true)
        },
    )?;
    for binding in &selected.views {
        let repo = read
            .projects()
            .repository(&binding.repository_id)?
            .ok_or_else(selection::unavailable)?;
        let source = read.indexing().repository_source(&repo.id)?;
        let language = (repo.languages.len() == 1).then(|| repo.languages[0].clone());
        let source_group = source.as_ref().and_then(|source| {
            let root = source.section_root.as_ref()?;
            let relative = std::path::Path::new(&source.location)
                .strip_prefix(root)
                .ok()?;
            relative
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .map(|path| path.to_string_lossy().replace('\\', "/"))
        });
        let recorded = metadata
            .get(repo.id.as_str())
            .unwrap_or(&serde_json::Value::Null);
        let source_group = recorded["sourceGroup"]
            .as_str()
            .map(str::to_owned)
            .or(source_group);
        repositories.push(Member {
            repository: repo.id,
            view: binding.view_id.clone(),
            name: repo.name,
            language,
            source_group,
            framework: recorded["framework"].as_str().map(str::to_owned),
            foundational: recorded
                .get("foundational")
                .or_else(|| recorded.get("isFoundational"))
                .and_then(serde_json::Value::as_bool),
            assigned: None,
            centrality: 0.0,
            internal_edges: 0,
            external_edges: 0,
        });
    }
    let by_repository: BTreeMap<_, _> = repositories
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.repository.as_str(), index))
        .collect();
    let relations = links
        .links
        .iter()
        .filter_map(|link| links::relation(link, &by_repository))
        .collect::<Vec<_>>();
    let detected = if links.gaps.iter().any(|gap| gap.starts_with("graph_")) {
        None
    } else {
        community::detect(repositories.len(), &relations, budget)
    };
    let mut gaps = links.gaps;
    let Some(detected) = detected else {
        gaps.push(
            budget
                .exhausted
                .unwrap_or("CommunityLinkCoverageIncomplete")
                .into(),
        );
        gaps.sort();
        gaps.dedup();
        return Ok(CommunityReport {
            computed_at,
            modularity: None,
            repositories,
            communities: Vec::new(),
            links: links.links,
            gaps,
        });
    };
    let mut groups = Vec::new();
    for (index, member) in repositories.iter_mut().enumerate() {
        member.assigned = detected.assignments[index];
        member.centrality = detected.centrality[index];
        if let Some(group) = member.assigned {
            while groups.len() <= group {
                groups.push(Vec::new());
            }
            groups[group].push(index);
        }
    }
    type Connected = BTreeMap<usize, (usize, BTreeSet<String>)>;
    let mut counts = vec![(0usize, 0usize, Connected::new()); groups.len()];
    for relation in &relations {
        let (source, target) = (relation.source, relation.target);
        if let (Some(from), Some(to)) =
            (repositories[source].assigned, repositories[target].assigned)
        {
            if from == to {
                counts[from].0 += 1;
                repositories[source].internal_edges += 1;
                repositories[target].internal_edges += 1;
            } else {
                for (member, group, other) in [(source, from, to), (target, to, from)] {
                    repositories[member].external_edges += 1;
                    counts[group].1 += 1;
                    let connection = counts[group].2.entry(other).or_default();
                    connection.0 += 1;
                    connection.1.insert(relation.kind.to_owned());
                }
            }
        }
    }
    let communities = groups
        .into_iter()
        .zip(counts)
        .enumerate()
        .map(
            |(ordinal, (members, (internal_edges, external_edges, connection_counts)))| {
                let membership: Vec<&Id> = members
                    .iter()
                    .map(|&index| &repositories[index].repository)
                    .collect();
                let digest = selection::digest(
                    &json!({"binding": selected.binding_key, "members": membership}),
                );
                let id = Id::new(format!(
                    "community_{}_{digest}",
                    &selected.binding_key[..16]
                ))
                .map_err(|_| selection::unavailable())?;
                let pairs = members.len() * members.len().saturating_sub(1) / 2;
                let mut bridges = members
                    .iter()
                    .copied()
                    .filter(|&index| repositories[index].centrality > 0.01)
                    .collect::<Vec<_>>();
                bridges.sort_by(|a, b| {
                    repositories[*b]
                        .centrality
                        .total_cmp(&repositories[*a].centrality)
                        .then_with(|| a.cmp(b))
                });
                bridges.truncate(3);
                let mut connections: Vec<_> = connection_counts
                    .into_iter()
                    .map(|(group, (count, kinds))| Connection {
                        group,
                        count,
                        kinds,
                    })
                    .collect();
                connections
                    .sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.group.cmp(&b.group)));
                Ok(Community {
                    id,
                    ordinal,
                    members,
                    bridges,
                    density: if pairs == 0 {
                        0.0
                    } else {
                        internal_edges as f64 / pairs as f64
                    },
                    internal_edges,
                    external_edges,
                    connections,
                })
            },
        )
        .collect::<Result<Vec<_>, Failure>>()?;
    Ok(CommunityReport {
        computed_at,
        modularity: Some(detected.modularity),
        repositories,
        communities,
        links: links.links,
        gaps,
    })
}
