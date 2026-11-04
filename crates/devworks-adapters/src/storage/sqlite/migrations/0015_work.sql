-- Planned work: features, task cards and standalone issues (DW-012). Additive: five new tables
-- and five indexes. The migration 1 `work_items`, `work_repositories`, `work_dependencies`,
-- `workflow_statuses` and `feature_plans` tables keep their exact shape; what they lack lives in
-- side tables keyed by the same identity, and a work row without its detail row reads with the
-- documented defaults (no search key, caller reference, completion or attribution, and its
-- creation as its last update).
-- Work items are never deleted as rows: a re-plan closing a card changes its workflow status.
CREATE TABLE work_details (
 work_id TEXT PRIMARY KEY REFERENCES work_items,
 project_id TEXT NOT NULL REFERENCES projects,
 -- Title and description folded by the service's one Unicode case folding (search).
 search_key TEXT NOT NULL CHECK(length(search_key) <= 200000),
 -- The caller's plan reference: a re-plan matches by it.
 caller_ref TEXT CHECK(caller_ref IS NULL OR length(caller_ref) BETWEEN 1 AND 128),
 -- How a card in a Done status completed: a human transition or verified acceptance evidence.
 completion TEXT CHECK(completion IS NULL OR completion IN ('Manual','Verified')),
 created_by TEXT REFERENCES actors,
 updated_by TEXT REFERENCES actors,
 updated_at INTEGER NOT NULL CHECK(updated_at >= 0),
 FOREIGN KEY(work_id, project_id) REFERENCES work_items(id, project_id)
) STRICT;
CREATE UNIQUE INDEX work_caller_refs ON work_details(project_id, caller_ref)
 WHERE caller_ref IS NOT NULL;
-- Queue, ordering and workflow revisions of a project: 'project' (features and standalone
-- issues), 'feature:<id>' (one feature's cards), '*' (any ordering change) and 'workflow'.
CREATE TABLE work_queues (
 project_id TEXT NOT NULL REFERENCES projects,
 queue_key TEXT NOT NULL CHECK(length(queue_key) BETWEEN 1 AND 160),
 revision INTEGER NOT NULL CHECK(revision > 0),
 PRIMARY KEY(project_id, queue_key)
) STRICT;
-- Requests for human approval of one exact pending plan revision (the decision is recorded by
-- the approvals item through the planning service's decision seam).
CREATE TABLE plan_proposals (
 id TEXT PRIMARY KEY,
 feature_id TEXT NOT NULL REFERENCES work_items,
 revision INTEGER NOT NULL CHECK(revision > 0),
 plan_revision INTEGER NOT NULL CHECK(plan_revision > 0),
 plan_digest TEXT NOT NULL CHECK(length(plan_digest) = 64),
 state TEXT NOT NULL CHECK(state IN ('Pending','Approved','Rejected','Superseded','Expired')),
 requested_by TEXT NOT NULL REFERENCES actors,
 requested_at INTEGER NOT NULL CHECK(requested_at >= 0),
 decided_by TEXT REFERENCES actors,
 decided_at INTEGER CHECK(decided_at IS NULL OR decided_at >= 0),
 summary TEXT NOT NULL CHECK(length(summary) <= 65536),
 CHECK((decided_by IS NULL) = (decided_at IS NULL))
) STRICT;
CREATE INDEX plan_proposals_by_feature ON plan_proposals(feature_id, requested_at, id);
-- A feature's execution-readiness assessment, bound to the plan revision and digest it reviewed.
CREATE TABLE feature_readiness (
 feature_id TEXT PRIMARY KEY REFERENCES work_items,
 revision INTEGER NOT NULL CHECK(revision > 0),
 body_json TEXT NOT NULL CHECK(json_valid(body_json) AND json_type(body_json) = 'object')
) STRICT;
-- The linked cards a pending feature stop came to cover after its intent was journaled (a card
-- linked while the stop was pending, or reached again by a reapply at a newer generation). Each
-- is fenced like the cards the intent names: the card's activation generation when it was
-- covered, and whether a reactivation was already pending then. A row only moves forward.
CREATE TABLE stop_coverage (
 operation_id TEXT NOT NULL REFERENCES operations,
 root_id TEXT NOT NULL REFERENCES work_items,
 generation INTEGER NOT NULL CHECK(generation >= 0),
 reactivate INTEGER NOT NULL CHECK(reactivate IN (0,1)),
 PRIMARY KEY(operation_id, root_id)
) STRICT;
CREATE INDEX work_repositories_by_repository ON work_repositories(repository_id, work_id);
CREATE INDEX workflow_statuses_by_project ON workflow_statuses(project_id, sort_order, id);
CREATE INDEX work_items_by_kind ON work_items(project_id, kind, parent_feature_id);
