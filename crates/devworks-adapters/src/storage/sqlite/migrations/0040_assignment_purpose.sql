-- Keep the primary work owner unique while permitting scoped children of an issue root.
ALTER TABLE assignments ADD COLUMN purpose TEXT NOT NULL DEFAULT 'Primary'
 CHECK(purpose IN ('Primary','Child'));
DROP INDEX one_live_assignment;
CREATE UNIQUE INDEX one_live_assignment ON assignments(work_id)
 WHERE purpose='Primary' AND state IN ('Ready','Active','Waiting');
CREATE INDEX assignment_child_root ON assignments(root_id,purpose,state,id);
