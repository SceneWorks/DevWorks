//! SQLite owns the atomic extraction checkpoint and per-version publication pointer.
use super::{
    conn::db,
    repos::{Snapshot, Unit},
    support::{corrupt, id_of, invalid},
};
use devworks_core::{
    ports::storage::{
        ArtifactScope, DocumentCandidate, DocumentCheckpoint, DocumentEmbeddingRun,
        DocumentIndexVersion, DocumentRankingBatch, DocumentRankingQuery, DocumentRead,
        DocumentRepository, StorageError,
    },
    protocol::{ActorId, Id, Revision},
    services::documents::{
        extraction::ExtractedBlock,
        vision::{VisionTarget, VisionUnit},
    },
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::sync::Arc;

fn encode(value: &impl Serialize) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|error| invalid(error.to_string()))
}

fn decode<T: DeserializeOwned>(value: &str) -> Result<T, StorageError> {
    serde_json::from_str(value).map_err(|error| corrupt(error.to_string()))
}

fn retrieval(conn: &Connection, id: &Id) -> Result<bool, StorageError> {
    let enabled = db(conn
        .query_row(
            "SELECT enabled FROM document_retrieval_preferences WHERE artifact_id=?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .optional())?;
    Ok(enabled.unwrap_or(true))
}

fn index(conn: &Connection, version: &Id) -> Result<Option<DocumentIndexVersion>, StorageError> {
    let row = db(conn
        .query_row(
            "SELECT latest_operation,stage,diagnostic,published_generation,published_revision
         FROM document_index_versions WHERE version_id=?1",
            [version.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            },
        )
        .optional())?;
    row.map(|(latest, stage, diagnostic, generation, revision)| {
        let published = match (generation, revision) {
            (Some(generation), Some(revision)) => Some((
                id_of(generation)?,
                Revision::new(revision).map_err(corrupt)?,
            )),
            (None, None) => None,
            _ => return Err(corrupt("document publication binding")),
        };
        let stage = serde_json::from_value(Value::String(stage))
            .map_err(|error| corrupt(error.to_string()))?;
        Ok(DocumentIndexVersion {
            version: version.clone(),
            latest: id_of(latest)?,
            stage,
            diagnostic: diagnostic.map(id_of).transpose()?,
            published,
        })
    })
    .transpose()
}

fn checkpoint(
    conn: &Connection,
    version: &Id,
    revision: &Revision,
) -> Result<Option<DocumentCheckpoint>, StorageError> {
    let row = db(conn
        .query_row(
            "SELECT id,source_digest,coverage_json,block_count FROM document_extractions
         WHERE version_id=?1 AND revision=?2",
            params![version.as_str(), revision.get()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u32>(3)?,
                ))
            },
        )
        .optional())?;
    row.map(|(id, source_digest, coverage, blocks)| {
        Ok(DocumentCheckpoint {
            id: id_of(id)?,
            version: version.clone(),
            revision: revision.clone(),
            source_digest,
            coverage: decode(&coverage)?,
            blocks,
        })
    })
    .transpose()
}

