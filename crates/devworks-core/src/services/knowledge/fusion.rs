//! One ranking for `knowledge.search` with words (RAG-05, RAG-07): word matches, meaning
//! matches and graph matches fused by reciprocal rank.
//!
//! # Sources
//!
//! Each source ([`KnowledgeRead::knowledge_candidates`]) reads candidate ids and keys only,
//! with the read scopes and every filter of the query in the store's own statement, before any
//! claim is materialized:
//!
//! * **Word matches** ([`ClaimSource::Words`]): every query word in the claim's text or its
//!   subject entity's, by BM25 over the read scopes.
//! * **Meaning matches** ([`ClaimSource::Vector`]), when the listing has a query vector
//!   ([`super::vectors`]): the nearest claim vectors in the selected space with a positive
//!   cosine similarity, whether or not the claim also matches the words.
//! * **Graph matches** ([`ClaimSource::Graph`]): claims one hop from an entity the query
//!   matches, that is, claims naming as subject or object a matched entity (a live entity of
//!   the read scopes whose text has any query word; the best `entityLimit` by BM25), ordered by
//!   their best matched entity's BM25, then newest first.
//!
//! # The fused head
//!
//! Each source contributes its best [`CANDIDATES`] to the head. A claim's tier is how many of
//! the word and meaning sources list it (0 to 2), and its score the sum, over all three sources
//! that list it, of `1 / (k + rank)`, its 1-based rank there; only rank positions count, never
//! raw scores. The head is ordered by tier, then score, then id (all descending). The tier
//! comes first because a word match found through its subject entity is also a graph match of
//! that entity: without it, a word-only claim about the entity the query names could tie or
//! beat a claim that both the words and the meaning find. `k` ([`FUSION_K`]) is twice the cap,
//! so within a tier a claim listed by more sources (the graph source too) always scores above
//! one listed by fewer: `n / (k + cap) > (n − 1) / (k + 1)` holds for `n ≤ 3` exactly when
//! `k > (n − 1)·cap − n`.
//!
//! # The tail
//!
//! After the head, the listing continues with every other match, each source in its own order
//! and unfused: the remaining word matches, then graph matches that are not word matches, then
//! meaning matches that are neither, leaving out the head's claims. The sources are disjoint
//! there, so every matching claim is listed exactly once, however many there are.
//!
//! # Work limit
//!
//! `workLimited` says that fusion ranked only the head: a source had more candidates than
//! [`CANDIDATES`], or more entities matched the words than the entity limit (claims of the
//! entities past it are then neither word matches through their subject nor graph matches).
//! The claims past the head are still listed, in their sources' order. The head is computed
//! whole for every page (at most three caps of ids); a page reads the tail by keyset from its
//! position. The meaning source scores every claim vector of the read scopes exactly (see
//! [`super::vectors`] for the measured cost and the decision).
//!
//! # Paging
//!
//! A position names its listing's mode (`H` hybrid, `L` lexical) and then either a head entry
//! (`f<tier>:<score>|<id>`, the exact score as Rust's shortest round-tripping decimal) or a tail
//! row (`t<source>:<key>:<key2>|<id>`, source `w`, `g` or `v`); a page resumes strictly after it,
//! moving from the head into the tail. A listing keeps the mode of its first page: a lexical
//! listing never embeds its query again, and a provider recovering in the middle of it does not
//! expire it. The cursor binds everything the order depends on: the query, the page limit, the
//! revision of every read scope (any write there expires it), the fusion constants, and for a
//! hybrid listing the space, how many claims of the read scopes had vectors in it when the
//! query vector was prepared (a vector stored between two pages expires it) and the SHA-256 of
//! the query vector (a provider returning another vector for the same text expires it). So a
//! claim appears exactly once across the pages while those hold. A position of another format
//! (`r…`, `v…`, `f…` without a mode, or a time) is `CursorExpired`, as is a hybrid cursor when the
//! page has no query vector.
//!
//! A token budget ([`super::budgeted`]) ends a page early, at the position of its last claim
//! that fits, head or tail alike. It never changes the order or a position, so it is not bound:
//! a caller may pass another budget on the next page.
use crate::fingerprint::sha256_hex;
use crate::ports::storage::{
    ClaimCandidateRead, ClaimCandidates, ClaimQuery, ClaimSource, ClaimVectorQuery, KnowledgeRead,
    StorageError, StoredClaim,
};
use crate::protocol::Id;
use crate::protocol::records::PageRequest;
use crate::services::cursor::{self, CursorBinding, Paged};
use std::cmp::Ordering;
use std::collections::HashMap;

