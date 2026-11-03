-- Artifact store lifecycle. Additive: the artifact tables of migration 1 gain reference and
-- current-version bookkeeping, explicit deletion state, creation provenance, the blob file
-- generation, removal claims and a visible removal error; backup pins and listing/navigation
-- indexes are new. Retained versions are never rewritten: deletion sets
-- artifact_versions.deleted_at (tombstone) and blobs.state.
ALTER TABLE artifacts ADD COLUMN reference_revision INTEGER NOT NULL DEFAULT 1 CHECK(reference_revision>0);
ALTER TABLE artifacts ADD COLUMN current_version_id TEXT REFERENCES artifact_versions;
ALTER TABLE artifacts ADD COLUMN state TEXT NOT NULL DEFAULT 'Live' CHECK(state IN ('Live','Deleted'));
ALTER TABLE artifacts ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE artifacts ADD COLUMN provenance_json TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(provenance_json));
-- The file incarnation a blob row names (`<digest>.<generation>`): every publication of new
-- bytes gets a generation never used before, so removing one incarnation can never remove
-- another. `claim_owner` names the holder of an `Unlinking` claim on that generation.
ALTER TABLE blobs ADD COLUMN generation TEXT NOT NULL DEFAULT '';
ALTER TABLE blobs ADD COLUMN claim_owner TEXT;
ALTER TABLE blobs ADD COLUMN last_error TEXT;
CREATE INDEX artifact_project_list ON artifacts(current_project_id,state,created_at DESC,id DESC);
CREATE INDEX artifact_version_blob ON artifact_versions(blob_digest,deleted_at);
CREATE INDEX artifact_version_artifact ON artifact_versions(artifact_id,version_no);
CREATE INDEX artifact_link_subject ON artifact_links(subject_kind,subject_id);
CREATE INDEX artifact_share_project ON artifact_shares(project_id,artifact_id);
CREATE INDEX blob_state ON blobs(state,digest);
-- A holder (a manual backup in progress) keeps pinned blobs from physical removal.
CREATE TABLE artifact_blob_pins (
 holder_id TEXT NOT NULL, blob_digest TEXT NOT NULL REFERENCES blobs, created_at INTEGER NOT NULL,
 PRIMARY KEY(holder_id,blob_digest)
) STRICT;
CREATE INDEX artifact_pin_blob ON artifact_blob_pins(blob_digest);
-- Unresolved staging operations in ID order (recovery pages them).
CREATE INDEX staging_unresolved ON operations(id,state) WHERE capability='artifacts.stage_file';
