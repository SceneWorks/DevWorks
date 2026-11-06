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