fn passage(
    conn: &Connection,
    checkpoint: &Id,
    block: &Id,
) -> Result<Option<ExtractedBlock>, StorageError> {
    let row = db(conn
        .query_row(
            "SELECT text,citation_json,headings_json FROM document_extracted_blocks
         WHERE extraction_id=?1 AND id=?2",
            params![checkpoint.as_str(), block.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional())?;
    row.map(decode_passage).transpose()
}

fn decode_passage(
    (text, citation, headings): (String, String, String),
) -> Result<ExtractedBlock, StorageError> {
    Ok(ExtractedBlock {
        text,
        citation: decode(&citation)?,
        headings: Arc::from(decode::<Vec<_>>(&headings)?),
    })
}

fn blocks(conn: &Connection, id: &Id) -> Result<Vec<ExtractedBlock>, StorageError> {
    let mut statement = db(conn.prepare_cached(
        "SELECT text,citation_json,headings_json FROM document_extracted_blocks
           WHERE extraction_id=?1 ORDER BY ordinal",
    ))?;
    let rows = db(statement.query_map([id.as_str()], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    }))?;
    rows.map(|row| decode_passage(db(row)?)).collect()
}

// Both the population check and ranking use the same authorized published posting set.
const SCOPED_POSTINGS: &str = "
             FROM document_lexical_rows l JOIN document_extracted_blocks b ON b.id=l.block_id
             JOIN document_extractions e ON e.id=b.extraction_id
             JOIN document_index_versions i ON i.version_id=e.version_id AND i.published_revision=e.revision
             JOIN artifact_versions v ON v.id=e.version_id
             JOIN artifacts a ON a.id=v.artifact_id
             LEFT JOIN document_retrieval_preferences p ON p.artifact_id=a.id
            WHERE a.state='Live' AND a.retention='Explicit' AND v.deleted_at IS NULL
              AND json_extract(a.provenance_json,'$.managedDocument')=1
              AND COALESCE(p.enabled,1)=1
              AND ((?1 IS NULL AND a.scope_kind='Workspace' AND (?2 IS NULL OR a.owner_actor_id=?2))
                OR (?1 IS NOT NULL AND (a.current_project_id=?1 OR EXISTS(
                     SELECT 1 FROM artifact_shares s WHERE s.artifact_id=a.id AND s.project_id=?1))
                  AND (?2 IS NULL OR a.scope_kind='Workspace' OR a.owner_actor_id IS NULL OR a.owner_actor_id=?2)))
              AND ((?3 IS NULL AND v.id=a.current_version_id)
                  OR (?3 IS NOT NULL AND v.id IN (SELECT value FROM json_each(?3))))";

fn has_vectors(
    conn: &Connection,
    scope: &ArtifactScope,
    owner: Option<&ActorId>,
    exact_versions: Option<&[Id]>,
    space: &Id,
) -> Result<bool, StorageError> {
    let exact = exact_versions.map(|ids| encode(&ids)).transpose()?;
    let sql = format!(
        "SELECT EXISTS (SELECT 1 FROM document_vectors d INDEXED BY document_vectors_space
           WHERE d.space_id=?4 AND EXISTS(
             SELECT 1 {SCOPED_POSTINGS} AND l.block_id=d.block_id
               AND EXISTS (SELECT 1 FROM document_embedding_runs r
                           WHERE r.extraction_id=e.id AND r.space_id=?4 AND r.complete=1)))"
    );
    db(db(conn.prepare_cached(&sql))?.query_row(
        params![
            scope.project().map(|p| p.0.as_str()),
            owner.map(|a| a.0.as_str()),
            exact,
            space.as_str(),
        ],
        |r| r.get(0),
    ))
}

