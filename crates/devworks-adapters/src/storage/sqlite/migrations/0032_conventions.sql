-- Head pointers and immutable revision snapshots are distinct: publication is one transaction.
CREATE TABLE convention_heads (
 id TEXT PRIMARY KEY, revision INTEGER NOT NULL CHECK(revision>0),
 FOREIGN KEY(id,revision) REFERENCES convention_versions(convention_id,revision) DEFERRABLE INITIALLY DEFERRED
) STRICT;
CREATE TABLE convention_versions (
 convention_id TEXT NOT NULL REFERENCES convention_heads DEFERRABLE INITIALLY DEFERRED,
 revision INTEGER NOT NULL CHECK(revision>0), scope_key TEXT NOT NULL,
 project_id TEXT REFERENCES projects, slug TEXT NOT NULL, title TEXT NOT NULL,
 title_folded TEXT NOT NULL, slug_folded TEXT NOT NULL, parent_id TEXT REFERENCES convention_heads,
 sort_order INTEGER NOT NULL, lifecycle TEXT NOT NULL CHECK(lifecycle IN('Active','Retired','Deleted')),
 content_ref_json TEXT CHECK(content_ref_json IS NULL OR json_valid(content_ref_json)),
 content_version_id TEXT REFERENCES artifact_versions,
 raw_content_version_id TEXT REFERENCES artifact_versions,
 write_boot_id TEXT NOT NULL REFERENCES app_boots, write_transaction TEXT NOT NULL,
 raw_content_ref_json TEXT CHECK(raw_content_ref_json IS NULL OR json_valid(raw_content_ref_json)),
 applicability_json TEXT NOT NULL CHECK(json_valid(applicability_json)),
 provenance_json TEXT NOT NULL CHECK(json_valid(provenance_json)),
 author_id TEXT NOT NULL REFERENCES actors, created_at INTEGER NOT NULL CHECK(created_at>=0),
 updated_at INTEGER NOT NULL CHECK(updated_at>=created_at), legacy_approved INTEGER NOT NULL CHECK(legacy_approved IN(0,1)),
 PRIMARY KEY(convention_id,revision),
 CHECK(content_version_id IS json_extract(content_ref_json,'$.versionId')),
 CHECK(raw_content_version_id IS json_extract(raw_content_ref_json,'$.versionId')),
 CHECK((project_id IS NULL AND scope_key='workspace') OR (project_id IS NOT NULL AND scope_key='project:'||project_id))
) STRICT;
CREATE INDEX convention_version_scope ON convention_versions(scope_key,lifecycle,convention_id,revision);
CREATE INDEX convention_version_slug ON convention_versions(scope_key,slug,convention_id,revision);
CREATE INDEX convention_version_parent ON convention_versions(parent_id,sort_order,convention_id,revision);
CREATE TABLE convention_scope_revisions (
 scope_key TEXT PRIMARY KEY, project_id TEXT REFERENCES projects,
 revision INTEGER NOT NULL CHECK(revision>0),
 CHECK((project_id IS NULL AND scope_key='workspace') OR (project_id IS NOT NULL AND scope_key='project:'||project_id))
) STRICT;
CREATE TABLE convention_resource_heads (
 id TEXT PRIMARY KEY, convention_id TEXT NOT NULL REFERENCES convention_heads,
 revision INTEGER NOT NULL CHECK(revision>0),
 FOREIGN KEY(id,revision) REFERENCES convention_resource_versions(resource_id,revision) DEFERRABLE INITIALLY DEFERRED
) STRICT;
CREATE TABLE convention_resource_versions (
 resource_id TEXT NOT NULL REFERENCES convention_resource_heads DEFERRABLE INITIALLY DEFERRED,
 revision INTEGER NOT NULL CHECK(revision>0), convention_id TEXT NOT NULL,
 convention_revision INTEGER NOT NULL CHECK(convention_revision>0), name TEXT NOT NULL, media_type TEXT NOT NULL,
 content_ref_json TEXT NOT NULL CHECK(json_valid(content_ref_json)),
 content_version_id TEXT NOT NULL REFERENCES artifact_versions,
 author_id TEXT NOT NULL REFERENCES actors, created_at INTEGER NOT NULL CHECK(created_at>=0),
 deleted_at INTEGER CHECK(deleted_at IS NULL OR deleted_at>=created_at),
 lifecycle TEXT NOT NULL CHECK(lifecycle IN('Active','Retired','Deleted')),
 PRIMARY KEY(resource_id,revision),
 FOREIGN KEY(convention_id,convention_revision) REFERENCES convention_versions(convention_id,revision),
 CHECK((lifecycle='Deleted')=(deleted_at IS NOT NULL)),
 CHECK(content_version_id=json_extract(content_ref_json,'$.versionId'))
) STRICT;
CREATE INDEX convention_resource_owner ON convention_resource_versions(convention_id,convention_revision,resource_id,revision);
CREATE TRIGGER convention_versions_immutable_update BEFORE UPDATE ON convention_versions BEGIN SELECT RAISE(ABORT,'immutable convention revision'); END;
CREATE TRIGGER convention_versions_immutable_delete BEFORE DELETE ON convention_versions BEGIN SELECT RAISE(ABORT,'immutable convention revision'); END;
CREATE TRIGGER convention_resource_versions_immutable_update BEFORE UPDATE ON convention_resource_versions BEGIN SELECT RAISE(ABORT,'immutable convention resource revision'); END;
CREATE TRIGGER convention_resource_versions_immutable_delete BEFORE DELETE ON convention_resource_versions BEGIN SELECT RAISE(ABORT,'immutable convention resource revision'); END;
CREATE TABLE convention_embedding_spaces (
 id TEXT PRIMARY KEY, provider TEXT NOT NULL, model TEXT NOT NULL,
 configuration_digest TEXT NOT NULL, dimensions INTEGER NOT NULL CHECK(dimensions>0),
 metric TEXT NOT NULL CHECK(metric='Cosine'), element_type TEXT NOT NULL CHECK(element_type='Float32')
) STRICT;
CREATE TABLE convention_indexes (
 generation_id TEXT PRIMARY KEY, convention_id TEXT NOT NULL, convention_revision INTEGER NOT NULL,
 resource_id TEXT, resource_revision INTEGER, operation_id TEXT NOT NULL REFERENCES operations,
 state TEXT NOT NULL CHECK(state IN('Draft','Published')),
 status TEXT NOT NULL CHECK(status IN('Queued','Indexing','Ready','LexicalOnly','Stale','Failed','Cancelled')),
 error TEXT, embedding_space TEXT REFERENCES convention_embedding_spaces, source_digest TEXT NOT NULL,
 created_at INTEGER NOT NULL CHECK(created_at>=0), updated_at INTEGER NOT NULL CHECK(updated_at>=created_at),
 FOREIGN KEY(convention_id,convention_revision) REFERENCES convention_versions(convention_id,revision),
 FOREIGN KEY(resource_id,resource_revision) REFERENCES convention_resource_versions(resource_id,revision),
 CHECK((resource_id IS NULL)=(resource_revision IS NULL))
) STRICT;
CREATE INDEX convention_index_owner ON convention_indexes(convention_id,resource_id,generation_id);
CREATE INDEX convention_index_operation ON convention_indexes(operation_id,generation_id);
CREATE TABLE convention_index_heads (
 convention_id TEXT NOT NULL REFERENCES convention_heads, resource_key TEXT NOT NULL,
 generation_id TEXT NOT NULL UNIQUE REFERENCES convention_indexes,
 PRIMARY KEY(convention_id,resource_key)
) STRICT;
CREATE TABLE convention_chunks (
 generation_id TEXT NOT NULL REFERENCES convention_indexes, id TEXT NOT NULL,
 chunk_index INTEGER NOT NULL CHECK(chunk_index>=0), section_path TEXT NOT NULL,
 section_path_ref_json TEXT CHECK(section_path_ref_json IS NULL OR json_valid(section_path_ref_json)),
 section_path_version_id TEXT REFERENCES artifact_versions,
 text TEXT NOT NULL, embedding_text TEXT NOT NULL, token_count INTEGER NOT NULL CHECK(token_count>=0),
 vector BLOB CHECK(vector IS NULL OR length(vector)%4=0),
 PRIMARY KEY(generation_id,id), UNIQUE(generation_id,chunk_index),
 CHECK((section_path_ref_json IS NULL)=(section_path_version_id IS NULL))
) STRICT;
CREATE TABLE convention_chunk_terms (
 generation_id TEXT NOT NULL, chunk_id TEXT NOT NULL, term TEXT NOT NULL,
 frequency INTEGER NOT NULL CHECK(frequency>0), PRIMARY KEY(generation_id,chunk_id,term),
 FOREIGN KEY(generation_id,chunk_id) REFERENCES convention_chunks(generation_id,id) ON DELETE CASCADE
) STRICT;
CREATE INDEX convention_term_chunks ON convention_chunk_terms(term,generation_id,chunk_id,frequency);
CREATE TRIGGER convention_space_immutable_update BEFORE UPDATE ON convention_embedding_spaces BEGIN SELECT RAISE(ABORT,'immutable convention embedding space'); END;
CREATE TRIGGER convention_space_immutable_delete BEFORE DELETE ON convention_embedding_spaces BEGIN SELECT RAISE(ABORT,'immutable convention embedding space'); END;
CREATE TRIGGER convention_index_source_immutable BEFORE UPDATE ON convention_indexes
 WHEN NEW.generation_id!=OLD.generation_id OR NEW.convention_id!=OLD.convention_id OR NEW.convention_revision!=OLD.convention_revision
 OR NEW.resource_id IS NOT OLD.resource_id OR NEW.resource_revision IS NOT OLD.resource_revision OR NEW.operation_id!=OLD.operation_id
 OR NEW.embedding_space IS NOT OLD.embedding_space OR NEW.source_digest!=OLD.source_digest OR NEW.created_at!=OLD.created_at
 BEGIN SELECT RAISE(ABORT,'immutable convention index source'); END;
