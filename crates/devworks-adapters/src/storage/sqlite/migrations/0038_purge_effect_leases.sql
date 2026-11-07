-- A purge effect is claimed only by this store's current boot. Recovery adopts the
-- same reviewed intent after its previous boot or bounded lease has ended.
ALTER TABLE project_purge_effects ADD COLUMN lease_owner TEXT;
ALTER TABLE project_purge_effects ADD COLUMN lease_boot_id TEXT;
ALTER TABLE project_purge_effects ADD COLUMN lease_until INTEGER;
CREATE INDEX project_purge_effect_pending ON project_purge_effects(state,lease_until,manifest_id,id);
