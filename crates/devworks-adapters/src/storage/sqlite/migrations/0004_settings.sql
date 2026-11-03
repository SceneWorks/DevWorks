-- Staged configuration proposals. Agents may author them; only an authenticated human
-- decision applies one, as a new configuration revision. Proposals never rewrite revisions.
CREATE TABLE configuration_proposals (
 id TEXT PRIMARY KEY, scope_key TEXT NOT NULL, namespace TEXT NOT NULL,
 base_revision INTEGER NOT NULL CHECK(base_revision>0),
 revision INTEGER NOT NULL CHECK(revision>0),
 state TEXT NOT NULL CHECK(state IN ('Pending','Approved','Rejected')),
 author_id TEXT NOT NULL REFERENCES actors, created_at INTEGER NOT NULL,
 decided_by TEXT REFERENCES actors, decided_at INTEGER, applied_revision INTEGER,
 body_json TEXT NOT NULL CHECK(json_valid(body_json)),
 CHECK((state='Pending')=(decided_by IS NULL)),
 CHECK((state='Approved')=(applied_revision IS NOT NULL)),
 FOREIGN KEY(scope_key,namespace,applied_revision)
  REFERENCES configuration_revisions(scope_key,namespace,revision)
) STRICT;
CREATE INDEX configuration_proposal_scope ON configuration_proposals(scope_key,namespace,state,id);
