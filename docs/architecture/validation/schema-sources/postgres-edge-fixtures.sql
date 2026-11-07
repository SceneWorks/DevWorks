CREATE SCHEMA pgx_user;
CREATE TABLE pgx_user.visible_metadata(id INTEGER PRIMARY KEY);
GRANT USAGE ON SCHEMA pgx_user TO metadata_reader;
GRANT REFERENCES ON ALL TABLES IN SCHEMA pgx_user TO metadata_reader;
CREATE MATERIALIZED VIEW public.materialized_parent AS SELECT id FROM public.parent WITH NO DATA;
CREATE UNIQUE INDEX materialized_parent_index ON public.materialized_parent(id);
GRANT REFERENCES ON public.materialized_parent TO metadata_reader;
-- A real150KB multibyte view definition exceeds the wire byte bound without exceeding
-- the character bound. It contains DDL-only fixture text, never source-table row values.
DO $metadata_fixture$
BEGIN
 EXECUTE 'CREATE OR REPLACE VIEW public.meta_oversized_definition AS SELECT '
  || quote_literal(repeat('界',50000)) || '::text AS metadata_only';
END
$metadata_fixture$;
