-- An extraction revision is an immutable citation identity. A native reader's algorithm
-- revision and a later vision result are separate reasons to allocate one.
CREATE TABLE document_extraction_keys (
 version_id TEXT NOT NULL REFERENCES artifact_versions(id),
 identity_key TEXT NOT NULL CHECK(length(identity_key) BETWEEN 1 AND 80),
 extraction_id TEXT NOT NULL UNIQUE REFERENCES document_extractions(id),
 vision_targets_json TEXT NOT NULL CHECK(json_valid(vision_targets_json)
   AND json_type(vision_targets_json)='array'),
 PRIMARY KEY(version_id,identity_key)
) STRICT;

INSERT INTO document_extraction_keys(version_id,identity_key,extraction_id,vision_targets_json)
SELECT version_id,'direct:'||revision,id,'[]' FROM document_extractions;

-- Each unit retains its image capture and the original routed attempt independently of
-- assembling a text checkpoint. The exact source owns these rows even after a scope move.
CREATE TABLE document_vision_units (
 extraction_id TEXT NOT NULL REFERENCES document_extractions(id),
 target_id TEXT NOT NULL,
 record_json TEXT NOT NULL CHECK(json_valid(record_json)),
 PRIMARY KEY(extraction_id,target_id)
) STRICT;

CREATE TRIGGER document_vision_retire_version BEFORE UPDATE OF deleted_at ON artifact_versions
 WHEN OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL
BEGIN
 DELETE FROM document_vision_units WHERE extraction_id IN
   (SELECT id FROM document_extractions WHERE version_id=OLD.id);
 DELETE FROM document_extraction_keys WHERE version_id=OLD.id;
END;
