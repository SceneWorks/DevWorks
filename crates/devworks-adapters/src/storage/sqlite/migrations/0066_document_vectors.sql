-- The run retains its exact extraction and selected route even when it produces no vectors.
CREATE TABLE document_embedding_runs (
 run_id TEXT PRIMARY KEY,
 extraction_id TEXT NOT NULL REFERENCES document_extractions(id),
 space_id TEXT REFERENCES embedding_spaces(id),
 configuration_digest TEXT NOT NULL CHECK(length(configuration_digest)=64),
 complete INTEGER NOT NULL DEFAULT 0 CHECK(complete IN (0,1) AND (complete=0 OR space_id IS NOT NULL))
) STRICT;
CREATE INDEX document_embedding_runs_published
 ON document_embedding_runs(extraction_id,space_id) WHERE complete=1;

-- Only complete runs expose these derivative rows. An unchanged extraction can resume a partial
-- batch or reuse the same qualified space without duplicating vector storage.
CREATE TABLE document_vectors (
 extraction_id TEXT NOT NULL REFERENCES document_extractions(id),
 space_id TEXT NOT NULL,
 dimensions INTEGER NOT NULL,
 block_id TEXT NOT NULL REFERENCES document_extracted_blocks(id),
 vector BLOB NOT NULL CHECK(length(vector)=dimensions*4),
 PRIMARY KEY(block_id,space_id),
 FOREIGN KEY(space_id,dimensions) REFERENCES embedding_spaces(id,dimensions)
) STRICT;
CREATE INDEX document_vectors_extraction ON document_vectors(extraction_id,space_id);

-- The derivative rows must be removed before migration 64's tombstone trigger retires
-- their extraction. Project purge already removes every prepared row in dependency order.
CREATE TRIGGER document_vector_retire_version BEFORE UPDATE OF deleted_at ON artifact_versions
 WHEN OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL
BEGIN
 DELETE FROM document_vectors WHERE extraction_id IN
   (SELECT id FROM document_extractions WHERE version_id=OLD.id);
 DELETE FROM document_embedding_runs WHERE extraction_id IN
   (SELECT id FROM document_extractions WHERE version_id=OLD.id);
END;
