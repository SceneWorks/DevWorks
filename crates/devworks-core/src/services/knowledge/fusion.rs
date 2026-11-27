//! One ranking for `knowledge.search` with words (RAG-05, RAG-07): word matches, meaning
//! matches and graph matches fused by reciprocal rank.
//!
//! # Sources
//!
//! Each source reads candidate ids only, with the read scopes and every filter of the query in
//! the store's own statement, before any claim is materialized:
//!
//! * **Word matches** ([`KnowledgeRead::knowledge_text_candidates`]): every query word in the
//!   claim's text or its subject entity's, by BM25 over the read scopes.
//! * **Meaning matches** ([`KnowledgeRead::knowledge_vector_candidates`]), when the page has a
//!   query vector ([`super::vectors`]): the nearest claim vectors in the selected space with a
//!   positive cosine similarity, whether or not the claim also matches the words.
//! * **Graph matches** ([`KnowledgeRead::knowledge_graph_candidates`]): claims one hop from an
//!   entity the query matches, that is, claims naming as subject or object a live entity of the
//!   read scopes whose text has any query word, newest first. A claim about an entity the
//!   query names, with none of the other words, and a claim pointing at that entity, are found
//!   this way.
//!
//! # Fusion
//!
//! A claim's score is the sum, over the sources that list it, of `1 / (k + rank)`, its 1-based
//! rank in that source; only rank positions count, never raw scores, so BM25 and cosine scales
//! never mix. Each source lists at most [`CANDIDATES`] claims, and `k` ([`FUSION_K`]) is twice
//! that cap. With `k ≥ 2·cap`, a claim listed by more sources always scores above one listed by
//! fewer: `n / (k + cap) > (n − 1) / (k + 1)` holds for `n ≤ 3` exactly when
//! `k > (n − 1)·cap − n`, so at `k = 2·cap` the worst claim of three sources beats the best of
//! two, and the worst of two the best of one. Among claims of the same number of sources, the
//! better ranks win. Ties are broken by id descending.
//!
//! # Limits
//!
//! A source that has more candidates than [`CANDIDATES`] contributes its best [`CANDIDATES`],
//! and the page says its results are work-limited. The fused list holds at most three caps of
//! claims and is computed whole for every page; only the page's claims are materialized.
//!
//! # Paging
//!
//! A position is `f<fused score>|<id>` (the exact score as Rust's shortest round-tripping
//! decimal), and a page resumes strictly after it in the order (score descending, id
//! descending). The cursor binds everything the fused order depends on: the query, the page
//! limit, the revision of every read scope (any write there expires it), whether the page had
//! a query vector and in which space, and how many claims of the read scopes had vectors in it
//! when the query vector was prepared (a vector stored between two pages expires it), so a
//! claim appears exactly once across the pages while those hold and the provider returns the
//! same query vector for the same text (it is embedded again for each page). A position of
//! another order (`r…`, `v…` or a time) is `CursorExpired`.
use crate::ports::storage::{ClaimQuery, ClaimVectorQuery, KnowledgeRead, StorageError};
use crate::protocol::Id;
use crate::protocol::records::PageRequest;
use crate::services::cursor::{self, CursorBinding, Paged};
use std::collections::HashMap;

/// The most candidates each source (word, meaning, graph) contributes to one fused ranking.
pub const CANDIDATES: u16 = 200;
/// The reciprocal rank constant `k`: twice [`CANDIDATES`] (see the module notes).
pub const FUSION_K: f64 = 2.0 * CANDIDATES as f64;

/// The fused ranking of one search.
#[derive(Debug, Default)]
pub(super) struct Fused {
    /// `(score, claim)` in listing order: score descending, then id descending.
    pub ranking: Vec<(f64, Id)>,
    /// Whether a source had more candidates than [`CANDIDATES`].
    pub work_limited: bool,
}

/// One source's best [`CANDIDATES`], and whether it had more.
fn capped(mut ids: Vec<Id>) -> (Vec<Id>, bool) {
    let more = ids.len() > usize::from(CANDIDATES);
    ids.truncate(usize::from(CANDIDATES));
    (ids, more)
}

/// Fuses ranked lists by reciprocal rank (`k` [`FUSION_K`]). A list names each claim once.
fn fuse(lists: &[Vec<Id>]) -> Vec<(f64, Id)> {
    let mut scores: HashMap<Id, f64> = HashMap::new();
    // Lists are added in one fixed order, so a claim's sum is the same on every page.
    for list in lists {
        for (rank, id) in list.iter().enumerate() {
            *scores.entry(id.clone()).or_default() += 1.0 / (FUSION_K + (rank + 1) as f64);
        }
    }
    let mut ranking: Vec<(f64, Id)> = scores.into_iter().map(|(id, s)| (s, id)).collect();
    ranking.sort_by(|(a, x), (b, y)| b.total_cmp(a).then_with(|| y.as_str().cmp(x.as_str())));
    ranking
}

/// The fused ranking of `query` (which has words) over the word, meaning (with `near`) and graph
/// sources.
pub(super) fn rank(
    read: &dyn KnowledgeRead,
    query: &ClaimQuery,
    near: Option<&ClaimVectorQuery<'_>>,
) -> Result<Fused, StorageError> {
    // One more than the cap tells whether a source has more.
    let probe = CANDIDATES + 1;
    let (words, mut limited) = capped(read.knowledge_text_candidates(query, probe)?);
    let mut lists = vec![words];
    if let Some(near) = near {
        let (meaning, more) = capped(read.knowledge_vector_candidates(query, near, probe)?);
        limited |= more;
        lists.push(meaning);
    }
    let (graph, more) = capped(read.knowledge_graph_candidates(query, probe)?);
    lists.push(graph);
    Ok(Fused {
        ranking: fuse(&lists),
        work_limited: limited || more,
    })
}

