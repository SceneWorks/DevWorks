-- Import profiles and runs are workspace-owned. Their records become project-local only when
-- the importer publishes a mapped destination through that project's existing write stores.
CREATE TABLE work_import_configs (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL REFERENCES actors(id),
    source_namespace TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK(revision > 0),
    record_json TEXT NOT NULL CHECK(json_valid(record_json)),
    UNIQUE(owner, source_namespace)
) STRICT;

CREATE TABLE work_import_runs (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    owner TEXT NOT NULL REFERENCES actors(id),
    source_namespace TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK(revision > 0),
    source_snapshot TEXT NOT NULL CHECK(length(source_snapshot) = 64),
    scope_digest TEXT NOT NULL CHECK(length(scope_digest) = 64),
    preview_digest TEXT NOT NULL CHECK(length(preview_digest) = 64),
    is_preview INTEGER NOT NULL CHECK(is_preview IN (0, 1)),
    completed_at_ms INTEGER,
    record_json TEXT NOT NULL CHECK(json_valid(record_json))
) STRICT;
CREATE INDEX work_import_runs_list ON work_import_runs(owner, sequence DESC);
CREATE INDEX work_import_runs_source ON work_import_runs(owner, source_namespace, sequence DESC);
CREATE INDEX work_import_runs_preview ON work_import_runs(owner, preview_digest) WHERE is_preview = 1;
CREATE INDEX work_import_runs_active ON work_import_runs(source_namespace, sequence DESC)
    WHERE is_preview = 0 AND completed_at_ms IS NULL;
CREATE INDEX work_import_runs_watermark ON work_import_runs(
    owner, source_namespace, scope_digest, completed_at_ms DESC
) WHERE is_preview = 0 AND completed_at_ms IS NOT NULL;

CREATE TABLE work_import_mappings (
    import_id TEXT NOT NULL,
    source_kind TEXT NOT NULL,
    source_id TEXT NOT NULL,
    local_id TEXT NOT NULL,
    source_digest TEXT NOT NULL CHECK(length(source_digest) = 64),
    observed_through_ms INTEGER NOT NULL CHECK(observed_through_ms > 0),
    destination_revision INTEGER CHECK(destination_revision IS NULL OR destination_revision > 0),
    project_id TEXT NOT NULL REFERENCES projects(id),
    PRIMARY KEY(import_id, source_kind, source_id)
) STRICT;
CREATE INDEX work_import_mappings_project ON work_import_mappings(project_id);
