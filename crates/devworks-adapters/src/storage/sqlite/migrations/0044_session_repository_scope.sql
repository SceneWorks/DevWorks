-- Capture the repository context of a session at creation, so later work membership edits do
-- not reroute an old direct message to a different repository's successor.
ALTER TABLE sessions ADD COLUMN repository_ids_json TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(repository_ids_json));
UPDATE sessions SET repository_ids_json = COALESCE((
 SELECT json_group_array(repository_id) FROM (
  SELECT repository_id FROM work_repositories WHERE work_id=sessions.root_id ORDER BY repository_id
 )
), '[]');
