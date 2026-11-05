-- Immutable reports are independent records; grading never changes its source session.
CREATE TABLE eval_runs (
 id TEXT PRIMARY KEY,
 suite_id TEXT NOT NULL,
 suite_revision INTEGER NOT NULL CHECK(suite_revision>=0),
 project_id TEXT REFERENCES projects,
 session_id TEXT NOT NULL REFERENCES sessions,
 session_revision INTEGER NOT NULL CHECK(session_revision>=0),
 session_sequence INTEGER NOT NULL CHECK(session_sequence>=0),
 measurement_digest TEXT NOT NULL CHECK(length(measurement_digest)=64 AND measurement_digest NOT GLOB '*[^0-9a-f]*'),
 created_at INTEGER NOT NULL CHECK(created_at>=0),
 source_watermark INTEGER NOT NULL CHECK(source_watermark>=0),
 report_json TEXT NOT NULL CHECK(json_valid(report_json) AND length(CAST(report_json AS BLOB))<=1048576)
) STRICT;
CREATE INDEX eval_runs_session ON eval_runs(session_id,id);
CREATE TRIGGER eval_runs_immutable_update BEFORE UPDATE ON eval_runs
 BEGIN SELECT RAISE(ABORT,'eval reports are immutable'); END;
CREATE TRIGGER eval_runs_immutable_delete BEFORE DELETE ON eval_runs
 BEGIN SELECT RAISE(ABORT,'eval reports are immutable'); END;
