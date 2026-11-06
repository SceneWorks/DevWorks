-- Selection ownership is durable and independent from coordinator/execution ownership.
CREATE TABLE allocation_claims (
 id TEXT PRIMARY KEY,
 project_id TEXT NOT NULL REFERENCES projects,
 root_id TEXT NOT NULL,
 work_id TEXT NOT NULL,
 kind TEXT NOT NULL CHECK(kind IN ('feature','task','issue')),
 queue_key TEXT NOT NULL CHECK(length(queue_key) BETWEEN 1 AND 160),
 owner_id TEXT NOT NULL REFERENCES actors,
 session_id TEXT REFERENCES sessions,
 ingress_namespace TEXT NOT NULL,
 invocation_id TEXT NOT NULL,
 fence INTEGER NOT NULL CHECK(fence>0),
 expires_at INTEGER NOT NULL CHECK(expires_at>=0),
 state TEXT NOT NULL CHECK(state IN ('Active','Transferred','Yielding','Released','Expired','Yielded','Invalidated')),
 revision INTEGER NOT NULL CHECK(revision>0),
 work_revision INTEGER NOT NULL CHECK(work_revision>0),
 parent_feature_id TEXT,
 root_control_revision INTEGER NOT NULL CHECK(root_control_revision>0),
 queue_revision INTEGER NOT NULL CHECK(queue_revision>0),
 feature_task_set_revision INTEGER CHECK(feature_task_set_revision IS NULL OR feature_task_set_revision>0),
 created_at INTEGER NOT NULL CHECK(created_at>=0),
 execution_id TEXT REFERENCES executions,
 FOREIGN KEY(work_id,project_id) REFERENCES work_items(id,project_id),
 FOREIGN KEY(root_id,project_id) REFERENCES work_items(id,project_id),
 FOREIGN KEY(parent_feature_id,project_id) REFERENCES work_items(id,project_id),
 UNIQUE(ingress_namespace,invocation_id)
) STRICT;
CREATE INDEX allocation_claims_project ON allocation_claims(project_id,id);
CREATE INDEX allocation_claims_work ON allocation_claims(work_id,fence);
CREATE UNIQUE INDEX allocation_claims_live_work ON allocation_claims(work_id)
 WHERE state IN ('Active','Transferred','Yielding');
CREATE UNIQUE INDEX allocation_claims_project_slot ON allocation_claims(project_id)
 WHERE queue_key='project' AND state IN ('Transferred','Yielding');
-- An unstarted selection made before a link/unlink/transfer cannot start under its old scope.
-- Transferred executions retain ownership and lineage; their service reconciles supervision.
CREATE TRIGGER allocation_claims_membership AFTER UPDATE OF parent_feature_id ON work_items
 WHEN OLD.parent_feature_id IS NOT NEW.parent_feature_id
 BEGIN
  UPDATE allocation_claims SET state='Invalidated',revision=revision+1
   WHERE work_id=NEW.id AND state='Active';
 END;

-- Explicit allocation keys persist complete responses even when no item was eligible.
CREATE TABLE allocation_replays (
 key TEXT PRIMARY KEY CHECK(length(key)=64 AND key NOT GLOB '*[^0-9a-f]*'),
 intent_digest TEXT NOT NULL CHECK(length(intent_digest)=64 AND intent_digest NOT GLOB '*[^0-9a-f]*'),
 result_json TEXT NOT NULL CHECK(json_valid(result_json) AND json_type(result_json)='object' AND length(CAST(result_json AS BLOB))<=1048576),
 claim_id TEXT REFERENCES allocation_claims
) STRICT;
CREATE TRIGGER allocation_replays_immutable_update BEFORE UPDATE ON allocation_replays
 BEGIN SELECT RAISE(ABORT,'allocation replays are immutable'); END;
CREATE TRIGGER allocation_replays_immutable_delete BEFORE DELETE ON allocation_replays
 BEGIN SELECT RAISE(ABORT,'allocation replays are immutable'); END;
