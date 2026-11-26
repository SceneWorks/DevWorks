-- Supply null columns on a store that only has the GitHub source.
ALTER TABLE watchers ADD COLUMN attached_pid INTEGER;
ALTER TABLE watchers ADD COLUMN attached_birth TEXT;
ALTER TABLE watchers ADD COLUMN assignment_id TEXT;
ALTER TABLE watchers ADD COLUMN child_session_id TEXT;
