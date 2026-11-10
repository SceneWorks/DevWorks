-- A delegated assignment retains the role and exact instruction context chosen at creation.
-- It is separate from the assignment's mutable checkpoint and survives supervisor restart.
CREATE TABLE agent_assignment_context (
 assignment_id TEXT PRIMARY KEY REFERENCES assignments(id),
 project_id TEXT NOT NULL REFERENCES projects(id),
 role_id TEXT NOT NULL CHECK(length(role_id) BETWEEN 1 AND 120),
 role_instructions TEXT NOT NULL,
 role_revisions_json TEXT NOT NULL CHECK(json_valid(role_revisions_json)),
 -- A coordinator brief combines several independently bounded work fields and an approved plan.
 brief TEXT NOT NULL CHECK(length(brief) >= 1),
 task_ids_json TEXT NOT NULL CHECK(json_valid(task_ids_json)),
 source_checkpoint_id TEXT,
 created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX agent_assignment_context_project ON agent_assignment_context(project_id,assignment_id);
