-- Retained diagnostic log records (LOG-01..03, OPS-01): host work, the UI, agent tools and
-- packages write diagnostics here, apart from history events, transcripts and model history.
-- Additive: one new table and its indexes; it references no table of migration 11.
-- `sequence` is AUTOINCREMENT so a sequence is never reused after pruning: readers tail the log
-- by "strictly after sequence N".
CREATE TABLE diagnostic_records (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE CHECK (length(id) BETWEEN 1 AND 128),
    recorded_at INTEGER NOT NULL CHECK (recorded_at >= 0),
    severity TEXT NOT NULL CHECK (severity IN ('Info', 'Warning', 'Error')),
    severity_rank INTEGER NOT NULL CHECK (
        severity_rank = CASE severity WHEN 'Info' THEN 0 WHEN 'Warning' THEN 1 ELSE 2 END
    ),
    component TEXT NOT NULL CHECK (length(component) BETWEEN 1 AND 1024),
    code TEXT NOT NULL CHECK (length(code) BETWEEN 1 AND 1024),
    message TEXT NOT NULL CHECK (length(CAST(message AS BLOB)) <= 65536),
    origin TEXT NOT NULL CHECK (origin IN ('Host', 'Reported')),
    actor_id TEXT NOT NULL CHECK (length(actor_id) BETWEEN 1 AND 128),
    correlation_id TEXT,
    project_id TEXT,
    operation TEXT,
    operation_id TEXT,
    target_kind TEXT,
    target_id TEXT,
    stage TEXT,
    source_revision TEXT,
    attempt INTEGER CHECK (attempt IS NULL OR attempt >= 0),
    started_at INTEGER CHECK (started_at IS NULL OR started_at >= 0),
    finished_at INTEGER CHECK (
        finished_at IS NULL OR (finished_at >= 0 AND (started_at IS NULL OR finished_at >= started_at))
    ),
    progress_completed INTEGER CHECK (progress_completed IS NULL OR progress_completed >= 0),
    progress_total INTEGER CHECK (
        progress_total IS NULL
        OR (progress_total >= 0 AND (progress_completed IS NULL OR progress_completed <= progress_total))
    ),
    outcome TEXT CHECK (
        outcome IS NULL
        OR outcome IN ('Running', 'Succeeded', 'Partial', 'Stale', 'Cancelled', 'Retryable', 'Failed')
    ),
    error_class TEXT CHECK (
        error_class IS NULL
        OR error_class IN (
            'Transient', 'Timeout', 'Cancelled', 'Configuration', 'Authentication', 'Permission',
            'NotFound', 'InvalidInput', 'ResourceExhausted', 'Dependency', 'Corruption', 'Internal',
            'Unknown'
        )
    ),
    output TEXT CHECK (output IS NULL OR length(CAST(output AS BLOB)) <= 65536),
    output_omitted_bytes INTEGER NOT NULL DEFAULT 0 CHECK (output_omitted_bytes >= 0),
    redactions INTEGER NOT NULL DEFAULT 0 CHECK (redactions >= 0),
    evidence_json TEXT CHECK (
        evidence_json IS NULL OR (json_valid(evidence_json) AND json_type(evidence_json) = 'object')
    ),
    metrics_json TEXT NOT NULL DEFAULT '[]' CHECK (
        json_valid(metrics_json) AND json_type(metrics_json) = 'array'
    ),
    recovery_json TEXT NOT NULL DEFAULT '[]' CHECK (
        json_valid(recovery_json) AND json_type(recovery_json) = 'array'
    )
) STRICT;
CREATE INDEX diagnostic_records_component ON diagnostic_records(component, sequence);
CREATE INDEX diagnostic_records_recorded ON diagnostic_records(recorded_at);
CREATE INDEX diagnostic_records_project ON diagnostic_records(project_id, sequence);
CREATE INDEX diagnostic_records_operation ON diagnostic_records(operation_id, sequence)
    WHERE operation_id IS NOT NULL;
CREATE INDEX diagnostic_records_correlation ON diagnostic_records(correlation_id, sequence)
    WHERE correlation_id IS NOT NULL;
