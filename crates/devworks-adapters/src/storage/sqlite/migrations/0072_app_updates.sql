-- One workspace-wide update authority. Poll snapshots replace in place; approval intent,
-- stable receipt and native effect observations also use the existing operation journal.
CREATE TABLE app_update_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    record_json TEXT NOT NULL CHECK (json_valid(record_json))
) STRICT;
