-- Projects, memberships and repository registrations (DW-011). Additive: four new tables and
-- four indexes. The migration 1 `projects`, `repositories` and `project_repositories` tables
-- keep their exact shape; what they lack lives in side tables keyed by the same identity, and a
-- row without its side row reads with the documented defaults (an empty description; a primary,
-- active registration named after its location with no recorded languages).
-- Projects are never deleted as rows: deletion tombstones `projects.lifecycle` as 'Deleted'.
CREATE TABLE project_details (
 project_id TEXT PRIMARY KEY REFERENCES projects,
 description TEXT NOT NULL CHECK(length(description) <= 65536),
 updated_at INTEGER NOT NULL CHECK(updated_at >= 0),
 archived_at INTEGER CHECK(archived_at IS NULL OR archived_at >= 0),
 deleted_at INTEGER CHECK(deleted_at IS NULL OR deleted_at >= 0),
 -- Advances with every archival; a launch recorded under an earlier epoch was stopped by one.
 archive_epoch INTEGER NOT NULL CHECK(archive_epoch >= 0),
 -- The archival's immediate project-wide stop has not run yet (restore and delete wait).
 archive_stop_pending INTEGER NOT NULL CHECK(archive_stop_pending IN (0,1))
) STRICT;
CREATE INDEX project_details_pending_stop ON project_details(project_id)
 WHERE archive_stop_pending = 1;
CREATE TABLE repository_details (
 repository_id TEXT PRIMARY KEY REFERENCES repositories,
 name TEXT NOT NULL CHECK(length(name) BETWEEN 1 AND 256),
 remote_url TEXT CHECK(remote_url IS NULL OR length(remote_url) BETWEEN 1 AND 4096),
 is_primary INTEGER NOT NULL CHECK(is_primary IN (0,1)),
 lifecycle TEXT NOT NULL CHECK(lifecycle IN ('Active','Archived','Deleted')),
 languages_json TEXT NOT NULL CHECK(json_valid(languages_json) AND json_type(languages_json)='array'),
 registered_at INTEGER NOT NULL CHECK(registered_at >= 0)
) STRICT;
-- App index views the indexing item publishes; removing an index removes these rows and their
-- texts only, never the registration, its membership or work history.
CREATE TABLE repository_index_views (
 view_id TEXT PRIMARY KEY,
 repository_id TEXT NOT NULL REFERENCES repositories,
 source_revision TEXT NOT NULL CHECK(length(source_revision) BETWEEN 1 AND 256),
 coverage TEXT NOT NULL CHECK(coverage IN ('Complete','Partial','Unavailable')),
 published_at INTEGER NOT NULL CHECK(published_at >= 0)
) STRICT;
CREATE INDEX repository_index_views_by_repository
 ON repository_index_views(repository_id, published_at DESC, view_id);
CREATE TABLE repository_texts (
 view_id TEXT NOT NULL REFERENCES repository_index_views,
 kind TEXT NOT NULL CHECK(kind IN ('Readme','Summary')),
 repository_id TEXT NOT NULL REFERENCES repositories,
 artifact_id TEXT NOT NULL, version_id TEXT NOT NULL,
 sha256 TEXT NOT NULL CHECK(length(sha256) = 64),
 byte_length INTEGER NOT NULL CHECK(byte_length >= 0),
 media_type TEXT NOT NULL CHECK(length(media_type) BETWEEN 1 AND 128),
 coverage TEXT NOT NULL CHECK(coverage IN ('Complete','Partial','Unavailable')),
 PRIMARY KEY(view_id, kind)
) STRICT;
CREATE INDEX projects_by_creation ON projects(created_at, id);
CREATE INDEX project_repositories_by_repository ON project_repositories(repository_id, project_id);
