-- Repository indexing (DW-023). Additive: five new tables, two defaulted columns on migration 1
-- graph tables and seven indexes. The migration 1 generation, unit, node, edge, view, pointer and
-- pin tables keep their shape and remain the graph payload store; nothing earlier wrote them.
--
-- Index source configuration of one section: the workspace ('workspace') or one project.
CREATE TABLE index_source_configs (
 scope_key TEXT PRIMARY KEY CHECK(length(scope_key) BETWEEN 1 AND 128),
 project_id TEXT REFERENCES projects,
 revision INTEGER NOT NULL CHECK(revision > 1),
 config_json TEXT NOT NULL CHECK(json_valid(config_json) AND json_type(config_json)='object'),
 updated_at INTEGER NOT NULL CHECK(updated_at >= 0),
 CHECK((scope_key = 'workspace') = (project_id IS NULL))
) STRICT;
-- The observed source identity of a registration (exact location, remote and HEAD).
CREATE TABLE repository_sources (
 repository_id TEXT PRIMARY KEY REFERENCES repositories,
 section TEXT NOT NULL CHECK(length(section) BETWEEN 1 AND 128),
 -- The base directory it was found under (its group is the path between the two).
 section_root TEXT CHECK(section_root IS NULL OR length(section_root) BETWEEN 1 AND 32768),
 location TEXT NOT NULL CHECK(length(location) BETWEEN 1 AND 32768),
 state TEXT NOT NULL CHECK(state IN ('Present','Missing','NotARepository')),
 head_oid TEXT CHECK(head_oid IS NULL OR length(head_oid) IN (40,64)),
 head_ref TEXT CHECK(head_ref IS NULL OR length(head_ref) BETWEEN 1 AND 1024),
 object_format TEXT NOT NULL CHECK(object_format IN ('Sha1','Sha256')),
 remote_url TEXT CHECK(remote_url IS NULL OR length(remote_url) BETWEEN 1 AND 4096),
 revision INTEGER NOT NULL CHECK(revision > 0),
 observed_at INTEGER NOT NULL CHECK(observed_at >= 0)
) STRICT;
-- The primary checkout and linked worktrees of a registration.
CREATE TABLE repository_worktrees (
 repository_id TEXT NOT NULL REFERENCES repositories,
 worktree_id TEXT NOT NULL CHECK(length(worktree_id) BETWEEN 1 AND 128),
 path TEXT NOT NULL CHECK(length(path) BETWEEN 1 AND 32768),
 head_oid TEXT CHECK(head_oid IS NULL OR length(head_oid) IN (40,64)),
 head_ref TEXT CHECK(head_ref IS NULL OR length(head_ref) BETWEEN 1 AND 1024),
 is_main INTEGER NOT NULL CHECK(is_main IN (0,1)),
 observed_at INTEGER NOT NULL CHECK(observed_at >= 0),
 PRIMARY KEY(repository_id, worktree_id)
) STRICT;
-- Index runs: claim (owner, lease, attempt), cancellation, checkpoint and outcome.
CREATE TABLE index_runs (
 id TEXT PRIMARY KEY,
 -- The request that recorded it: one run per repository, cancelled together.
 batch_id TEXT NOT NULL,
 kind TEXT NOT NULL CHECK(kind IN ('Index','Reindex','Overlay')),
 repository_id TEXT NOT NULL REFERENCES repositories,
 project_id TEXT REFERENCES projects,
 overlay_id TEXT,
 state TEXT NOT NULL CHECK(state IN ('Queued','Running','Succeeded','Failed','Cancelled')),
 stage TEXT NOT NULL CHECK(length(stage) BETWEEN 1 AND 64),
 revision INTEGER NOT NULL CHECK(revision > 0),
 attempt INTEGER NOT NULL CHECK(attempt >= 0),
 max_attempts INTEGER NOT NULL CHECK(max_attempts BETWEEN 1 AND 16),
 owner TEXT, lease_until INTEGER,
 cancel_requested_at INTEGER,
 config_revision INTEGER NOT NULL CHECK(config_revision > 0),
 config_digest TEXT NOT NULL CHECK(length(config_digest) = 64),
 source_oid TEXT CHECK(source_oid IS NULL OR length(source_oid) IN (40,64)),
 generation_id TEXT,
 completed_units INTEGER NOT NULL CHECK(completed_units >= 0),
 total_units INTEGER CHECK(total_units IS NULL OR total_units >= 0),
 output_view_id TEXT,
 error_code TEXT, error_message TEXT CHECK(error_message IS NULL OR length(error_message) <= 4096),
 deterministic INTEGER NOT NULL CHECK(deterministic IN (0,1)),
 warnings_json TEXT NOT NULL CHECK(json_valid(warnings_json) AND json_type(warnings_json)='array'),
 requested_by TEXT NOT NULL,
 created_at INTEGER NOT NULL CHECK(created_at >= 0),
 started_at INTEGER, finished_at INTEGER,
 next_attempt_at INTEGER NOT NULL CHECK(next_attempt_at >= 0),
 CHECK((kind = 'Overlay') = (overlay_id IS NOT NULL)),
 CHECK((owner IS NULL) = (lease_until IS NULL))
) STRICT;
-- One active run per repository baseline and per overlay.
CREATE UNIQUE INDEX index_runs_one_active ON index_runs(repository_id, COALESCE(overlay_id,''))
 WHERE state IN ('Queued','Running');
