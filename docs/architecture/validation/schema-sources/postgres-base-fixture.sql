-- Admin-only psql setup inside an isolated PostgreSQL qualification container.
-- metadata_reader has no table SELECT grant; it reads catalogs only. The owner routine's
-- definition is hidden from it, so its captures are Partial on purpose.
CREATE ROLE metadata_reader LOGIN PASSWORD 'devworks_metadata_reader';
CREATE DATABASE metadata_fixture;
CREATE DATABASE metadata_limits;
\connect metadata_fixture
CREATE TABLE public.parent(id INTEGER PRIMARY KEY, note TEXT NOT NULL DEFAULT 'metadata default');
CREATE TABLE public.child(id INTEGER PRIMARY KEY, parent_id INTEGER NOT NULL REFERENCES public.parent(id));
CREATE INDEX child_pair ON public.child(parent_id, id);
INSERT INTO public.parent(id, note) VALUES (1, 'SOURCE_ROW_SENTINEL_NEVER_INDEX');
CREATE VIEW public.read_parent AS SELECT id, note FROM public.parent;
CREATE FUNCTION public.describe_parent(input_id INTEGER) RETURNS INTEGER LANGUAGE sql STABLE AS 'SELECT input_id';
GRANT REFERENCES ON ALL TABLES IN SCHEMA public TO metadata_reader;
\connect metadata_limits
DO $metadata_fixture$
BEGIN
 EXECUTE 'CREATE TABLE public.limits_parent(id INTEGER PRIMARY KEY, long_default TEXT DEFAULT '
  || quote_literal(repeat('x', 6000)) || ')';
END
$metadata_fixture$;
GRANT REFERENCES ON ALL TABLES IN SCHEMA public TO metadata_reader;