fn ranked(
    conn: &Connection,
    query: &DocumentRankingQuery<'_>,
) -> Result<DocumentRankingBatch, StorageError> {
    if (query.words.is_empty() && query.dense.is_none())
        || !(1..=500).contains(&query.max_results)
        || query.max_blocks > 1_000_000
    {
        return Err(invalid("document ranking query bounds"));
    }
    let exact = query.exact_versions.map(|ids| encode(&ids)).transpose()?;
    let (space, vector) = if let Some(dense) = &query.dense {
        let dimensions = cosine_dimensions(conn, dense.space)?;
        (
            Some(dense.space.as_str()),
            Some(vector_blob(dense.vector, dimensions)?),
        )
    } else {
        (None, None)
    };
    let sql = format!(
        "WITH eligible AS MATERIALIZED (
           SELECT l.id AS posting, b.id AS block, e.id AS checkpoint, e.version_id,
                  b.ordinal, i.published_generation AS generation
           {SCOPED_POSTINGS}
              AND (?7 IS NULL OR json_array_length(?4)>0 OR EXISTS(
                    SELECT 1 FROM document_vectors d WHERE d.block_id=b.id AND d.space_id=?7
                      AND EXISTS(SELECT 1 FROM document_embedding_runs r
                                 WHERE r.extraction_id=e.id AND r.space_id=?7 AND r.complete=1)))
            ORDER BY e.version_id,b.ordinal,b.id LIMIT (?5+1)
         ), tested AS MATERIALIZED (
           SELECT * FROM eligible ORDER BY version_id,ordinal,block LIMIT ?5
         ), lexical AS MATERIALIZED (
           SELECT e.posting,COUNT(*) AS score
             FROM tested e CROSS JOIN json_each(?4) word
            WHERE EXISTS(SELECT 1 FROM document_lexical_fts
                          WHERE rowid=e.posting AND document_lexical_fts MATCH word.value)
            GROUP BY e.posting
         ), dense AS MATERIALIZED (
           SELECT e.posting,max(0.0,1.0-vec_distance_cosine(d.vector,?8)) AS score
             FROM tested e JOIN document_vectors d ON d.block_id=e.block AND d.space_id=?7
            WHERE EXISTS(SELECT 1 FROM document_embedding_runs r
                         WHERE r.extraction_id=e.checkpoint AND r.space_id=?7 AND r.complete=1)
         ), scores AS MATERIALIZED (
           SELECT e.*, CASE WHEN ?7 IS NULL THEN COALESCE(l.score,0)
             WHEN json_array_length(?4)=0 THEN COALESCE(d.score,0)
             ELSE 0.35*COALESCE(l.score,0)/json_array_length(?4)+0.65*COALESCE(d.score,0)
             END score
             FROM tested e LEFT JOIN lexical l ON l.posting=e.posting
                  LEFT JOIN dense d ON d.posting=e.posting
         ), ranked AS MATERIALIZED (
           SELECT * FROM scores WHERE score>0
            ORDER BY score DESC,version_id,ordinal,block LIMIT (?6+1)
         )
         SELECT (SELECT COUNT(*) FROM eligible)>?5, r.checkpoint,r.block,r.generation, EXISTS(SELECT 1 FROM dense)
           FROM (SELECT 1) LEFT JOIN ranked r ON TRUE
          ORDER BY r.score DESC,r.version_id,r.ordinal,r.block",
    );
    let mut statement = db(conn.prepare_cached(&sql))?;
    // Only scoped, tested rows participate in matching or sqlite-vec scoring. Lexical
    // scores use the request's term count so another posting cannot shift the scale.
    let mut rows = db(statement.query(params![
        query.scope.project().map(|project| project.0.as_str()),
        query.owner.map(|actor| actor.0.as_str()),
        exact,
        encode(&query.words)?,
        query.max_blocks,
        query.max_results,
        space,
        vector,
    ]))?;
    let mut batch = DocumentRankingBatch {
        candidates: Vec::new(),
        work_limited: false,
        used_vectors: false,
    };
    while let Some(row) = db(rows.next())? {
        batch.work_limited = db(row.get(0))?;
        batch.used_vectors = db(row.get(4))?;
        if let Some(checkpoint) = db(row.get::<_, Option<String>>(1))? {
            batch.candidates.push(DocumentCandidate {
                checkpoint: id_of(checkpoint)?,
                block: id_of(db(row.get(2))?)?,
                generation: id_of(db(row.get(3))?)?,
            });
        }
    }
    Ok(batch)
}

fn cosine_dimensions(conn: &Connection, space: &Id) -> Result<u32, StorageError> {
    db(conn
        .query_row(
            "SELECT dimensions FROM embedding_spaces WHERE id=?1
               AND metric='Cosine' AND element_type='Float32'",
            [space.as_str()],
            |row| row.get(0),
        )
        .optional())?
    .ok_or_else(|| invalid("unknown document embedding space"))
}

