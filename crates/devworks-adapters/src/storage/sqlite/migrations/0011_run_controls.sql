-- DevWorks store migration 0011: run root controls (CONTROL-01..13).
-- Additive. root_controls, executions, assignments and blockers (migration 1) were never written
-- by an earlier build; they gain the columns run control needs, all defaulted or nullable:
-- * root_controls: the redirect context revision, the adopted active plan revision, the stop
--   epoch and the journaled stop operation, a reactivation request made while the previous
--   activation is still stopping, and the update time.
-- * executions (activations): purpose, claim epoch (fence), bounded retry bookkeeping, the
--   requested/acknowledged/stopped stop stages with their cause, the attributed outcome and the
--   active plan revision the activation was accepted under.
-- * assignments: the approved scope unit and digest, plan and instruction revisions it is bound
--   to, and its creation time; at most one live coordinator assignment per root.
-- * blockers: an inspectable reason, creation and clearance attribution.
-- New tables: run_instructions (redirect identity and its received/acknowledged/incorporated
-- stages, linked to the coordinator transcript) and session_turns (at most one inference or
-- continuation turn in flight per session). Never edit this file after release: add a new
-- migration.
ALTER TABLE root_controls ADD COLUMN instruction_revision INTEGER NOT NULL DEFAULT 0 CHECK(instruction_revision>=0);
ALTER TABLE root_controls ADD COLUMN adopted_plan_revision INTEGER;
ALTER TABLE root_controls ADD COLUMN stop_epoch INTEGER NOT NULL DEFAULT 0 CHECK(stop_epoch>=0);
ALTER TABLE root_controls ADD COLUMN stop_operation_id TEXT REFERENCES operations;
ALTER TABLE root_controls ADD COLUMN reactivate INTEGER NOT NULL DEFAULT 0 CHECK(reactivate IN (0,1));
ALTER TABLE root_controls ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0;
CREATE INDEX root_control_stop ON root_controls(stop_operation_id) WHERE stop_operation_id IS NOT NULL;
ALTER TABLE executions ADD COLUMN purpose TEXT NOT NULL DEFAULT 'Resume' CHECK(purpose IN ('Planning','Implementation','Resume'));
ALTER TABLE executions ADD COLUMN owner_epoch INTEGER NOT NULL DEFAULT 0 CHECK(owner_epoch>=0);
ALTER TABLE executions ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts>=0);
ALTER TABLE executions ADD COLUMN ready_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE executions ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE executions ADD COLUMN stop_cause TEXT CHECK(stop_cause IS NULL OR stop_cause IN ('Pause','Stop','Shutdown','Restart','Archive','Failure','Superseded'));
ALTER TABLE executions ADD COLUMN stop_requested_at INTEGER;
ALTER TABLE executions ADD COLUMN stop_acknowledged_at INTEGER;
ALTER TABLE executions ADD COLUMN stopped_at INTEGER;
ALTER TABLE executions ADD COLUMN outcome TEXT CHECK(outcome IS NULL OR outcome IN ('Completed','Failed','Cancelled','Interrupted','Prevented'));
ALTER TABLE executions ADD COLUMN detail TEXT CHECK(detail IS NULL OR length(detail)<=4096);
ALTER TABLE executions ADD COLUMN plan_revision INTEGER;
CREATE INDEX execution_root ON executions(root_id,generation);
CREATE INDEX execution_live ON executions(phase,boot_id,ready_at,id) WHERE phase<>'Stopped';
ALTER TABLE assignments ADD COLUMN scope_key TEXT CHECK(scope_key IS NULL OR length(scope_key) BETWEEN 1 AND 256);
ALTER TABLE assignments ADD COLUMN scope_digest TEXT;
ALTER TABLE assignments ADD COLUMN plan_revision INTEGER;
ALTER TABLE assignments ADD COLUMN instruction_revision INTEGER NOT NULL DEFAULT 0 CHECK(instruction_revision>=0);
ALTER TABLE assignments ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
CREATE UNIQUE INDEX one_root_coordinator ON assignments(root_id)
 WHERE parent_id IS NULL AND state IN ('Ready','Active','Waiting');
ALTER TABLE blockers ADD COLUMN reason TEXT NOT NULL DEFAULT '' CHECK(length(reason)<=4096);
ALTER TABLE blockers ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE blockers ADD COLUMN cleared_at INTEGER;
ALTER TABLE blockers ADD COLUMN cleared_by TEXT REFERENCES actors;
CREATE INDEX blocker_assignment ON blockers(assignment_id,state) WHERE assignment_id IS NOT NULL;
CREATE INDEX blocker_operation ON blockers(operation_id,state) WHERE operation_id IS NOT NULL;
CREATE TABLE run_instructions (
 id TEXT PRIMARY KEY, root_id TEXT NOT NULL REFERENCES work_items,
 seq INTEGER NOT NULL CHECK(seq>0), actor_id TEXT NOT NULL REFERENCES actors,
 message TEXT NOT NULL CHECK(length(message)<=65536),
 attachments_json TEXT NOT NULL CHECK(json_valid(attachments_json) AND json_type(attachments_json)='array'),
 control_revision INTEGER NOT NULL CHECK(control_revision>0),
 instruction_revision INTEGER NOT NULL CHECK(instruction_revision>0),
 stage TEXT NOT NULL CHECK(stage IN ('Received','Acknowledged','Incorporated')),
 received_at INTEGER NOT NULL, acknowledged_at INTEGER, acknowledged_by TEXT REFERENCES executions,
 incorporated_at INTEGER,
 disposition_json TEXT CHECK(disposition_json IS NULL OR json_valid(disposition_json)),
 session_id TEXT REFERENCES sessions, transcript_entry_id TEXT,
 revision INTEGER NOT NULL CHECK(revision>0),
 UNIQUE(root_id,seq),
 CHECK((stage='Received')=(acknowledged_at IS NULL)),
 CHECK((stage='Incorporated')=(incorporated_at IS NOT NULL))
) STRICT;
CREATE TABLE session_turns (
 session_id TEXT PRIMARY KEY REFERENCES sessions, turn_id TEXT NOT NULL UNIQUE,
 assignment_id TEXT NOT NULL REFERENCES assignments, execution_id TEXT NOT NULL REFERENCES executions,
 owner_epoch INTEGER NOT NULL, boot_id TEXT NOT NULL REFERENCES app_boots,
 started_at INTEGER NOT NULL
) STRICT;
