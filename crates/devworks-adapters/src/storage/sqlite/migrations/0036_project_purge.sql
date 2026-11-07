-- Reviewed project purge intent and exact recovery effects. No source primary-key text.
CREATE TABLE project_purge_manifests (
 id TEXT PRIMARY KEY NOT NULL,
 project_id TEXT NOT NULL REFERENCES projects,
 revision INTEGER NOT NULL CHECK(revision>0),
 phase TEXT NOT NULL CHECK(phase IN('Prepared','RemovingBytes','Held','Complete','Superseded')),
 project_revision INTEGER NOT NULL CHECK(project_revision>0),
 scope_revision INTEGER NOT NULL CHECK(scope_revision>0),
 store_id TEXT NOT NULL,
 restore_epoch INTEGER NOT NULL CHECK(restore_epoch>=0),
 actor_id TEXT NOT NULL REFERENCES actors,
 content_digest TEXT NOT NULL CHECK(length(content_digest)=64),
 policy_digest TEXT NOT NULL CHECK(length(policy_digest)=64),
 witness_digest TEXT NOT NULL CHECK(length(witness_digest)=64),
 body_json TEXT NOT NULL CHECK(json_valid(body_json)),
 created_at INTEGER NOT NULL,
 operation_id TEXT REFERENCES operations,
 CHECK(json_extract(body_json,'$.id')=id),
 CHECK(json_extract(body_json,'$.projectId')=project_id),
 CHECK(CAST(json_extract(body_json,'$.revision') AS INTEGER)=revision),
 CHECK(CAST(json_extract(body_json,'$.projectRevision') AS INTEGER)=project_revision),
 CHECK(json_extract(body_json,'$.phase')=phase),
 CHECK(json_extract(body_json,'$.digest')=content_digest)
) STRICT;
CREATE INDEX project_purge_by_project ON project_purge_manifests(project_id,created_at,id);
CREATE TABLE project_purge_entries (
 manifest_id TEXT NOT NULL REFERENCES project_purge_manifests,
 ordinal INTEGER NOT NULL CHECK(ordinal>=0),
 entry_digest TEXT NOT NULL CHECK(length(entry_digest)=64),
 body_json TEXT NOT NULL CHECK(json_valid(body_json)),
 PRIMARY KEY(manifest_id,entry_digest),
 UNIQUE(manifest_id,ordinal),
 CHECK(json_extract(body_json,'$.id')=entry_digest)
) STRICT;
CREATE TABLE project_purge_rows (
 manifest_id TEXT NOT NULL REFERENCES project_purge_manifests,
 table_name TEXT NOT NULL,
 key_digest TEXT NOT NULL CHECK(length(key_digest)=64),
 row_digest TEXT NOT NULL CHECK(length(row_digest)=64),
 action TEXT NOT NULL CHECK(action IN('Delete','Scrub','Keep')),
 reason TEXT NOT NULL,
 PRIMARY KEY(manifest_id,table_name,key_digest)
) STRICT;
CREATE TABLE project_purge_effects (
 manifest_id TEXT NOT NULL REFERENCES project_purge_manifests,
 id TEXT NOT NULL,
 kind TEXT NOT NULL,
 owner_digest TEXT NOT NULL CHECK(length(owner_digest)=64),
 body_json TEXT CHECK(body_json IS NULL OR json_valid(body_json)),
 byte_count INTEGER NOT NULL CHECK(byte_count>=0),
 state TEXT NOT NULL CHECK(state IN('Pending','Held','Removed','Kept')),
 error_code TEXT,
 PRIMARY KEY(manifest_id,id)
) STRICT;
CREATE TABLE project_purged_subjects (
 table_name TEXT NOT NULL,
 key_digest TEXT NOT NULL CHECK(length(key_digest)=64),
 project_id TEXT NOT NULL REFERENCES projects,
 manifest_id TEXT NOT NULL REFERENCES project_purge_manifests,
 identity_json TEXT NOT NULL CHECK(json_valid(identity_json)),
 PRIMARY KEY(table_name,key_digest)
) STRICT;
-- Durable attribution for host-managed transient files. User inputs/Git work outputs
-- are never registered as deletable managed files here.
CREATE TABLE project_managed_files (
 id TEXT PRIMARY KEY NOT NULL,
 project_id TEXT NOT NULL REFERENCES projects,
 kind TEXT NOT NULL CHECK(kind IN('DiagnosticExport','BrowserUpload','ClientPaste')),
 relative_path TEXT NOT NULL,
 identity_json TEXT NOT NULL CHECK(json_valid(identity_json)),
 byte_count INTEGER NOT NULL CHECK(byte_count>=0),
 state TEXT NOT NULL CHECK(state IN('Live','Removing','Removed')),
 created_at INTEGER NOT NULL
) STRICT;

-- Exact reviewed-row exceptions; all original conditional cleanup predicates remain.

DROP TRIGGER allocation_replays_immutable_delete;
CREATE TRIGGER allocation_replays_immutable_delete BEFORE DELETE ON allocation_replays
 WHEN NOT project_purge_allowed('allocation_replays',json_array(OLD.key),'Delete')
 BEGIN SELECT RAISE(ABORT,'allocation replays are immutable'); END;