fn vector_blob(vector: &[f32], dimensions: u32) -> Result<Vec<u8>, StorageError> {
    let square_norm = vector
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>();
    if vector.len() != dimensions as usize || !square_norm.is_finite() || square_norm == 0.0 {
        return Err(invalid(
            "document vector must match its space and have a finite nonzero norm",
        ));
    }
    // Use a unit vector for the rebuildable cosine index. The native journal keeps the
    // original Float32 artifact, including its exact byte digest.
    let norm = square_norm.sqrt();
    Ok(vector
        .iter()
        .flat_map(|value| ((f64::from(*value) / norm) as f32).to_le_bytes())
        .collect())
}

fn embedding_run(
    conn: &Connection,
    run: &Id,
) -> Result<Option<DocumentEmbeddingRun>, StorageError> {
    let row = db(conn.query_row(
        "SELECT extraction_id,space_id,configuration_digest,complete FROM document_embedding_runs
           WHERE run_id=?1",
        [run.as_str()],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get(2)?, r.get(3)?)),
    ).optional())?;
    row.map(|(extraction, space, configuration_digest, complete)| {
        Ok(DocumentEmbeddingRun {
            run: run.clone(),
            extraction: id_of(extraction)?,
            space: space.map(id_of).transpose()?,
            configuration_digest,
            complete,
        })
    })
    .transpose()
}

fn pending_space<'a>(
    conn: &Connection,
    run: &'a DocumentEmbeddingRun,
) -> Result<&'a Id, StorageError> {
    if run.complete || embedding_run(conn, &run.run)?.as_ref() != Some(run) {
        return Err(invalid("document vector preparation changed"));
    }
    run.space
        .as_ref()
        .ok_or_else(|| invalid("document run has no qualified space"))
}

