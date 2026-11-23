-- Global sensitive-column defaults are independent of any connection. Source policies follow
-- the shared registration and never belong to a single project referencing that connection.
CREATE TABLE database_sensitive_policies (
    source_key TEXT PRIMARY KEY,
    source_id TEXT REFERENCES schema_sources(id) ON DELETE CASCADE,
    policy_json TEXT NOT NULL CHECK(json_valid(policy_json) AND json_type(policy_json)='object'),
    CHECK(source_key=coalesce(source_id,''))
) STRICT, WITHOUT ROWID;
