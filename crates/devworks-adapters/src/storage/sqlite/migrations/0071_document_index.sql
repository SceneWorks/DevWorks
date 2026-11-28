CREATE TABLE document_retrieval_preferences (
 artifact_id TEXT PRIMARY KEY REFERENCES artifacts(id),
 enabled INTEGER NOT NULL CHECK(enabled IN (0,1))
) STRICT;

CREATE TABLE document_extractions (
 id TEXT PRIMARY KEY,
 version_id TEXT NOT NULL REFERENCES artifact_versions(id),
 revision INTEGER NOT NULL CHECK(revision>0),
 source_digest TEXT NOT NULL CHECK(length(source_digest)=64),
 coverage_json TEXT NOT NULL CHECK(json_valid(coverage_json)),
 block_count INTEGER NOT NULL CHECK(block_count BETWEEN 0 AND 16384),
 UNIQUE(version_id,revision)
) STRICT;

CREATE TABLE document_extracted_blocks (
 id TEXT PRIMARY KEY,
 extraction_id TEXT NOT NULL REFERENCES document_extractions(id),
 ordinal INTEGER NOT NULL CHECK(ordinal>=0),
 text TEXT NOT NULL CHECK(length(CAST(text AS BLOB)) BETWEEN 1 AND 4096),
 citation_json TEXT NOT NULL CHECK(json_valid(citation_json)),
 headings_json TEXT NOT NULL CHECK(json_valid(headings_json)),
 UNIQUE(extraction_id,ordinal)
) STRICT;

CREATE TABLE document_index_versions (
 version_id TEXT PRIMARY KEY REFERENCES artifact_versions(id),
 latest_operation TEXT NOT NULL,
 stage TEXT NOT NULL CHECK(stage IN ('Queued','Extracted','Ready','Partial','Failed','Cancelled','Removed')),
 diagnostic TEXT,
 published_generation TEXT,
 published_revision INTEGER CHECK(published_revision>0),
 CHECK((published_generation IS NULL)=(published_revision IS NULL))
) STRICT;

-- Explicit version deletion makes extracted content unreachable immediately. The version
-- tombstone remains for typed unavailability, and physical original-byte cleanup is separate.
CREATE TRIGGER retire_document_version AFTER UPDATE OF deleted_at ON artifact_versions
 WHEN OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL
BEGIN
 DELETE FROM document_index_versions WHERE version_id=NEW.id;
 DELETE FROM document_extracted_blocks WHERE extraction_id IN
   (SELECT id FROM document_extractions WHERE version_id=NEW.id);
 DELETE FROM document_extractions WHERE version_id=NEW.id;
END;

CREATE TRIGGER retire_document_preferences AFTER UPDATE OF state ON artifacts
 WHEN OLD.state='Live' AND NEW.state<>'Live'
BEGIN
 DELETE FROM document_retrieval_preferences WHERE artifact_id=NEW.id;
END;
