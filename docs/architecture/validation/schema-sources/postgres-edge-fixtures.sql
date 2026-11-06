CREATE SCHEMA pgx_user;
CREATE TABLE pgx_user.visible_metadata(id INTEGER PRIMARY KEY);
GRANT USAGE ON SCHEMA pgx_user TO metadata_reader;
GRANT REFERENCES ON ALL TABLES IN SCHEMA pgx_user TO metadata_reader;
CREATE MATERIALIZED VIEW public.materialized_parent AS SELECT id FROM public.parent WITH NO DATA;
CREATE UNIQUE INDEX materialized_parent_index ON public.materialized_parent(id);
GRANT REFERENCES ON public.materialized_parent TO metadata_reader;
