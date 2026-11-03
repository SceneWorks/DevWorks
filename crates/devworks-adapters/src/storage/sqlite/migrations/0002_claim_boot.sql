-- DevWorks store migration 0002: bind each queue claim to the boot epoch that made it.
-- A claim is current only within its boot ("current boot" in the queue-claim unit of work):
-- renew/settle/fail require it, and startup recovery reclaims only claims of earlier boots, so a
-- live claim of the running process can never be stolen. Additive and nullable (no claim).
-- Never edit an applied migration; its SHA-256 is recorded in schema_migrations.
ALTER TABLE queue_entries ADD COLUMN claim_boot_id TEXT REFERENCES app_boots;