/// The most candidates each source (word, meaning, graph) contributes to the fused head.
pub const CANDIDATES: u16 = 200;
/// The reciprocal rank constant `k`: twice [`CANDIDATES`] (see the module notes).
pub const FUSION_K: f64 = 2.0 * CANDIDATES as f64;

/// Whether a listing fuses meaning matches. A listing keeps its mode across its pages: the
/// cursor position carries it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    Hybrid,
    Lexical,
}
impl Mode {
    fn tag(self) -> char {
        match self {
            Self::Hybrid => 'H',
            Self::Lexical => 'L',
        }
    }
    fn of(tag: char) -> Option<Self> {
        match tag {
            'H' => Some(Self::Hybrid),
            'L' => Some(Self::Lexical),
            _ => None,
        }
    }
}

/// The mode of the listing a request continues, or `None` for a first page. A cursor of
/// another format is `CursorExpired`.
pub(super) fn continued_mode(request: &PageRequest) -> Result<Option<Mode>, StorageError> {
    let Some(c) = &request.cursor else {
        return Ok(None);
    };
    let at = cursor::peek(&c.0)?;
    at.chars()
        .next()
        .and_then(Mode::of)
        .map(Some)
        .ok_or(StorageError::CursorExpired)
}

/// One claim of the fused head.
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    /// How many of the word and meaning sources list the claim (0..=2).
    tier: u8,
    /// The reciprocal rank sum over all sources.
    score: f64,
    id: Id,
}

/// Head order: tier descending, then fused score descending, then id descending.
fn head_order(a: (u8, f64, &str), b: (u8, f64, &str)) -> Ordering {
    b.0.cmp(&a.0)
        .then_with(|| b.1.total_cmp(&a.1))
        .then_with(|| b.2.cmp(a.2))
}

/// The fused head of one search.
#[derive(Debug)]
pub(super) struct Fused {
    mode: Mode,
    head: Vec<Entry>,
    /// The head's claims, left out of the tail.
    ids: Vec<Id>,
    /// Fusion ranked only the head: a source had more candidates than [`CANDIDATES`], or more
    /// entities matched the words than the entity limit.
    pub work_limited: bool,
}

/// One source's best [`CANDIDATES`], and whether it had more.
fn capped(c: ClaimCandidates) -> (Vec<Id>, bool) {
    let more = c.items.len() > usize::from(CANDIDATES) || c.entities_limited;
    let ids = c
        .items
        .into_iter()
        .take(usize::from(CANDIDATES))
        .map(|c| c.id)
        .collect();
    (ids, more)
}

/// Fuses ranked lists by reciprocal rank (`k` [`FUSION_K`]); the first `tiered` lists decide
/// a claim's tier. A list names each claim once.
fn fuse(lists: &[Vec<Id>], tiered: usize) -> Vec<Entry> {
    let mut scores: HashMap<Id, (u8, f64)> = HashMap::new();
    // Lists are added in one fixed order, so a claim's sum is the same on every page.
    for (i, list) in lists.iter().enumerate() {
        for (rank, id) in list.iter().enumerate() {
            let e = scores.entry(id.clone()).or_default();
            e.0 += u8::from(i < tiered);
            e.1 += 1.0 / (FUSION_K + (rank + 1) as f64);
        }
    }
    let mut head: Vec<Entry> = scores
        .into_iter()
        .map(|(id, (tier, score))| Entry { tier, score, id })
        .collect();
    head.sort_by(|a, b| {
        head_order(
            (a.tier, a.score, a.id.as_str()),
            (b.tier, b.score, b.id.as_str()),
        )
    });
    head
}

/// The sources a listing of `mode` reads, in tail order.
fn sources(mode: Mode) -> &'static [ClaimSource] {
    match mode {
        Mode::Hybrid => &[ClaimSource::Words, ClaimSource::Graph, ClaimSource::Vector],
        Mode::Lexical => &[ClaimSource::Words, ClaimSource::Graph],
    }
}

/// A tail read: the head's ids it leaves out, and the row it resumes after.
type Tail<'a> = (&'a [Id], Option<((f64, i64), &'a str)>);

fn read_source<'a>(
    read: &dyn KnowledgeRead,
    query: &ClaimQuery,
    source: ClaimSource,
    near: Option<ClaimVectorQuery<'a>>,
    tail: Option<Tail<'a>>,
    limit: u16,
) -> Result<ClaimCandidates, StorageError> {
    read.knowledge_candidates(
        query,
        &ClaimCandidateRead {
            source,
            near: near.filter(|_| source == ClaimSource::Vector),
            tail: tail.is_some(),
            exclude: tail.map_or(&[], |(ids, _)| ids),
            after: tail.and_then(|(_, after)| after),
            limit,
        },
    )
}

