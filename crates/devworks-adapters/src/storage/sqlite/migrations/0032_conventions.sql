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
 index_source_revision INTEGER NOT NULL DEFAULT 1 CHECK(index_source_revision>0),
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
 source_ref_json TEXT NOT NULL CHECK(json_valid(source_ref_json)), source_version_id TEXT NOT NULL REFERENCES artifact_versions,
 operation_id TEXT NOT NULL REFERENCES operations,state TEXT NOT NULL CHECK(state IN('Draft','Published')),
 status TEXT NOT NULL CHECK(status IN('Queued','Indexing','Ready','LexicalOnly','Stale','Failed','Cancelled')),
 error TEXT,embedding_space TEXT REFERENCES convention_embedding_spaces,source_digest TEXT NOT NULL,
 created_at INTEGER NOT NULL CHECK(created_at>=0),updated_at INTEGER NOT NULL CHECK(updated_at>=created_at),
 FOREIGN KEY(convention_id,convention_revision) REFERENCES convention_versions(convention_id,revision),
 UNIQUE(convention_id,operation_id),
 CHECK(source_version_id=json_extract(source_ref_json,'$.versionId'))
) STRICT;
CREATE INDEX convention_index_owner ON convention_indexes(convention_id,generation_id);
CREATE INDEX convention_index_operation ON convention_indexes(operation_id,generation_id);
CREATE TABLE convention_index_heads (
 convention_id TEXT PRIMARY KEY REFERENCES convention_heads,generation_id TEXT NOT NULL UNIQUE REFERENCES convention_indexes
) STRICT;
CREATE TABLE convention_sections (
 generation_id TEXT NOT NULL REFERENCES convention_indexes,id TEXT NOT NULL,parent_id TEXT,
 heading TEXT,level INTEGER NOT NULL CHECK(level BETWEEN 0 AND 3),source_offset INTEGER,source_length INTEGER,
 token_count INTEGER NOT NULL CHECK(token_count>=0),PRIMARY KEY(generation_id,id),
 FOREIGN KEY(generation_id,parent_id) REFERENCES convention_sections(generation_id,id),
 CHECK((level=0 AND parent_id IS NULL AND heading IS NOT NULL AND source_offset IS NULL AND source_length IS NULL)
 OR(level>0 AND parent_id IS NOT NULL AND source_offset>=0 AND source_length>=0
 AND (heading IS NULL OR (length(cast(heading AS BLOB))<=65536 AND length(cast(heading AS BLOB))=source_length AND instr(heading,char(0))=0))))
) STRICT;
CREATE UNIQUE INDEX convention_section_root ON convention_sections(generation_id) WHERE level=0;
CREATE UNIQUE INDEX convention_section_source_offset ON convention_sections(generation_id,source_offset) WHERE level>0;
CREATE TRIGGER convention_section_source_overlap BEFORE INSERT ON convention_sections
 WHEN NEW.level>0 AND EXISTS(SELECT 1 FROM convention_sections s WHERE s.generation_id=NEW.generation_id AND s.level>0
 AND s.source_offset<NEW.source_offset+NEW.source_length AND NEW.source_offset<s.source_offset+s.source_length)
 BEGIN SELECT RAISE(ABORT,'overlapping convention source sections'); END;
