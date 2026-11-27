CREATE TABLE document_transfers (
    id TEXT PRIMARY KEY,
    artifact_id TEXT NOT NULL REFERENCES artifacts,
    source_project_id TEXT NOT NULL REFERENCES projects,
    destination_project_id TEXT NOT NULL REFERENCES projects,
    proposal_json TEXT NOT NULL CHECK(json_valid(proposal_json)),
    revision INTEGER NOT NULL CHECK(revision IN (1,2)),
    state TEXT NOT NULL CHECK(state IN ('Pending','Shared','Moved','Declined')),
    CHECK(source_project_id <> destination_project_id),
    CHECK((state = 'Pending') = (revision = 1))
) STRICT;
CREATE INDEX document_transfers_artifact ON document_transfers(artifact_id);
CREATE INDEX document_transfers_source ON document_transfers(source_project_id);
CREATE INDEX document_transfers_destination ON document_transfers(destination_project_id);
