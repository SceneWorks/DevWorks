-- Isolated PostgreSQL17 fixture only: selected target is accessible, postgres is not.
CREATE ROLE metadata_selected_only LOGIN PASSWORD 'devworks_metadata_reader';
REVOKE CONNECT ON DATABASE postgres FROM PUBLIC;
GRANT CONNECT ON DATABASE postgres TO metadata_reader;
GRANT CONNECT ON DATABASE metadata_fixture TO metadata_selected_only;
GRANT USAGE ON SCHEMA public TO metadata_selected_only;
GRANT REFERENCES ON ALL TABLES IN SCHEMA public TO metadata_selected_only;
