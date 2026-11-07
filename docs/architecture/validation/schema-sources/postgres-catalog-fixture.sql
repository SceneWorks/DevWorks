-- Admin-only psql setup inside the isolated PostgreSQL17 qualification container.
SELECT format('CREATE DATABASE %I', 'aaa_metadata_discovery_' || lpad(n::text,3,'0'))
FROM generate_series(0,129) AS fixture(n)
\gexec