macro_rules! document_reads {
    ($store:ty) => {
        impl DocumentRead for $store {
            fn document_retrieval_enabled(&self, id: &Id) -> Result<bool, StorageError> {
                retrieval(self.conn, id)
            }
            fn document_index(
                &self,
                version: &Id,
            ) -> Result<Option<DocumentIndexVersion>, StorageError> {
                index(self.conn, version)
            }
            fn document_checkpoint(
                &self,
                version: &Id,
                revision: &Revision,
            ) -> Result<Option<DocumentCheckpoint>, StorageError> {
                checkpoint(self.conn, version, revision)
            }
            fn latest_document_checkpoint(
                &self,
                version: &Id,
            ) -> Result<Option<DocumentCheckpoint>, StorageError> {
                let revision: Option<i64> = db(self
                    .conn
                    .query_row(
                        "SELECT revision FROM document_extractions WHERE version_id=?1
                     ORDER BY revision DESC LIMIT 1",
                        [version.as_str()],
                        |row| row.get(0),
                    )
                    .optional())?;
                match revision {
                    Some(revision) => checkpoint(
                        self.conn,
                        version,
                        &Revision::new(revision).map_err(corrupt)?,
                    ),
                    None => Ok(None),
                }
            }
            fn document_passage(
                &self,
                checkpoint: &Id,
                block: &Id,
            ) -> Result<Option<ExtractedBlock>, StorageError> {
                passage(self.conn, checkpoint, block)
            }
            fn document_blocks(
                &self,
                checkpoint: &Id,
            ) -> Result<Vec<ExtractedBlock>, StorageError> {
                blocks(self.conn, checkpoint)
            }
            fn has_document_vectors(
                &self,
                scope: &ArtifactScope,
                owner: Option<&ActorId>,
                exact_versions: Option<&[Id]>,
                space: &Id,
            ) -> Result<bool, StorageError> {
                has_vectors(self.conn, scope, owner, exact_versions, space)
            }

            fn rank_document_blocks(
                &self,
                query: &DocumentRankingQuery<'_>,
            ) -> Result<DocumentRankingBatch, StorageError> {
                ranked(self.conn, query)
            }
        }
    };
}
document_reads!(Snapshot<'_>);
document_reads!(Unit<'_>);

impl DocumentRepository for Unit<'_> {
    fn document_extraction_by_key(
        &self,
        version: &Id,
        key: &str,
    ) -> Result<Option<DocumentCheckpoint>, StorageError> {
        let revision = db(self
            .conn
            .query_row(
                "SELECT e.revision FROM document_extraction_keys k JOIN document_extractions e
               ON e.id=k.extraction_id AND e.version_id=k.version_id
              WHERE k.version_id=?1 AND k.identity_key=?2",
                params![version.as_str(), key],
                |row| row.get::<_, i64>(0),
            )
            .optional())?;
        revision
            .map(|revision| {
                checkpoint(
                    self.conn,
                    version,
                    &Revision::new(revision).map_err(corrupt)?,
                )
            })
            .transpose()
            .map(Option::flatten)
    }

    fn latest_document_direct_extraction(
        &self,
        version: &Id,
        algorithm: &Revision,
    ) -> Result<Option<DocumentCheckpoint>, StorageError> {
        let key = format!("direct:{}", algorithm.get());
        let revision = db(self
            .conn
            .query_row(
                "SELECT e.revision FROM document_extraction_keys k JOIN document_extractions e
               ON e.id=k.extraction_id AND e.version_id=k.version_id
              WHERE k.version_id=?1 AND (k.identity_key=?2 OR k.identity_key GLOB ?3)
              ORDER BY e.revision DESC LIMIT 1",
                params![version.as_str(), key, format!("{key}:*")],
                |row| row.get::<_, i64>(0),
            )
            .optional())?;
        match revision {
            Some(revision) => checkpoint(
                self.conn,
                version,
                &Revision::new(revision).map_err(corrupt)?,
            ),
            None => Ok(None),
        }
    }

    fn document_vision_targets(&self, extraction: &Id) -> Result<Vec<VisionTarget>, StorageError> {
        let encoded = db(self.conn.query_row(
            "SELECT vision_targets_json FROM document_extraction_keys WHERE extraction_id=?1",
            [extraction.as_str()],
            |row| row.get::<_, String>(0),
        ))?;
        decode(&encoded)
    }

    fn next_document_extraction_revision(&self, version: &Id) -> Result<Revision, StorageError> {
        let previous = db(self.conn.query_row(
            "SELECT COALESCE(MAX(revision),0) FROM document_extractions WHERE version_id=?1",
            [version.as_str()],
            |row| row.get::<_, i64>(0),
        ))?;
        Revision::new(
            previous
                .checked_add(1)
                .ok_or_else(|| invalid("document extraction revision overflow"))?,
        )
        .map_err(invalid)
    }

    fn bind_document_extraction(
        &mut self,
        extraction: &Id,
        key: &str,
        targets: &[VisionTarget],
    ) -> Result<(), StorageError> {
        let changed = db(self.conn.execute(
            "INSERT INTO document_extraction_keys
               (version_id,identity_key,extraction_id,vision_targets_json)
             SELECT version_id,?2,id,?3 FROM document_extractions WHERE id=?1",
            params![extraction.as_str(), key, encode(&targets)?],
        ))?;
        if changed != 1 {
            return Err(invalid("document extraction missing"));
        }
        Ok(())
    }

    fn document_vision_unit(
        &self,
        extraction: &Id,
        target: &Id,
    ) -> Result<Option<VisionUnit>, StorageError> {
        let row = db(self.conn.query_row(
            "SELECT record_json FROM document_vision_units WHERE extraction_id=?1 AND target_id=?2",
            params![extraction.as_str(), target.as_str()],
            |row| row.get::<_, String>(0),
        ).optional())?;
        row.map(|value| decode(&value)).transpose()
    }

    fn put_document_vision_unit(&mut self, unit: &VisionUnit) -> Result<(), StorageError> {
        db(self.conn.execute(
            "INSERT INTO document_vision_units(extraction_id,target_id,record_json) VALUES(?1,?2,?3)
               ON CONFLICT(extraction_id,target_id) DO UPDATE SET record_json=excluded.record_json",
            params![unit.extraction.as_str(),unit.target.as_str(), encode(unit)?],
        ))?;
        Ok(())
    }

    fn insert_document_embedding_run(
        &mut self,
        run: &DocumentEmbeddingRun,
    ) -> Result<(), StorageError> {
        if run.complete {
            return Err(invalid(
                "document vector completion must follow its full population",
            ));
        }
        // The current queue claim is validated by the caller. Retained vector metadata follows
        // the extraction independently of eventual cleanup of the delivery.
        let inserted = db(self.conn.execute(
            "INSERT INTO document_embedding_runs (run_id,extraction_id,space_id,configuration_digest)
             SELECT id,?2,?3,?4 FROM queue_entries WHERE id=?1 AND consumer='document_index'
               AND state='Claimed'",
            params![run.run.as_str(), run.extraction.as_str(), run.space.as_ref().map(Id::as_str),
                run.configuration_digest],
        ))?;
        if inserted != 1 {
            return Err(invalid(
                "document embeddings require their claimed delivery",
            ));
        }
        Ok(())
    }

    fn document_embedding_run(
        &self,
        run: &Id,
    ) -> Result<Option<DocumentEmbeddingRun>, StorageError> {
        embedding_run(self.conn, run)
    }

    fn pending_document_vectors(
        &self,
        run: &DocumentEmbeddingRun,
        limit: u16,
    ) -> Result<Vec<(Id, String)>, StorageError> {
        if !(1..=32).contains(&limit) {
            return Err(invalid("document vector batch limit"));
        }
        let space = pending_space(self.conn, run)?;
        let mut statement = db(self.conn.prepare_cached(
            "SELECT b.id,b.text FROM document_extracted_blocks b
              WHERE b.extraction_id=?1 AND NOT EXISTS (
                SELECT 1 FROM document_vectors d WHERE d.block_id=b.id AND d.space_id=?2)
              ORDER BY b.ordinal,b.id LIMIT ?3",
        ))?;
        let rows = db(statement.query_map(
            params![run.extraction.as_str(), space.as_str(), limit],
            |row| Ok((row.get::<_, String>(0)?, row.get(1)?)),
        ))?;
        rows.map(|row| {
            let (id, text) = db(row)?;
            Ok((id_of(id)?, text))
        })
        .collect()
    }

    fn insert_document_vectors(
        &mut self,
        run: &DocumentEmbeddingRun,
        vectors: &[(Id, Vec<f32>)],
    ) -> Result<(), StorageError> {
        if !(1..=32).contains(&vectors.len()) {
            return Err(invalid("document vector batch population"));
        }
        let space = pending_space(self.conn, run)?;
        let dimensions = cosine_dimensions(self.conn, space)?;
        let mut statement = db(self.conn.prepare_cached(
            "INSERT INTO document_vectors (extraction_id,space_id,dimensions,block_id,vector)
             SELECT extraction_id,?2,?3,id,?5 FROM document_extracted_blocks
               WHERE extraction_id=?1 AND id=?4",
        ))?;
        for (block, vector) in vectors {
            let inserted = db(statement.execute(params![
                run.extraction.as_str(),
                space.as_str(),
                dimensions,
                block.as_str(),
                vector_blob(vector, dimensions)?
            ]))?;
            if inserted != 1 {
                return Err(invalid(
                    "document vector block differs from the captured extraction",
                ));
            }
        }
        Ok(())
    }

    fn complete_document_vectors(
        &mut self,
        run: &DocumentEmbeddingRun,
    ) -> Result<(), StorageError> {
        let space = pending_space(self.conn, run)?;
        let changed = db(self.conn.execute(
            "UPDATE document_embedding_runs SET complete=1 WHERE run_id=?1 AND
             (SELECT block_count FROM document_extractions WHERE id=?2)=
             (SELECT COUNT(*) FROM document_vectors WHERE extraction_id=?2 AND space_id=?3)",
            params![run.run.as_str(), run.extraction.as_str(), space.as_str()],
        ))?;
        if changed != 1 {
            return Err(invalid(
                "cannot publish an incomplete document vector population",
            ));
        }
        Ok(())
    }

    fn set_document_retrieval(&mut self, artifact: &Id, enabled: bool) -> Result<(), StorageError> {
        db(self.conn.execute(
            "INSERT INTO document_retrieval_preferences (artifact_id,enabled) VALUES (?1,?2)
             ON CONFLICT(artifact_id) DO UPDATE SET enabled=excluded.enabled",
            params![artifact.as_str(), enabled],
        ))?;
        Ok(())
    }

    fn put_document_index(&mut self, index: &DocumentIndexVersion) -> Result<(), StorageError> {
        let stage =
            serde_json::to_value(index.stage).map_err(|error| invalid(error.to_string()))?;
        let (generation, revision) = index
            .published
            .as_ref()
            .map(|(generation, revision)| (generation.as_str(), revision.get()))
            .unzip();
        db(self.conn.execute(
            "INSERT INTO document_index_versions
             (version_id,latest_operation,stage,diagnostic,published_generation,published_revision)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(version_id) DO UPDATE SET latest_operation=excluded.latest_operation,
             stage=excluded.stage, diagnostic=excluded.diagnostic,
             published_generation=excluded.published_generation,published_revision=excluded.published_revision",
            params![index.version.as_str(), index.latest.as_str(), stage.as_str(),
                index.diagnostic.as_ref().map(Id::as_str), generation, revision],
        ))?;
        Ok(())
    }

    fn remove_document_index(&mut self, artifact: &Id) -> Result<bool, StorageError> {
        let changed = db(self.conn.execute(
            "UPDATE document_index_versions SET published_generation=NULL,published_revision=NULL,
             stage='Removed',diagnostic=NULL WHERE stage<>'Removed' AND version_id IN
             (SELECT id FROM artifact_versions WHERE artifact_id=?1 AND deleted_at IS NULL)",
            [artifact.as_str()],
        ))?;
        for table in ["document_embedding_runs", "document_vectors"] {
            db(self.conn.execute(
                &format!(
                    "DELETE FROM {table} WHERE extraction_id IN (
                  SELECT e.id FROM document_extractions e JOIN artifact_versions v
                    ON v.id=e.version_id WHERE v.artifact_id=?1)"
                ),
                [artifact.as_str()],
            ))?;
        }
        Ok(changed != 0)
    }

    fn insert_document_checkpoint(
        &mut self,
        checkpoint: &DocumentCheckpoint,
        blocks: &[ExtractedBlock],
    ) -> Result<(), StorageError> {
        if blocks.len() != checkpoint.blocks as usize {
            return Err(invalid("document extraction block count"));
        }
        db(self.conn.execute(
            "INSERT INTO document_extractions (id,version_id,revision,source_digest,coverage_json,block_count)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![checkpoint.id.as_str(), checkpoint.version.as_str(), checkpoint.revision.get(),
                checkpoint.source_digest, encode(&checkpoint.coverage)?, checkpoint.blocks],
        ))?;
        let mut insert = db(self.conn.prepare_cached(
            "INSERT INTO document_extracted_blocks
             (id,extraction_id,ordinal,text,citation_json,headings_json) VALUES (?1,?2,?3,?4,?5,?6)",
        ))?;
        for (ordinal, block) in blocks.iter().enumerate() {
            db(insert.execute(params![
                block.citation.block_id.as_str(),
                checkpoint.id.as_str(),
                ordinal as i64,
                block.text,
                encode(&block.citation)?,
                encode(&block.headings.as_ref())?,
            ]))?;
        }
        Ok(())
    }
}
