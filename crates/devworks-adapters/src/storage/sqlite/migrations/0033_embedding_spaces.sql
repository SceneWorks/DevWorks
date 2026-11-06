-- DW-119: immutable semantic identities, append-only host qualification, explicit selection.
-- The original embedding_spaces table remains the vector FK owner. A candidate is not a
-- registered vector space until its native qualification has completed successfully.
CREATE TABLE router_embedding_candidates (
    id TEXT PRIMARY KEY CHECK(length(id) BETWEEN 1 AND 128 AND id NOT GLOB '*[^A-Za-z0-9_-]*'),
    space_id TEXT NOT NULL CHECK(length(space_id) BETWEEN 1 AND 128 AND space_id NOT GLOB '*[^A-Za-z0-9_-]*'),
    configuration_digest TEXT NOT NULL CHECK(length(configuration_digest)=64),
    body_json TEXT NOT NULL CHECK(json_valid(body_json) AND json_type(body_json)='object'),
    created_at INTEGER NOT NULL CHECK(created_at>=0)
) STRICT;
CREATE INDEX router_embedding_candidates_space ON router_embedding_candidates(space_id,id);
CREATE TABLE router_embedding_qualifications (
    candidate_id TEXT NOT NULL REFERENCES router_embedding_candidates(id),
    revision INTEGER NOT NULL CHECK(revision>0),
    state TEXT NOT NULL CHECK(state IN ('Unqualified','Qualifying','Qualified','Failed','Unknown','Cancelled')),
    operation_id TEXT REFERENCES operations(id),
    body_json TEXT NOT NULL CHECK(json_valid(body_json) AND json_type(body_json)='object'),
    updated_at INTEGER NOT NULL CHECK(updated_at>=0),
    PRIMARY KEY(candidate_id,revision)
) STRICT;
CREATE INDEX router_embedding_qualifications_operation ON router_embedding_qualifications(operation_id);
CREATE TABLE router_embedding_selection (
    revision INTEGER PRIMARY KEY CHECK(revision>0),
    candidate_id TEXT REFERENCES router_embedding_candidates(id),
    space_id TEXT REFERENCES embedding_spaces(id),
    CHECK((candidate_id IS NULL)=(space_id IS NULL))
) STRICT;
INSERT INTO router_embedding_selection(revision,candidate_id,space_id) VALUES(1,NULL,NULL);
-- Native request/admission identities belong to one durable qualification intent. Historical
-- revisions repeat the same probe, so these are lookup indexes rather than per-row uniqueness.
CREATE INDEX router_embedding_probe_request ON router_embedding_qualifications(
    json_extract(body_json,'$.probe.requestId'),operation_id) WHERE operation_id IS NOT NULL;
CREATE INDEX router_embedding_probe_admission ON router_embedding_qualifications(
    json_extract(body_json,'$.probe.admissionKey'),operation_id) WHERE operation_id IS NOT NULL;

-- A query cursor cannot use history sequence alone: journal status and original authority can
-- change without appending history. These indexed dependency lookups never materialize the
-- candidate population in Rust. The counter is a change fence, not a count or model identity.
CREATE TABLE router_embedding_inventory (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    revision INTEGER NOT NULL CHECK(revision>0)
) STRICT;
INSERT INTO router_embedding_inventory(singleton,revision) VALUES(1,1);
CREATE INDEX router_embedding_candidate_connection ON router_embedding_candidates(
    json_extract(body_json,'$.connectionId'));
