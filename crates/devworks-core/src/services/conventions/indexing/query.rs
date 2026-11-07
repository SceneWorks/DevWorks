//! Query preparation owns routed embedding work; the final fresh snapshot owns all ranking.
use super::{
    IndexService,
    embedding::{EmbeddingConfiguration, EmbeddingFailure, EmbeddingOrigin, EmbeddingRequest},
    preparation::InputPreparation,
    rank,
};
use crate::{
    context::InvocationContext,
    fingerprint::sha256_hex,
    gateway::AdmittedQuery,
    operations::conventions as ops,
    ports::{Preparation, storage::*},
    protocol::{
        Id,
        records::*,
        support::{BoundedList, BoundedText, Finite, GraphText},
    },
    services::{
        conventions::{access, model},
        cursor::CursorBinding,
        sessions::{Failure, counter, rev},
    },
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug)]
pub struct PreparedSearch {
    witness: String,
    query_id: Id,
    query_fingerprint: String,
    offset: usize,
    request_id: Option<Id>,
    custody_request_id: Option<Id>,
    custody_operation_id: Option<crate::protocol::OperationId>,
    vector: Option<Vec<f32>>,
    reason: Option<String>,
    configuration: EmbeddingConfiguration,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Position {
    offset: usize,
    query_id: Id,
    request_id: Option<Id>,
    dense: bool,
    #[serde(default)]
    custody_request_id: Option<Id>,
    #[serde(default)]
    custody_operation_id: Option<crate::protocol::OperationId>,
}
struct Snapshot {
    witness: String,
    fingerprint: String,
    binding: CursorBinding,
    filter: ConventionQuery,
    revision: i64,
    source_revision: i64,
    top_k: u16,
    configuration: EmbeddingConfiguration,
    documents: u64,
}
fn invalid(message: &str) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
impl IndexService {
    fn search_snapshot(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        input: &ops::SearchInput,
    ) -> Result<Snapshot, Failure> {
        if !crate::authority::authorization_holds(read, query.authorization())? {
            return Err(StorageError::ScopeDenied.into());
        }
        let scoped = access::scope(read, ctx, Some(&input.scope), false)?;
        let top_k = if query.schema_revision().0 >= 3 {
            input
                .top_k
                .as_ref()
                .and_then(|v| v.0.parse::<i64>().ok())
                .unwrap_or(10)
                .clamp(1, 50) as u16
        } else {
            input.page.limit.get().clamp(1, 50) as u16
        };
        let filter = ConventionQuery {
            scopes: scoped.readable.clone(),
            lifecycles: vec!["Active".into()],
            applicability: input.applicability.0.iter().map(|s| s.0.clone()).collect(),
            ..Default::default()
        };
        let config = self.configuration_read(read, ctx)?;
        let revision = read
            .conventions()
            .convention_index_revision(&filter.scopes)?;
        let sources = filter
            .scopes
            .iter()
            .map(|s| {
                read.conventions()
                    .convention_scope_revision(s)
                    .map(|r| json!({"scope":s.key(),"revision":r}))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let fingerprint=sha256_hex(crate::jcs::canonicalize(&json!({"scope":input.scope,"text":input.text,"applicability":input.applicability,"topK":top_k,"pageLimit":input.page.limit,"schema":query.schema_revision().0})).map_err(|_|invalid("search identity"))?.as_bytes());
        let principal = read.authority().principal(ctx.principal())?;
        let policy =
            crate::authority::policy::resolve_policy(read.configuration(), ctx.project_id())?
                .digest();
        let witness=sha256_hex(json!({"query":fingerprint,"sources":sources,"index":revision,"configuration":config.source_digest,"principal":principal.map(|p|(p.revision,p.active)),"policy":policy,"registry":read.packages().registry_revision()?}).to_string().as_bytes());
        let documents = read
            .conventions()
            .convention_lexical_statistics(&filter, &[])?
            .documents;
        Ok(Snapshot {
            binding: CursorBinding::new(ctx, "conventions.search", &[&witness], revision),
            witness,
            fingerprint,
            filter,
            revision,
            source_revision: read
                .conventions()
                .convention_scope_revision(&scoped.exact)?,
            top_k,
            configuration: config,
            documents,
        })
    }
    pub fn prepare_search(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
        input: &ops::SearchInput,
    ) -> Result<PreparedSearch, Failure> {
        let snapshot = read_snapshot(
            &*self.store,
            &ReadOptions {
                cancellation: env.cancellation.cloned(),
                deadline_ms: env.deadline_ms,
            },
            |read| Ok(self.search_snapshot(read, ctx, query, input)),
        )??;
        if env.stopped(self.clock.now_ms().get()) || self.is_stopped() {
            return Err(StorageError::Cancelled.into());
        }
        let position = input
            .page
            .cursor
            .as_ref()
            .map(|cursor| {
                snapshot.binding.decode(&cursor.0).and_then(|s| {
                    serde_json::from_str::<Position>(&s).map_err(|_| StorageError::CursorExpired)
                })
            })
            .transpose()?;
        let query_id = position
            .as_ref()
            .map_or_else(|| query.query_id().0.clone(), |p| p.query_id.clone());
        if position.as_ref().is_some_and(|p| {
            p.offset >= snapshot.top_k as usize || p.dense != p.request_id.is_some()
        }) {
            return Err(StorageError::CursorExpired.into());
        }
        let origin = EmbeddingOrigin::Query {
            query_id: query_id.clone(),
            fingerprint: snapshot.fingerprint.clone(),
            scope_digest: snapshot.witness.clone(),
        };
        self.validate_query_custody(ctx, &origin, &snapshot.configuration.source_digest)?;
        let mut prepared = PreparedSearch {
            witness: snapshot.witness.clone(),
            query_id: query_id.clone(),
            query_fingerprint: snapshot.fingerprint.clone(),
            offset: position.as_ref().map_or(0, |p| p.offset),
            request_id: None,
            custody_request_id: None,
            custody_operation_id: None,
            vector: None,
            reason: snapshot.configuration.reason.clone(),
            configuration: snapshot.configuration.clone(),
        };
        // Frozen SearchAsync returns before provider work for whitespace or no authored chunks.
        if input.text.0.chars().all(char::is_whitespace) || snapshot.documents == 0 {
            prepared.reason = Some(
                if snapshot.documents == 0 {
                    "No current published convention chunks."
                } else {
                    "The query is whitespace."
                }
                .into(),
            );
            return Ok(prepared);
        }
        if let Some(space) = snapshot.configuration.space {
            // A lexical cursor stays lexical even when its original optional provider failed.
            let lexical_cursor = position.as_ref().is_some_and(|p| !p.dense);
            if position
                .as_ref()
                .is_some_and(|p| !p.dense && p.custody_request_id.is_none())
            {
                prepared.reason = Some("The original query used lexical retrieval.".into());
                return Ok(prepared);
            }
            let cancel = env.cancellation.cloned().unwrap_or_default();
            let (context, authorization, expected, available, selected, requested) = (
                ctx.clone(),
                query.authorization().clone(),
                snapshot.configuration.source_digest.clone(),
                self.native_available(),
                space.clone(),
                input.scope.clone(),
            );
            self.write_options(
                WriteOptions {
                    cancellation: Some(cancel.clone()),
                    deadline_ms: env.deadline_ms,
                    ..Default::default()
                },
                move |tx| {
                    if &tx.store().store_id != context.store_id()
                        || tx.store().restore_epoch != context.restore_epoch().get()
                        || tx.actors().actor_kind(context.principal())?
                            != Some(context.principal_kind())
                        || !crate::authority::authorization_holds_in(tx, &context, &authorization)?
                    {
                        return Err(StorageError::ScopeDenied);
                    }
                    access::scope_in(tx, &context, Some(&requested), false)
                        .map_err(|_| StorageError::ScopeDenied)?;
                    let now = super::configuration::resolved_configuration(
                        &crate::services::router::embedding_spaces::resolve_selected_in(
                            tx, &context,
                        )?,
                        available,
                    );
                    if now.source_digest != expected || now.space.as_ref() != Some(&selected) {
                        return Err(StorageError::CursorExpired);
                    }
                    tx.conventions().put_convention_embedding_space(&selected)
                },
            )?;
            let preparation = InputPreparation {
                context: ctx,
                origin: &origin,
                embedding_space: &space.id,
                configuration_source_digest: &snapshot.configuration.source_digest,
                texts: &[input.text.0.as_str()],
                fences: query.authorization().fences(),
                cancellation: &cancel,
                deadline_ms: env.deadline_ms,
            };
            let inputs = if position.is_some() {
                self.existing_embedding_inputs(preparation)
            } else {
                self.prepare_embedding_inputs(preparation)
            }?;
            if position.as_ref().is_some_and(|p| {
                p.request_id.as_ref().or(p.custody_request_id.as_ref()) != Some(&inputs.request_id)
                    || p.custody_operation_id
                        .as_ref()
                        .is_some_and(|id| id != &inputs.operation_id)
            }) {
                return Err(StorageError::CursorExpired.into());
            }
            prepared.custody_request_id = Some(inputs.request_id.clone());
            prepared.custody_operation_id = Some(inputs.operation_id.clone());
            if lexical_cursor {
                prepared.reason = Some("The original query used lexical retrieval.".into());
                return Ok(prepared);
            }
            let request = EmbeddingRequest {
                request_id: inputs.request_id.clone(),
                origin,
                space: space.clone(),
                configuration_source_digest: snapshot.configuration.source_digest,
                preparation_operation: inputs.operation_id,
                input_refs: inputs.input_refs,
                fences: query.authorization().fences().to_vec(),
                cancellation: cancel,
                deadline_ms: env.deadline_ms,
            };
            let result = if position.is_some() {
                self.observe_batch(&inputs.context, request)
            } else {
                self.embed_batch(&inputs.context, request)
            };
            match result {
                Ok(result) if result.request_id==inputs.request_id && result.space==space && result.vectors.len()==1 && result.vectors[0].len()==space.dimensions as usize && result.vectors[0].iter().all(|v|v.is_finite())=>{prepared.vector=result.vectors.into_iter().next();prepared.request_id=Some(inputs.request_id);prepared.reason=None;}
                Ok(_)=>return Err(invalid("query embedding population/binding mismatch").into()),
                Err(EmbeddingFailure::Cancelled)=>return Err(StorageError::Cancelled.into()),
                Err(EmbeddingFailure::Storage(error))=>return Err(error.into()),
                Err(_) if position.is_some()=>return Err(StorageError::CursorExpired.into()),
                Err(EmbeddingFailure::Unknown{..})=>prepared.reason=Some("The native query embedding outcome is unknown; results use lexical retrieval.".into()),
                Err(EmbeddingFailure::NotStarted{..})=>return Err(invalid("native send returned observer-only absence").into()),
                Err(EmbeddingFailure::Unavailable(_))=>prepared.reason=Some("The configured native embedding route is unavailable; results use lexical retrieval.".into()),
            }
        }
        Ok(prepared)
    }
    pub fn search(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        input: &ops::SearchInput,
        prepared: &PreparedSearch,
    ) -> Result<ConventionSearchPage, Failure> {
        let snapshot = self.search_snapshot(read, ctx, query, input)?;
        if snapshot.witness != prepared.witness
            || snapshot.fingerprint != prepared.query_fingerprint
        {
            return Err(StorageError::CursorExpired.into());
        }
        let terms = rank::terms(&input.text.0).into_keys().collect::<Vec<_>>();
        let stats = read
            .conventions()
            .convention_lexical_statistics(&snapshot.filter, &terms)?;
        let weighted = stats
            .document_frequencies
            .into_iter()
            .map(|t| ConventionWeightedTerm {
                term: t.term,
                idf: rank::inverse_document_frequency(stats.documents, u64::from(t.frequency)),
            })
            .collect::<Vec<_>>();
        let average = if stats.documents == 0 {
            1.0
        } else {
            (stats.total_tokens as f64 / stats.documents as f64).max(1.0)
        };
        let rows = read.conventions().convention_ranked_chunks(
            &snapshot.filter,
            &weighted,
            average,
            prepared
                .vector
                .as_ref()
                .and_then(|_| prepared.configuration.space.as_ref().map(|s| s.id.as_str())),
            prepared.vector.as_deref(),
            snapshot.top_k,
        )?;
        if prepared.offset > rows.len() {
            return Err(StorageError::CursorExpired.into());
        }
        let limit = (input.page.limit.get() as usize).min(50);
        let end = (prepared.offset + limit).min(rows.len());
        let mut items = Vec::new();
        for hit in &rows[prepared.offset..end] {
            let owner = read
                .conventions()
                .convention(&hit.chunk.convention_id, None, &snapshot.filter.scopes)?
                .ok_or(StorageError::CursorExpired)?;
            let indexed_path = ConventionIndexedPath {
                generation_id: hit.chunk.generation_id.clone(),
                chunk_id: hit.chunk.id.clone(),
            };
            let section_path =
                crate::services::conventions::path_source(read, ctx, &owner, &indexed_path)?;
            items.push(ConventionSearchHit {
                convention: model::record(&owner, self.status(read, &owner)?)?,
                chunk_id: hit.chunk.id.clone(),
                chunk_index: counter(hit.chunk.index),
                section_path,
                excerpt: GraphText(hit.chunk.excerpt.clone()),
                score: serde_json::from_value::<Finite>(json!(hit.score))
                    .map_err(|_| invalid("search score"))?,
                indexed_path,
            });
        }
        crate::services::conventions::queries::page_budget(&mut items)?;
        let end = prepared.offset + items.len();
        let has_more = end < rows.len();
        let (gaps, more_gaps) = self.search_gaps(
            read,
            &snapshot.filter,
            prepared.configuration.space.as_ref().map(|s| s.id.as_str()),
        )?;
        let next_cursor = has_more.then(|| {
            BoundedText(
                snapshot.binding.encode(
                    &serde_json::to_string(&Position {
                        offset: end,
                        query_id: prepared.query_id.clone(),
                        request_id: prepared.request_id.clone(),
                        dense: prepared.vector.is_some(),
                        custody_request_id: prepared.custody_request_id.clone(),
                        custody_operation_id: prepared.custody_operation_id.clone(),
                    })
                    .expect("search cursor"),
                ),
            )
        });
        let embedding_operation =
            match (&prepared.custody_request_id, &prepared.custody_operation_id) {
                (Some(request), Some(operation)) => {
                    let inputs = read
                        .conventions()
                        .convention_embedding_inputs(request)?
                        .ok_or(StorageError::CursorExpired)?;
                    let op = read
                        .operations()
                        .operation(operation)?
                        .ok_or(StorageError::CursorExpired)?;
                    if inputs.actor_id != *ctx.principal()
                        || inputs.project_id.as_ref() != ctx.project_id()
                        || op.capability != "conventions.embedding_prepare"
                        || op.payload["requestId"] != json!(request)
                        || op.payload["origin"]["kind"] != "Query"
                        || op.payload["origin"]["queryId"] != json!(prepared.query_id)
                        || op.payload["origin"]["fingerprint"] != prepared.query_fingerprint
                        || op.payload["origin"]["scopeDigest"] != prepared.witness
                    {
                        return Err(StorageError::CursorExpired.into());
                    }
                    Some(OperationRef {
                        operation_id: op.id,
                        state: op.state,
                    })
                }
                (None, None) => None,
                _ => return Err(invalid("query embedding custody pair missing").into()),
            };
        Ok(ConventionSearchPage {
            items: BoundedList(items),
            next_cursor,
            has_more,
            scope_revision: rev(snapshot.revision),
            source_scope_revision: Some(rev(snapshot.source_revision)),
            retrieval: ConventionRetrieval {
                mode: if prepared.vector.is_some() {
                    ConventionRetrievalMode::Hybrid
                } else {
                    ConventionRetrievalMode::Lexical
                },
                embedding_space: prepared.vector.as_ref().and_then(|_| {
                    prepared
                        .configuration
                        .space
                        .as_ref()
                        .map(|s| BoundedText(s.id.clone()))
                }),
                reason: prepared.reason.clone().map(BoundedText),
                gaps: BoundedList(gaps.into_iter().map(BoundedText).collect()),
                more_gaps,
                embedding_request_id: prepared.custody_request_id.clone(),
                embedding_operation,
            },
        })
    }
    fn search_gaps(
        &self,
        read: &dyn ReadSnapshot,
        filter: &ConventionQuery,
        space: Option<&str>,
    ) -> Result<(Vec<String>, bool), StorageError> {
        let mut after = None;
        let mut gaps = Vec::new();
        let mut more = false;
        loop {
            let owners = read.conventions().conventions(filter, after.as_ref(), 64)?;
            if owners.is_empty() {
                break;
            }
            for owner in &owners {
                let index = read.conventions().convention_index_head(&owner.id)?;
                let gap = match index {
                    None => Some("Unindexed"),
                    Some(ref g)
                        if g.state != "Published"
                            || !matches!(g.status.as_str(), "Ready" | "LexicalOnly") =>
                    {
                        Some("IndexNotCurrent")
                    }
                    Some(ref g)
                        if space.is_some()
                            && (g.embedding_space.as_deref() != space || g.status != "Ready") =>
                    {
                        Some("MissingCompatibleVectors")
                    }
                    _ => None,
                };
                if let Some(reason) = gap {
                    if gaps.len() < 32 {
                        gaps.push(format!("{}:{reason}", owner.id.as_str()));
                    } else {
                        more = true;
                    }
                }
            }
            after = owners.last().map(StoredConvention::position);
        }
        Ok((gaps, more))
    }
}
