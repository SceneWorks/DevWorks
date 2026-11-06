-- A draft can be staged guide by guide, but public Help only reads published generations.
CREATE TABLE help_generations (
 id TEXT PRIMARY KEY,
 operation_id TEXT NOT NULL REFERENCES operations,
 state TEXT NOT NULL CHECK(state IN ('Draft','Published')),
 app_version TEXT,
 registry_revision INTEGER CHECK(registry_revision > 0),
 source_digest TEXT,
 help_revision INTEGER CHECK(help_revision > 0),
 published_at INTEGER CHECK(published_at >= 0)
) STRICT;
CREATE INDEX help_draft_page ON help_generations(state,id);
CREATE TABLE help_head (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 revision INTEGER NOT NULL CHECK(revision > 0),
 app_version TEXT NOT NULL,
 registry_revision INTEGER NOT NULL CHECK(registry_revision > 0),
 source_digest TEXT NOT NULL,
 generation_id TEXT REFERENCES help_generations,
 generation_operation_id TEXT REFERENCES operations,
 updated_at INTEGER NOT NULL CHECK(updated_at >= 0)
) STRICT;
INSERT INTO help_head VALUES(1,1,'',1,'',NULL,NULL,0);
CREATE TABLE help_topics (
 generation_id TEXT NOT NULL REFERENCES help_generations,
 id TEXT NOT NULL,
 version TEXT NOT NULL,
 title TEXT NOT NULL,
 content_digest TEXT NOT NULL,
 content_ref_json TEXT CHECK(content_ref_json IS NULL OR (json_valid(content_ref_json) AND json_type(content_ref_json)='object')),
 artifact_id TEXT REFERENCES artifacts,
 version_id TEXT REFERENCES artifact_versions,
 provenance_json TEXT NOT NULL CHECK(json_valid(provenance_json)),
 related_operations_json TEXT NOT NULL CHECK(json_valid(related_operations_json) AND json_type(related_operations_json)='array'),
 screen_ids_json TEXT NOT NULL CHECK(json_valid(screen_ids_json) AND json_type(screen_ids_json)='array'),
 source_kind TEXT NOT NULL CHECK(source_kind IN ('Bundled','Package','Generated')),
 package_id TEXT,
 manifest_digest TEXT,
 availability TEXT NOT NULL CHECK(availability IN ('Available','Missing','Stale','Unavailable')),
 reason TEXT,
 character_count INTEGER NOT NULL CHECK(character_count >= 0),
 topic_revision INTEGER NOT NULL CHECK(topic_revision > 0),
 body TEXT NOT NULL,
 search_text TEXT NOT NULL,
 PRIMARY KEY(generation_id,id),
 CHECK((content_ref_json IS NULL)=(artifact_id IS NULL)),
 CHECK((content_ref_json IS NULL)=(version_id IS NULL)),
 CHECK((availability='Available')=(content_ref_json IS NOT NULL)),
 CHECK(availability='Available' OR (body='' AND character_count=0 AND provenance_json='null')),
 CHECK((source_kind='Package')=(package_id IS NOT NULL)),
 CHECK((source_kind='Package')=(manifest_digest IS NOT NULL))
) STRICT;
CREATE INDEX help_topic_package_page ON help_topics(generation_id,package_id,id);
CREATE TABLE help_topic_screens (
 generation_id TEXT NOT NULL,
 topic_id TEXT NOT NULL,
 screen_id TEXT NOT NULL,
 PRIMARY KEY(generation_id,topic_id,screen_id),
 FOREIGN KEY(generation_id,topic_id) REFERENCES help_topics(generation_id,id) ON DELETE CASCADE
) STRICT;
CREATE INDEX help_screen_page ON help_topic_screens(generation_id,screen_id,topic_id);
CREATE TRIGGER help_topic_immutable_update BEFORE UPDATE ON help_topics BEGIN
 SELECT RAISE(ABORT,'help documents are immutable');
END;
CREATE TRIGGER help_topic_published_delete BEFORE DELETE ON help_topics
 WHEN EXISTS(SELECT 1 FROM help_generations WHERE id=OLD.generation_id AND state='Published') BEGIN
 SELECT RAISE(ABORT,'published help documents are immutable');
END;
CREATE TRIGGER help_generation_published_update BEFORE UPDATE ON help_generations
 WHEN OLD.state='Published' BEGIN
 SELECT RAISE(ABORT,'published help generations are immutable');
END;
CREATE TRIGGER help_generation_published_delete BEFORE DELETE ON help_generations
 WHEN OLD.state='Published' BEGIN
 SELECT RAISE(ABORT,'published help generations are immutable');
END;

CREATE TRIGGER help_topic_published_insert BEFORE INSERT ON help_topics
 WHEN EXISTS(SELECT 1 FROM help_generations WHERE id=NEW.generation_id AND state='Published') BEGIN
 SELECT RAISE(ABORT,'published help documents are immutable');
END;
CREATE TRIGGER help_screen_published_insert BEFORE INSERT ON help_topic_screens
 WHEN EXISTS(SELECT 1 FROM help_generations WHERE id=NEW.generation_id AND state='Published') BEGIN
 SELECT RAISE(ABORT,'published help associations are immutable');
END;
CREATE TRIGGER help_screen_immutable_update BEFORE UPDATE ON help_topic_screens BEGIN
 SELECT RAISE(ABORT,'help associations are immutable');
END;
CREATE TRIGGER help_screen_published_delete BEFORE DELETE ON help_topic_screens
 WHEN EXISTS(SELECT 1 FROM help_generations WHERE id=OLD.generation_id AND state='Published') BEGIN
 SELECT RAISE(ABORT,'published help associations are immutable');
END;
