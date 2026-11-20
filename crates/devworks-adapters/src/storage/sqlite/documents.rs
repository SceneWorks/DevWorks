//! SQLite owns the atomic extraction checkpoint and per-version publication pointer.
use super::{
    conn::db,
    repos::{Snapshot, Unit},
    support::{corrupt, id_of, invalid},
};
use devworks_core::{
    ports::storage::{
        DocumentCandidate, DocumentCheckpoint, DocumentIndexVersion, DocumentLexicalBatch,
        DocumentLexicalQuery, DocumentRead, DocumentRepository, StorageError,
    },
    protocol::{Id, Revision},
    services::documents::extraction::ExtractedBlock,
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
    row.map(|(text, citation, headings)| {
        Ok(ExtractedBlock {
            text,
            citation: decode(&citation)?,
            headings: Arc::from(decode::<Vec<_>>(&headings)?),
        })
    })
    .transpose()
}

fn lexical(
    conn: &Connection,
    query: &DocumentLexicalQuery<'_>,
) -> Result<DocumentLexicalBatch, StorageError> {
    if query.words.is_empty()
        || !(1..=500).contains(&query.max_results)
        || query.max_blocks > 1_000_000
    {
        return Err(invalid("document lexical query bounds"));
    }
    let exact = query.exact_versions.map(|ids| encode(&ids)).transpose()?;
    let mut statement = db(conn.prepare_cached(
        "WITH eligible AS MATERIALIZED (
           SELECT l.id AS posting, b.id AS block, e.id AS checkpoint, e.version_id,
                  b.ordinal, i.published_generation AS generation
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
                  OR (?3 IS NOT NULL AND v.id IN (SELECT value FROM json_each(?3))))
            ORDER BY e.version_id,b.ordinal,b.id LIMIT (?5+1)
         ), tested AS MATERIALIZED (
           SELECT * FROM eligible ORDER BY version_id,ordinal,block LIMIT ?5
         ), ranked AS MATERIALIZED (
           SELECT e.*,COUNT(*) AS score
             FROM tested e CROSS JOIN json_each(?4) word
            WHERE EXISTS(SELECT 1 FROM document_lexical_fts
                          WHERE rowid=e.posting AND document_lexical_fts MATCH word.value)
            GROUP BY e.posting
            ORDER BY score DESC,version_id,ordinal,block LIMIT (?6+1)
         )
         SELECT (SELECT COUNT(*) FROM eligible)>?5, r.checkpoint,r.block,r.generation
           FROM (SELECT 1) LEFT JOIN ranked r ON TRUE
          ORDER BY r.score DESC,r.version_id,r.ordinal,r.block",
    ))?;
    // No global FTS ranking statistics, or passage text, enter the candidate selection.
    let mut rows = db(statement.query(params![
        query.scope.project().map(|project| project.0.as_str()),
        query.owner.map(|actor| actor.0.as_str()),
        exact,
        encode(&query.words)?,
        query.max_blocks,
        query.max_results,
    ]))?;
    let mut batch = DocumentLexicalBatch {
        candidates: Vec::new(),
        work_limited: false,
    };
    while let Some(row) = db(rows.next())? {
        batch.work_limited = db(row.get(0))?;
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
            fn document_lexical(
                &self,
                query: &DocumentLexicalQuery<'_>,
            ) -> Result<DocumentLexicalBatch, StorageError> {
                lexical(self.conn, query)
            }
        }
    };
}
document_reads!(Snapshot<'_>);
document_reads!(Unit<'_>);

impl DocumentRepository for Unit<'_> {
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