CREATE TABLE convention_chunks (
 generation_id TEXT NOT NULL REFERENCES convention_indexes,id TEXT NOT NULL,
 chunk_index INTEGER NOT NULL CHECK(chunk_index>=0),section_id TEXT NOT NULL,text TEXT NOT NULL,
 token_count INTEGER NOT NULL CHECK(token_count>=0),vector BLOB CHECK(vector IS NULL OR length(vector)%4=0),
 PRIMARY KEY(generation_id,id),UNIQUE(generation_id,chunk_index),
 FOREIGN KEY(generation_id,section_id) REFERENCES convention_sections(generation_id,id)
) STRICT;
CREATE TABLE convention_terms (
 generation_id TEXT NOT NULL REFERENCES convention_indexes,id INTEGER NOT NULL,term TEXT NOT NULL,
 PRIMARY KEY(generation_id,id),UNIQUE(generation_id,term)
) STRICT;
CREATE TABLE convention_section_terms (
 generation_id TEXT NOT NULL,section_id TEXT NOT NULL,term_id INTEGER NOT NULL,frequency INTEGER NOT NULL CHECK(frequency>0),
 PRIMARY KEY(generation_id,section_id,term_id),
 FOREIGN KEY(generation_id,section_id) REFERENCES convention_sections(generation_id,id) ON DELETE CASCADE,
 FOREIGN KEY(generation_id,term_id) REFERENCES convention_terms(generation_id,id)
) STRICT;
CREATE TABLE convention_chunk_terms (
 generation_id TEXT NOT NULL,chunk_id TEXT NOT NULL,term_id INTEGER NOT NULL,frequency INTEGER NOT NULL CHECK(frequency>0),
 PRIMARY KEY(generation_id,chunk_id,term_id),
 FOREIGN KEY(generation_id,chunk_id) REFERENCES convention_chunks(generation_id,id) ON DELETE CASCADE,
 FOREIGN KEY(generation_id,term_id) REFERENCES convention_terms(generation_id,id)
) STRICT;
CREATE INDEX convention_term_chunks ON convention_chunk_terms(generation_id,term_id,chunk_id,frequency);
CREATE INDEX convention_term_sections ON convention_section_terms(generation_id,term_id,section_id,frequency);
CREATE TABLE convention_term_batches (
 generation_id TEXT NOT NULL REFERENCES convention_indexes,kind TEXT NOT NULL CHECK(kind IN('Section','Chunk')),
 owner_id TEXT NOT NULL,batch_index INTEGER NOT NULL CHECK(batch_index>=0),digest TEXT NOT NULL,
 PRIMARY KEY(generation_id,kind,owner_id,batch_index)
) STRICT;
CREATE TRIGGER convention_space_immutable_update BEFORE UPDATE ON convention_embedding_spaces BEGIN SELECT RAISE(ABORT,'immutable convention embedding space'); END;
CREATE TRIGGER convention_space_immutable_delete BEFORE DELETE ON convention_embedding_spaces BEGIN SELECT RAISE(ABORT,'immutable convention embedding space'); END;
CREATE TRIGGER convention_index_source_immutable BEFORE UPDATE ON convention_indexes
 WHEN NEW.generation_id!=OLD.generation_id OR NEW.convention_id!=OLD.convention_id OR NEW.convention_revision!=OLD.convention_revision
 OR NEW.source_ref_json!=OLD.source_ref_json OR NEW.operation_id!=OLD.operation_id
 OR NEW.embedding_space IS NOT OLD.embedding_space OR NEW.source_digest!=OLD.source_digest OR NEW.created_at!=OLD.created_at
 BEGIN SELECT RAISE(ABORT,'immutable convention index source'); END;