/// The fused head of `query` (which has words): the best [`CANDIDATES`] of the word, meaning
/// (with `near`) and graph sources.
pub(super) fn rank(
    read: &dyn KnowledgeRead,
    query: &ClaimQuery,
    near: Option<ClaimVectorQuery<'_>>,
) -> Result<Fused, StorageError> {
    let mode = if near.is_some() {
        Mode::Hybrid
    } else {
        Mode::Lexical
    };
    // One more than the cap tells whether a source has more.
    let probe = CANDIDATES + 1;
    let mut limited = false;
    let mut lists = Vec::new();
    // Word and meaning sources first: they decide the tier.
    for source in [ClaimSource::Words, ClaimSource::Vector] {
        if source == ClaimSource::Vector && near.is_none() {
            continue;
        }
        let (ids, more) = capped(read_source(read, query, source, near, None, probe)?);
        limited |= more;
        lists.push(ids);
    }
    let tiered = lists.len();
    let (graph, more) = capped(read_source(
        read,
        query,
        ClaimSource::Graph,
        near,
        None,
        probe,
    )?);
    lists.push(graph);
    let head = fuse(&lists, tiered);
    Ok(Fused {
        mode,
        ids: head.iter().map(|e| e.id.clone()).collect(),
        head,
        work_limited: limited || more,
    })
}

/// The cursor binding parts of a fused search of `mode`: the fusion's own constants and, for a
/// hybrid listing, the meaning source's inputs: the space, how many claims of the read scopes
/// had vectors in it when the query vector was prepared, and the query vector's digest.
pub(super) fn binding_parts(mode: Mode, vector: Option<(&Id, i64, &[f32])>) -> Vec<String> {
    let mut parts = vec![format!("fused k={FUSION_K} cap={CANDIDATES}")];
    parts.push(match (mode, vector) {
        (Mode::Hybrid, Some((space, embedded, v))) => {
            let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
            format!(
                "vectors={}:{embedded}:{}",
                space.as_str(),
                sha256_hex(&bytes)
            )
        }
        _ => "lexical".into(),
    });
    parts
}

fn source_tag(source: ClaimSource) -> char {
    match source {
        ClaimSource::Words => 'w',
        ClaimSource::Graph => 'g',
        ClaimSource::Vector => 'v',
    }
}

/// A position in a fused listing.
#[derive(Debug, PartialEq)]
enum Position<'a> {
    Head(u8, f64, &'a str),
    Tail(ClaimSource, (f64, i64), &'a str),
}

fn head_position(mode: Mode, e: &Entry) -> String {
    format!("{}f{}:{}|{}", mode.tag(), e.tier, e.score, e.id.as_str())
}

fn tail_position(mode: Mode, source: ClaimSource, key: (f64, i64), id: &Id) -> String {
    format!(
        "{}t{}:{}:{}|{}",
        mode.tag(),
        source_tag(source),
        key.0,
        key.1,
        id.as_str()
    )
}

/// Parses a position of a `mode` listing; any other is `CursorExpired`.
fn parse(at: &str, mode: Mode) -> Result<Position<'_>, StorageError> {
    let expired = || StorageError::CursorExpired;
    let (key, id) = at.split_once('|').ok_or_else(expired)?;
    let key = key.strip_prefix(mode.tag()).ok_or_else(expired)?;
    let finite = |s: &str| s.parse::<f64>().ok().filter(|f| f.is_finite());
    if let Some(rest) = key.strip_prefix('f') {
        let (tier, score) = rest.split_once(':').ok_or_else(expired)?;
        let tier: u8 = tier.parse().ok().filter(|t| *t <= 2).ok_or_else(expired)?;
        return Ok(Position::Head(tier, finite(score).ok_or_else(expired)?, id));
    }
    let rest = key.strip_prefix('t').ok_or_else(expired)?;
    let mut parts = rest.splitn(3, ':');
    let (Some(tag), Some(score), Some(second)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(expired());
    };
    let source = sources(mode)
        .iter()
        .copied()
        .find(|s| tag.len() == 1 && tag.starts_with(source_tag(*s)))
        .ok_or_else(expired)?;
    Ok(Position::Tail(
        source,
        (
            finite(score).ok_or_else(expired)?,
            second.parse().map_err(|_| expired())?,
        ),
        id,
    ))
}

