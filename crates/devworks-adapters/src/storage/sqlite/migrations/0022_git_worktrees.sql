-- Owned task/integration worktree identity. Historical ownership survives local cleanup.
CREATE TABLE git_owned_worktrees (
 id TEXT PRIMARY KEY,
 repository_id TEXT NOT NULL REFERENCES repositories,
 work_id TEXT NOT NULL REFERENCES work_items,
 owner_id TEXT NOT NULL REFERENCES actors,
 root_id TEXT NOT NULL,
 path TEXT NOT NULL UNIQUE,
 branch TEXT NOT NULL,
 payload TEXT NOT NULL CHECK(json_valid(payload)),
 revision INTEGER NOT NULL CHECK(revision > 0),
 CHECK(json_extract(payload,'$.id') IS id),
 CHECK(json_extract(payload,'$.repository_id') IS repository_id),
 CHECK(json_extract(payload,'$.work_id') IS work_id),
 CHECK(json_extract(payload,'$.owner') IS owner_id),
 CHECK(json_extract(payload,'$.registered_root_id') IS root_id),
 CHECK(json_extract(payload,'$.path') IS path),
 CHECK(json_extract(payload,'$.branch') IS branch),
 CHECK(json_extract(payload,'$.revision') IS revision)
) STRICT;
CREATE INDEX git_owned_worktrees_work ON git_owned_worktrees(work_id,id);
CREATE UNIQUE INDEX git_owned_worktrees_active_branch ON git_owned_worktrees(repository_id,branch) WHERE json_extract(payload,'$.removed')=0;
CREATE INDEX git_owned_worktrees_pending ON git_owned_worktrees(id) WHERE json_extract(payload,'$.pending_operation_id') IS NOT NULL;
