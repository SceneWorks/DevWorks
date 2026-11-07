-- Database source configuration holds credential references, never credential values or source rows.
CREATE TABLE schema_sources (
 id TEXT PRIMARY KEY,
 revision INTEGER NOT NULL CHECK(revision>0),
 configuration_revision INTEGER NOT NULL CHECK(configuration_revision>0),
 owner_id TEXT NOT NULL REFERENCES actors(id),
 deleted INTEGER NOT NULL CHECK(deleted IN(0,1)),
 body_json TEXT NOT NULL CHECK(json_valid(body_json) AND json_type(body_json)='object'),
 CHECK(json_extract(body_json,'$.configuration_revision')=configuration_revision),
 CHECK(json_extract(body_json,'$.connection.id')=id),
 CHECK(json_extract(body_json,'$.connection.revision')=CAST(revision AS TEXT)),
 CHECK(json_extract(body_json,'$.owner_id')=owner_id)
) STRICT;
CREATE INDEX schema_sources_owner ON schema_sources(owner_id,id) WHERE deleted=0;
CREATE TABLE schema_views (
 id TEXT PRIMARY KEY,
 connection_id TEXT NOT NULL REFERENCES schema_sources(id),
 source_revision INTEGER NOT NULL CHECK(source_revision>0),
 configuration_revision INTEGER NOT NULL CHECK(configuration_revision>0),
 coverage_json TEXT NOT NULL CHECK(json_valid(coverage_json)),
 operation_id TEXT NOT NULL REFERENCES operations(id),
 digest TEXT NOT NULL CHECK(length(digest)=64 AND digest NOT GLOB '*[^0-9a-f]*'),
 sealed INTEGER NOT NULL CHECK(sealed IN(0,1)),
 created_at INTEGER NOT NULL CHECK(created_at>=0),
 UNIQUE(connection_id,operation_id)
) STRICT;
CREATE INDEX schema_views_source ON schema_views(connection_id,id);
CREATE INDEX schema_views_revision ON schema_views(connection_id,source_revision DESC,id);
CREATE TABLE schema_objects (
 view_id TEXT NOT NULL REFERENCES schema_views(id) ON DELETE CASCADE,
 id TEXT NOT NULL,
 kind TEXT NOT NULL,
 parent_id TEXT,
 qualified_name TEXT NOT NULL,
 body_json TEXT NOT NULL CHECK(json_valid(body_json) AND json_type(body_json)='object'),
 PRIMARY KEY(view_id,id),
 CHECK(json_extract(body_json,'$.id')=id AND json_extract(body_json,'$.viewId')=view_id)
) STRICT;
CREATE INDEX schema_objects_parent ON schema_objects(view_id,parent_id,id);
CREATE INDEX schema_objects_kind ON schema_objects(view_id,kind,id);
CREATE TABLE schema_relations (
 view_id TEXT NOT NULL REFERENCES schema_views(id) ON DELETE CASCADE,
 id TEXT NOT NULL,
 from_id TEXT NOT NULL,
 to_id TEXT NOT NULL,
 body_json TEXT NOT NULL CHECK(json_valid(body_json) AND json_type(body_json)='object'),
 PRIMARY KEY(view_id,id),
 FOREIGN KEY(view_id,from_id) REFERENCES schema_objects(view_id,id),
 FOREIGN KEY(view_id,to_id) REFERENCES schema_objects(view_id,id),
 CHECK(json_extract(body_json,'$.id')=id AND json_extract(body_json,'$.viewId')=view_id)
) STRICT;
CREATE INDEX schema_relations_from ON schema_relations(view_id,from_id,id);
CREATE INDEX schema_relations_to ON schema_relations(view_id,to_id,id);
-- Immutable exact result witnesses survive later source configuration changes or index removal.
CREATE TABLE schema_publications (
 operation_id TEXT PRIMARY KEY REFERENCES operations(id),
 connection_id TEXT NOT NULL REFERENCES schema_sources(id),
 body_json TEXT NOT NULL CHECK(json_valid(body_json) AND json_type(body_json)='object')
) STRICT;
CREATE TABLE schema_inventory(singleton INTEGER PRIMARY KEY CHECK(singleton=1),revision INTEGER NOT NULL CHECK(revision>0)) STRICT;
INSERT INTO schema_inventory VALUES(1,1);
CREATE TRIGGER schema_sources_insert AFTER INSERT ON schema_sources BEGIN UPDATE schema_inventory SET revision=revision+1 WHERE singleton=1; END;
CREATE TRIGGER schema_sources_update AFTER UPDATE ON schema_sources BEGIN UPDATE schema_inventory SET revision=revision+1 WHERE singleton=1; END;
CREATE TRIGGER schema_views_delete AFTER DELETE ON schema_views BEGIN UPDATE schema_inventory SET revision=revision+1 WHERE singleton=1; END;

CREATE INDEX schema_source_credential ON schema_sources(json_extract(body_json,'$.connection.credentialRef')) WHERE deleted=0;

CREATE TABLE schema_read_pins (
 id TEXT PRIMARY KEY,
 view_id TEXT NOT NULL REFERENCES schema_views(id) ON DELETE CASCADE,
 intent_digest TEXT NOT NULL CHECK(length(intent_digest)=64),
 expires_at INTEGER NOT NULL,
 body_json TEXT NOT NULL CHECK(json_valid(body_json))
) STRICT;
CREATE INDEX schema_read_pin_intent ON schema_read_pins(intent_digest,expires_at,id);
CREATE INDEX schema_read_pin_expiry ON schema_read_pins(expires_at,id);
CREATE INDEX schema_read_pin_view ON schema_read_pins(view_id,expires_at);
