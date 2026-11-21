-- The first authorized input and successful tier belong to the session, not to a pin.
CREATE TABLE router_auto_sessions (
    session_id TEXT PRIMARY KEY REFERENCES sessions(id),
    source_json TEXT NOT NULL CHECK (json_valid(source_json)),
    systemone_operation TEXT,
    low_plan_json TEXT CHECK (low_plan_json IS NULL OR json_valid(low_plan_json)),
    decision_json TEXT CHECK (decision_json IS NULL OR json_valid(decision_json))
) STRICT;

-- The auxiliary Low classifier never consumes a conversational or SystemOne rotation.
CREATE TABLE router_auto_low_cursor (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    next INTEGER NOT NULL CHECK (next >= 0)
) STRICT;
INSERT INTO router_auto_low_cursor VALUES (1, 0);
