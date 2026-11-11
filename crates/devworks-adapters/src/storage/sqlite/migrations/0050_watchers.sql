-- Watchers (DW-94): a watcher observes a source for its owning session; its observed outcome is
-- one durable event delivered into that session's input and acknowledged only when a turn that
-- carried it completes.
CREATE TABLE watchers (
 id TEXT PRIMARY KEY,
 revision INTEGER NOT NULL CHECK(revision>=1),
 phase TEXT NOT NULL CHECK(phase IN ('Active','Stopping','Settled','Cancelled')),
 source_kind TEXT NOT NULL CHECK(source_kind IN ('Process')),
 project_id TEXT REFERENCES projects,
 session_id TEXT NOT NULL REFERENCES sessions,
 actor_id TEXT NOT NULL,
 process_operation_id TEXT UNIQUE,
 executable_ref TEXT NOT NULL,
 tail_bytes INTEGER NOT NULL CHECK(tail_bytes BETWEEN 0 AND 65536),
 outcome_kind TEXT CHECK(outcome_kind IS NULL OR outcome_kind IN
  ('Succeeded','Failed','Cancelled','TimedOut','Interrupted','Lost','Uncertain')),
 exit_code INTEGER,
 signal INTEGER,
 outcome_detail TEXT CHECK(outcome_detail IS NULL OR length(outcome_detail)<=1024),
 duration_ms INTEGER CHECK(duration_ms IS NULL OR duration_ms>=0),
 output_tail BLOB CHECK(output_tail IS NULL OR length(output_tail)<=65536),
 output_truncated INTEGER NOT NULL CHECK(output_truncated IN (0,1)),
 stop_requested INTEGER NOT NULL CHECK(stop_requested IN (0,1)),
 created_at INTEGER NOT NULL CHECK(created_at>=0),
 updated_at INTEGER NOT NULL CHECK(updated_at>=created_at),
 settled_at INTEGER,
 CHECK((phase='Settled')=(outcome_kind IS NOT NULL)),
 CHECK((phase IN ('Settled','Cancelled'))=(settled_at IS NOT NULL))
) STRICT;
CREATE INDEX watchers_session ON watchers(session_id,id);
CREATE INDEX watchers_project ON watchers(project_id,id);
CREATE INDEX watchers_open ON watchers(id) WHERE phase IN ('Active','Stopping');

CREATE TABLE watcher_events (
 id TEXT PRIMARY KEY,
 watcher_id TEXT NOT NULL UNIQUE REFERENCES watchers,
 session_id TEXT NOT NULL REFERENCES sessions,
 project_id TEXT REFERENCES projects,
 state TEXT NOT NULL CHECK(state IN ('Pending','Delivered','Acknowledged')),
 payload_json TEXT NOT NULL CHECK(json_valid(payload_json) AND length(CAST(payload_json AS BLOB))<=262144),
 transcript_sequence INTEGER,
 carried_turn_id TEXT,
 created_at INTEGER NOT NULL CHECK(created_at>=0),
 delivered_at INTEGER,
 acknowledged_at INTEGER,
 CHECK((state='Pending')=(transcript_sequence IS NULL)),
 CHECK((state='Pending')=(delivered_at IS NULL)),
 CHECK((state='Acknowledged')=(acknowledged_at IS NOT NULL)),
 CHECK(carried_turn_id IS NULL OR state='Delivered')
) STRICT;
CREATE INDEX watcher_events_session ON watcher_events(session_id,state,created_at,id);
CREATE INDEX watcher_events_turn ON watcher_events(carried_turn_id) WHERE carried_turn_id IS NOT NULL;
