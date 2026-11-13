use super::*;
use devworks_core::protocol::{OperationId, ProjectId};
fn index_record(r: &Row<'_>) -> rusqlite::Result<StoredConventionIndex> {
    Ok(StoredConventionIndex {
        generation_id: identifier(r, 0)?,
        convention_id: identifier(r, 1)?,
        convention_revision: r.get(2)?,
        source_ref: value(r, 3)?,
        operation_id: OperationId(identifier(r, 4)?),
        state: r.get(5)?,
        status: r.get(6)?,
        error: r.get(7)?,
        embedding_space: r.get(8)?,
        source_digest: r.get(9)?,
        created_at_ms: r.get(10)?,
        updated_at_ms: r.get(11)?,
    })
}
const INDEX_COLUMNS: &str = "generation_id,convention_id,convention_revision,source_ref_json,operation_id,state,status,error,embedding_space,source_digest,created_at,updated_at";
fn generation(conn: &Connection, id: &Id) -> Result<Option<StoredConventionIndex>, StorageError> {
    db(conn
        .query_row(
            &format!("SELECT {INDEX_COLUMNS} FROM convention_indexes WHERE generation_id=?1"),
            [id.as_str()],
            index_record,
        )
        .optional())
}
fn head(conn: &Connection, owner: &Id) -> Result<Option<StoredConventionIndex>, StorageError> {
    let id: Option<Id> = db(conn
        .query_row(
            "SELECT generation_id FROM convention_index_heads WHERE convention_id=?1",
            [owner.as_str()],
            |r| identifier(r, 0),
        )
        .optional())?;
    id.map(|id| generation(conn, &id))
        .transpose()
        .map(Option::flatten)
}
fn space(
    conn: &Connection,
    id: &str,
) -> Result<Option<StoredConventionEmbeddingSpace>, StorageError> {
    db(conn.query_row("SELECT id,provider,model,configuration_digest,dimensions,metric,element_type FROM convention_embedding_spaces WHERE id=?1",[id],|r|Ok(StoredConventionEmbeddingSpace {id:r.get(0)?,provider:r.get(1)?,model:r.get(2)?,configuration_digest:r.get(3)?,dimensions:r.get(4)?,metric:r.get(5)?,element_type:r.get(6)?})).optional())
}
fn digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn operation(conn: &Connection, id: &OperationId) -> Result<String, StorageError> {
    let row: Option<(String, String)> = db(conn
        .query_row(
            "SELECT capability,state FROM operations WHERE id=?1",
            [id.0.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional())?;
    match row {
        Some((cap, state)) if cap == "conventions.index" => Ok(state),
        _ => Err(invalid(
            "convention index requires a real conventions.index operation",
        )),
    }
}
fn active_operation(conn: &Connection, id: &OperationId) -> Result<(), StorageError> {
    if matches!(
        operation(conn, id)?.as_str(),
        "Failed" | "NeedsInput" | "CancelledBeforeDispatch" | "ObservedComplete"
    ) {
        Err(invalid(
            "terminal convention index operation cannot publish",
        ))
    } else {
        Ok(())
    }
}
fn source(
    conn: &Connection,
    index: &StoredConventionIndex,
    expected: i64,
) -> Result<ArtifactRef, StorageError> {
    let owner =
        current(conn, &index.convention_id)?.ok_or_else(|| invalid("convention owner missing"))?;
    if owner.revision != expected {
        return Err(conflict(&owner.id, expected, owner.revision));
    }
    let captured = convention(
        conn,
        &index.convention_id,
        Some(index.convention_revision),
        std::slice::from_ref(&owner.scope),
    )?
    .ok_or_else(|| invalid("captured owner scope changed"))?;
    if owner.lifecycle != "Active"
        || owner.title != captured.title
        || owner.content_ref.as_ref() != Some(&index.source_ref)
        || captured.content_ref.as_ref() != Some(&index.source_ref)
    {
        return Err(invalid("convention body/title/scope changed"));
    }
    let reference = backing(conn, &owner.scope, &index.source_ref)?;
    if reference.sha256.as_str() != index.source_digest {
        return Err(invalid("convention source digest changed"));
    }
    Ok(reference)
}
fn vector_bytes(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|v| v.to_le_bytes()).collect()
}
fn decode_vector(bytes: Vec<u8>) -> Result<Vec<f32>, StorageError> {
    if !bytes.len().is_multiple_of(4) {
        return Err(invalid("corrupt convention vector length"));
    }
    let vector = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect::<Vec<_>>();
    if vector.iter().any(|v| !v.is_finite()) {
        return Err(invalid("nonfinite stored convention vector"));
    }
    Ok(vector)
}

fn section_record(r: &Row<'_>) -> rusqlite::Result<StoredConventionSection> {
    Ok(StoredConventionSection {
        generation_id: identifier(r, 0)?,
        id: identifier(r, 1)?,
        parent_id: r
            .get::<_, Option<String>>(2)?
            .map(Id::new)
            .transpose()
            .map_err(|e| conversion(2, std::io::Error::other(e.to_string())))?,
        heading: r.get(3)?,
        level: r.get(4)?,
        source_offset: r.get(5)?,
        source_length: r.get(6)?,
        token_count: r.get(7)?,
    })
}
const SECTION_COLUMNS: &str =
    "generation_id,id,parent_id,heading,level,source_offset,source_length,token_count";
fn section(
    conn: &Connection,
    g: &Id,
    id: &Id,
) -> Result<Option<StoredConventionSection>, StorageError> {
    db(conn
        .query_row(
            &format!(
                "SELECT {SECTION_COLUMNS} FROM convention_sections WHERE generation_id=?1 AND id=?2"
            ),
            params![g.as_str(), id.as_str()],
            section_record,
        )
        .optional())
}
fn chunk(
    conn: &Connection,
    index: &StoredConventionIndex,
    id: &Id,
) -> Result<Option<StoredConventionChunk>, StorageError> {
    let r=db(conn.query_row("SELECT chunk_index,section_id,text,token_count,vector FROM convention_chunks WHERE generation_id=?1 AND id=?2",params![index.generation_id.as_str(),id.as_str()],|r|Ok((r.get(0)?,identifier(r,1)?,r.get(2)?,r.get(3)?,r.get::<_,Option<Vec<u8>>>(4)?))).optional())?;
    r.map(|r| {
        Ok(StoredConventionChunk {
            generation_id: index.generation_id.clone(),
            id: id.clone(),
            convention_id: index.convention_id.clone(),
            convention_revision: index.convention_revision,
            index: r.0,
            section_id: r.1,
            text: r.2,
            token_count: r.3,
            vector: r.4.map(decode_vector).transpose()?,
        })
    })
    .transpose()
}
const HEADER_JOINS: &str = "convention_indexes g JOIN convention_heads h ON h.id=g.convention_id JOIN convention_versions c ON c.convention_id=h.id AND c.revision=h.revision JOIN convention_versions captured ON captured.convention_id=g.convention_id AND captured.revision=g.convention_revision JOIN artifact_versions av ON av.id=g.source_version_id JOIN artifacts a ON a.id=av.artifact_id JOIN blobs b ON b.digest=av.blob_digest";
const VALID_HEADER: &str = "c.lifecycle='Active' AND c.scope_key=captured.scope_key AND c.title=captured.title AND json(c.content_ref_json)=json(g.source_ref_json) AND json(captured.content_ref_json)=json(g.source_ref_json) AND av.deleted_at IS NULL AND a.state='Live' AND a.current_project_id IS c.project_id AND b.state='Present' AND av.artifact_id=json_extract(g.source_ref_json,'$.artifactId') AND av.blob_digest=g.source_digest AND av.blob_digest=json_extract(g.source_ref_json,'$.sha256') AND av.media_type=json_extract(g.source_ref_json,'$.mediaType') AND b.byte_count=CAST(json_extract(g.source_ref_json,'$.byteLength') AS INTEGER)";
// Body chunks and ancestor postings remain proportional to actual source; only numeric IDs
// and vectors enter candidate materialization. Scoped/current binding precedes every statistic.
// The corpus is read from the published index heads outward (CROSS JOIN keeps that order), so
// a convention without an index costs a search nothing.
fn eligible() -> String {
    format!(
        "WITH RECURSIVE headers AS MATERIALIZED(SELECT g.generation_id,g.convention_id,g.convention_revision,g.status,g.embedding_space,g.source_digest,c.title FROM convention_index_heads ih CROSS JOIN {HEADER_JOINS} WHERE ih.generation_id=g.generation_id AND g.state='Published' AND g.status IN('Ready','LexicalOnly') AND {VALID_HEADER} AND c.scope_key IN(SELECT value FROM json_each(?1,'$.scopes')) AND NOT EXISTS(SELECT 1 FROM json_each(?1,'$.applicability') f WHERE f.value NOT IN(SELECT value FROM json_each(c.applicability_json))) AND (NOT json_extract(?1,'$.filterParent') OR c.parent_id IS json_extract(?1,'$.parent')) AND (json_extract(?1,'$.slug') IS NULL OR c.slug=json_extract(?1,'$.slug')) AND (json_extract(?1,'$.title') IS NULL OR instr(c.title_folded,json_extract(?1,'$.title'))>0) AND (json_extract(?1,'$.name') IS NULL OR convention_like(json_extract(?1,'$.name'),c.slug_folded) OR convention_like(json_extract(?1,'$.name'),c.title_folded))), ancestors(generation_id,section_id,ancestor_id,parent_id,token_count,depth) AS(SELECT s.generation_id,s.id,s.id,s.parent_id,s.token_count,1 FROM convention_sections s JOIN headers h ON h.generation_id=s.generation_id UNION ALL SELECT a.generation_id,a.section_id,s.id,s.parent_id,s.token_count,a.depth+1 FROM ancestors a JOIN convention_sections s ON s.generation_id=a.generation_id AND s.id=a.parent_id WHERE a.depth<4), eligible AS MATERIALIZED(SELECT h.*,k.id,k.chunk_index,k.section_id,k.token_count+coalesce((SELECT sum(a.token_count) FROM ancestors a WHERE a.generation_id=k.generation_id AND a.section_id=k.section_id),0) token_count,k.vector FROM headers h JOIN convention_chunks k ON k.generation_id=h.generation_id), requested_terms AS MATERIALIZED(SELECT d.generation_id,d.id,d.term FROM convention_terms d JOIN headers h ON h.generation_id=d.generation_id WHERE d.term IN(SELECT CASE type WHEN 'object' THEN json_extract(value,'$.term') ELSE value END FROM json_each(?2))), effective_terms AS(SELECT generation_id,chunk_id,term_id,sum(frequency) frequency FROM(SELECT t.generation_id,t.chunk_id,t.term_id,t.frequency FROM convention_chunk_terms t JOIN requested_terms d ON d.generation_id=t.generation_id AND d.id=t.term_id UNION ALL SELECT k.generation_id,k.id,t.term_id,t.frequency FROM convention_section_terms t JOIN requested_terms d ON d.generation_id=t.generation_id AND d.id=t.term_id JOIN ancestors a ON a.generation_id=t.generation_id AND a.ancestor_id=t.section_id JOIN eligible k ON k.generation_id=a.generation_id AND k.section_id=a.section_id) GROUP BY generation_id,chunk_id,term_id)"
    )
}
fn query(q: &ConventionQuery) -> Result<String, StorageError> {
    states(&q.lifecycles)?;
    if !q.lifecycles.is_empty() && !q.lifecycles.iter().any(|s| s == "Active") {
        return Ok(json!({"scopes":[],"applicability":[]}).to_string());
    }
    let keys: Value = serde_json::from_str(&scopes(&q.scopes)?).expect("serialized scopes");
    Ok(json!({"scopes":keys,"applicability":q.applicability,"filterParent":q.parent_id.is_some(),"parent":q.parent_id.as_ref().and_then(|p|p.as_ref()).map(Id::as_str),"slug":q.slug,"title":q.title.as_ref().map(|s|s.to_lowercase()),"name":q.name_pattern.as_ref().map(|s|s.to_lowercase())}).to_string())
}
fn valid_term(term: &str) -> bool {
    let mut b = term.bytes();
    b.next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && b.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}
fn statistics(
    conn: &Connection,
    q: &ConventionQuery,
    requested: &[String],
) -> Result<ConventionLexicalStatistics, StorageError> {
    if requested.len() > 4096
        || requested.iter().any(|t| !valid_term(t))
        || requested.iter().collect::<HashSet<_>>().len() != requested.len()
    {
        return Err(invalid("invalid lexical terms"));
    }
    let q = query(q)?;
    let e = eligible();
    let (documents, total): (i64, i64) = db(conn.query_row(
        &format!("{e} SELECT count(*),coalesce(sum(token_count),0) FROM eligible"),
        params![q, json!(requested).to_string()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ))?;
    let mut stmt=db(conn.prepare_cached(&format!("{e} SELECT requested.value,count(t.chunk_id) FROM json_each(?2) requested LEFT JOIN(SELECT d.term,t.chunk_id FROM effective_terms t JOIN convention_terms d ON d.generation_id=t.generation_id AND d.id=t.term_id) t ON t.term=requested.value GROUP BY requested.value ORDER BY requested.value")))?;
    let frequencies = db(db(
        stmt.query_map(params![q, json!(requested).to_string()], |r| {
            Ok(ConventionTerm {
                term: r.get(0)?,
                frequency: r.get(1)?,
            })
        }),
    )?
    .collect())?;
    Ok(ConventionLexicalStatistics {
        documents: u64::try_from(documents).map_err(|_| invalid("negative corpus"))?,
        total_tokens: u64::try_from(total).map_err(|_| invalid("negative tokens"))?,
        document_frequencies: frequencies,
    })
}
fn ranked(
    conn: &Connection,
    q: &ConventionQuery,
    weighted: &[ConventionWeightedTerm],
    average: f64,
    space_id: Option<&str>,
    vector: Option<&[f32]>,
    limit: u16,
) -> Result<Vec<ConventionRankedChunk>, StorageError> {
    if limit == 0
        || limit > 50
        || !average.is_finite()
        || average < 1.0
        || space_id.is_some() != vector.is_some()
        || weighted.len() > 4096
        || weighted
            .iter()
            .any(|t| !valid_term(&t.term) || !t.idf.is_finite() || t.idf < 0.0)
        || weighted
            .iter()
            .map(|t| &t.term)
            .collect::<HashSet<_>>()
            .len()
            != weighted.len()
    {
        return Err(invalid("invalid convention ranking parameters"));
    }
    if let (Some(id), Some(vector)) = (space_id, vector) {
        let space = space(conn, id)?.ok_or_else(|| invalid("embedding space missing"))?;
        validate_vector(vector, space.dimensions)?;
    }
    let q = query(q)?;
    let excerpt_query = weighted
        .iter()
        .map(|term| term.term.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let weighted = json!(
        weighted
            .iter()
            .map(|t| json!({"term":t.term,"idf":t.idf}))
            .collect::<Vec<_>>()
    )
    .to_string();
    let eligible = eligible();
    let compatible_dense =
        "?4 IS NOT NULL AND e.embedding_space=?4 AND e.vector IS NOT NULL AND e.status='Ready'";
    let dense_score = format!(
        "CASE WHEN {compatible_dense} THEN max(0.0,1.0-vec_distance_cosine(e.vector,?5)) ELSE 0.0 END"
    );
    // Correct the frozen sparse-score approximation: top4K limits dense-only candidate
    // discovery, never the actual compatible cosine of an admitted lexical match.
    // A positive dense-only row below the cutoff has at least 4K included rows with
    // no smaller cosine/nonnegative lexical contribution; their stable tie order matches.
    let sql = format!(
        "{eligible}, lexical AS MATERIALIZED (SELECT e.generation_id,e.id,sum(json_extract(w.value,'$.idf')*(t.frequency*2.2)/(t.frequency+1.2*(0.25+0.75*e.token_count/?3))) lex FROM eligible e JOIN effective_terms t ON t.generation_id=e.generation_id AND t.chunk_id=e.id JOIN convention_terms td ON td.generation_id=t.generation_id AND td.id=t.term_id JOIN json_each(?2) w ON json_extract(w.value,'$.term')=td.term GROUP BY e.generation_id,e.id), lexical_max AS(SELECT coalesce(max(lex),0) maximum FROM lexical), dense AS MATERIALIZED (SELECT e.generation_id,e.id,{dense_score} dense FROM eligible e WHERE {compatible_dense} ORDER BY dense DESC,e.title COLLATE BINARY,e.convention_id,e.generation_id,e.chunk_index,e.id LIMIT ?6), candidates AS(SELECT generation_id,id FROM lexical WHERE lex>0 UNION SELECT generation_id,id FROM dense WHERE dense>0), candidate_scores AS MATERIALIZED (SELECT e.generation_id,e.id,e.title,e.convention_id,e.chunk_index,coalesce(l.lex,0.0) lex,{dense_score} dense FROM candidates n JOIN eligible e ON e.generation_id=n.generation_id AND e.id=n.id LEFT JOIN lexical l ON l.generation_id=e.generation_id AND l.id=e.id), scored AS (SELECT c.*,0.35*(CASE WHEN m.maximum>0 THEN c.lex/m.maximum ELSE 0.0 END)+0.65*c.dense score FROM candidate_scores c CROSS JOIN lexical_max m) SELECT generation_id,id,lex,dense,score FROM scored WHERE score>0 ORDER BY score DESC,title COLLATE BINARY,convention_id,generation_id,chunk_index,id LIMIT ?7"
    );
    let mut stmt = db(conn.prepare_cached(&sql))?;
    let rows = db(db(stmt.query_map(
        params![
            q,
            weighted,
            average,
            space_id,
            vector.map(vector_bytes),
            i64::from(limit) * 4,
            i64::from(limit)
        ],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, f64>(3)?,
                r.get::<_, f64>(4)?,
            ))
        },
    ))?
    .collect::<rusqlite::Result<Vec<_>>>())?;
    rows.into_iter()
        .map(|r| {
            let id = Id::new(r.0).map_err(|_| invalid("invalid generation"))?;
            let index = generation(conn, &id)?.ok_or_else(|| invalid("index disappeared"))?;
            let id = Id::new(r.1).map_err(|_| invalid("invalid chunk"))?;
            Ok(ConventionRankedChunk {
                chunk: compact_chunk(conn, &index, &id, &excerpt_query)?,
                lexical_score: r.2,
                dense_score: r.3,
                score: r.4,
            })
        })
        .collect()
}
fn compact_chunk(
    conn: &Connection,
    index: &StoredConventionIndex,
    id: &Id,
    query: &str,
) -> Result<ConventionChunkMatch, StorageError> {
    db(conn.query_row("SELECT chunk_index,section_id,text FROM convention_chunks WHERE generation_id=?1 AND id=?2",params![index.generation_id.as_str(),id.as_str()],|r|{let text=r.get_ref(2)?.as_str()?;Ok(ConventionChunkMatch{generation_id:index.generation_id.clone(),id:id.clone(),convention_id:index.convention_id.clone(),convention_revision:index.convention_revision,index:r.get(0)?,section_id:identifier(r,1)?,source_digest:index.source_digest.clone(),excerpt:devworks_core::services::conventions::indexing::rank::excerpt(text,query)})}))
}
fn validate_vector(vector: &[f32], dimensions: u32) -> Result<(), StorageError> {
    if vector.len() != dimensions as usize
        || vector.is_empty()
        || vector.iter().any(|v| !v.is_finite())
        || vector.iter().all(|v| *v == 0.0)
    {
        Err(invalid(
            "convention vector must match exact space, finite and nonzero",
        ))
    } else {
        Ok(())
    }
}
macro_rules! reads {($ty:ty)=>{impl ConventionIndexRead for $ty {
 fn convention_section(&self,g:&Id,id:&Id)->Result<Option<StoredConventionSection>,StorageError>{section(self.conn,g,id)}
 fn convention_sections(&self,g:&Id,after:Option<&Id>,limit:u16)->Result<Vec<StoredConventionSection>,StorageError>{let limit=page(limit)?;let mut stmt=db(self.conn.prepare_cached(&format!("SELECT {SECTION_COLUMNS} FROM convention_sections WHERE generation_id=?1 AND (?2 IS NULL OR id>?2) ORDER BY id LIMIT ?3")))?;db(db(stmt.query_map(params![g.as_str(),after.map(Id::as_str),limit],section_record))?.collect())}
 fn convention_chunk(&self,g:&Id,id:&Id)->Result<Option<StoredConventionChunk>,StorageError>{match generation(self.conn,g)?{Some(index)=>chunk(self.conn,&index,id),None=>Ok(None)}}
    fn convention_chunk_count(&self,id:&Id)->Result<u64,StorageError>{let count:i64=db(self.conn.query_row("SELECT count(*) FROM convention_chunks WHERE generation_id=?1",[id.as_str()],|r|r.get(0)))?;u64::try_from(count).map_err(|_|invalid("negative chunk count"))}
    fn convention_index_status(&self,id:&Id)->Result<Option<StoredConventionIndex>,StorageError>{index_status(self.conn,id)}
    fn convention_index_revision(&self,scopes:&[KnowledgeScope])->Result<i64,StorageError>{index_revision(self.conn,scopes)}
    fn convention_embedding_inputs(&self,id:&Id)->Result<Option<StoredConventionEmbeddingInputs>,StorageError>{embedding_inputs(self.conn,id)}
    fn convention_embedding_space(&self,id:&str)->Result<Option<StoredConventionEmbeddingSpace>,StorageError>{space(self.conn,id)}
    fn convention_index_head(&self,id:&Id)->Result<Option<StoredConventionIndex>,StorageError>{published_head(self.conn,id)}
    fn convention_index_gaps(&self,q:&ConventionQuery,space:Option<&str>,limit:u16)->Result<Vec<(Id,String)>,StorageError>{gaps(self.conn,q,space,limit)}
    fn convention_index_generation(&self,id:&Id)->Result<Option<StoredConventionIndex>,StorageError>{generation(self.conn,id)}
    fn convention_index_generations(&self,owner:Option<&Id>,operation:Option<&OperationId>,after:Option<&Id>,limit:u16)->Result<Vec<StoredConventionIndex>,StorageError>{let limit=page(limit)?;let mut s=db(self.conn.prepare_cached(&format!("SELECT {INDEX_COLUMNS} FROM convention_indexes WHERE (?1 IS NULL OR convention_id=?1) AND (?2 IS NULL OR operation_id=?2) AND (?3 IS NULL OR generation_id>?3) ORDER BY generation_id LIMIT ?4")))?;db(db(s.query_map(params![owner.map(Id::as_str),operation.map(|o|o.0.as_str()),after.map(Id::as_str),limit],index_record))?.collect())}
    fn convention_chunks(&self,id:&Id,after:Option<i64>,limit:u16)->Result<Vec<StoredConventionChunk>,StorageError>{let limit=page(limit)?;if after.is_some_and(|v|v<0){return Err(invalid("negative chunk cursor"));}let Some(index)=generation(self.conn,id)?else{return Ok(Vec::new());};let mut s=db(self.conn.prepare_cached("SELECT id FROM convention_chunks WHERE generation_id=?1 AND chunk_index>coalesce(?2,-1) ORDER BY chunk_index LIMIT ?3"))?;let ids=db(db(s.query_map(params![id.as_str(),after,limit],|r|identifier(r,0)))?.collect::<rusqlite::Result<Vec<_>>>())?;ids.into_iter().map(|id|chunk(self.conn,&index,&id)?.ok_or_else(||invalid("chunk disappeared"))).collect()}
    fn convention_lexical_statistics(&self,q:&ConventionQuery,terms:&[String])->Result<ConventionLexicalStatistics,StorageError>{statistics(self.conn,q,terms)}
    fn convention_ranked_chunks(&self,q:&ConventionQuery,terms:&[ConventionWeightedTerm],average:f64,space:Option<&str>,vector:Option<&[f32]>,limit:u16)->Result<Vec<ConventionRankedChunk>,StorageError>{ranked(self.conn,q,terms,average,space,vector,limit)}
}};}
reads!(Snapshot<'_>);
reads!(Unit<'_>);

fn same_source(a: &StoredConventionIndex, b: &StoredConventionIndex) -> bool {
    a.generation_id == b.generation_id
        && a.convention_id == b.convention_id
        && a.convention_revision == b.convention_revision
        && a.source_ref == b.source_ref
        && a.operation_id == b.operation_id
        && a.embedding_space == b.embedding_space
        && a.source_digest == b.source_digest
        && a.created_at_ms == b.created_at_ms
}
fn valid_status(status: &str) -> bool {
    matches!(
        status,
        "Queued" | "Indexing" | "Ready" | "LexicalOnly" | "Stale" | "Failed" | "Cancelled"
    )
}
pub(super) fn invalidate(conn: &Connection, owner: &Id, now: i64) -> Result<(), StorageError> {
    db(conn.execute("UPDATE convention_indexes SET status='Stale',updated_at=max(updated_at,?2) WHERE convention_id=?1 AND state='Draft' AND status IN('Queued','Indexing')",params![owner.as_str(),now]))?;
    bump_index_scope(conn, owner)
}
fn staging(conn: &Connection, id: &Id) -> Result<StoredConventionIndex, StorageError> {
    let g = generation(conn, id)?.ok_or_else(|| invalid("generation missing"))?;
    if g.state != "Draft" || !matches!(g.status.as_str(), "Queued" | "Indexing") {
        return Err(invalid("generation is not staging"));
    }
    active_operation(conn, &g.operation_id)?;
    Ok(g)
}
impl ConventionIndexRepository for Unit<'_> {
    fn insert_convention_embedding_inputs(
        &mut self,
        r: &StoredConventionEmbeddingInputs,
    ) -> Result<StoredConventionEmbeddingInputs, StorageError> {
        if !digest(&r.context_digest)
            || !digest(&r.input_digest)
            || r.created_at_ms < 0
            || r.input_refs.is_empty()
            || r.input_refs.len() > 32
            || space(self.conn, &r.embedding_space)?.is_none()
        {
            return Err(invalid("invalid immutable convention embedding inputs"));
        }
        let scope = r
            .project_id
            .clone()
            .map_or(KnowledgeScope::Workspace, KnowledgeScope::Project);
        let mut descriptors = Vec::with_capacity(r.input_refs.len());
        for reference in &r.input_refs {
            let reference = backing(self.conn, &scope, reference)?;
            let (provenance, owner) =
                trusted_derivative(self.conn, &reference, &self.info.host_actor)?;
            let derived = &provenance["derived"];
            if owner.as_deref() != Some(r.actor_id.0.as_str())
                || derived["kind"] != "ConventionEmbeddingInput"
                || derived["requestId"].as_str() != Some(r.request_id.as_str())
                || derived["contextDigest"] != r.context_digest
                || derived["embeddingSpace"] != r.embedding_space
                || derived["inputDigest"] != r.input_digest
                || derived["inputIndex"].as_u64() != Some(descriptors.len() as u64)
            {
                return Err(invalid(
                    "embedding input artifact lacks its exact owned request association",
                ));
            }
            descriptors
                .push(json!({"sha256":reference.sha256,"byteCount":reference.byte_length.get()}));
        }
        let canonical = devworks_core::jcs::canonicalize(&json!(descriptors))
            .map_err(|_| invalid("invalid input descriptors"))?;
        if devworks_core::fingerprint::sha256_hex(canonical.as_bytes()) != r.input_digest {
            return Err(invalid(
                "convention input digest differs from exact ordered artifact bytes",
            ));
        }
        if let Some(old) = embedding_inputs(self.conn, &r.request_id)? {
            return if old.actor_id == r.actor_id
                && old.project_id == r.project_id
                && old.context_digest == r.context_digest
                && old.embedding_space == r.embedding_space
                && old.input_digest == r.input_digest
            {
                Ok(old)
            } else {
                Err(invalid(
                    "immutable convention embedding request binding changed",
                ))
            };
        }
        atomic(self.conn, || {
            db(self.conn.execute("INSERT INTO convention_embedding_inputs(request_id,actor_id,project_id,context_digest,embedding_space,input_digest,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![r.request_id.as_str(),r.actor_id.0.as_str(),r.project_id.as_ref().map(|p|p.0.as_str()),r.context_digest,r.embedding_space,r.input_digest,r.created_at_ms]))?;
            for (i, reference) in r.input_refs.iter().enumerate() {
                db(self.conn.execute("INSERT INTO convention_embedding_input_refs(request_id,ordinal,artifact_id,version_id,reference_json) VALUES(?1,?2,?3,?4,?5)",params![r.request_id.as_str(),i as i64,reference["artifactId"].as_str(),reference["versionId"].as_str(),reference.to_string()]))?;
            }
            Ok(r.clone())
        })
    }

    fn put_convention_embedding_space(
        &mut self,
        s: &StoredConventionEmbeddingSpace,
    ) -> Result<(), StorageError> {
        if !text(&s.id, 1, 4096)
            || !text(&s.provider, 1, 4096)
            || !text(&s.model, 1, 4096)
            || !digest(&s.configuration_digest)
            || s.dimensions == 0
            || s.metric != "Cosine"
            || s.element_type != "Float32"
        {
            return Err(invalid("invalid exact convention embedding space"));
        }
        if let Some(old) = space(self.conn, &s.id)? {
            return if old == *s {
                Ok(())
            } else {
                Err(invalid("embedding space identity is immutable"))
            };
        }
        db(self.conn.execute("INSERT INTO convention_embedding_spaces(id,provider,model,configuration_digest,dimensions,metric,element_type) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![s.id,s.provider,s.model,s.configuration_digest,s.dimensions,s.metric,s.element_type]))?;
        Ok(())
    }
    fn stage_convention_index(&mut self, r: &StoredConventionIndex) -> Result<(), StorageError> {
        if r.state != "Draft"
            || r.status != "Queued"
            || r.error.is_some()
            || r.convention_revision < 1
            || !digest(&r.source_digest)
            || r.created_at_ms < 0
            || r.updated_at_ms != r.created_at_ms
        {
            return Err(invalid("invalid index generation"));
        }
        if let Some(old) = generation(self.conn, &r.generation_id)? {
            return if same_source(&old, r) {
                Ok(())
            } else {
                Err(invalid("immutable index source changed"))
            };
        }
        active_operation(self.conn, &r.operation_id)?;
        let revision = current(self.conn, &r.convention_id)?
            .ok_or_else(|| invalid("convention owner missing"))?
            .revision;
        let reference = source(self.conn, r, revision)?;
        if let Some(id) = &r.embedding_space {
            space(self.conn, id)?.ok_or_else(|| invalid("embedding space missing"))?;
        }
        atomic(self.conn, || {
            db(self.conn.execute("INSERT INTO convention_indexes(generation_id,convention_id,convention_revision,source_ref_json,source_version_id,operation_id,state,status,error,embedding_space,source_digest,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,'Draft','Queued',NULL,?7,?8,?9,?9)",params![r.generation_id.as_str(),r.convention_id.as_str(),r.convention_revision,r.source_ref.to_string(),reference.version_id.as_str(),r.operation_id.0.as_str(),r.embedding_space,r.source_digest,r.created_at_ms]))?;
            bump_index_scope(self.conn, &r.convention_id)
        })
    }
    fn stage_convention_section(
        &mut self,
        s: &StoredConventionSection,
    ) -> Result<(), StorageError> {
        let index = staging(self.conn, &s.generation_id)?;
        if s.token_count < 0 || s.level > 3 {
            return Err(invalid("invalid section"));
        }
        if s.level == 0 {
            let owner = convention(
                self.conn,
                &index.convention_id,
                Some(index.convention_revision),
                &[current(self.conn, &index.convention_id)?
                    .ok_or_else(|| invalid("owner missing"))?
                    .scope],
            )?
            .ok_or_else(|| invalid("captured owner missing"))?;
            if s.parent_id.is_some()
                || s.heading.as_ref() != Some(&owner.title)
                || s.source_offset.is_some()
                || s.source_length.is_some()
            {
                return Err(invalid("section root must be exact captured title"));
            }
        } else {
            let p = s
                .parent_id
                .as_ref()
                .and_then(|id| section(self.conn, &s.generation_id, id).transpose())
                .transpose()?
                .ok_or_else(|| invalid("section parent missing"))?;
            let reference: ArtifactRef = serde_json::from_value(index.source_ref)
                .map_err(|_| invalid("source reference invalid"))?;
            if p.level >= s.level
                || s.heading.as_ref().is_some_and(|heading| {
                    heading.len() > 64 * 1024
                        || !text(heading, 0, 65536)
                        || Some(heading.len() as i64) != s.source_length
                })
                || s.source_offset.is_none_or(|o| o < 0)
                || s.source_length.is_none_or(|n| n < 0)
                || s.source_offset
                    .zip(s.source_length)
                    .and_then(|(o, n)| o.checked_add(n))
                    .is_none_or(|end| end > reference.byte_length.get())
            {
                return Err(invalid("invalid section source span/ancestor"));
            }
        }
        if let Some((offset, length)) = s.source_offset.zip(s.source_length) {
            let overlap:bool=db(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM convention_sections WHERE generation_id=?1 AND id!=?2 AND level>0 AND (source_offset=?3 OR (source_offset<?4 AND ?3<source_offset+source_length)))",params![s.generation_id.as_str(),s.id.as_str(),offset,offset+length],|r|r.get(0)))?;
            if overlap {
                return Err(invalid(
                    "section source spans must be distinct and nonoverlapping",
                ));
            }
        }
        if let Some(old) = section(self.conn, &s.generation_id, &s.id)? {
            return if old == *s {
                Ok(())
            } else {
                Err(invalid("immutable section changed"))
            };
        }
        db(self.conn.execute("INSERT INTO convention_sections(generation_id,id,parent_id,heading,level,source_offset,source_length,token_count) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![s.generation_id.as_str(),s.id.as_str(),s.parent_id.as_ref().map(Id::as_str),s.heading,s.level,s.source_offset,s.source_length,s.token_count]))?;
        Ok(())
    }
    fn stage_convention_section_terms(
        &mut self,
        g: &Id,
        id: &Id,
        batch: i64,
        digest: &str,
        terms: &[ConventionTerm],
    ) -> Result<(), StorageError> {
        stage_terms(self.conn, g, "Section", id, batch, digest, terms)
    }
    fn stage_convention_chunk_terms(
        &mut self,
        g: &Id,
        id: &Id,
        batch: i64,
        digest: &str,
        terms: &[ConventionTerm],
    ) -> Result<(), StorageError> {
        stage_terms(self.conn, g, "Chunk", id, batch, digest, terms)
    }
    fn stage_convention_chunks(
        &mut self,
        g: &Id,
        chunks: &[StoredConventionChunk],
    ) -> Result<(), StorageError> {
        if chunks.len() > usize::from(MAX_PAGE) {
            return Err(invalid("chunk batch exceeds bound"));
        }
        let index = staging(self.conn, g)?;
        let dimensions = index
            .embedding_space
            .as_deref()
            .map(|s| space(self.conn, s)?.ok_or_else(|| invalid("space missing")))
            .transpose()?
            .map(|s| s.dimensions);
        atomic(self.conn, || {
            for c in chunks {
                if &c.generation_id != g
                    || c.convention_id != index.convention_id
                    || c.convention_revision != index.convention_revision
                    || c.index < 0
                    || c.token_count < 0
                    || c.text.len() > 16_777_215
                    || section(self.conn, g, &c.section_id)?.is_none()
                {
                    return Err(invalid("invalid chunk source"));
                }
                if let Some(v) = &c.vector {
                    validate_vector(v, dimensions.ok_or_else(|| invalid("vector lacks space"))?)?;
                }
                if let Some(old) = chunk(self.conn, &index, &c.id)? {
                    if old == *c {
                        continue;
                    }
                    return Err(invalid("immutable chunk changed"));
                }
                db(self.conn.execute("INSERT INTO convention_chunks(generation_id,id,chunk_index,section_id,text,token_count,vector) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![g.as_str(),c.id.as_str(),c.index,c.section_id.as_str(),c.text,c.token_count,c.vector.as_deref().map(vector_bytes)]))?;
            }
            Ok(())
        })
    }
    fn set_convention_index_status(
        &mut self,
        id: &Id,
        op: &OperationId,
        expected: &str,
        status: &str,
        error: Option<&str>,
    ) -> Result<(), StorageError> {
        let index =
            generation(self.conn, id)?.ok_or_else(|| invalid("convention generation missing"))?;
        if &index.operation_id != op
            || index.status != expected
            || !valid_status(status)
            || error.is_some_and(|e| !text(e, 1, 4096))
        {
            return Err(invalid("convention index status CAS refused"));
        }
        let transition = index.status == status
            || match index.status.as_str() {
                "Queued" => matches!(status, "Indexing" | "Failed" | "Cancelled" | "Stale"),
                "Indexing" => matches!(status, "Failed" | "Cancelled" | "Stale"),
                _ => false,
            };
        if !transition || index.state != "Draft" || matches!(status, "Ready" | "LexicalOnly") {
            return Err(invalid("only atomic index publication sets ready status"));
        }
        if status != "Cancelled" && status != "Failed" {
            active_operation(self.conn, op)?;
        }
        atomic(self.conn, || {
            db(self.conn.execute("UPDATE convention_indexes SET status=?2,error=?3,updated_at=max(updated_at,?4) WHERE generation_id=?1 AND status=?5",params![id.as_str(),status,error,self.now,expected]))?;
            bump_index_scope(self.conn, &index.convention_id)?;
            Ok(())
        })
    }
    fn publish_convention_index(
        &mut self,
        g: &Id,
        expected: i64,
        expected_head: Option<&Id>,
        count: i64,
    ) -> Result<(), StorageError> {
        let index = staging(self.conn, g)?;
        source(self.conn, &index, expected)?;
        if count < 0
            || head(self.conn, &index.convention_id)?
                .as_ref()
                .map(|g| &g.generation_id)
                != expected_head
        {
            return Err(invalid("index head/count CAS refused"));
        }
        let (actual,first,last,vectors):(i64,Option<i64>,Option<i64>,i64)=db(self.conn.query_row("SELECT count(*),min(chunk_index),max(chunk_index),count(vector) FROM convention_chunks WHERE generation_id=?1",[g.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))))?;
        let incomplete:bool=db(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM convention_chunks c WHERE c.generation_id=?1 AND c.token_count!=coalesce((SELECT sum(t.frequency) FROM convention_chunk_terms t WHERE t.generation_id=c.generation_id AND t.chunk_id=c.id),0)) OR EXISTS(SELECT 1 FROM convention_sections s WHERE s.generation_id=?1 AND s.token_count!=coalesce((SELECT sum(t.frequency) FROM convention_section_terms t WHERE t.generation_id=s.generation_id AND t.section_id=s.id),0)) OR NOT EXISTS(SELECT 1 FROM convention_sections WHERE generation_id=?1 AND level=0)",[g.as_str()],|r|r.get(0)))?;
        if actual != count
            || (count > 0 && (first != Some(0) || last != count.checked_sub(1)))
            || incomplete
        {
            return Err(invalid("generation chunks/sections/postings incomplete"));
        }
        let ready = index.embedding_space.is_some() && vectors == count;
        let status = if ready { "Ready" } else { "LexicalOnly" };
        atomic(self.conn, || {
            db(self.conn.execute("UPDATE convention_indexes SET state='Published',status=?2,error=CASE WHEN ?2='Ready' THEN NULL ELSE error END,updated_at=max(updated_at,?3) WHERE generation_id=?1",params![g.as_str(),status,self.now]))?;
            db(self.conn.execute("INSERT INTO convention_index_heads(convention_id,generation_id) VALUES(?1,?2) ON CONFLICT(convention_id) DO UPDATE SET generation_id=excluded.generation_id",params![index.convention_id.as_str(),g.as_str()]))?;
            bump_index_scope(self.conn, &index.convention_id)
        })
    }
    fn discard_convention_index_chunks(
        &mut self,
        g: &Id,
        op: &OperationId,
        limit: u16,
    ) -> Result<bool, StorageError> {
        let limit = page(limit)?;
        let index = generation(self.conn, g)?.ok_or_else(|| invalid("generation missing"))?;
        let current: bool = db(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM convention_index_heads WHERE generation_id=?1)",
            [g.as_str()],
            |r| r.get(0),
        ))?;
        if &index.operation_id != op || current {
            return Err(invalid("current/mismatched generation cleanup refused"));
        }
        if index.state == "Draft"
            && !matches!(
                operation(self.conn, op)?.as_str(),
                "Failed" | "NeedsInput" | "CancelledBeforeDispatch" | "ObservedComplete"
            )
        {
            return Err(invalid("live draft cleanup refused"));
        }
        atomic(self.conn, || {
            db(self.conn.execute("DELETE FROM convention_term_batches WHERE generation_id=?1 AND kind='Chunk' AND owner_id IN(SELECT id FROM convention_chunks WHERE generation_id=?1 ORDER BY chunk_index LIMIT ?2)",params![g.as_str(),limit]))?;
            db(self.conn.execute("DELETE FROM convention_chunks WHERE generation_id=?1 AND id IN(SELECT id FROM convention_chunks WHERE generation_id=?1 ORDER BY chunk_index LIMIT ?2)",params![g.as_str(),limit]))?;
            // Parent-first staging, leaf-first bounded cleanup keeps every surviving span resolvable.
            db(self.conn.execute("DELETE FROM convention_term_batches WHERE generation_id=?1 AND kind='Section' AND owner_id IN(SELECT s.id FROM convention_sections s WHERE s.generation_id=?1 AND NOT EXISTS(SELECT 1 FROM convention_sections child WHERE child.generation_id=s.generation_id AND child.parent_id=s.id) AND NOT EXISTS(SELECT 1 FROM convention_chunks c WHERE c.generation_id=s.generation_id AND c.section_id=s.id) ORDER BY s.id LIMIT ?2)",params![g.as_str(),limit]))?;
            db(self.conn.execute("DELETE FROM convention_sections WHERE generation_id=?1 AND id IN(SELECT s.id FROM convention_sections s WHERE s.generation_id=?1 AND NOT EXISTS(SELECT 1 FROM convention_sections child WHERE child.generation_id=s.generation_id AND child.parent_id=s.id) AND NOT EXISTS(SELECT 1 FROM convention_chunks c WHERE c.generation_id=s.generation_id AND c.section_id=s.id) ORDER BY s.id LIMIT ?2)",params![g.as_str(),limit]))?;
            db(self.conn.execute("DELETE FROM convention_terms WHERE generation_id=?1 AND id IN(SELECT d.id FROM convention_terms d WHERE d.generation_id=?1 AND NOT EXISTS(SELECT 1 FROM convention_chunk_terms t WHERE t.generation_id=d.generation_id AND t.term_id=d.id) AND NOT EXISTS(SELECT 1 FROM convention_section_terms t WHERE t.generation_id=d.generation_id AND t.term_id=d.id) ORDER BY d.id LIMIT ?2)",params![g.as_str(),limit]))?;
            let remaining:bool=db(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM convention_chunks WHERE generation_id=?1) OR EXISTS(SELECT 1 FROM convention_sections WHERE generation_id=?1) OR EXISTS(SELECT 1 FROM convention_terms WHERE generation_id=?1)",[g.as_str()],|r|r.get(0)))?;
            Ok(!remaining)
        })
    }
}
fn stage_terms(
    conn: &Connection,
    g: &Id,
    kind: &str,
    id: &Id,
    batch: i64,
    claimed: &str,
    terms: &[ConventionTerm],
) -> Result<(), StorageError> {
    staging(conn, g)?;
    let total = terms
        .iter()
        .try_fold(0_u64, |s, t| s.checked_add(u64::from(t.frequency)))
        .ok_or_else(|| invalid("term count overflow"))?;
    if batch < 0
        || total == 0
        || total > 500
        || terms
            .iter()
            .any(|t| !valid_term(&t.term) || t.frequency == 0)
        || terms.windows(2).any(|w| w[0].term >= w[1].term)
    {
        return Err(invalid("invalid bounded sorted term batch"));
    }
    let canonical = devworks_core::jcs::canonicalize(&json!(terms))
        .map_err(|_| invalid("term batch encoding"))?;
    if devworks_core::fingerprint::sha256_hex(canonical.as_bytes()) != claimed {
        return Err(invalid("term batch digest mismatch"));
    }
    let old:Option<String>=db(conn.query_row("SELECT digest FROM convention_term_batches WHERE generation_id=?1 AND kind=?2 AND owner_id=?3 AND batch_index=?4",params![g.as_str(),kind,id.as_str(),batch],|r|r.get(0)).optional())?;
    if let Some(old) = old {
        return if old == claimed {
            Ok(())
        } else {
            Err(invalid("immutable term batch changed"))
        };
    }
    let (table, column) = if kind == "Section" {
        ("convention_section_terms", "section_id")
    } else {
        ("convention_chunk_terms", "chunk_id")
    };
    let declared: Option<i64> = db(conn
        .query_row(
            if kind == "Section" {
                "SELECT token_count FROM convention_sections WHERE generation_id=?1 AND id=?2"
            } else {
                "SELECT token_count FROM convention_chunks WHERE generation_id=?1 AND id=?2"
            },
            params![g.as_str(), id.as_str()],
            |r| r.get(0),
        )
        .optional())?;
    let declared = declared.ok_or_else(|| invalid("posting owner missing"))?;
    let previous: i64 = db(conn.query_row(
        &format!(
            "SELECT coalesce(sum(frequency),0) FROM {table} WHERE generation_id=?1 AND {column}=?2"
        ),
        params![g.as_str(), id.as_str()],
        |r| r.get(0),
    ))?;
    if previous
        .checked_add(total as i64)
        .is_none_or(|n| n > declared)
    {
        return Err(invalid("postings exceed declared source tokens"));
    }
    atomic(conn, || {
        for t in terms {
            db(conn.execute("INSERT INTO convention_terms(generation_id,id,term) SELECT ?1,coalesce(max(id),0)+1,?2 FROM convention_terms WHERE generation_id=?1 ON CONFLICT(generation_id,term) DO NOTHING",params![g.as_str(),t.term]))?;
            db(conn.execute(&format!("INSERT INTO {table}(generation_id,{column},term_id,frequency) SELECT ?1,?2,id,?4 FROM convention_terms WHERE generation_id=?1 AND term=?3 ON CONFLICT(generation_id,{column},term_id) DO UPDATE SET frequency=frequency+excluded.frequency"),params![g.as_str(),id.as_str(),t.term,t.frequency]))?;
        }
        db(conn.execute("INSERT INTO convention_term_batches(generation_id,kind,owner_id,batch_index,digest) VALUES(?1,?2,?3,?4,?5)",params![g.as_str(),kind,id.as_str(),batch,claimed]))?;
        Ok(())
    })
}
fn embedding_inputs(
    conn: &Connection,
    id: &Id,
) -> Result<Option<StoredConventionEmbeddingInputs>, StorageError> {
    let row=db(conn.query_row("SELECT actor_id,project_id,context_digest,embedding_space,input_digest,created_at FROM convention_embedding_inputs WHERE request_id=?1",[id.as_str()],|r|Ok((identifier(r,0)?,r.get::<_,Option<String>>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,i64>(5)?))).optional())?;
    row.map(|row| {
        let mut stmt=db(conn.prepare_cached("SELECT reference_json FROM convention_embedding_input_refs WHERE request_id=?1 ORDER BY ordinal"))?;
        let refs=db(db(stmt.query_map([id.as_str()],|r|value(r,0)))?.collect())?;
        Ok(StoredConventionEmbeddingInputs {request_id:id.clone(),actor_id:ActorId(row.0),project_id:row.1.map(Id::new).transpose().map_err(|_|invalid("invalid embedding input project"))?.map(ProjectId),context_digest:row.2,embedding_space:row.3,input_digest:row.4,input_refs:refs,created_at_ms:row.5})
    }).transpose()
}

fn trusted_derivative(
    conn: &Connection,
    reference: &ArtifactRef,
    host: &ActorId,
) -> Result<(Value, Option<String>), StorageError> {
    let (provenance, owner): (Value, Option<String>) = db(conn.query_row(
        "SELECT provenance_json,owner_actor_id FROM artifacts WHERE id=?1",
        [reference.artifact_id.as_str()],
        |r| Ok((value(r, 0)?, r.get(1)?)),
    ))?;
    if provenance["principalKind"] != "Host"
        || provenance["producedBy"].as_str() != Some(host.0.as_str())
    {
        return Err(invalid(
            "convention derivative must name its actual producing Host",
        ));
    }
    Ok((provenance, owner))
}

fn index_status(
    conn: &Connection,
    owner: &Id,
) -> Result<Option<StoredConventionIndex>, StorageError> {
    let pending:Option<Id>=db(conn.query_row(&format!("SELECT g.generation_id FROM {HEADER_JOINS} JOIN operations o ON o.id=g.operation_id WHERE g.convention_id=?1 AND {VALID_HEADER} AND g.state='Draft' AND g.status IN('Queued','Indexing') AND o.state NOT IN('Failed','NeedsInput','CancelledBeforeDispatch','ObservedComplete') ORDER BY g.created_at DESC,g.generation_id DESC LIMIT 1"),[owner.as_str()],|r|identifier(r,0)).optional())?;
    if let Some(id) = pending {
        return generation(conn, &id);
    }
    published_head(conn, owner)
}
fn published_head(
    conn: &Connection,
    owner: &Id,
) -> Result<Option<StoredConventionIndex>, StorageError> {
    let Some(mut r) = head(conn, owner)? else {
        return Ok(None);
    };
    let valid:bool=db(conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {HEADER_JOINS} WHERE g.generation_id=?1 AND {VALID_HEADER})"),[r.generation_id.as_str()],|r|r.get(0)))?;
    if !valid {
        r.status = "Stale".into();
    }
    Ok(Some(r))
}
/// One statement over the matching conventions, stopping at `limit` gaps, instead of a head
/// and header read per convention.
fn gaps(
    conn: &Connection,
    q: &ConventionQuery,
    space: Option<&str>,
    limit: u16,
) -> Result<Vec<(Id, String)>, StorageError> {
    let limit = page(limit)?;
    let (scope, filter) = super::filter(q)?;
    let (_, order) = super::order(&q.order);
    let filter_sql = super::FILTER;
    let reason = format!(
        "CASE WHEN ih.generation_id IS NULL THEN 'Unindexed' \
         WHEN ig.state<>'Published' OR ig.status NOT IN('Ready','LexicalOnly') \
          OR NOT EXISTS(SELECT 1 FROM {HEADER_JOINS} WHERE g.generation_id=ih.generation_id AND {VALID_HEADER}) \
          THEN 'IndexNotCurrent' \
         WHEN ?3 IS NOT NULL AND (ig.embedding_space IS NOT ?3 OR ig.status<>'Ready') \
          THEN 'MissingCompatibleVectors' END"
    );
    let sql = format!(
        "SELECT h.id,{reason} FROM convention_heads h CROSS JOIN convention_versions v ON v.convention_id=h.id AND v.revision=h.revision \
         LEFT JOIN convention_index_heads ih ON ih.convention_id=h.id \
         LEFT JOIN convention_indexes ig ON ig.generation_id=ih.generation_id \
         WHERE {filter_sql} AND ({reason}) IS NOT NULL ORDER BY {order} LIMIT ?4"
    );
    let mut stmt = db(conn.prepare_cached(&sql))?;
    db(db(stmt.query_map(params![scope, filter, space, limit], |r| {
        Ok((identifier(r, 0)?, r.get::<_, String>(1)?))
    }))?
    .collect())
}
fn index_revision(conn: &Connection, allowed: &[KnowledgeScope]) -> Result<i64, StorageError> {
    let scopes = scopes(allowed)?;
    db(conn.query_row("SELECT 1+coalesce(sum(revision-1),0) FROM convention_index_scope_revisions WHERE scope_key IN(SELECT value FROM json_each(?1))",[scopes],|r|r.get(0)))
}
fn bump_index_scope(conn: &Connection, owner: &Id) -> Result<(), StorageError> {
    let owner = current(conn, owner)?.ok_or_else(|| invalid("index scope owner missing"))?;
    let revision = index_revision(conn, std::slice::from_ref(&owner.scope))?
        .checked_add(1)
        .ok_or_else(|| invalid("convention index revision overflow"))?;
    db(conn.execute("INSERT INTO convention_index_scope_revisions(scope_key,project_id,revision) VALUES(?1,?2,?3) ON CONFLICT(scope_key) DO UPDATE SET revision=excluded.revision",params![owner.scope.key(),owner.scope.project().map(|p|p.0.as_str()),revision]))?;
    Ok(())
}
