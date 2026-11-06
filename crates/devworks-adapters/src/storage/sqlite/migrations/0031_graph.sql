-- Exact graph annotations, private captured source and bounded multi-view read leases.
CREATE TABLE graph_trust (
 repository_id TEXT NOT NULL REFERENCES repositories,
 unit_key TEXT NOT NULL, node_id TEXT NOT NULL,
 view_id TEXT NOT NULL REFERENCES index_views ON DELETE CASCADE,
 view_digest TEXT NOT NULL CHECK(length(view_digest)=64),
 content_digest TEXT NOT NULL CHECK(length(content_digest)>0),
 revision INTEGER NOT NULL CHECK(revision>=2),
 trusted INTEGER NOT NULL CHECK(trusted IN(0,1)),
 updated_by TEXT NOT NULL REFERENCES actors, updated_at INTEGER NOT NULL,
 PRIMARY KEY(repository_id,unit_key,node_id,view_digest,content_digest)
) STRICT;
CREATE TABLE graph_trust_changes (
 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
 repository_id TEXT NOT NULL, view_digest TEXT NOT NULL
) STRICT;
CREATE INDEX graph_trust_changes_scope ON graph_trust_changes(repository_id,view_digest,sequence);
CREATE TABLE graph_read_pins (
 id TEXT PRIMARY KEY, owner_namespace TEXT NOT NULL,
 store_id TEXT NOT NULL, restore_epoch INTEGER NOT NULL CHECK(restore_epoch>=0),
 scope_digest TEXT NOT NULL CHECK(length(scope_digest)=64),
 intent_digest TEXT NOT NULL CHECK(length(intent_digest)=64),
 scope_json TEXT NOT NULL CHECK(json_valid(scope_json) AND json_type(scope_json)='object'),
 revision INTEGER NOT NULL CHECK(revision>0),
 created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL CHECK(expires_at>created_at)
) STRICT;
CREATE TABLE graph_read_pin_views (
 pin_id TEXT NOT NULL REFERENCES graph_read_pins ON DELETE CASCADE,
 repository_id TEXT NOT NULL REFERENCES repositories,
 view_id TEXT NOT NULL REFERENCES index_views,
 view_digest TEXT NOT NULL CHECK(length(view_digest)=64),
 index_pin_id TEXT NOT NULL UNIQUE REFERENCES index_pins,
 PRIMARY KEY(pin_id,repository_id)
) STRICT;
CREATE INDEX graph_read_pin_view ON graph_read_pin_views(view_id,pin_id);
CREATE TRIGGER graph_pin_release_child AFTER DELETE ON graph_read_pin_views BEGIN
 DELETE FROM index_pins WHERE id=OLD.index_pin_id;
END;
CREATE TABLE graph_source_captures (
 id TEXT PRIMARY KEY, repository_id TEXT NOT NULL REFERENCES repositories,
 unit_key TEXT NOT NULL, source_digest TEXT NOT NULL CHECK(length(source_digest)>0),
 blob_digest TEXT NOT NULL REFERENCES blobs, blob_generation TEXT NOT NULL,
 byte_count INTEGER NOT NULL CHECK(byte_count>=0), created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX graph_source_blob ON graph_source_captures(blob_digest);
CREATE TABLE graph_source_bindings (
 generation_id TEXT NOT NULL, unit_key TEXT NOT NULL,
 capture_id TEXT NOT NULL REFERENCES graph_source_captures,
 PRIMARY KEY(generation_id,unit_key),
 FOREIGN KEY(generation_id,unit_key) REFERENCES source_units ON DELETE CASCADE
) STRICT;
CREATE INDEX graph_source_binding_capture ON graph_source_bindings(capture_id);
CREATE TRIGGER graph_source_bind_insert BEFORE INSERT ON source_units
WHEN json_type(NEW.payload_json,'$.sourceCapture') IS NOT NULL AND json_type(NEW.payload_json,'$.sourceCapture')!='null' BEGIN
 SELECT CASE WHEN NOT EXISTS(
  SELECT 1 FROM graph_source_captures c JOIN index_generations g ON g.id=NEW.generation_id
  JOIN blobs b ON b.digest=c.blob_digest
  WHERE c.id=json_extract(NEW.payload_json,'$.sourceCapture.id')
  AND c.repository_id=g.source_key AND c.unit_key=NEW.unit_key
  AND c.source_digest=NEW.content_digest AND c.blob_digest=json_extract(NEW.payload_json,'$.sourceCapture.sha256')
  AND json_type(NEW.payload_json,'$.sourceCapture.byteCount') IN('integer','text')
  AND CAST(json_extract(NEW.payload_json,'$.sourceCapture.byteCount') AS TEXT) NOT GLOB '*[^0-9]*'
  AND length(CAST(json_extract(NEW.payload_json,'$.sourceCapture.byteCount') AS TEXT))>0
  AND c.byte_count=CAST(json_extract(NEW.payload_json,'$.sourceCapture.byteCount') AS INTEGER)
  AND c.blob_generation=b.generation AND c.byte_count=b.byte_count AND b.state='Present'
  AND NEW.action='Replace'
 ) THEN RAISE(ABORT,'source capture binding mismatch') END;
END;
CREATE TRIGGER graph_source_bind_after_insert AFTER INSERT ON source_units
WHEN json_type(NEW.payload_json,'$.sourceCapture') IS NOT NULL AND json_type(NEW.payload_json,'$.sourceCapture')!='null' BEGIN
 INSERT INTO graph_source_bindings(generation_id,unit_key,capture_id)
 VALUES(NEW.generation_id,NEW.unit_key,json_extract(NEW.payload_json,'$.sourceCapture.id'));
END;
CREATE TRIGGER graph_source_unbind AFTER DELETE ON graph_source_bindings BEGIN
 DELETE FROM graph_source_captures WHERE id=OLD.capture_id
 AND NOT EXISTS(SELECT 1 FROM graph_source_bindings WHERE capture_id=OLD.capture_id);
END;
CREATE TRIGGER graph_source_collect AFTER DELETE ON graph_source_captures BEGIN
 UPDATE blobs SET state='UnlinkPending' WHERE digest=OLD.blob_digest AND state='Present'
 AND NOT EXISTS(SELECT 1 FROM graph_source_captures WHERE blob_digest=OLD.blob_digest)
 AND NOT EXISTS(SELECT 1 FROM artifact_versions WHERE blob_digest=OLD.blob_digest AND deleted_at IS NULL);
END;
