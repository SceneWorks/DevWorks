-- Watchers can observe a PID and birth token without owning the process (DW-98).
CREATE TABLE watchers_next (
 id TEXT PRIMARY KEY,
 revision INTEGER NOT NULL CHECK(revision>=1),
 phase TEXT NOT NULL CHECK(phase IN ('Active','Stopping','Settled','Cancelled')),
 source_kind TEXT NOT NULL CHECK(source_kind IN ('Process','Probe','Timer','File','AttachedProcess')),
 project_id TEXT REFERENCES projects,
 session_id TEXT NOT NULL REFERENCES sessions,
 actor_id TEXT NOT NULL,
 process_operation_id TEXT UNIQUE,
 executable_ref TEXT,
 tail_bytes INTEGER NOT NULL CHECK(tail_bytes BETWEEN 0 AND 65536),
 outcome_kind TEXT CHECK(outcome_kind IS NULL OR outcome_kind IN
  ('Succeeded','Failed','Cancelled','TimedOut','Interrupted','Lost','Uncertain','Exited')),
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
 probe_job_operation_id TEXT,
 probe_interval_ms INTEGER CHECK(probe_interval_ms IS NULL OR probe_interval_ms>=1000),
 probe_exit_code INTEGER,
 probe_output_pattern TEXT CHECK(probe_output_pattern IS NULL OR length(probe_output_pattern)<=4096),
 probe_runs INTEGER NOT NULL DEFAULT 0 CHECK(probe_runs>=0),
 probe_next_run_at INTEGER,
 due_at INTEGER,
 expires_at INTEGER,
 match_pattern TEXT CHECK(match_pattern IS NULL OR length(match_pattern) BETWEEN 1 AND 1024),
 match_each INTEGER NOT NULL DEFAULT 0 CHECK(match_each IN (0,1)),
 match_min_interval_ms INTEGER CHECK(match_min_interval_ms IS NULL OR match_min_interval_ms>=100),
 match_scanned INTEGER NOT NULL DEFAULT 0 CHECK(match_scanned>=0),
 match_pending INTEGER NOT NULL DEFAULT 0 CHECK(match_pending>=0),
 match_total INTEGER NOT NULL DEFAULT 0 CHECK(match_total>=0),
 match_last_event_at INTEGER,
 file_path TEXT,
 file_scan_json TEXT,
 attached_pid INTEGER CHECK(attached_pid BETWEEN 1 AND 4294967295),
 attached_birth TEXT CHECK(length(attached_birth) BETWEEN 1 AND 256),
 CHECK((phase='Settled')=(outcome_kind IS NOT NULL)),
 CHECK((phase IN ('Settled','Cancelled'))=(settled_at IS NOT NULL)),
 CHECK((source_kind IN ('Timer','File','AttachedProcess'))=(executable_ref IS NULL)),
 CHECK((source_kind='Probe')=(probe_interval_ms IS NOT NULL AND probe_job_operation_id IS NOT NULL)),
 CHECK((source_kind='Timer')=(due_at IS NOT NULL)),
 CHECK((source_kind='File')=(file_path IS NOT NULL)),
 CHECK(source_kind<>'File' OR match_pattern IS NOT NULL),
 CHECK((attached_pid IS NULL)=(attached_birth IS NULL)),
 CHECK((source_kind='AttachedProcess')=(attached_pid IS NOT NULL)),
 CHECK(source_kind<>'AttachedProcess' OR
       (process_operation_id IS NULL AND tail_bytes=0 AND stop_requested=0)),
 CHECK(outcome_kind<>'Exited' OR source_kind='AttachedProcess')
) STRICT;
INSERT INTO watchers_next (id,revision,phase,source_kind,project_id,session_id,actor_id,process_operation_id,executable_ref,
  tail_bytes,outcome_kind,exit_code,signal,outcome_detail,duration_ms,output_tail,output_truncated,
  stop_requested,created_at,updated_at,settled_at,probe_job_operation_id,probe_interval_ms,
  probe_exit_code,probe_output_pattern,probe_runs,probe_next_run_at,due_at,expires_at,match_pattern,
  match_each,match_min_interval_ms,match_scanned,match_pending,match_total,match_last_event_at,
  file_path,file_scan_json)
 SELECT id,revision,phase,source_kind,project_id,session_id,actor_id,process_operation_id,executable_ref,
  tail_bytes,outcome_kind,exit_code,signal,outcome_detail,duration_ms,output_tail,output_truncated,
  stop_requested,created_at,updated_at,settled_at,probe_job_operation_id,probe_interval_ms,
  probe_exit_code,probe_output_pattern,probe_runs,probe_next_run_at,due_at,expires_at,match_pattern,
  match_each,match_min_interval_ms,match_scanned,match_pending,match_total,match_last_event_at,
  file_path,file_scan_json
 FROM watchers;
DROP TABLE watchers;
ALTER TABLE watchers_next RENAME TO watchers;
CREATE INDEX watchers_session ON watchers(session_id,created_at,id);
CREATE INDEX watchers_project ON watchers(project_id,created_at,id);
CREATE INDEX watchers_open ON watchers(id) WHERE phase IN ('Active','Stopping');
