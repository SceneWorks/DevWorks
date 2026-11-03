-- DevWorks store migration 0008: receipt reply artifacts and workspace-scoped artifacts.
-- Artifacts gain an owner scope: a project (as before) or the workspace, which has an owning
-- principal and no project. They also gain a retention class: 'Explicit' artifacts are kept
-- until explicitly deleted (ART-05); 'Receipt' artifacts are the durable typed replies of
-- command receipts, retained with their receipt and never user-deletable. The project columns
-- become nullable, which SQLite can only do by rebuilding the table; the rebuild copies every
-- row unchanged (scope 'Project', retention 'Explicit') and keeps every reference to it. It runs
-- with foreign-key enforcement off for its one transaction (SQLite's documented table-rebuild
-- procedure); the full foreign-key check must pass before it commits.
-- receipt_replies records the published reply of each receipt (one per receipt and command).
-- Every receipt committed by an earlier build gets its publication intent here, so it too is
-- published. Never edit this file after release: add a new migration.
CREATE TABLE artifacts_rebuilt (
 id TEXT PRIMARY KEY, origin_project_id TEXT REFERENCES projects,
 origin_work_id TEXT REFERENCES work_items, title TEXT NOT NULL, revision INTEGER NOT NULL,
 current_project_id TEXT REFERENCES projects, scope_revision INTEGER NOT NULL,
 reference_revision INTEGER NOT NULL DEFAULT 1 CHECK(reference_revision>0),
 current_version_id TEXT REFERENCES artifact_versions,
 state TEXT NOT NULL DEFAULT 'Live' CHECK(state IN ('Live','Deleted')),
 created_at INTEGER NOT NULL DEFAULT 0,
 provenance_json TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(provenance_json)),
 scope_kind TEXT NOT NULL DEFAULT 'Project' CHECK(scope_kind IN ('Project','Workspace')),
 owner_actor_id TEXT REFERENCES actors,
 retention TEXT NOT NULL DEFAULT 'Explicit' CHECK(retention IN ('Explicit','Receipt')),
 CHECK((scope_kind='Project')=(current_project_id IS NOT NULL)),
 CHECK(scope_kind='Project' OR owner_actor_id IS NOT NULL),
 CHECK(retention='Explicit' OR owner_actor_id IS NOT NULL),
 CHECK(origin_work_id IS NULL OR origin_project_id IS NOT NULL)
) STRICT;
INSERT INTO artifacts_rebuilt(id,origin_project_id,origin_work_id,title,revision,current_project_id,
 scope_revision,reference_revision,current_version_id,state,created_at,provenance_json)
 SELECT id,origin_project_id,origin_work_id,title,revision,current_project_id,scope_revision,
 reference_revision,current_version_id,state,created_at,provenance_json FROM artifacts;
DROP TABLE artifacts;
ALTER TABLE artifacts_rebuilt RENAME TO artifacts;
CREATE INDEX artifact_project_list ON artifacts(current_project_id,state,created_at DESC,id DESC);
CREATE INDEX artifact_workspace_list ON artifacts(scope_kind,state,created_at DESC,id DESC)
 WHERE scope_kind='Workspace';
CREATE TABLE receipt_replies (
 actor_id TEXT NOT NULL, scope_key TEXT NOT NULL, idempotency_key TEXT NOT NULL,
 command_id TEXT NOT NULL UNIQUE,
 artifact_id TEXT NOT NULL UNIQUE REFERENCES artifacts,
 version_id TEXT NOT NULL REFERENCES artifact_versions,
 schema_json TEXT NOT NULL CHECK(json_valid(schema_json)), published_at INTEGER NOT NULL,
 PRIMARY KEY(actor_id,scope_key,idempotency_key),
 FOREIGN KEY(actor_id,scope_key,idempotency_key) REFERENCES command_receipts
) STRICT;
INSERT INTO queue_entries(id,consumer,event_id,priority,ready_at,state,payload_json,event_seq)
 SELECT 'receipt-reply-' || lower(hex(randomblob(16))), 'receipt_replies', e.id, 0, r.created_at,
  'Ready', json_object('actorId',r.actor_id,'scopeKey',r.scope_key,'requestKey',r.idempotency_key,
  'commandId',e.aggregate_id), e.seq
 FROM command_receipts r JOIN events e ON e.aggregate_kind='command' AND e.kind='CommandDecided'
  AND e.aggregate_id=json_extract(r.result_json,'$.receipt.commandId')
 WHERE NOT EXISTS(SELECT 1 FROM queue_entries q WHERE q.consumer='receipt_replies' AND q.event_id=e.id);