CREATE INDEX router_embedding_resultArtifact_artifactId ON router_embedding_qualifications(json_extract(body_json,'$.evidence.resultArtifact.artifactId'));
CREATE INDEX router_embedding_resultArtifact_versionId ON router_embedding_qualifications(json_extract(body_json,'$.evidence.resultArtifact.versionId'));
CREATE INDEX router_embedding_resultArtifact_sha256 ON router_embedding_qualifications(json_extract(body_json,'$.evidence.resultArtifact.sha256'));
CREATE INDEX router_embedding_vectorArtifact_artifactId ON router_embedding_qualifications(json_extract(body_json,'$.evidence.vectorArtifact.artifactId'));
CREATE INDEX router_embedding_vectorArtifact_versionId ON router_embedding_qualifications(json_extract(body_json,'$.evidence.vectorArtifact.versionId'));
CREATE INDEX router_embedding_vectorArtifact_sha256 ON router_embedding_qualifications(json_extract(body_json,'$.evidence.vectorArtifact.sha256'));
CREATE TRIGGER router_embedding_candidates_inventory_insert AFTER INSERT ON router_embedding_candidates
BEGIN UPDATE router_embedding_inventory SET revision=revision+1 WHERE singleton=1; END;
CREATE TRIGGER router_embedding_qualifications_inventory_insert AFTER INSERT ON router_embedding_qualifications
BEGIN UPDATE router_embedding_inventory SET revision=revision+1 WHERE singleton=1; END;
CREATE TRIGGER router_embedding_selection_inventory_insert AFTER INSERT ON router_embedding_selection
BEGIN UPDATE router_embedding_inventory SET revision=revision+1 WHERE singleton=1; END;
CREATE TRIGGER router_embedding_connection_insert AFTER INSERT ON router_connections
WHEN EXISTS(SELECT 1 FROM router_embedding_candidates WHERE json_extract(body_json,'$.connectionId')=NEW.id)
BEGIN UPDATE router_embedding_inventory SET revision=revision+1 WHERE singleton=1; END;
CREATE TRIGGER router_embedding_connection_update AFTER UPDATE ON router_connections
WHEN EXISTS(SELECT 1 FROM router_embedding_candidates WHERE json_extract(body_json,'$.connectionId')=NEW.id)
BEGIN UPDATE router_embedding_inventory SET revision=revision+1 WHERE singleton=1; END;
CREATE TRIGGER router_embedding_connection_delete AFTER DELETE ON router_connections
WHEN EXISTS(SELECT 1 FROM router_embedding_candidates WHERE json_extract(body_json,'$.connectionId')=OLD.id)
BEGIN UPDATE router_embedding_inventory SET revision=revision+1 WHERE singleton=1; END;
CREATE TRIGGER router_embedding_operation_update AFTER UPDATE ON operations
WHEN EXISTS(SELECT 1 FROM router_embedding_qualifications WHERE operation_id=NEW.id)
BEGIN UPDATE router_embedding_inventory SET revision=revision+1 WHERE singleton=1; END;
CREATE TRIGGER router_embedding_artifacts_update AFTER UPDATE ON artifacts
WHEN EXISTS(SELECT 1 FROM router_embedding_qualifications WHERE json_extract(body_json,'$.evidence.resultArtifact.artifactId')=NEW.id) OR EXISTS(SELECT 1 FROM router_embedding_qualifications WHERE json_extract(body_json,'$.evidence.vectorArtifact.artifactId')=NEW.id)
BEGIN UPDATE router_embedding_inventory SET revision=revision+1 WHERE singleton=1; END;
CREATE TRIGGER router_embedding_artifact_versions_update AFTER UPDATE ON artifact_versions
WHEN EXISTS(SELECT 1 FROM router_embedding_qualifications WHERE json_extract(body_json,'$.evidence.resultArtifact.versionId')=NEW.id) OR EXISTS(SELECT 1 FROM router_embedding_qualifications WHERE json_extract(body_json,'$.evidence.vectorArtifact.versionId')=NEW.id)
BEGIN UPDATE router_embedding_inventory SET revision=revision+1 WHERE singleton=1; END;
CREATE TRIGGER router_embedding_blobs_update AFTER UPDATE ON blobs
WHEN EXISTS(SELECT 1 FROM router_embedding_qualifications WHERE json_extract(body_json,'$.evidence.resultArtifact.sha256')=NEW.digest) OR EXISTS(SELECT 1 FROM router_embedding_qualifications WHERE json_extract(body_json,'$.evidence.vectorArtifact.sha256')=NEW.digest)
BEGIN UPDATE router_embedding_inventory SET revision=revision+1 WHERE singleton=1; END;
CREATE TRIGGER router_embedding_principal_insert AFTER INSERT ON principal_authority
BEGIN UPDATE router_embedding_inventory SET revision=revision+1 WHERE singleton=1; END;
CREATE TRIGGER router_embedding_principal_update AFTER UPDATE ON principal_authority
BEGIN UPDATE router_embedding_inventory SET revision=revision+1 WHERE singleton=1; END;
CREATE TRIGGER router_embedding_principal_delete AFTER DELETE ON principal_authority
BEGIN UPDATE router_embedding_inventory SET revision=revision+1 WHERE singleton=1; END;
CREATE TRIGGER router_embedding_configuration_insert AFTER INSERT ON configuration_revisions
BEGIN UPDATE router_embedding_inventory SET revision=revision+1 WHERE singleton=1; END;
