-- Extend the native UI staging owner to feature plans/decisions and convention
-- editor sources. Preserve all existing reservations, identities and state.
CREATE TABLE project_managed_files_v37 (
 id TEXT PRIMARY KEY NOT NULL,
 project_id TEXT NOT NULL REFERENCES projects,
 kind TEXT NOT NULL CHECK(kind IN('DiagnosticExport','BrowserUpload','ClientPaste','ClientStaging','Preview')),
 relative_path TEXT NOT NULL,
 identity_json TEXT NOT NULL CHECK(json_valid(identity_json)),
 byte_count INTEGER NOT NULL CHECK(byte_count>=0),
 state TEXT NOT NULL CHECK(state IN('Writing','Live','Removing','Removed')),
 created_at INTEGER NOT NULL
) STRICT;
INSERT INTO project_managed_files_v37 SELECT * FROM project_managed_files;
DROP TABLE project_managed_files;
ALTER TABLE project_managed_files_v37 RENAME TO project_managed_files;
CREATE UNIQUE INDEX project_managed_file_live_path ON project_managed_files(kind,relative_path) WHERE state<>'Removed';
