-- Scoped analysis attempts keep exact inputs and independently visible phase checkpoints.
CREATE TABLE analysis_runs (
    id TEXT PRIMARY KEY NOT NULL,
    operation_id TEXT NOT NULL UNIQUE,
    scope_key TEXT NOT NULL,
    binding_key TEXT NOT NULL CHECK(length(binding_key) = 64),
    lineage_key TEXT NOT NULL CHECK(length(lineage_key) = 64),
    requested_at_ms INTEGER NOT NULL,
    revision INTEGER NOT NULL CHECK(revision >= 1),
    payload_json TEXT NOT NULL CHECK(json_valid(payload_json))
) STRICT;
CREATE INDEX analysis_runs_binding ON analysis_runs(binding_key, requested_at_ms DESC, id);
CREATE INDEX analysis_runs_lineage ON analysis_runs(lineage_key, requested_at_ms DESC, id);
CREATE INDEX analysis_runs_scope ON analysis_runs(scope_key, requested_at_ms DESC, id);
CREATE INDEX analysis_runs_retention ON analysis_runs(requested_at_ms DESC, id);
