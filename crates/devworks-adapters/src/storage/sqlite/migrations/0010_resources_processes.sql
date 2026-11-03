-- DevWorks store migration 0010: resource reservations and owned processes (RES-01..03).
-- Additive. resources (migration 1, never written by an earlier build) gains its name, scope,
-- owning project, physical flag, labels and the fence/queue counters; the canonical key stays in
-- canonical_identity ('machine:<name>' or 'project:<project>:<name>'). resource_leases holds
-- every queued, granted, uncertain and ended lease with its fence, owner lineage, priority and
-- lifecycle times: one open lease per holder and resource, one grant per fence. owned_processes
-- records each launched process tree with its native identity (pid plus birth token, process
-- group or job), its boot and attribution, so recovery can reconcile trees of earlier boots.
-- resource_grants (migration 1) stays unused. Never edit this file after release: add a new
-- migration.
ALTER TABLE resources ADD COLUMN name TEXT NOT NULL DEFAULT '';
ALTER TABLE resources ADD COLUMN scope TEXT NOT NULL DEFAULT 'machine' CHECK(scope IN ('machine','project'));
ALTER TABLE resources ADD COLUMN project_id TEXT REFERENCES projects;
ALTER TABLE resources ADD COLUMN physical INTEGER NOT NULL DEFAULT 0 CHECK(physical IN (0,1));
ALTER TABLE resources ADD COLUMN labels_json TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(labels_json) AND json_type(labels_json)='array');
ALTER TABLE resources ADD COLUMN fence_seq INTEGER NOT NULL DEFAULT 0 CHECK(fence_seq>=0);
ALTER TABLE resources ADD COLUMN queue_seq INTEGER NOT NULL DEFAULT 0 CHECK(queue_seq>=0);
ALTER TABLE resources ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE resources ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0;
CREATE INDEX resource_project ON resources(project_id,canonical_identity);
CREATE TABLE resource_leases (
 id TEXT PRIMARY KEY, resource_id TEXT NOT NULL REFERENCES resources,
 state TEXT NOT NULL CHECK(state IN ('Waiting','Held','Releasing','Uncertain','Released')),
 end_reason TEXT CHECK(end_reason IS NULL OR end_reason IN ('Released','Expired','Cancelled','Reconciled')),
 owner_id TEXT NOT NULL REFERENCES actors, holder_key TEXT NOT NULL CHECK(length(holder_key)>0),
 session_id TEXT, project_id TEXT REFERENCES projects, root_id TEXT, assignment_id TEXT,
 boot_id TEXT NOT NULL REFERENCES app_boots,
 purpose TEXT CHECK(purpose IS NULL OR length(purpose) BETWEEN 1 AND 500),
 priority INTEGER NOT NULL CHECK(priority BETWEEN 0 AND 3),
 units INTEGER NOT NULL CHECK(units BETWEEN 1 AND 500), lease_ms INTEGER NOT NULL CHECK(lease_ms>0),
 queue_seq INTEGER NOT NULL CHECK(queue_seq>0), fence INTEGER NOT NULL CHECK(fence>=0),
 requested_at INTEGER NOT NULL, granted_at INTEGER, renewed_at INTEGER,
 expires_at INTEGER NOT NULL, ended_at INTEGER, unreachable_since INTEGER,
 revision INTEGER NOT NULL CHECK(revision>0),
 CHECK((state='Released')=(end_reason IS NOT NULL)),
 CHECK((state='Released')=(ended_at IS NOT NULL)),
 CHECK(state IN ('Waiting','Released') OR (fence>0 AND granted_at IS NOT NULL)),
 CHECK(state<>'Waiting' OR (fence=0 AND granted_at IS NULL))
) STRICT;
CREATE UNIQUE INDEX resource_lease_holder ON resource_leases(resource_id,holder_key) WHERE state<>'Released';
CREATE UNIQUE INDEX resource_lease_fence ON resource_leases(resource_id,fence) WHERE fence>0;
CREATE INDEX resource_lease_queue ON resource_leases(resource_id,priority DESC,queue_seq) WHERE state='Waiting';
CREATE INDEX resource_lease_occupancy ON resource_leases(resource_id,fence) WHERE state IN ('Held','Releasing','Uncertain');
CREATE INDEX resource_lease_open ON resource_leases(id) WHERE state<>'Released';
CREATE INDEX resource_lease_session ON resource_leases(session_id,id) WHERE state<>'Released';
CREATE INDEX resource_lease_root ON resource_leases(root_id,id) WHERE state<>'Released';
CREATE TABLE owned_processes (
 id TEXT PRIMARY KEY, boot_id TEXT NOT NULL REFERENCES app_boots,
 state TEXT NOT NULL CHECK(state IN ('Launching','Running','Stopping','Reaped','Uncertain','Failed','Lost')),
 pid INTEGER CHECK(pid IS NULL OR pid>0), birth TEXT, process_group INTEGER,
 platform TEXT NOT NULL CHECK(platform IN ('unix','windows')),
 program TEXT NOT NULL CHECK(length(program) BETWEEN 1 AND 256),
 argv_sha256 TEXT NOT NULL CHECK(length(argv_sha256)=64),
 operation_id TEXT REFERENCES operations, lease_id TEXT REFERENCES resource_leases,
 actor_id TEXT NOT NULL REFERENCES actors,
 session_id TEXT, project_id TEXT REFERENCES projects, root_id TEXT, assignment_id TEXT,
 exit_code INTEGER, detail TEXT CHECK(detail IS NULL OR length(detail)<=1024),
 created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
 revision INTEGER NOT NULL CHECK(revision>0),
 CHECK(state NOT IN ('Running','Stopping') OR (pid IS NOT NULL AND birth IS NOT NULL))
) STRICT;
CREATE INDEX owned_process_lease ON owned_processes(lease_id,id) WHERE lease_id IS NOT NULL;
CREATE INDEX owned_process_unsettled ON owned_processes(id) WHERE state NOT IN ('Reaped','Failed');
