-- Immutable scope and selected IDs of an explicit operational retention request. The outcome and
-- preview consumption commit with the owned rows in the same transaction. No content is copied.
CREATE TABLE operational_retention_previews (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL REFERENCES actors(id),
    kind TEXT NOT NULL CHECK(kind IN ('Diagnostics','IndexRuns','Messages','Sessions')),
    revision INTEGER NOT NULL CHECK(revision > 0),
    expires_at_ms INTEGER NOT NULL,
    native_run_id TEXT UNIQUE,
    operation_id TEXT UNIQUE REFERENCES operations(id),
    record_json TEXT NOT NULL CHECK(json_valid(record_json))
) STRICT;
CREATE INDEX operational_retention_previews_expiry ON operational_retention_previews(expires_at_ms,id);

-- Global operator previews are oldest-first. Indexed windows keep the candidate and hold reads
-- bounded. Per-target and per-session relation indexes already serve the correlated witnesses.
CREATE INDEX diagnostic_retention_by_age ON diagnostic_records(recorded_at,id);
CREATE INDEX index_run_retention_by_age ON index_runs(COALESCE(finished_at,created_at),id);
CREATE INDEX agent_message_retention_by_age ON agent_messages(created_at,id);
CREATE INDEX session_retention_by_age ON sessions(max(created_at,COALESCE(last_seen_at,0),COALESCE(closed_at,0)),id);
CREATE INDEX agent_message_events_by_message ON agent_message_events(message_id,sequence);
CREATE INDEX agent_message_replies ON agent_messages(in_reply_to_id) WHERE in_reply_to_id IS NOT NULL;
CREATE INDEX index_runs_retention_by_target ON index_runs(repository_id,COALESCE(overlay_id,''),created_at,id);
CREATE INDEX sessions_by_conversation ON sessions(conversation_id);

-- A later removal can be below the highest removed sequence when retained rows have gaps.
ALTER TABLE diagnostic_retention ADD COLUMN retention_generation INTEGER NOT NULL DEFAULT 0
  CHECK(retention_generation>=0);

-- The remaining owners store session identity in their typed captured invocation.
CREATE INDEX decision_approval_session_retention
  ON decision_approvals(json_extract(payload_json,'$.sourceContext.invocationContext.session'))
  WHERE state='Pending';
CREATE INDEX decision_approval_legacy_session_retention
  ON decision_approvals(json_extract(payload_json,'$.invocation.session')) WHERE state='Pending';
CREATE INDEX approval_resume_session_retention
  ON approval_resumes(json_extract(invocation_json,'$.session')) WHERE state IN ('Pending','Unknown');
CREATE INDEX operation_session_retention
  ON operations(json_extract(payload_json,'$.sourceContext.invocationContext.session'))
  WHERE state NOT IN ('ObservedComplete','Failed','CancelledBeforeDispatch');
