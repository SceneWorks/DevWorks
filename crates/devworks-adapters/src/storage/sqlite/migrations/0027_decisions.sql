-- Source proposal tables retain their business state. New plan proposals retain exact content;
-- legacy missing snapshots stay NULL rather than reconstructing a historical plan.
ALTER TABLE plan_proposals ADD COLUMN reviewed_snapshot_json TEXT CHECK(reviewed_snapshot_json IS NULL OR (json_valid(reviewed_snapshot_json) AND json_type(reviewed_snapshot_json)='object'));
CREATE TABLE decision_approvals (
 id TEXT PRIMARY KEY,
 scope_key TEXT NOT NULL,
 project_id TEXT REFERENCES projects,
 revision INTEGER NOT NULL CHECK(revision>0),
 state TEXT NOT NULL CHECK(state IN ('Pending','Approved','Rejected','Superseded','Expired')),
 operation_kind TEXT NOT NULL,
 target_id TEXT NOT NULL,
 target_revision INTEGER NOT NULL CHECK(target_revision>0),
 content_digest TEXT NOT NULL CHECK(length(content_digest)=64),
 scope_digest TEXT NOT NULL CHECK(length(scope_digest)=64),
 summary TEXT NOT NULL,
 artifact_json TEXT CHECK(artifact_json IS NULL OR json_valid(artifact_json)),
 payload_json TEXT NOT NULL CHECK(json_valid(payload_json) AND json_type(payload_json)='object'),
 author_id TEXT NOT NULL REFERENCES actors,
 author_kind TEXT NOT NULL,
 created_at INTEGER NOT NULL CHECK(created_at>=0),
 decided_by TEXT REFERENCES actors,
 decided_at INTEGER CHECK(decided_at IS NULL OR decided_at>=0),
 rationale TEXT,
 restore_epoch INTEGER NOT NULL CHECK(restore_epoch>=0),
 CHECK(scope_key=CASE WHEN project_id IS NULL THEN 'workspace' ELSE 'project:'||project_id END),
 CHECK((decided_by IS NULL)=(decided_at IS NULL))
) STRICT;
CREATE INDEX decision_approvals_scope ON decision_approvals(scope_key,state,created_at DESC,id DESC);
CREATE INDEX configuration_proposal_inbox ON configuration_proposals(namespace,created_at DESC,id DESC);
CREATE INDEX plan_proposal_inbox ON plan_proposals(requested_at DESC,id DESC);
CREATE TABLE human_decisions (
 source TEXT NOT NULL CHECK(source IN ('Settings','FeaturePlan','Generic')),
 proposal_id TEXT NOT NULL,
 project_id TEXT REFERENCES projects,
 reviewed_revision INTEGER NOT NULL CHECK(reviewed_revision>0),
 decision TEXT NOT NULL CHECK(decision IN ('Approved','Rejected')),
 rationale TEXT,
 actor_id TEXT NOT NULL REFERENCES actors,
 decided_at INTEGER NOT NULL CHECK(decided_at>=0),
 content_digest TEXT NOT NULL CHECK(length(content_digest)=64),
 scope_digest TEXT NOT NULL CHECK(length(scope_digest)=64),
 reviewed_snapshot_json TEXT NOT NULL CHECK(json_valid(reviewed_snapshot_json) AND json_type(reviewed_snapshot_json)='object'),
 PRIMARY KEY(source,proposal_id)
) STRICT;
CREATE INDEX human_decisions_project ON human_decisions(project_id,proposal_id);
CREATE TRIGGER human_decisions_immutable_update BEFORE UPDATE ON human_decisions
 BEGIN SELECT RAISE(ABORT,'human decisions are immutable'); END;
CREATE TRIGGER human_decisions_immutable_delete BEFORE DELETE ON human_decisions
 BEGIN SELECT RAISE(ABORT,'human decisions are immutable'); END;