/// The cursor binding parts of a fused search: the fusion's own constants and the meaning
/// source's inputs (the space and how many claims of the read scopes had vectors in it when the
/// query vector was prepared), or `lexical`.
pub(super) fn binding_parts(vectors: Option<(&Id, i64)>) -> Vec<String> {
    vec![
        format!("fused k={FUSION_K} cap={CANDIDATES}"),
        match vectors {
            Some((space, embedded)) => format!("vectors={}:{embedded}", space.as_str()),
            None => "lexical".into(),
        },
    ]
}

fn position((score, id): &(f64, Id)) -> String {
    format!("f{score}|{}", id.as_str())
}

/// Parses a fused position; any other order's position is `CursorExpired`.
fn parse(at: &str) -> Result<(f64, &str), StorageError> {
    let (key, id) = at.split_once('|').ok_or(StorageError::CursorExpired)?;
    let score = key
        .strip_prefix('f')
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|s| s.is_finite())
        .ok_or(StorageError::CursorExpired)?;
    Ok((score, id))
}

/// One page of a fused ranking, through the shared pager: the claims strictly after the
/// request's position, at most its limit. Every fused listing pages here.
pub(super) fn page(
    binding: &CursorBinding,
    request: &PageRequest,
    fused: &Fused,
) -> Result<Paged<Id>, StorageError> {
    let rows = cursor::page(
        binding,
        request,
        |after, n| {
            let start = match after.map(parse).transpose()? {
                None => 0,
                Some((s, id)) => fused
                    .ranking
                    .partition_point(|(t, x)| *t > s || (*t == s && x.as_str() >= id)),
            };
            Ok::<_, StorageError>(
                fused.ranking[start..]
                    .iter()
                    .take(usize::from(n))
                    .cloned()
                    .collect(),
            )
        },
        position,
    )?;
    rows.try_map(|(_, id)| Ok(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(names: &[&str]) -> Vec<Id> {
        names.iter().map(|n| Id::new(*n).unwrap()).collect()
    }
    fn order(ranking: &[(f64, Id)]) -> Vec<&str> {
        ranking.iter().map(|(_, id)| id.as_str()).collect()
    }

    #[test]
    fn more_sources_always_outrank_fewer_at_the_caps() {
        let cap = usize::from(CANDIDATES);
        let names: Vec<String> = (0..cap).map(|i| format!("c-{i:03}")).collect();
        let mut words: Vec<Id> = names.iter().map(|n| Id::new(n.as_str()).unwrap()).collect();
        let mut meaning = words.clone();
        let mut graph = words.clone();
        // `both` is last in two lists; `three` last in all three; `one` first in one list.
        words[cap - 1] = Id::new("both").unwrap();
        meaning[cap - 1] = Id::new("both").unwrap();
        words[cap - 2] = Id::new("three").unwrap();
        meaning[cap - 2] = Id::new("three").unwrap();
        graph[cap - 1] = Id::new("three").unwrap();
        graph[0] = Id::new("one").unwrap();
        // Everything else in two or three lists would blur the check: give the rest one list each.
        for (i, slot) in meaning.iter_mut().enumerate().take(cap - 2) {
            *slot = Id::new(format!("m-{i:03}")).unwrap();
        }
        for (i, slot) in graph.iter_mut().enumerate().take(cap - 1).skip(1) {
            *slot = Id::new(format!("g-{i:03}")).unwrap();
        }
        let ranking = fuse(&[words, meaning, graph]);
        let at = |name: &str| {
            ranking
                .iter()
                .position(|(_, id)| id.as_str() == name)
                .unwrap()
        };
        assert_eq!(at("three"), 0, "{:?}", &order(&ranking)[..3]);
        assert_eq!(at("both"), 1, "{:?}", &order(&ranking)[..3]);
        assert!(at("one") > at("both"));
    }

    #[test]
    fn ranks_not_scores_decide_and_ties_fall_to_the_id() {
        let ranking = fuse(&[ids(&["a", "b"]), ids(&["c", "b"])]);
        assert_eq!(order(&ranking), ["b", "c", "a"]);
        assert_eq!(ranking[1].0, ranking[2].0, "first of one list each: a tie");
    }

    #[test]
    fn a_source_past_its_cap_is_cut_and_says_so() {
        let many: Vec<Id> = (0..=CANDIDATES)
            .map(|i| Id::new(format!("c-{i:03}")).unwrap())
            .collect();
        let (kept, more) = capped(many.clone());
        assert!(more);
        assert_eq!(kept.len(), usize::from(CANDIDATES));
        let (kept, more) = capped(many[..usize::from(CANDIDATES)].to_vec());
        assert!(!more);
        assert_eq!(kept.len(), usize::from(CANDIDATES));
    }

    #[test]
    fn only_a_fused_position_resumes_a_fused_listing() {
        let ranking = fuse(&[ids(&["a", "b", "c"])]);
        let at = position(&ranking[0]);
        assert_eq!(parse(&at).unwrap(), (ranking[0].0, "a"));
        for old in [
            "r1.25|knowledgeclaim-1",
            "v0.5|knowledgeclaim-1",
            "1700000000000|knowledgeclaim-1",
            "fNaN|knowledgeclaim-1",
            "finf|knowledgeclaim-1",
            "f0.1",
        ] {
            assert!(
                matches!(parse(old), Err(StorageError::CursorExpired)),
                "{old}"
            );
        }
    }
}
