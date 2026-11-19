-- Record the open file's identity and the match cursor at its unterminated line.
ALTER TABLE watchers ADD COLUMN file_scan_json TEXT;
