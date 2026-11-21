ALTER TABLE allocation_claims ADD COLUMN automatic INTEGER NOT NULL DEFAULT 0
 CHECK(automatic IN (0,1) AND (automatic=0 OR queue_key='project'));
DROP INDEX allocation_claims_project_slot;
CREATE UNIQUE INDEX allocation_claims_project_slot ON allocation_claims(project_id,automatic)
 WHERE queue_key='project' AND state IN ('Transferred','Yielding');
