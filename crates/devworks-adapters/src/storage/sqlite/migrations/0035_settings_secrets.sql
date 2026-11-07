-- Only opaque identifiers and custody state. Credential values remain in SecretStore.
CREATE TABLE settings_secret_handles (
 handle TEXT PRIMARY KEY NOT NULL,
 state TEXT NOT NULL CHECK(state IN('Writing','Ready','Retained','Deleting','Deleted')),
 expires_at INTEGER NOT NULL
) STRICT;
