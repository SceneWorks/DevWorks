//! SQLite owns the atomic extraction checkpoint and per-version publication pointer.
use super::{
    conn::db,
    repos::{Snapshot, Unit},
    support::{corrupt, id_of, invalid},
};
use devworks_core::{
    ports::storage::{
        DocumentCheckpoint, DocumentIndexVersion, DocumentRead, DocumentRepository, StorageError,
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
            fn document_passage(
                &self,
                checkpoint: &Id,
                block: &Id,
            ) -> Result<Option<ExtractedBlock>, StorageError> {
                passage(self.conn, checkpoint, block)
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