DROP TRIGGER convention_chunk_cleanup_guard;
CREATE TRIGGER convention_chunk_cleanup_guard BEFORE DELETE ON convention_chunks
 WHEN NOT project_purge_allowed('convention_chunks',json_array(OLD.generation_id,OLD.id),'Delete') AND (EXISTS(SELECT 1 FROM convention_index_heads WHERE generation_id=OLD.generation_id))
 BEGIN SELECT RAISE(ABORT,'current convention chunks cannot be discarded'); END;

DROP TRIGGER convention_import_artifacts_cleanup_guard;
CREATE TRIGGER convention_import_artifacts_cleanup_guard BEFORE DELETE ON convention_import_artifacts
 WHEN NOT project_purge_allowed('convention_import_artifacts',json_array(OLD.operation_id,OLD.ordinal),'Delete') AND (NOT EXISTS(SELECT 1 FROM convention_imports d JOIN operations o ON o.id=d.operation_id WHERE d.operation_id=OLD.operation_id
 AND d.phase IN('Failed','Cancelled') AND o.state IN('Failed','NeedsInput','CancelledBeforeDispatch','ObservedComplete')))
 BEGIN SELECT RAISE(ABORT,'live convention import capture'); END;

DROP TRIGGER convention_import_metadata_immutable_delete;
CREATE TRIGGER convention_import_metadata_immutable_delete BEFORE DELETE ON convention_import_metadata
 WHEN NOT project_purge_allowed('convention_import_metadata',json_array(OLD.operation_id),'Delete')
 BEGIN SELECT RAISE(ABORT,'retained convention import metadata'); END;

DROP TRIGGER convention_index_immutable_delete;
CREATE TRIGGER convention_index_immutable_delete BEFORE DELETE ON convention_indexes
 WHEN NOT project_purge_allowed('convention_indexes',json_array(OLD.generation_id),'Delete')
 BEGIN SELECT RAISE(ABORT,'retained convention index source'); END;

DROP TRIGGER convention_input_refs_immutable_delete;
CREATE TRIGGER convention_input_refs_immutable_delete BEFORE DELETE ON convention_embedding_input_refs
 WHEN NOT project_purge_allowed('convention_embedding_input_refs',json_array(OLD.request_id,OLD.ordinal),'Delete')
 BEGIN SELECT RAISE(ABORT,'retained convention embedding input ref'); END;

DROP TRIGGER convention_inputs_immutable_delete;
CREATE TRIGGER convention_inputs_immutable_delete BEFORE DELETE ON convention_embedding_inputs
 WHEN NOT project_purge_allowed('convention_embedding_inputs',json_array(OLD.request_id),'Delete')
 BEGIN SELECT RAISE(ABORT,'retained convention embedding inputs'); END;

DROP TRIGGER convention_resource_versions_immutable_delete;
CREATE TRIGGER convention_resource_versions_immutable_delete BEFORE DELETE ON convention_resource_versions
 WHEN NOT project_purge_allowed('convention_resource_versions',json_array(OLD.resource_id,OLD.revision),'Delete')
 BEGIN SELECT RAISE(ABORT,'immutable convention resource revision'); END;

DROP TRIGGER convention_versions_immutable_delete;
CREATE TRIGGER convention_versions_immutable_delete BEFORE DELETE ON convention_versions
 WHEN NOT project_purge_allowed('convention_versions',json_array(OLD.convention_id,OLD.revision),'Delete')
 BEGIN SELECT RAISE(ABORT,'immutable convention revision'); END;

DROP TRIGGER eval_runs_immutable_delete;
CREATE TRIGGER eval_runs_immutable_delete BEFORE DELETE ON eval_runs
 WHEN NOT project_purge_allowed('eval_runs',json_array(OLD.id),'Delete')
 BEGIN SELECT RAISE(ABORT,'eval reports are immutable'); END;

DROP TRIGGER human_decisions_immutable_delete;
CREATE TRIGGER human_decisions_immutable_delete BEFORE DELETE ON human_decisions
 WHEN NOT project_purge_allowed('human_decisions',json_array(OLD.source,OLD.proposal_id),'Delete')
 BEGIN SELECT RAISE(ABORT,'human decisions are immutable'); END;

DROP TRIGGER usage_attempts_immutable_delete;
CREATE TRIGGER usage_attempts_immutable_delete BEFORE DELETE ON usage_attempts
 WHEN NOT project_purge_allowed('usage_attempts',json_array(OLD.attempt_id),'Delete')
 BEGIN SELECT RAISE(ABORT,'usage attempts are retained'); END;

DROP TRIGGER usage_changes_immutable_delete;
CREATE TRIGGER usage_changes_immutable_delete BEFORE DELETE ON usage_changes
 WHEN NOT project_purge_allowed('usage_changes',json_array(OLD.event_id),'Delete')
 BEGIN SELECT RAISE(ABORT,'usage changes are immutable'); END;

DROP TRIGGER usage_observations_immutable_delete;
CREATE TRIGGER usage_observations_immutable_delete BEFORE DELETE ON usage_observations
 WHEN NOT project_purge_allowed('usage_observations',json_array(OLD.attempt_id,OLD.observation_key),'Delete')
 BEGIN SELECT RAISE(ABORT,'usage observations are immutable'); END;

DROP TRIGGER usage_origins_immutable_delete;
CREATE TRIGGER usage_origins_immutable_delete BEFORE DELETE ON usage_origins
 WHEN NOT project_purge_allowed('usage_origins',json_array(OLD.source_kind,OLD.source_id),'Delete')
 BEGIN SELECT RAISE(ABORT,'usage origins are immutable'); END;
