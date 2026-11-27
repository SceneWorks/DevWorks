-- Eval suites (DW-72) are a third kind of versioned instruction item: file-backed folders in the
-- shared and harness eval roots, with the same immutable revisions and approval-bound
-- activation as skills. Rebuild the item table to admit the kind; every row is kept as it is.
CREATE TABLE instructions_v71 (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK(kind IN ('Skill','Role','Suite')),
    name TEXT NOT NULL CHECK(length(name) BETWEEN 1 AND 128),
    scope_key TEXT NOT NULL,
    project_id TEXT,
    origin TEXT NOT NULL CHECK(origin IN ('Shared','Harness','Role')),
    root_path TEXT,
    source_path TEXT,
    latest_revision INTEGER NOT NULL CHECK(latest_revision >= 0),
    active_revision INTEGER CHECK(active_revision IS NULL OR active_revision BETWEEN 1 AND latest_revision),
    enabled INTEGER NOT NULL CHECK(enabled IN (0,1)),
    materialized_digest TEXT,
    observed_digest TEXT,
    present INTEGER NOT NULL CHECK(present IN (0,1)),
    version INTEGER NOT NULL CHECK(version > 0),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    confined_project_id TEXT,
    CHECK((scope_key = 'workspace' AND project_id IS NULL) OR scope_key = 'project:' || project_id),
    CHECK(confined_project_id IS NULL OR (project_id IS NULL AND active_revision IS NULL)),
    CHECK((kind = 'Role') = (origin = 'Role')),
    CHECK(kind <> 'Suite' OR scope_key = 'workspace'),
    UNIQUE(kind, scope_key, name, origin)
) STRICT;
INSERT INTO instructions_v71 SELECT * FROM instructions;
DROP TABLE instructions;
ALTER TABLE instructions_v71 RENAME TO instructions;
CREATE INDEX instructions_project ON instructions(project_id, id) WHERE project_id IS NOT NULL;
CREATE INDEX instructions_confined ON instructions(confined_project_id, id) WHERE confined_project_id IS NOT NULL;
