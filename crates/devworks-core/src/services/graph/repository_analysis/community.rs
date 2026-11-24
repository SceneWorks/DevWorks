//! Weighted communities and unweighted betweenness for the exact cross-repository relations.
use super::super::budget::Budget;
use std::collections::{BTreeMap, VecDeque};

const MAX_ITERATIONS: usize = 100;

#[derive(Clone, Copy)]
pub(super) struct Relation<'a> {
    pub source: usize,
    pub target: usize,
    pub kind: &'a str,
}

pub(super) struct Communities {
    /// Isolated repositories belong in the graph projection without a cluster.
    pub assignments: Vec<Option<usize>>,
    pub centrality: Vec<f64>,
    pub modularity: f64,
}

type Adjacency = Vec<Vec<(usize, f64)>>;

pub(super) fn detect(
    repositories: usize,
    links: &[Relation<'_>],
    budget: &mut Budget,
) -> Option<Communities> {
    let adjacency = weights(repositories, links, budget)?;
    let (assignments, modularity) = partition(&adjacency, budget)?;
    let centrality = betweenness(&adjacency, budget)?;
    Some(Communities {
        assignments,
        centrality,
        modularity,
    })
}

/// Mirrors the frozen BuildWeightedAdjacency. Each unordered pair contributes
/// once to the graph, even when several types link it in either direction.
fn weights(repositories: usize, links: &[Relation<'_>], budget: &mut Budget) -> Option<Adjacency> {
    let mut directed = BTreeMap::<(usize, usize), f64>::new();
    for link in links {
        if !budget.visit() {
            return None;
        }
        if link.source == link.target {
            continue;
        }
        if link.source >= repositories || link.target >= repositories {
            return None;
        }
        let weight = match link.kind {
            "PUBLISHES" | "CONSUMES" => 3.0,
            "HTTP_CALLS" => 2.0,
            _ => 1.0,
        };
        *directed.entry((link.source, link.target)).or_default() += weight;
    }

    let mut adjacency = vec![Vec::new(); repositories];
    for (&(source, target), &forward) in &directed {
        let reverse = directed.get(&(target, source)).copied().unwrap_or_default();
        if source > target && reverse > 0.0 {
            continue;
        }
        let weight = if reverse > 0.0 {
            (forward + reverse) * 1.5
        } else {
            forward
        };
        adjacency[source].push((target, weight));
        adjacency[target].push((source, weight));
    }
    for neighbors in &mut adjacency {
        neighbors.sort_by_key(|&(index, _)| index);
    }
    Some(adjacency)
}

/// The frozen Louvain pass keeps the first neighbor on equal positive gains and
/// uses the symmetric adjacency's existing edges for the final modularity sum.
fn partition(adjacency: &Adjacency, budget: &mut Budget) -> Option<(Vec<Option<usize>>, f64)> {
    let strengths: Vec<f64> = adjacency
        .iter()
        .map(|neighbors| neighbors.iter().map(|(_, weight)| weight).sum())
        .collect();
    let total_weight = strengths.iter().sum::<f64>() / 2.0;
    if total_weight == 0.0 {
        return Some((vec![None; adjacency.len()], 0.0));
    }
    let mut communities: Vec<_> = (0..adjacency.len()).collect();
    let mut totals = strengths.clone();
    let normalization = 2.0 * total_weight;
    let gain_denominator = normalization * total_weight;
    let mut weights_by_community = vec![0.0; adjacency.len()];
    let mut touched = Vec::with_capacity(adjacency.len());

    for _ in 0..MAX_ITERATIONS {
        let mut moved = false;
        for (index, neighbors) in adjacency.iter().enumerate() {
            if !budget.visit() {
                return None;
            }
            if neighbors.is_empty() {
                continue;
            }
            let current = communities[index];
            let strength = strengths[index];
            for &(other, weight) in neighbors {
                let community = communities[other];
                if weights_by_community[community] == 0.0 {
                    touched.push(community);
                }
                weights_by_community[community] += weight;
            }
            totals[current] -= strength;
            let mut best = current;
            let mut best_gain = 0.0;
            for community in touched.drain(..) {
                let weight = std::mem::take(&mut weights_by_community[community]);
                let gain = weight / total_weight - totals[community] * strength / gain_denominator;
                if gain > best_gain {
                    best = community;
                    best_gain = gain;
                }
            }
            totals[best] += strength;
            communities[index] = best;
            moved |= best != current;
        }
        if !moved {
            break;
        }
    }

    let mut modularity = 0.0;
    let mut ordinals = BTreeMap::new();
    let mut assignments = Vec::with_capacity(adjacency.len());
    for (index, neighbors) in adjacency.iter().enumerate() {
        if !budget.visit() {
            return None;
        }
        if neighbors.is_empty() {
            assignments.push(None);
            continue;
        }
        let community = communities[index];
        for &(other, weight) in neighbors {
            if communities[other] == community {
                modularity += weight - strengths[index] * strengths[other] / normalization;
            }
        }
        let next = ordinals.len();
        assignments.push(Some(*ordinals.entry(community).or_insert(next)));
    }
    Some((assignments, modularity / normalization))
}

/// Unweighted Brandes, with the source's normalization over linked repositories.
fn betweenness(adjacency: &Adjacency, budget: &mut Budget) -> Option<Vec<f64>> {
    let linked = adjacency
        .iter()
        .filter(|neighbors| !neighbors.is_empty())
        .count();
    let mut centrality = vec![0.0; adjacency.len()];
    if linked <= 2 {
        return Some(centrality);
    }

    let mut queue = VecDeque::new();
    let mut stack = Vec::with_capacity(adjacency.len());
    let mut predecessors = vec![Vec::new(); adjacency.len()];
    let mut paths = vec![0.0; adjacency.len()];
    let mut distance = vec![usize::MAX; adjacency.len()];
    let mut dependencies = vec![0.0; adjacency.len()];
    for (source, neighbors) in adjacency.iter().enumerate() {
        if neighbors.is_empty() {
            continue;
        }
        paths[source] = 1.0;
        distance[source] = 0;
        queue.push_back(source);
        while let Some(vertex) = queue.pop_front() {
            stack.push(vertex);
            for &(other, _) in &adjacency[vertex] {
                if !budget.visit() {
                    return None;
                }
                if distance[other] == usize::MAX {
                    distance[other] = distance[vertex] + 1;
                    queue.push_back(other);
                }
                if distance[other] == distance[vertex] + 1 {
                    paths[other] += paths[vertex];
                    predecessors[other].push(vertex);
                }
            }
        }
        while let Some(vertex) = stack.pop() {
            for predecessor in predecessors[vertex].drain(..) {
                dependencies[predecessor] +=
                    paths[predecessor] / paths[vertex] * (1.0 + dependencies[vertex]);
            }
            if vertex != source {
                centrality[vertex] += dependencies[vertex];
            }
            paths[vertex] = 0.0;
            distance[vertex] = usize::MAX;
            dependencies[vertex] = 0.0;
        }
    }
    let normalization = ((linked - 1) * (linked - 2)) as f64;
    for score in &mut centrality {
        *score /= normalization;
    }
    Some(centrality)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::data::QueryLimits;

    fn budget(max_visited: u32) -> Budget {
        Budget::new(&QueryLimits {
            max_results: 100,
            max_visited,
            deadline_ms: 30_000,
        })
    }

    #[test]
    fn weighted_pair_accumulates_distinct_types_and_applies_the_bidirectional_bonus_once() {
        let links = [
            Relation {
                source: 0,
                target: 1,
                kind: "PUBLISHES",
            },
            Relation {
                source: 0,
                target: 1,
                kind: "HTTP_CALLS",
            },
            Relation {
                source: 1,
                target: 0,
                kind: "CONSUMES",
            },
            Relation {
                source: 2,
                target: 1,
                kind: "REFERENCES_PACKAGE",
            },
        ];
        let adjacency = weights(4, &links, &mut budget(100)).unwrap();
        assert_eq!(
            adjacency,
            vec![
                vec![(1, 12.0)],
                vec![(0, 12.0), (2, 1.0)],
                vec![(1, 1.0)],
                vec![],
            ]
        );
    }

    #[test]
    fn centrality_uses_only_linked_repositories_and_does_not_assign_isolated_nodes() {
        let links = [
            Relation {
                source: 0,
                target: 1,
                kind: "HTTP_CALLS",
            },
            Relation {
                source: 1,
                target: 2,
                kind: "HTTP_CALLS",
            },
        ];
        let report = detect(4, &links, &mut budget(1000)).unwrap();
        assert_eq!(report.assignments, vec![Some(0), Some(0), Some(0), None]);
        assert_eq!(report.centrality, vec![0.0, 1.0, 0.0, 0.0]);
        assert!((report.modularity - 0.5).abs() < 1e-9);
    }

    #[test]
    fn exhausted_traversal_does_not_publish_an_exact_partition() {
        let links = [
            Relation {
                source: 0,
                target: 1,
                kind: "HTTP_CALLS",
            },
            Relation {
                source: 1,
                target: 2,
                kind: "CONSUMES",
            },
        ];
        let mut limited = budget(4);
        assert!(detect(3, &links, &mut limited).is_none());
        assert_eq!(limited.exhausted, Some("graph_visited_limit"));
    }
}