/// One page of a fused listing, through the shared pager: the head's claims strictly after the
/// request's position, then each source's remaining claims in its own order (word matches,
/// then graph matches that are not word matches, then meaning matches that are neither), the
/// head's claims left out, at most the request's limit, then cut to `budget`
/// ([`super::budgeted`]), which ends the page early at a claim's own head or tail position.
/// Every fused listing pages here.
pub(super) fn page(
    read: &dyn KnowledgeRead,
    query: &ClaimQuery,
    near: Option<ClaimVectorQuery<'_>>,
    binding: &CursorBinding,
    request: &PageRequest,
    fused: &Fused,
    budget: Option<u32>,
) -> Result<Paged<StoredClaim>, StorageError> {
    let mode = fused.mode;
    let rows = cursor::page(
        binding,
        request,
        |after, n| {
            let n = usize::from(n);
            let mut rows: Vec<(String, Id)> = Vec::new();
            let after = after.map(|a| parse(a, mode)).transpose()?;
            let (first, mut resume) = match after {
                Some(Position::Tail(source, key, id)) => (
                    sources(mode)
                        .iter()
                        .position(|s| *s == source)
                        .ok_or(StorageError::CursorExpired)?,
                    Some((key, id)),
                ),
                head => {
                    let start = match head {
                        Some(Position::Head(tier, score, id)) => fused.head.partition_point(|e| {
                            head_order((e.tier, e.score, e.id.as_str()), (tier, score, id))
                                != Ordering::Greater
                        }),
                        _ => 0,
                    };
                    rows.extend(
                        fused.head[start..]
                            .iter()
                            .take(n)
                            .map(|e| (head_position(mode, e), e.id.clone())),
                    );
                    (0, None)
                }
            };
            for &source in &sources(mode)[first..] {
                if rows.len() >= n {
                    break;
                }
                let need = u16::try_from(n - rows.len()).expect("page size");
                let found = read_source(
                    read,
                    query,
                    source,
                    near,
                    Some((&fused.ids, resume.take())),
                    need,
                )?;
                rows.extend(
                    found
                        .items
                        .into_iter()
                        .map(|c| (tail_position(mode, source, c.key, &c.id), c.id)),
                );
            }
            Ok::<_, StorageError>(rows)
        },
        |(position, _)| position.clone(),
    )?;
    let ids: Vec<Id> = rows.items.iter().map(|(_, id)| id.clone()).collect();
    let claims = read.knowledge_claims_by_id(&ids)?;
    if claims.len() != ids.len() {
        return Err(StorageError::InvalidRecord(
            "a listed claim was not read".into(),
        ));
    }
    let rows = Paged {
        items: rows
            .items
            .into_iter()
            .map(|(position, _)| position)
            .zip(claims)
            .collect(),
        next_cursor: rows.next_cursor,
    };
    super::budgeted(
        rows,
        binding,
        budget,
        |(_, c)| c,
        |(position, _)| position.clone(),
    )
    .try_map(|(_, c)| Ok(c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::storage::ClaimCandidate;

    fn ids(names: &[&str]) -> Vec<Id> {
        names.iter().map(|n| Id::new(*n).unwrap()).collect()
    }
    fn order(head: &[Entry]) -> Vec<&str> {
        head.iter().map(|e| e.id.as_str()).collect()
    }

    #[test]
    fn more_sources_always_outrank_fewer_at_the_caps() {
        let cap = usize::from(CANDIDATES);
        let mut words: Vec<Id> = (0..cap)
            .map(|i| Id::new(format!("c-{i:03}")).unwrap())
            .collect();
        let mut meaning: Vec<Id> = (0..cap)
            .map(|i| Id::new(format!("m-{i:03}")).unwrap())
            .collect();
        let mut graph: Vec<Id> = (0..cap)
            .map(|i| Id::new(format!("g-{i:03}")).unwrap())
            .collect();
        // `both` is last of the word and meaning lists; `three` next to last there and last of
        // the graph list; `one` first of the graph list only.
        words[cap - 1] = Id::new("both").unwrap();
        meaning[cap - 1] = Id::new("both").unwrap();
        words[cap - 2] = Id::new("three").unwrap();
        meaning[cap - 2] = Id::new("three").unwrap();
        graph[cap - 1] = Id::new("three").unwrap();
        graph[0] = Id::new("one").unwrap();
        let head = fuse(&[words, meaning, graph], 2);
        let at = |name: &str| head.iter().position(|e| e.id.as_str() == name).unwrap();
        assert_eq!(at("three"), 0, "{:?}", &order(&head)[..3]);
        assert_eq!(at("both"), 1, "{:?}", &order(&head)[..3]);
        assert!(
            at("one") > at("c-000"),
            "a graph-only claim is below every word match"
        );
    }

    #[test]
    fn a_word_match_found_through_its_subject_never_beats_a_word_and_meaning_match() {
        // `subject` is the best word match and, through its subject entity, the best graph
        // match: two sources at rank 1. `both` is second of the word and meaning lists.
        let head = fuse(
            &[
                ids(&["subject", "both"]),
                ids(&["near", "both"]),
                ids(&["subject"]),
            ],
            2,
        );
        assert_eq!(order(&head), ["both", "subject", "near"]);
        assert!(
            head[1].score > head[0].score,
            "the tier, not the score, decides"
        );
    }

    #[test]
    fn ranks_not_scores_decide_and_ties_fall_to_the_id() {
        let head = fuse(&[ids(&["a", "b"]), ids(&["c", "b"])], 2);
        assert_eq!(order(&head), ["b", "c", "a"]);
        assert_eq!(
            head[1].score, head[2].score,
            "first of one list each: a tie"
        );
    }

    #[test]
    fn a_source_past_its_cap_is_cut_and_says_so() {
        let many = |n: u16, entities_limited| ClaimCandidates {
            items: (0..n)
                .map(|i| ClaimCandidate {
                    key: (f64::from(n - i), 0),
                    id: Id::new(format!("c-{i:03}")).unwrap(),
                })
                .collect(),
            entities_limited,
        };
        let (kept, more) = capped(many(CANDIDATES + 1, false));
        assert!(more);
        assert_eq!(kept.len(), usize::from(CANDIDATES));
        let (kept, more) = capped(many(CANDIDATES, false));
        assert!(!more);
        assert_eq!(kept.len(), usize::from(CANDIDATES));
        // More matched entities than the entity limit limit the listing too.
        assert!(capped(many(3, true)).1);
    }

    #[test]
    fn only_a_position_of_the_listings_own_mode_and_format_resumes_it() {
        let head = fuse(&[ids(&["a", "b", "c"])], 1);
        let at = head_position(Mode::Hybrid, &head[0]);
        assert_eq!(
            parse(&at, Mode::Hybrid).unwrap(),
            Position::Head(1, head[0].score, "a")
        );
        assert!(matches!(
            parse(&at, Mode::Lexical),
            Err(StorageError::CursorExpired)
        ));
        let id = Id::new("c-1").unwrap();
        let at = tail_position(Mode::Lexical, ClaimSource::Graph, (1.5, 1_700), &id);
        assert_eq!(
            parse(&at, Mode::Lexical).unwrap(),
            Position::Tail(ClaimSource::Graph, (1.5, 1_700), "c-1")
        );
        // A lexical listing has no meaning tail.
        let at = tail_position(Mode::Lexical, ClaimSource::Vector, (0.5, 0), &id);
        assert!(matches!(
            parse(&at, Mode::Lexical),
            Err(StorageError::CursorExpired)
        ));
        for old in [
            "r1.25|knowledgeclaim-1",
            "v0.5|knowledgeclaim-1",
            "f0.0024|knowledgeclaim-1",
            "1700000000000|knowledgeclaim-1",
            "Hf3:0.1|knowledgeclaim-1",
            "HfNaN|knowledgeclaim-1",
            "Htx:1:2|knowledgeclaim-1",
            "Htw:1|knowledgeclaim-1",
            "Hf1:0.1",
        ] {
            assert!(
                matches!(parse(old, Mode::Hybrid), Err(StorageError::CursorExpired)),
                "{old}"
            );
        }
    }

    #[test]
    fn a_hybrid_binding_names_the_query_vectors_digest() {
        let space = Id::new("space-1").unwrap();
        let a = binding_parts(Mode::Hybrid, Some((&space, 3, &[1.0, 0.0])));
        let b = binding_parts(Mode::Hybrid, Some((&space, 3, &[1.0, 0.000_001])));
        assert_ne!(a, b);
        assert_eq!(
            binding_parts(Mode::Lexical, Some((&space, 3, &[1.0, 0.0]))),
            binding_parts(Mode::Lexical, None)
        );
    }
}