CREATE INDEX index_runs_by_repository ON index_runs(repository_id, created_at DESC, id);
CREATE INDEX index_runs_by_time ON index_runs(created_at DESC, id);
CREATE INDEX index_runs_by_batch ON index_runs(batch_id, id);
CREATE INDEX index_runs_claimable ON index_runs(state, next_attempt_at, created_at)
 WHERE state IN ('Queued','Running');
-- Worktree and ref overlays over a repository's baseline.
CREATE TABLE index_overlays (
 id TEXT PRIMARY KEY,
 repository_id TEXT NOT NULL REFERENCES repositories,
 -- A worktree overlay names its worktree; a ref overlay (no worktree) is identified by the
 -- commit its ref resolved to (head_oid).
 worktree_id TEXT CHECK(worktree_id IS NULL OR length(worktree_id) BETWEEN 1 AND 128),
 worktree_path TEXT CHECK(worktree_path IS NULL OR length(worktree_path) BETWEEN 1 AND 32768),
 state TEXT NOT NULL CHECK(state IN ('Queued','Indexing','Ready','Failed','Stale','Expired','Unavailable')),
 revision INTEGER NOT NULL CHECK(revision > 0),
 base_generation_id TEXT, base_oid TEXT, head_oid TEXT, head_ref TEXT, capture_digest TEXT,
 branch_generation_id TEXT, worktree_generation_id TEXT, view_id TEXT, run_id TEXT,
 reason TEXT CHECK(reason IS NULL OR length(reason) <= 4096),
 created_at INTEGER NOT NULL CHECK(created_at >= 0),
 updated_at INTEGER NOT NULL CHECK(updated_at >= 0),
 expires_at INTEGER NOT NULL CHECK(expires_at >= 0),
 CHECK((worktree_id IS NULL) = (worktree_path IS NULL)),
 CHECK(worktree_id IS NOT NULL OR (head_oid IS NOT NULL AND capture_digest IS NULL
  AND worktree_generation_id IS NULL)),
 UNIQUE(repository_id, worktree_id)
) STRICT;
CREATE UNIQUE INDEX index_overlays_by_commit ON index_overlays(repository_id, head_oid)
 WHERE worktree_id IS NULL;
CREATE INDEX index_overlays_due ON index_overlays(expires_at)
 WHERE state NOT IN ('Expired','Unavailable');
-- Unit and edge provenance (language, mode, spans live in node payloads; resolution and
-- confidence in edge payloads). Defaulted, so rows of any earlier writer stay valid.
ALTER TABLE source_units ADD COLUMN payload_json TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(payload_json));
ALTER TABLE graph_edges ADD COLUMN payload_json TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(payload_json));
CREATE INDEX graph_nodes_by_name ON graph_nodes(generation_id, name, unit_key, node_id);
-- Linking by exact qualified name (domain extractors name their targets that way).
CREATE INDEX graph_nodes_by_qualified_name
 ON graph_nodes(generation_id, json_extract(payload_json, '$.qualifiedName'), unit_key, node_id);
CREATE INDEX index_generations_by_source ON index_generations(source_key, id);
