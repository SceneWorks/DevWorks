-- Native continuation bindings use account identity, not rotating credential identity.
ALTER TABLE router_connections ADD COLUMN account_revision INTEGER NOT NULL DEFAULT 1 CHECK(account_revision > 0);
