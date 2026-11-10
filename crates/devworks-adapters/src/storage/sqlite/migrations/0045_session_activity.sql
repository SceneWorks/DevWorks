-- An authenticated empty inbox poll proves the holder is still active without creating a
-- transcript entry or changing the session's authority revision.
ALTER TABLE sessions ADD COLUMN last_seen_at INTEGER;
