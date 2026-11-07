-- Native continuation bindings use account identity, not rotating credential identity.
ALTER TABLE router_connections ADD COLUMN account_revision INTEGER NOT NULL DEFAULT 1 CHECK(account_revision > 0);

-- Retain identity generations after removal so a reused connection ID cannot revive an old binding.
CREATE TABLE provider_connection_identities (
    id TEXT PRIMARY KEY CHECK(length(id) BETWEEN 1 AND 128 AND id NOT GLOB '*[^A-Za-z0-9_-]*'),
    account_revision INTEGER NOT NULL CHECK(account_revision > 0)
) STRICT;
INSERT INTO provider_connection_identities(id,account_revision)
    SELECT id,account_revision FROM router_connections;

-- Exact OAuth refresh ownership survives app closure and never replays a possibly spent token.
ALTER TABLE router_connections ADD COLUMN refresh_operation_id TEXT REFERENCES operations(id) DEFAULT NULL;
