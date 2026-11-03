-- Host authority. Agents, packages, MCP clients and schedules act only through an active
-- principal binding the user or host created; revoking it advances its revision. Grants are
-- minted by the user through the trusted local input path, exercised only by their holder and
-- spent by compare-and-set on their revision. Neither table holds secrets.
CREATE TABLE principal_authority (
 actor_id TEXT PRIMARY KEY REFERENCES actors,
 role TEXT, project_id TEXT REFERENCES projects,
 tool_families_json TEXT CHECK(tool_families_json IS NULL OR json_valid(tool_families_json)),
 state TEXT NOT NULL CHECK(state IN ('Active','Revoked')),
 revision INTEGER NOT NULL CHECK(revision>0),
 bound_by TEXT NOT NULL REFERENCES actors, updated_at INTEGER NOT NULL
) STRICT;
CREATE TABLE authority_grants (
 id TEXT PRIMARY KEY, actor_id TEXT NOT NULL REFERENCES actors,
 kind TEXT NOT NULL CHECK(kind IN ('Operation','NativeExecution','PrimaryRepositoryDeletion')),
 issued_by TEXT NOT NULL REFERENCES actors,
 revision INTEGER NOT NULL CHECK(revision>0),
 state TEXT NOT NULL CHECK(state IN ('Active','Consumed','Revoked')),
 binding_json TEXT NOT NULL CHECK(json_valid(binding_json)),
 expires_at INTEGER, uses_remaining INTEGER CHECK(uses_remaining IS NULL OR uses_remaining>=0),
 created_at INTEGER NOT NULL, revoked_by TEXT REFERENCES actors,
 CHECK((state='Revoked')=(revoked_by IS NOT NULL))
) STRICT;
CREATE INDEX authority_grant_holder ON authority_grants(actor_id,state,id);