CREATE TABLE approval_resumes (
 approval_id TEXT PRIMARY KEY REFERENCES decision_approvals,
 revision INTEGER NOT NULL CHECK(revision>0),
 state TEXT NOT NULL CHECK(state IN ('Pending','Admitted','Refused','Unknown')),
 principal_id TEXT NOT NULL REFERENCES actors,
 principal_kind TEXT NOT NULL,
 invocation_json TEXT NOT NULL CHECK(json_valid(invocation_json) AND json_type(invocation_json)='object'),
 operation TEXT NOT NULL,
 schema_revision INTEGER NOT NULL CHECK(schema_revision>0),
 input_json TEXT NOT NULL CHECK(json_valid(input_json) AND json_type(input_json)='object'),
 request_key TEXT NOT NULL UNIQUE,
 request_digest TEXT NOT NULL CHECK(length(request_digest)=64),
 grant_id TEXT REFERENCES authority_grants,
 created_at INTEGER NOT NULL CHECK(created_at>=0),
 updated_at INTEGER NOT NULL CHECK(updated_at>=created_at),
 result_json TEXT CHECK(result_json IS NULL OR json_valid(result_json))
) STRICT;
CREATE INDEX approval_resumes_pending ON approval_resumes(approval_id) WHERE state IN ('Pending','Unknown');
CREATE TABLE notifications (
 id TEXT PRIMARY KEY,
 revision INTEGER NOT NULL CHECK(revision>0),
 user_id TEXT NOT NULL REFERENCES actors,
 project_id TEXT REFERENCES projects,
 root_id TEXT REFERENCES work_items,
 decision_id TEXT,
 category TEXT NOT NULL,
 priority INTEGER NOT NULL,
 title TEXT NOT NULL,
 body TEXT NOT NULL,
 source_id TEXT NOT NULL,
 links_json TEXT NOT NULL CHECK(json_valid(links_json) AND json_type(links_json)='array'),
 requires_input INTEGER NOT NULL CHECK(requires_input IN (0,1)),
 is_read INTEGER NOT NULL CHECK(is_read IN (0,1)),
 acknowledged INTEGER NOT NULL CHECK(acknowledged IN (0,1)),
 created_at INTEGER NOT NULL CHECK(created_at>=0),
 updated_at INTEGER NOT NULL CHECK(updated_at>=created_at),
 UNIQUE(user_id,source_id),
 UNIQUE(id,user_id),
 FOREIGN KEY(root_id,project_id) REFERENCES work_items(id,project_id)
) STRICT;
CREATE INDEX notifications_inbox ON notifications(user_id,created_at DESC,id DESC);
CREATE INDEX notifications_project ON notifications(user_id,project_id,created_at DESC,id DESC);
CREATE INDEX notifications_completion ON notifications(user_id,acknowledged,created_at DESC,id DESC) WHERE category='Completion';

-- The local host binds its actual UI Human identity; registered historical humans do not
-- automatically become notification recipients.
CREATE TABLE notification_audience (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 user_id TEXT NOT NULL REFERENCES actors
) STRICT;

-- One stable OS delivery identity per actual recipient/card; reads, acknowledgments and OS
-- dismissals do not change it. Submitted records mean the OS accepted a submission, not seen.
CREATE TABLE os_notification_deliveries (
 user_id TEXT NOT NULL REFERENCES actors,
 notification_id TEXT NOT NULL,
 revision INTEGER NOT NULL CHECK(revision>0),
 boot_id TEXT NOT NULL REFERENCES app_boots,
 attempt INTEGER NOT NULL CHECK(attempt>0),
 state TEXT NOT NULL CHECK(state IN ('Claimed','Submitted','Denied','Suppressed','Unavailable','Unknown')),
 detail TEXT CHECK(detail IS NULL OR length(detail)<=4096),
 created_at INTEGER NOT NULL CHECK(created_at>=0),
 updated_at INTEGER NOT NULL CHECK(updated_at>=created_at),
 PRIMARY KEY(user_id,notification_id),
 FOREIGN KEY(notification_id,user_id) REFERENCES notifications(id,user_id)
) STRICT;
CREATE INDEX os_notification_inflight ON os_notification_deliveries(boot_id,user_id,notification_id) WHERE state='Claimed';
