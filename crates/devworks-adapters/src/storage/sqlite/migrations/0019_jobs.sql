-- DevWorks store migration 0019: durable schedules and manual jobs (JOB-01..09).
-- Additive. schedules and job_runs (migration 1, never written by an earlier build) gain the
-- job's title, scope, schedule, kind, authorization, hold, restore and project-archive
-- activation epochs, interval anchor, interrupted run and lifecycle times, and the run's sequence, definition snapshot,
-- project archive and store restore epochs it was authorized under, occurrence instant, trigger, operations, boot, times, reason, preserved hold, cancellation,
-- resumptions, agent principal and session, skipped-for link and project. A partial unique
-- index allows one live title per scope and one active run (Queued, Running, Holding) per job.
-- job_run_links, job_checkpoints, job_actions and job_run_usage hold the work a run started,
-- discovery cursors, action identities and per-run usage. Never edit this file after release:
-- add a new migration.
ALTER TABLE schedules ADD COLUMN title TEXT NOT NULL DEFAULT '' CHECK(length(title) <= 256);
ALTER TABLE schedules ADD COLUMN kind TEXT NOT NULL DEFAULT 'Native' CHECK(kind IN ('Native','Inference'));
ALTER TABLE schedules ADD COLUMN schedule_json TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(schedule_json));
ALTER TABLE schedules ADD COLUMN authorized INTEGER NOT NULL DEFAULT 0 CHECK(authorized IN (0,1));
ALTER TABLE schedules ADD COLUMN authorized_by TEXT REFERENCES actors;
ALTER TABLE schedules ADD COLUMN hold TEXT CHECK(hold IS NULL OR hold IN ('ReactivationRequired','AuthorizationRequired','ProjectClosed'));
ALTER TABLE schedules ADD COLUMN activated_epoch INTEGER NOT NULL DEFAULT 0 CHECK(activated_epoch >= 0);
ALTER TABLE schedules ADD COLUMN anchor_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE schedules ADD COLUMN project_epoch INTEGER NOT NULL DEFAULT 0 CHECK(project_epoch >= 0);
ALTER TABLE schedules ADD COLUMN interrupted_run_id TEXT;
ALTER TABLE schedules ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE schedules ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE schedules ADD COLUMN deleted_at INTEGER;
CREATE UNIQUE INDEX schedule_title ON schedules(ifnull(project_id,''), title COLLATE NOCASE) WHERE deleted_at IS NULL;
CREATE INDEX schedule_scope ON schedules(project_id, title COLLATE NOCASE, id) WHERE deleted_at IS NULL;
ALTER TABLE job_runs ADD COLUMN seq INTEGER NOT NULL DEFAULT 0 CHECK(seq >= 0);
ALTER TABLE job_runs ADD COLUMN project_id TEXT REFERENCES projects;
ALTER TABLE job_runs ADD COLUMN job_revision INTEGER NOT NULL DEFAULT 0;
ALTER TABLE job_runs ADD COLUMN project_epoch INTEGER NOT NULL DEFAULT 0 CHECK(project_epoch >= 0);
ALTER TABLE job_runs ADD COLUMN restore_epoch INTEGER NOT NULL DEFAULT 0 CHECK(restore_epoch >= 0);
ALTER TABLE job_runs ADD COLUMN definition_json TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(definition_json));
ALTER TABLE job_runs ADD COLUMN occurrence_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE job_runs ADD COLUMN trigger TEXT NOT NULL DEFAULT 'Scheduled' CHECK(trigger IN ('Scheduled','Manual'));
ALTER TABLE job_runs ADD COLUMN operation_id TEXT REFERENCES operations;
ALTER TABLE job_runs ADD COLUMN inner_operation_id TEXT REFERENCES operations;
ALTER TABLE job_runs ADD COLUMN boot_id TEXT REFERENCES app_boots;
ALTER TABLE job_runs ADD COLUMN started_at INTEGER;
ALTER TABLE job_runs ADD COLUMN finished_at INTEGER;
ALTER TABLE job_runs ADD COLUMN reason TEXT CHECK(reason IS NULL OR length(reason) BETWEEN 1 AND 4096);
ALTER TABLE job_runs ADD COLUMN hold TEXT CHECK(hold IS NULL OR length(hold) BETWEEN 1 AND 256);
ALTER TABLE job_runs ADD COLUMN cancel_requested INTEGER NOT NULL DEFAULT 0 CHECK(cancel_requested IN (0,1));
ALTER TABLE job_runs ADD COLUMN resumptions INTEGER NOT NULL DEFAULT 0 CHECK(resumptions >= 0);
ALTER TABLE job_runs ADD COLUMN session_id TEXT;
ALTER TABLE job_runs ADD COLUMN agent_actor TEXT REFERENCES actors;
ALTER TABLE job_runs ADD COLUMN skipped_for TEXT;
ALTER TABLE job_runs ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
CREATE UNIQUE INDEX job_run_seq ON job_runs(schedule_id, seq);
CREATE UNIQUE INDEX job_run_active ON job_runs(schedule_id) WHERE state IN ('Queued','Running','Holding');
CREATE INDEX job_run_open ON job_runs(state, id) WHERE state IN ('Queued','Running','Holding');
CREATE TABLE job_run_links (
 run_id TEXT NOT NULL REFERENCES job_runs, root_id TEXT NOT NULL CHECK(length(root_id) > 0),
 PRIMARY KEY(run_id, root_id)
) STRICT;
CREATE TABLE job_checkpoints (
 schedule_id TEXT NOT NULL REFERENCES schedules, source TEXT NOT NULL CHECK(length(source) BETWEEN 1 AND 256),
 cursor_json TEXT NOT NULL CHECK(json_valid(cursor_json)), revision INTEGER NOT NULL CHECK(revision > 0),
 run_id TEXT NOT NULL REFERENCES job_runs, updated_at INTEGER NOT NULL,
 PRIMARY KEY(schedule_id, source)
) STRICT;
CREATE TABLE job_actions (
 schedule_id TEXT NOT NULL REFERENCES schedules, action_key TEXT NOT NULL CHECK(length(action_key) BETWEEN 1 AND 512),
 run_id TEXT NOT NULL REFERENCES job_runs,
 state TEXT NOT NULL CHECK(state IN ('Intent','Done','Failed','Unknown')),
 effect_json TEXT CHECK(effect_json IS NULL OR json_valid(effect_json)),
 revision INTEGER NOT NULL CHECK(revision > 0), created_at INTEGER NOT NULL, settled_at INTEGER,
 CHECK((state = 'Intent') = (settled_at IS NULL)),
 PRIMARY KEY(schedule_id, action_key)
) STRICT;
CREATE INDEX job_action_unsettled ON job_actions(run_id, action_key) WHERE state = 'Intent';
CREATE TABLE job_run_usage (
 run_id TEXT NOT NULL REFERENCES job_runs, seq INTEGER NOT NULL CHECK(seq > 0),
 request_id TEXT NOT NULL CHECK(length(request_id) > 0), purpose TEXT NOT NULL CHECK(length(purpose) BETWEEN 1 AND 256),
 model_id TEXT NOT NULL CHECK(length(model_id) > 0),
 input_tokens INTEGER NOT NULL CHECK(input_tokens >= 0), output_tokens INTEGER NOT NULL CHECK(output_tokens >= 0),
 cache_read_tokens INTEGER NOT NULL CHECK(cache_read_tokens >= 0), cache_write_tokens INTEGER NOT NULL CHECK(cache_write_tokens >= 0),
 amount_usd TEXT, price_kind TEXT NOT NULL CHECK(price_kind IN ('Reported','Estimated','Free','Untracked')),
 price_origin TEXT NOT NULL CHECK(length(price_origin) > 0), price_revision INTEGER,
 incomplete INTEGER NOT NULL CHECK(incomplete IN (0,1)), recorded_at INTEGER NOT NULL,
 CHECK((price_kind = 'Untracked') = (amount_usd IS NULL)),
 PRIMARY KEY(run_id, seq), UNIQUE(run_id, request_id)
) STRICT;