CREATE TRIGGER convention_index_immutable_delete BEFORE DELETE ON convention_indexes BEGIN SELECT RAISE(ABORT,'retained convention index source'); END;
CREATE TRIGGER convention_chunks_immutable_update BEFORE UPDATE ON convention_chunks BEGIN SELECT RAISE(ABORT,'immutable convention chunk'); END;
CREATE TRIGGER convention_sections_immutable_update BEFORE UPDATE ON convention_sections BEGIN SELECT RAISE(ABORT,'immutable convention section'); END;
CREATE TRIGGER convention_chunk_cleanup_guard BEFORE DELETE ON convention_chunks
 WHEN EXISTS(SELECT 1 FROM convention_index_heads WHERE generation_id=OLD.generation_id)
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
-- Streamed imports bind actual staged blobs privately; public artifact/catalog rows appear
-- together only after final source/scope/parent/count/journal validation.
CREATE TABLE convention_imports (
 operation_id TEXT PRIMARY KEY REFERENCES operations, convention_id TEXT NOT NULL UNIQUE,
 scope_key TEXT NOT NULL, project_id TEXT REFERENCES projects, author_id TEXT NOT NULL REFERENCES actors,
 source_ref_json TEXT NOT NULL CHECK(json_valid(source_ref_json)), source_version_id TEXT NOT NULL REFERENCES artifact_versions,
 expected_scope_revision INTEGER NOT NULL CHECK(expected_scope_revision>0), revision INTEGER NOT NULL CHECK(revision>0),
 phase TEXT NOT NULL CHECK(phase IN('Queued','Parsing','Ready','Published','Failed','Cancelled')),
 checkpoint_json TEXT NOT NULL CHECK(json_valid(checkpoint_json)), processed_resources INTEGER NOT NULL CHECK(processed_resources>=0),
 total_resources INTEGER CHECK(total_resources IS NULL OR total_resources>=0), error TEXT,
 created_at INTEGER NOT NULL CHECK(created_at>=0), updated_at INTEGER NOT NULL CHECK(updated_at>=created_at),
 CHECK((project_id IS NULL AND scope_key='workspace') OR(project_id IS NOT NULL AND scope_key='project:'||project_id)),
 CHECK(source_version_id=json_extract(source_ref_json,'$.versionId'))
) STRICT;
CREATE INDEX convention_import_scope ON convention_imports(project_id,operation_id);
CREATE TABLE convention_import_metadata (
 operation_id TEXT PRIMARY KEY REFERENCES convention_imports,
 convention_id TEXT NOT NULL, scope_key TEXT NOT NULL, project_id TEXT REFERENCES projects,
 revision INTEGER NOT NULL CHECK(revision=1), slug TEXT NOT NULL,title TEXT NOT NULL,
 title_folded TEXT NOT NULL,slug_folded TEXT NOT NULL,parent_id TEXT REFERENCES convention_heads,
 expected_parent_revision INTEGER CHECK(expected_parent_revision IS NULL OR expected_parent_revision>0),
 sort_order INTEGER NOT NULL,lifecycle TEXT NOT NULL CHECK(lifecycle IN('Active','Retired','Deleted')),
 content_ref_json TEXT NOT NULL CHECK(json_valid(content_ref_json)),raw_content_ref_json TEXT CHECK(raw_content_ref_json IS NULL OR json_valid(raw_content_ref_json)),
 applicability_json TEXT NOT NULL CHECK(json_valid(applicability_json)),provenance_json TEXT NOT NULL CHECK(json_valid(provenance_json)),
 author_id TEXT NOT NULL REFERENCES actors,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,legacy_approved INTEGER NOT NULL CHECK(legacy_approved IN(0,1)),
 CHECK((parent_id IS NULL)=(expected_parent_revision IS NULL))
) STRICT;
CREATE TABLE convention_import_artifacts (
 operation_id TEXT NOT NULL REFERENCES convention_imports,ordinal INTEGER NOT NULL CHECK(ordinal>=0),
 kind TEXT NOT NULL CHECK(kind IN('Markdown','Raw','Resource')),
 resource_id TEXT UNIQUE,resource_key TEXT,resource_name TEXT,resource_lifecycle TEXT,resource_media_type TEXT,
 artifact_id TEXT NOT NULL UNIQUE,version_id TEXT NOT NULL UNIQUE,
 artifact_json TEXT NOT NULL CHECK(json_valid(artifact_json)),version_json TEXT NOT NULL CHECK(json_valid(version_json)),
 blob_digest TEXT NOT NULL REFERENCES blobs,blob_generation TEXT NOT NULL,byte_count INTEGER NOT NULL CHECK(byte_count>=0),
 staging_operation_id TEXT NOT NULL REFERENCES operations,staging_attempt INTEGER NOT NULL CHECK(staging_attempt>0),
 staging_generation INTEGER,managed_path TEXT NOT NULL,
 PRIMARY KEY(operation_id,ordinal),
 CHECK((kind='Resource')=(resource_id IS NOT NULL)),CHECK((kind='Resource')=(resource_key IS NOT NULL)),
 CHECK((kind='Resource')=(resource_name IS NOT NULL)),CHECK((kind='Resource')=(resource_lifecycle IS NOT NULL)),
 CHECK((kind='Resource')=(resource_media_type IS NOT NULL)),
 CHECK(json_extract(artifact_json,'$.id')=artifact_id AND json_extract(version_json,'$.id')=version_id),
 CHECK(json_extract(version_json,'$.artifact_id')=artifact_id AND json_extract(version_json,'$.blob_digest')=blob_digest)
) STRICT;
CREATE UNIQUE INDEX convention_import_entry_key ON convention_import_artifacts(operation_id,resource_key) WHERE kind='Resource';
CREATE UNIQUE INDEX convention_import_document ON convention_import_artifacts(operation_id,kind) WHERE kind!='Resource';
CREATE INDEX convention_import_blob ON convention_import_artifacts(blob_digest,operation_id);
CREATE TRIGGER convention_import_binding_immutable BEFORE UPDATE ON convention_imports
 WHEN NEW.operation_id!=OLD.operation_id OR NEW.convention_id!=OLD.convention_id OR NEW.scope_key!=OLD.scope_key
 OR NEW.project_id IS NOT OLD.project_id OR NEW.author_id!=OLD.author_id OR NEW.source_ref_json!=OLD.source_ref_json
 OR NEW.expected_scope_revision!=OLD.expected_scope_revision OR NEW.created_at!=OLD.created_at
 BEGIN SELECT RAISE(ABORT,'immutable convention import binding'); END;
CREATE TRIGGER convention_import_metadata_immutable_update BEFORE UPDATE ON convention_import_metadata BEGIN SELECT RAISE(ABORT,'immutable convention import metadata'); END;
CREATE TRIGGER convention_import_metadata_immutable_delete BEFORE DELETE ON convention_import_metadata BEGIN SELECT RAISE(ABORT,'retained convention import metadata'); END;
CREATE TRIGGER convention_import_artifacts_immutable_update BEFORE UPDATE ON convention_import_artifacts BEGIN SELECT RAISE(ABORT,'immutable convention import capture'); END;
CREATE TRIGGER convention_import_artifacts_cleanup_guard BEFORE DELETE ON convention_import_artifacts
 WHEN NOT EXISTS(SELECT 1 FROM convention_imports d JOIN operations o ON o.id=d.operation_id WHERE d.operation_id=OLD.operation_id
 AND d.phase IN('Failed','Cancelled') AND o.state IN('Failed','NeedsInput','CancelledBeforeDispatch','ObservedComplete'))
 BEGIN SELECT RAISE(ABORT,'live convention import capture'); END;