CREATE TRIGGER convention_index_immutable_delete BEFORE DELETE ON convention_indexes BEGIN SELECT RAISE(ABORT,'retained convention index source'); END;
CREATE TRIGGER convention_chunks_immutable_update BEFORE UPDATE ON convention_chunks BEGIN SELECT RAISE(ABORT,'immutable convention chunk'); END;
CREATE TRIGGER convention_terms_immutable_update BEFORE UPDATE ON convention_chunk_terms BEGIN SELECT RAISE(ABORT,'immutable convention term'); END;
CREATE TRIGGER convention_chunk_cleanup_guard BEFORE DELETE ON convention_chunks
 WHEN EXISTS(SELECT 1 FROM convention_index_heads h JOIN convention_indexes g ON g.generation_id=h.generation_id
 WHERE h.generation_id=OLD.generation_id AND g.status IN('Ready','LexicalOnly'))
 BEGIN SELECT RAISE(ABORT,'current convention chunks cannot be discarded'); END;
CREATE TABLE convention_embedding_inputs (
 request_id TEXT PRIMARY KEY, actor_id TEXT NOT NULL REFERENCES actors,
 project_id TEXT REFERENCES projects, context_digest TEXT NOT NULL,
 embedding_space TEXT NOT NULL REFERENCES convention_embedding_spaces,
 input_digest TEXT NOT NULL, created_at INTEGER NOT NULL CHECK(created_at>=0)
) STRICT;
CREATE INDEX convention_embedding_inputs_project ON convention_embedding_inputs(project_id,request_id);
CREATE TABLE convention_embedding_input_refs (
 request_id TEXT NOT NULL REFERENCES convention_embedding_inputs, ordinal INTEGER NOT NULL CHECK(ordinal>=0 AND ordinal<32),
 artifact_id TEXT NOT NULL REFERENCES artifacts, version_id TEXT NOT NULL REFERENCES artifact_versions,
 reference_json TEXT NOT NULL CHECK(json_valid(reference_json)), PRIMARY KEY(request_id,ordinal),
 CHECK(artifact_id=json_extract(reference_json,'$.artifactId') AND version_id=json_extract(reference_json,'$.versionId'))
) STRICT;
CREATE TRIGGER convention_inputs_immutable_update BEFORE UPDATE ON convention_embedding_inputs BEGIN SELECT RAISE(ABORT,'immutable convention embedding inputs'); END;
CREATE TRIGGER convention_inputs_immutable_delete BEFORE DELETE ON convention_embedding_inputs BEGIN SELECT RAISE(ABORT,'retained convention embedding inputs'); END;
CREATE TRIGGER convention_input_refs_immutable_update BEFORE UPDATE ON convention_embedding_input_refs BEGIN SELECT RAISE(ABORT,'immutable convention embedding input ref'); END;
CREATE TRIGGER convention_input_refs_immutable_delete BEFORE DELETE ON convention_embedding_input_refs BEGIN SELECT RAISE(ABORT,'retained convention embedding input ref'); END;
CREATE TABLE convention_index_scope_revisions (
 scope_key TEXT PRIMARY KEY, project_id TEXT REFERENCES projects, revision INTEGER NOT NULL CHECK(revision>0),
 CHECK((project_id IS NULL AND scope_key='workspace') OR(project_id IS NOT NULL AND scope_key='project:'||project_id))
) STRICT;
