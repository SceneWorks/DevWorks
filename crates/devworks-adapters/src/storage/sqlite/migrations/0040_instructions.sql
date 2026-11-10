-- Versioned skills and role instructions. Revisions are immutable; content lives in managed
-- artifacts. Only an Approved revision can be active. Shared skill sources are never written.
-- A workspace item or revision drafted by a project-confined agent is confined to that project
-- (confined_project_id) until a person approves it, so no other project can read the draft.
CREATE TABLE instructions (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK(kind IN ('Skill','Role')),
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
    UNIQUE(kind, scope_key, name, origin)
) STRICT;
CREATE INDEX instructions_project ON instructions(project_id, id) WHERE project_id IS NOT NULL;
CREATE INDEX instructions_confined ON instructions(confined_project_id, id) WHERE confined_project_id IS NOT NULL;
CREATE TABLE instruction_revisions (
    instruction_id TEXT NOT NULL REFERENCES instructions(id),
    revision INTEGER NOT NULL CHECK(revision > 0),
    content_json TEXT NOT NULL CHECK(json_valid(content_json)),
    content_digest TEXT NOT NULL CHECK(length(content_digest) = 64),
    state TEXT NOT NULL CHECK(state IN ('Draft','Proposed','Approved','Rejected')),
    author TEXT NOT NULL,
    origin_json TEXT NOT NULL CHECK(json_valid(origin_json)),
    resources_json TEXT NOT NULL CHECK(json_valid(resources_json)),
    applicability_json TEXT NOT NULL CHECK(json_valid(applicability_json)),
    approval_id TEXT UNIQUE,
    created_at INTEGER NOT NULL,
    decided_at INTEGER,
    confined_project_id TEXT,
    PRIMARY KEY(instruction_id, revision),
    CHECK(approval_id IS NULL OR state IN ('Proposed','Approved','Rejected')),
    CHECK(confined_project_id IS NULL OR state <> 'Approved')
) STRICT;
CREATE INDEX instruction_revisions_confined ON instruction_revisions(confined_project_id) WHERE confined_project_id IS NOT NULL;
