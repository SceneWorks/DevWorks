use super::*;
use devworks_core::protocol::{OperationId, ProjectId};

fn index_record(row: &Row<'_>) -> rusqlite::Result<StoredConventionIndex> {
    Ok(StoredConventionIndex {
        generation_id: identifier(row, 0)?,
        convention_id: identifier(row, 1)?,
        convention_revision: row.get(2)?,
        resource_id: row
            .get::<_, Option<String>>(3)?
            .map(|s| Id::new(s).map_err(|e| conversion(3, std::io::Error::other(e.to_string()))))
            .transpose()?,
        resource_revision: row.get(4)?,
        operation_id: OperationId(identifier(row, 5)?),
        state: row.get(6)?,
        status: row.get(7)?,
        error: row.get(8)?,
        embedding_space: row.get(9)?,
        source_digest: row.get(10)?,
        created_at_ms: row.get(11)?,
        updated_at_ms: row.get(12)?,
    })
}
const INDEX_COLUMNS: &str = "generation_id,convention_id,convention_revision,resource_id,resource_revision,operation_id,state,status,error,embedding_space,source_digest,created_at,updated_at";
fn generation(conn: &Connection, id: &Id) -> Result<Option<StoredConventionIndex>, StorageError> {
    db(conn
        .query_row(
            &format!("SELECT {INDEX_COLUMNS} FROM convention_indexes WHERE generation_id=?1"),
            [id.as_str()],
            index_record,
        )
        .optional())
}
fn head(
    conn: &Connection,
    owner: &Id,
    resource: Option<&Id>,
) -> Result<Option<StoredConventionIndex>, StorageError> {
    let id:Option<String>=db(conn.query_row("SELECT generation_id FROM convention_index_heads WHERE convention_id=?1 AND resource_key=?2",params![owner.as_str(),resource.map_or("",Id::as_str)],|r|r.get(0)).optional())?;
    id.map(|s| Id::new(s).map_err(|_| invalid("invalid index head")))
        .transpose()?
        .map(|id| generation(conn, &id))
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
    let owner = current(conn, &index.convention_id)?
        .ok_or_else(|| invalid("convention index owner missing"))?;
    if owner.revision != expected {
        return Err(conflict(&owner.id, expected, owner.revision));
    }
    if owner.lifecycle != "Active" {
        return Err(invalid("convention index owner is not active"));
    }
    let captured:Option<(String,String)>=db(conn.query_row("SELECT scope_key,title FROM convention_versions WHERE convention_id=?1 AND revision=?2",params![index.convention_id.as_str(),index.convention_revision],|r|Ok((r.get(0)?,r.get(1)?))).optional())?;
    if captured.as_ref() != Some(&(owner.scope.key(), owner.title.clone())) {
        return Err(invalid("convention index owner scope/title changed"));
    }
    let reference = if let Some(resource) = &index.resource_id {
        let row:Option<(String,i64,String,String)>=db(conn.query_row("SELECT r.convention_id,r.revision,r.lifecycle,r.content_ref_json FROM convention_resource_heads h JOIN convention_resource_versions r ON r.resource_id=h.id AND r.revision=h.revision WHERE h.id=?1",[resource.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional())?;
        let Some((id, revision, state, reference)) = row else {
            return Err(invalid("convention index resource missing"));
        };
        if id != owner.id.as_str() || Some(revision) != index.resource_revision || state != "Active"
        {
            return Err(invalid("convention index resource revision changed"));
        }
        serde_json::from_str(&reference).map_err(|_| invalid("invalid resource reference"))?
    } else {
        if index.convention_revision != expected {
            return Err(conflict(&owner.id, index.convention_revision, expected));
        }
        owner
            .content_ref
            .ok_or_else(|| invalid("convention body is unavailable"))?
    };
    let reference = backing(conn, &owner.scope, &reference)?;
    if reference.sha256.as_str() != index.source_digest {
        return Err(invalid("convention index source digest changed"));
    }
    Ok(reference)
}
fn terms(
    conn: &Connection,
    generation: &Id,
    chunk: &Id,
) -> Result<Vec<ConventionTerm>, StorageError> {
    let mut stmt=db(conn.prepare_cached("SELECT term,frequency FROM convention_chunk_terms WHERE generation_id=?1 AND chunk_id=?2 ORDER BY term"))?;
    db(db(
        stmt.query_map(params![generation.as_str(), chunk.as_str()], |r| {
            Ok(ConventionTerm {
                term: r.get(0)?,
                frequency: r.get(1)?,
            })
        }),
    )?
    .collect())
}
fn vector_bytes(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|v| v.to_le_bytes()).collect()
}
fn decode_vector(bytes: Vec<u8>) -> Result<Vec<f32>, StorageError> {
    if bytes.len() % 4 != 0 {
        return Err(invalid("corrupt convention vector length"));
    }
    let vector = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().expect("four bytes")))
        .collect::<Vec<_>>();
    if vector.iter().any(|v| !v.is_finite()) {
        return Err(invalid("nonfinite stored convention vector"));
    }
    Ok(vector)
}
fn chunk(
    conn: &Connection,
    index: &StoredConventionIndex,
    id: &Id,
) -> Result<Option<StoredConventionChunk>, StorageError> {
    let row=db(conn.query_row("SELECT chunk_index,section_path,text,embedding_text,token_count,vector,section_path_ref_json FROM convention_chunks WHERE generation_id=?1 AND id=?2",params![index.generation_id.as_str(),id.as_str()],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,i64>(4)?,r.get::<_,Option<Vec<u8>>>(5)?,r.get::<_,Option<String>>(6)?))).optional())?;
    row.map(|r| {
        Ok(StoredConventionChunk {
            generation_id: index.generation_id.clone(),
            id: id.clone(),
            convention_id: index.convention_id.clone(),
            convention_revision: index.convention_revision,
            resource_id: index.resource_id.clone(),
            resource_revision: index.resource_revision,
            index: r.0,
            section_path: r.1,
            section_path_ref: r
                .6
                .map(|s| {
                    serde_json::from_str(&s).map_err(|_| invalid("invalid chunk path reference"))
                })
                .transpose()?,
            text: r.2,
            embedding_text: r.3,
            token_count: r.4,
            terms: terms(conn, &index.generation_id, id)?,
            vector: r.5.map(decode_vector).transpose()?,
        })
    })
    .transpose()
}
// A resource survives an unrelated body revision only when its exact active resource version
// and every captured owner field used in the embedding breadcrumb still match. Body indexes
// require the exact current convention revision. Artifact liveness/scope is checked again here.
const ELIGIBLE: &str = "WITH eligible AS MATERIALIZED (SELECT g.generation_id,g.convention_id,g.resource_id,g.embedding_space,c.title,k.id,k.chunk_index,k.token_count,k.vector FROM convention_index_heads ih JOIN convention_indexes g ON g.generation_id=ih.generation_id JOIN convention_heads h ON h.id=g.convention_id JOIN convention_versions c ON c.convention_id=h.id AND c.revision=h.revision JOIN convention_versions captured ON captured.convention_id=g.convention_id AND captured.revision=g.convention_revision LEFT JOIN convention_resource_heads rh ON rh.id=g.resource_id LEFT JOIN convention_resource_versions r ON r.resource_id=rh.id AND r.revision=rh.revision JOIN convention_chunks k ON k.generation_id=g.generation_id JOIN artifact_versions av ON av.id=json_extract(CASE WHEN g.resource_id IS NULL THEN c.content_ref_json ELSE r.content_ref_json END,'$.versionId') JOIN artifacts a ON a.id=av.artifact_id JOIN blobs b ON b.digest=av.blob_digest WHERE g.state='Published' AND g.status IN('Ready','LexicalOnly') AND c.lifecycle='Active' AND c.scope_key=captured.scope_key AND c.title=captured.title AND ((g.resource_id IS NULL AND g.convention_revision=h.revision) OR (g.resource_id IS NOT NULL AND r.convention_id=h.id AND r.revision=g.resource_revision AND r.lifecycle='Active')) AND c.scope_key IN(SELECT value FROM json_each(?1,'$.scopes')) AND NOT EXISTS(SELECT 1 FROM json_each(?1,'$.applicability') f WHERE f.value NOT IN(SELECT value FROM json_each(c.applicability_json))) AND (NOT json_extract(?1,'$.filterParent') OR c.parent_id IS json_extract(?1,'$.parent')) AND (json_extract(?1,'$.slug') IS NULL OR c.slug=json_extract(?1,'$.slug')) AND (json_extract(?1,'$.title') IS NULL OR instr(c.title_folded,json_extract(?1,'$.title'))>0) AND av.deleted_at IS NULL AND a.state='Live' AND a.current_project_id IS c.project_id AND b.state='Present' AND av.artifact_id=json_extract(CASE WHEN g.resource_id IS NULL THEN c.content_ref_json ELSE r.content_ref_json END,'$.artifactId') AND av.blob_digest=g.source_digest AND av.blob_digest=json_extract(CASE WHEN g.resource_id IS NULL THEN c.content_ref_json ELSE r.content_ref_json END,'$.sha256') AND b.byte_count=CAST(json_extract(CASE WHEN g.resource_id IS NULL THEN c.content_ref_json ELSE r.content_ref_json END,'$.byteLength') AS INTEGER))";
fn query(q: &ConventionQuery) -> Result<String, StorageError> {
    states(&q.lifecycles)?;
    if !q.lifecycles.is_empty() && !q.lifecycles.iter().any(|s| s == "Active") {
        return Ok(json!({"scopes":[],"applicability":[]}).to_string());
    }
    let keys: Value = serde_json::from_str(&scopes(&q.scopes)?).expect("serialized scopes");
    Ok(json!({"scopes":keys,"applicability":q.applicability,"filterParent":q.parent_id.is_some(),"parent":q.parent_id.as_ref().and_then(|p|p.as_ref()).map(Id::as_str),"slug":q.slug,"title":q.title.as_ref().map(|s|s.to_lowercase())}).to_string())
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
    if requested.len() > 4096 || requested.iter().any(|t| !valid_term(t)) {
        return Err(invalid("invalid convention lexical query terms"));
    }
    let q = query(q)?;
    let (documents, total): (i64, i64) = db(conn.query_row(
        &format!("{ELIGIBLE} SELECT count(*),coalesce(sum(token_count),0) FROM eligible"),
        [&q],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ))?;
    let mut stmt=db(conn.prepare_cached(&format!("{ELIGIBLE} SELECT requested.value,count(t.term) FROM json_each(?2) requested LEFT JOIN (SELECT t.term,t.generation_id,t.chunk_id FROM convention_chunk_terms t JOIN eligible e ON e.generation_id=t.generation_id AND e.id=t.chunk_id) t ON t.term=requested.value GROUP BY requested.value ORDER BY requested.value")))?;
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
        documents: u64::try_from(documents).map_err(|_| invalid("negative corpus count"))?,
        total_tokens: u64::try_from(total).map_err(|_| invalid("negative corpus token count"))?,
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
    let weighted = json!(
        weighted
            .iter()
            .map(|t| json!({"term":t.term,"idf":t.idf}))
            .collect::<Vec<_>>()
    )
    .to_string();
    let sql = format!(
        "{ELIGIBLE}, lexical AS MATERIALIZED (SELECT e.generation_id,e.id,sum(json_extract(w.value,'$.idf')*(t.frequency*2.2)/(t.frequency+1.2*(0.25+0.75*e.token_count/?3))) lex FROM eligible e JOIN convention_chunk_terms t ON t.generation_id=e.generation_id AND t.chunk_id=e.id JOIN json_each(?2) w ON json_extract(w.value,'$.term')=t.term GROUP BY e.generation_id,e.id), lexical_max AS(SELECT coalesce(max(lex),0) maximum FROM lexical), dense AS MATERIALIZED (SELECT e.generation_id,e.id,max(0.0,1.0-vec_distance_cosine(e.vector,?5)) dense FROM eligible e WHERE ?4 IS NOT NULL AND e.embedding_space=?4 AND e.vector IS NOT NULL ORDER BY dense DESC,e.title COLLATE BINARY,e.convention_id,coalesce(e.resource_id,''),e.generation_id,e.chunk_index,e.id LIMIT ?6), candidates AS(SELECT generation_id,id FROM lexical WHERE lex>0 UNION SELECT generation_id,id FROM dense WHERE dense>0), scored AS (SELECT e.generation_id,e.id,e.title,e.convention_id,e.resource_id,e.chunk_index,coalesce(l.lex,0.0) lex,coalesce(d.dense,0.0) dense,0.35*(CASE WHEN m.maximum>0 THEN coalesce(l.lex,0.0)/m.maximum ELSE 0.0 END)+0.65*coalesce(d.dense,0.0) score FROM candidates n JOIN eligible e ON e.generation_id=n.generation_id AND e.id=n.id LEFT JOIN lexical l ON l.generation_id=e.generation_id AND l.id=e.id LEFT JOIN dense d ON d.generation_id=e.generation_id AND d.id=e.id CROSS JOIN lexical_max m) SELECT generation_id,id,lex,dense,score FROM scored WHERE score>0 ORDER BY score DESC,title COLLATE BINARY,convention_id,coalesce(resource_id,''),generation_id,chunk_index,id LIMIT ?7"
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
                chunk: chunk(conn, &index, &id)?.ok_or_else(|| invalid("chunk disappeared"))?,
                lexical_score: r.2,
                dense_score: r.3,
                score: r.4,
            })
        })
        .collect()
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
    fn convention_embedding_inputs(&self,id:&Id)->Result<Option<StoredConventionEmbeddingInputs>,StorageError>{embedding_inputs(self.conn,id)}
    fn convention_embedding_space(&self,id:&str)->Result<Option<StoredConventionEmbeddingSpace>,StorageError>{space(self.conn,id)}
    fn convention_index_head(&self,id:&Id,resource:Option<&Id>)->Result<Option<StoredConventionIndex>,StorageError>{head(self.conn,id,resource)}
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
        && a.resource_id == b.resource_id
        && a.resource_revision == b.resource_revision
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
pub(super) fn invalidate(
    conn: &Connection,
    owner: &Id,
    resource: Option<&Id>,
    all: bool,
    now: i64,
) -> Result<(), StorageError> {
    db(conn.execute("UPDATE convention_indexes SET status='Stale',error=NULL,updated_at=max(updated_at,?4) WHERE convention_id=?1 AND (?3 OR resource_id IS ?2) AND status NOT IN('Failed','Cancelled')",params![owner.as_str(),resource.map(Id::as_str),all,now]))?;
    Ok(())
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
            || r.resource_id.is_some() != r.resource_revision.is_some()
            || r.resource_revision.is_some_and(|n| n < 1)
            || r.convention_revision < 1
            || !digest(&r.source_digest)
            || r.created_at_ms < 0
            || r.updated_at_ms != r.created_at_ms
        {
            return Err(invalid("invalid convention index generation"));
        }
        if let Some(old) = generation(self.conn, &r.generation_id)? {
            return if same_source(&old, r) {
                Ok(())
            } else {
                Err(invalid("convention generation source is immutable"))
            };
        }
        active_operation(self.conn, &r.operation_id)?;
        source(self.conn, r, r.convention_revision)?;
        if let Some(id) = &r.embedding_space {
            space(self.conn, id)?.ok_or_else(|| invalid("embedding space missing"))?;
        }
        db(self.conn.execute("INSERT INTO convention_indexes(generation_id,convention_id,convention_revision,resource_id,resource_revision,operation_id,state,status,error,embedding_space,source_digest,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",params![r.generation_id.as_str(),r.convention_id.as_str(),r.convention_revision,r.resource_id.as_ref().map(Id::as_str),r.resource_revision,r.operation_id.0.as_str(),r.state,r.status,r.error,r.embedding_space,r.source_digest,r.created_at_ms,r.updated_at_ms]))?;
        Ok(())
    }
    fn stage_convention_chunks(
        &mut self,
        id: &Id,
        chunks: &[StoredConventionChunk],
    ) -> Result<(), StorageError> {
        if chunks.len() > usize::from(MAX_PAGE) {
            return Err(invalid("convention chunk batch exceeds 500"));
        }
        let index =
            generation(self.conn, id)?.ok_or_else(|| invalid("convention generation missing"))?;
        if index.state != "Draft" || !matches!(index.status.as_str(), "Queued" | "Indexing") {
            return Err(invalid("convention generation is not staging"));
        }
        active_operation(self.conn, &index.operation_id)?;
        let owner =
            current(self.conn, &index.convention_id)?.ok_or_else(|| invalid("owner missing"))?;
        let dimensions = index
            .embedding_space
            .as_deref()
            .map(|id| space(self.conn, id)?.ok_or_else(|| invalid("embedding space missing")))
            .transpose()?
            .map(|s| s.dimensions);
        atomic(self.conn, || {
            for c in chunks {
                if &c.generation_id != id
                    || c.convention_id != index.convention_id
                    || c.convention_revision != index.convention_revision
                    || c.resource_id != index.resource_id
                    || c.resource_revision != index.resource_revision
                    || c.index < 0
                    || c.token_count < 0
                    || c.text.contains('\0')
                    || c.embedding_text.contains('\0')
                    || c.section_path.contains('\0')
                    || c.text.len() > 16_777_215
                    || c.embedding_text.len() as u64
                        > devworks_core::ports::packages::MAX_BUNDLE_BYTES
                    || c.section_path.len() > 16_777_215
                {
                    return Err(invalid("invalid convention chunk source"));
                }
                let sum = c
                    .terms
                    .iter()
                    .try_fold(0_i64, |sum, t| sum.checked_add(i64::from(t.frequency)))
                    .ok_or_else(|| invalid("convention token count overflow"))?;
                if sum != c.token_count
                    || c.terms
                        .iter()
                        .any(|t| !valid_term(&t.term) || t.frequency == 0)
                    || c.terms.windows(2).any(|w| w[0].term >= w[1].term)
                {
                    return Err(invalid(
                        "convention chunk requires exact sorted term frequencies",
                    ));
                }
                if let Some(vector) = &c.vector {
                    validate_vector(
                        vector,
                        dimensions
                            .ok_or_else(|| invalid("vector requires exact embedding space"))?,
                    )?;
                }
                if let Some(old) = chunk(self.conn, &index, &c.id)? {
                    if old == *c {
                        continue;
                    }
                    return Err(invalid("convention chunk is immutable"));
                }
                let path = if let Some(reference) = &c.section_path_ref {
                    let path = backing(self.conn, &owner.scope, reference)?;
                    if path.byte_length.get() != c.section_path.len() as i64
                        || path.sha256.as_str()
                            != devworks_core::fingerprint::sha256_hex(c.section_path.as_bytes())
                    {
                        return Err(invalid(
                            "convention breadcrumb artifact differs from exact path",
                        ));
                    }
                    Some(path)
                } else {
                    None
                };
                db(self.conn.execute("INSERT INTO convention_chunks(generation_id,id,chunk_index,section_path,section_path_ref_json,section_path_version_id,text,embedding_text,token_count,vector) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",params![id.as_str(),c.id.as_str(),c.index,c.section_path,c.section_path_ref.as_ref().map(Value::to_string),path.as_ref().map(|r|r.version_id.as_str()),c.text,c.embedding_text,c.token_count,c.vector.as_deref().map(vector_bytes)]))?;
                for term in &c.terms {
                    db(self.conn.execute("INSERT INTO convention_chunk_terms(generation_id,chunk_id,term,frequency) VALUES(?1,?2,?3,?4)",params![id.as_str(),c.id.as_str(),term.term,term.frequency]))?;
                }
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
        if index.state != "Draft" || matches!(status, "Ready" | "LexicalOnly") {
            return Err(invalid("only atomic index publication sets ready status"));
        }
        if status != "Cancelled" && status != "Failed" {
            active_operation(self.conn, op)?;
        }
        db(self.conn.execute("UPDATE convention_indexes SET status=?2,error=?3,updated_at=max(updated_at,?4) WHERE generation_id=?1 AND status=?5",params![id.as_str(),status,error,self.now,expected]))?;
        Ok(())
    }
    fn publish_convention_index(
        &mut self,
        id: &Id,
        expected: i64,
        expected_head: Option<&Id>,
        count: i64,
    ) -> Result<(), StorageError> {
        let mut index =
            generation(self.conn, id)?.ok_or_else(|| invalid("convention generation missing"))?;
        if index.state != "Draft"
            || !matches!(index.status.as_str(), "Queued" | "Indexing")
            || count < 0
        {
            return Err(invalid("convention generation cannot publish"));
        }
        active_operation(self.conn, &index.operation_id)?;
        source(self.conn, &index, expected)?;
        if head(self.conn, &index.convention_id, index.resource_id.as_ref())?
            .as_ref()
            .map(|h| &h.generation_id)
            != expected_head
        {
            return Err(invalid("convention index head CAS refused"));
        }
        let (actual,first,last,vectors):(i64,Option<i64>,Option<i64>,i64)=db(self.conn.query_row("SELECT count(*),min(chunk_index),max(chunk_index),count(vector) FROM convention_chunks WHERE generation_id=?1",[id.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))))?;
        if actual != count
            || (count > 0 && (first != Some(0) || last != count.checked_sub(1)))
            || (vectors != 0 && vectors != count)
        {
            return Err(invalid("convention generation is incomplete"));
        }
        let owner = current(self.conn, &index.convention_id)?.expect("validated owner");
        let mut paths=db(self.conn.prepare_cached("SELECT section_path_ref_json FROM convention_chunks WHERE generation_id=?1 AND section_path_ref_json IS NOT NULL"))?;
        let references = db(paths.query_map([id.as_str()], |r| value(r, 0)))?;
        for reference in references {
            backing(self.conn, &owner.scope, &db(reference)?)?;
        }
        index.status = if index.embedding_space.is_some() && vectors == count {
            "Ready"
        } else {
            "LexicalOnly"
        }
        .into();
        atomic(self.conn, || {
            if let Some(old) = expected_head {
                db(self.conn.execute("UPDATE convention_indexes SET status='Stale',updated_at=max(updated_at,?2) WHERE generation_id=?1",params![old.as_str(),self.now]))?;
            }
            db(self.conn.execute("UPDATE convention_indexes SET state='Published',status=?2,error=NULL,updated_at=max(updated_at,?3) WHERE generation_id=?1",params![id.as_str(),index.status,self.now]))?;
            db(self.conn.execute("INSERT INTO convention_index_heads(convention_id,resource_key,generation_id) VALUES(?1,?2,?3) ON CONFLICT(convention_id,resource_key) DO UPDATE SET generation_id=excluded.generation_id",params![index.convention_id.as_str(),index.resource_id.as_ref().map_or("",Id::as_str),id.as_str()]))?;
            Ok(())
        })
    }
    fn discard_convention_index_chunks(
        &mut self,
        id: &Id,
        op: &OperationId,
        limit: u16,
    ) -> Result<bool, StorageError> {
        let limit = page(limit)?;
        let index =
            generation(self.conn, id)?.ok_or_else(|| invalid("convention generation missing"))?;
        if &index.operation_id != op {
            return Err(invalid("convention cleanup operation mismatch"));
        }
        let current: bool = db(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM convention_index_heads WHERE generation_id=?1)",
            [id.as_str()],
            |r| r.get(0),
        ))?;
        if current && matches!(index.status.as_str(), "Ready" | "LexicalOnly") {
            return Err(invalid("current convention chunks cannot be discarded"));
        }
        let state = operation(self.conn, op)?;
        if index.state == "Draft"
            && !matches!(
                state.as_str(),
                "Failed" | "NeedsInput" | "CancelledBeforeDispatch" | "ObservedComplete"
            )
        {
            return Err(invalid("live convention draft cannot be discarded"));
        }
        atomic(self.conn, || {
            db(self.conn.execute("DELETE FROM convention_chunks WHERE generation_id=?1 AND id IN(SELECT id FROM convention_chunks WHERE generation_id=?1 ORDER BY chunk_index LIMIT ?2)",params![id.as_str(),limit]))?;
            let remaining: bool = db(self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM convention_chunks WHERE generation_id=?1)",
                [id.as_str()],
                |r| r.get(0),
            ))?;
            Ok(!remaining)
        })
    }
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
