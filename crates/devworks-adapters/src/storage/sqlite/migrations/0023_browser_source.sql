-- Nullable for historical/unbound evidence; never backfill a guessed checkout.
ALTER TABLE browser_sessions ADD COLUMN tested_repository TEXT CHECK(tested_repository IS NULL OR json_valid(tested_repository));
ALTER TABLE browser_sessions ADD COLUMN source_binding TEXT CHECK(source_binding IS NULL OR json_valid(source_binding));
ALTER TABLE browser_observations ADD COLUMN tested_repository TEXT CHECK(tested_repository IS NULL OR json_valid(tested_repository));
CREATE TRIGGER browser_source_immutable BEFORE UPDATE OF tested_repository,source_binding ON browser_sessions
WHEN NEW.tested_repository IS NOT OLD.tested_repository OR NEW.source_binding IS NOT OLD.source_binding
BEGIN SELECT RAISE(ABORT, 'browser source is immutable'); END;
