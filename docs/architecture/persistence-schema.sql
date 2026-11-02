-- DevWorks initial persistence schema, architecture draft 0.2, 2026-11-01.
-- Design fixture, NOT a production migration or an implemented Rust adapter.
-- Fixed relational core; versioned JSON holds validated subsystem payloads.
-- All IDs are opaque app IDs. Times are UTC milliseconds. No secrets belong here.
PRAGMA foreign_keys = ON;
BEGIN IMMEDIATE;
CREATE TABLE store_meta (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), store_id TEXT NOT NULL UNIQUE,
 schema_version INTEGER NOT NULL, minimum_reader INTEGER NOT NULL,
 minimum_writer INTEGER NOT NULL, restore_epoch INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE TABLE schema_migrations (
 version INTEGER PRIMARY KEY, checksum TEXT NOT NULL, applied_at INTEGER NOT NULL,
 app_build TEXT NOT NULL
) STRICT;
CREATE TABLE app_boots (
 id TEXT PRIMARY KEY, started_at INTEGER NOT NULL, stopped_at INTEGER,
 shutdown_state TEXT NOT NULL
) STRICT;
CREATE TABLE actors (
 id TEXT PRIMARY KEY, kind TEXT NOT NULL, display_name TEXT NOT NULL
) STRICT;
CREATE TABLE projects (
 id TEXT PRIMARY KEY, title TEXT NOT NULL, lifecycle TEXT NOT NULL,
 revision INTEGER NOT NULL CHECK(revision>0), scope_revision INTEGER NOT NULL,
 created_at INTEGER NOT NULL
) STRICT;
CREATE TABLE repositories (
 id TEXT PRIMARY KEY, canonical_location TEXT NOT NULL UNIQUE,
 revision INTEGER NOT NULL CHECK(revision>0)
) STRICT;
CREATE TABLE project_repositories (
 project_id TEXT NOT NULL REFERENCES projects, repository_id TEXT NOT NULL REFERENCES repositories,
 PRIMARY KEY(project_id,repository_id)
) STRICT;
CREATE TABLE workflow_statuses (
 id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects,
 name TEXT NOT NULL, semantic TEXT NOT NULL, sort_order INTEGER NOT NULL,
 revision INTEGER NOT NULL, UNIQUE(id,project_id)
) STRICT;
CREATE TABLE work_items (
 id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects,
 kind TEXT NOT NULL CHECK(kind IN ('feature','issue','task')),
 -- parent_feature_id is authoritative for both issue/task views; legacy association stays NULL.
 parent_feature_id TEXT, associated_feature_id TEXT, status_id TEXT,
 title TEXT NOT NULL, priority INTEGER NOT NULL, manual_order INTEGER NOT NULL,
 revision INTEGER NOT NULL CHECK(revision>0), scope_revision INTEGER NOT NULL,
 created_at INTEGER NOT NULL, brief_json TEXT NOT NULL CHECK(json_valid(brief_json)),
 UNIQUE(id,project_id),
 -- Project tasks may be standalone; assignment parentage is stored separately.
 CHECK(kind<>'feature' OR parent_feature_id IS NULL),
 CHECK(associated_feature_id IS NULL),
 FOREIGN KEY(parent_feature_id,project_id) REFERENCES work_items(id,project_id),
 FOREIGN KEY(associated_feature_id,project_id) REFERENCES work_items(id,project_id),
 FOREIGN KEY(status_id,project_id) REFERENCES workflow_statuses(id,project_id)
) STRICT;
CREATE INDEX work_queue ON work_items(project_id,priority DESC,manual_order,created_at,id);
CREATE INDEX feature_tasks ON work_items(parent_feature_id,priority DESC,manual_order,id);
CREATE TABLE work_repositories (
 work_id TEXT NOT NULL REFERENCES work_items, repository_id TEXT NOT NULL REFERENCES repositories,
 PRIMARY KEY(work_id,repository_id)
) STRICT;
CREATE TABLE work_dependencies (
 work_id TEXT NOT NULL REFERENCES work_items, prerequisite_id TEXT NOT NULL REFERENCES work_items,
 condition_kind TEXT NOT NULL, PRIMARY KEY(work_id,prerequisite_id,condition_kind),
 CHECK(work_id<>prerequisite_id)
) STRICT;
CREATE INDEX dependency_reverse ON work_dependencies(prerequisite_id,work_id);
-- One feature-owned plan state; ordinary events retain change history.
-- Revision tokens guard concurrent edits/approval, not a user-facing version catalog.
CREATE TABLE feature_plans (
 feature_id TEXT PRIMARY KEY REFERENCES work_items,
 revision INTEGER NOT NULL CHECK(revision>0),
 approved_plan_json TEXT CHECK(approved_plan_json IS NULL OR json_valid(approved_plan_json)),
 approved_by TEXT REFERENCES actors, approved_at INTEGER,
 pending_plan_json TEXT CHECK(pending_plan_json IS NULL OR json_valid(pending_plan_json)),
 pending_revision INTEGER CHECK(pending_revision IS NULL OR pending_revision>0),
 pending_proposal_id TEXT REFERENCES approvals,
 CHECK((approved_plan_json IS NULL)=(approved_by IS NULL)),
 CHECK((approved_plan_json IS NULL)=(approved_at IS NULL)),
 CHECK((pending_plan_json IS NULL)=(pending_revision IS NULL)),
 CHECK(pending_proposal_id IS NULL OR pending_plan_json IS NOT NULL)
) STRICT;
CREATE TABLE root_controls (
 root_id TEXT PRIMARY KEY REFERENCES work_items,
 desired_state TEXT NOT NULL CHECK(desired_state IN ('Enabled','Paused')),
 revision INTEGER NOT NULL CHECK(revision>0), current_generation INTEGER NOT NULL,
 restart_boot_id TEXT REFERENCES app_boots
) STRICT;
CREATE TABLE conversations (
 id TEXT PRIMARY KEY, root_id TEXT REFERENCES work_items, created_at INTEGER NOT NULL
) STRICT;
CREATE TABLE sessions (
 id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL REFERENCES conversations,
 agent_id TEXT NOT NULL REFERENCES actors, root_id TEXT REFERENCES work_items,
 context_epoch INTEGER NOT NULL, revision INTEGER NOT NULL, pin_json TEXT CHECK(pin_json IS NULL OR json_valid(pin_json))
) STRICT;
CREATE INDEX sessions_root ON sessions(root_id,id);
CREATE TABLE executions (
 id TEXT PRIMARY KEY, root_id TEXT NOT NULL REFERENCES work_items,
 boot_id TEXT NOT NULL REFERENCES app_boots, generation INTEGER NOT NULL,
 phase TEXT NOT NULL CHECK(phase IN ('Pending','Claimed','Running','Stopping','Stopped')),
 owner_id TEXT, lease_until INTEGER, revision INTEGER NOT NULL,
 UNIQUE(root_id,generation), CHECK((owner_id IS NULL)=(lease_until IS NULL))
) STRICT;
CREATE UNIQUE INDEX one_root_activation ON executions(root_id)
 WHERE phase IN ('Pending','Claimed','Running','Stopping');
CREATE TABLE assignments (
 id TEXT PRIMARY KEY, root_id TEXT NOT NULL REFERENCES work_items,
 work_id TEXT NOT NULL REFERENCES work_items, parent_id TEXT REFERENCES assignments,
 session_id TEXT NOT NULL REFERENCES sessions, execution_id TEXT REFERENCES executions,
 state TEXT NOT NULL, generation INTEGER NOT NULL, slot_held INTEGER NOT NULL CHECK(slot_held IN (0,1)),
 revision INTEGER NOT NULL, checkpoint_json TEXT CHECK(checkpoint_json IS NULL OR json_valid(checkpoint_json))
) STRICT;
CREATE UNIQUE INDEX one_live_assignment ON assignments(work_id)
 WHERE state IN ('Ready','Active','Waiting');
CREATE INDEX assignment_root_state ON assignments(root_id,state,slot_held,id);
CREATE INDEX assignment_parent ON assignments(parent_id,id);
CREATE TABLE operations (
 id TEXT PRIMARY KEY, root_id TEXT REFERENCES work_items,
 assignment_id TEXT REFERENCES assignments, capability TEXT NOT NULL,
 schema_version INTEGER NOT NULL, args_digest TEXT NOT NULL,
 payload_json TEXT NOT NULL CHECK(json_valid(payload_json)), state TEXT NOT NULL,
 revision INTEGER NOT NULL, created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX operations_root_state ON operations(root_id,state,id);
CREATE TABLE operation_attempts (
 id TEXT PRIMARY KEY, operation_id TEXT NOT NULL REFERENCES operations,
 attempt_no INTEGER NOT NULL, boot_id TEXT NOT NULL REFERENCES app_boots,
 execution_id TEXT REFERENCES executions, owner_generation INTEGER,
 control_revision INTEGER, policy_revision INTEGER, state TEXT NOT NULL,
 observations_json TEXT NOT NULL CHECK(json_valid(observations_json)),
 UNIQUE(operation_id,attempt_no)
) STRICT;
CREATE INDEX unresolved_attempts ON operation_attempts(state,operation_id);
CREATE TABLE blockers (
 id TEXT PRIMARY KEY, root_id TEXT REFERENCES work_items,
 assignment_id TEXT REFERENCES assignments, operation_id TEXT REFERENCES operations,
 cause TEXT NOT NULL, origin_id TEXT NOT NULL, origin_revision INTEGER NOT NULL,
 state TEXT NOT NULL, revision INTEGER NOT NULL, clearance_json TEXT NOT NULL CHECK(json_valid(clearance_json))
) STRICT;
CREATE UNIQUE INDEX blocker_origin ON blockers(ifnull(root_id,''),ifnull(assignment_id,''),
 ifnull(operation_id,''),cause,origin_id,origin_revision) WHERE state='Active';
CREATE INDEX blocker_root ON blockers(root_id,state,cause);
CREATE TABLE approvals (
 id TEXT PRIMARY KEY, operation_id TEXT NOT NULL REFERENCES operations,
 proposal_revision INTEGER NOT NULL, binding_digest TEXT NOT NULL,
 state TEXT NOT NULL, decided_by TEXT REFERENCES actors, decided_at INTEGER,
 consumed_attempt_id TEXT REFERENCES operation_attempts,
 UNIQUE(operation_id,proposal_revision)
) STRICT;
CREATE UNIQUE INDEX approval_attempt ON approvals(consumed_attempt_id) WHERE consumed_attempt_id IS NOT NULL;
CREATE TABLE resume_grants (
 id TEXT PRIMARY KEY, actor_id TEXT NOT NULL REFERENCES actors, root_id TEXT NOT NULL REFERENCES work_items,
 revision INTEGER NOT NULL, state TEXT NOT NULL, authority_source TEXT NOT NULL,
 binding_json TEXT NOT NULL CHECK(json_valid(binding_json)), expires_at INTEGER, uses_remaining INTEGER
) STRICT;
CREATE TABLE command_receipts (
 actor_id TEXT NOT NULL REFERENCES actors, scope_key TEXT NOT NULL, idempotency_key TEXT NOT NULL,
 command_kind TEXT NOT NULL, payload_digest TEXT NOT NULL,
 operation_id TEXT REFERENCES operations, result_json TEXT NOT NULL CHECK(json_valid(result_json)),
 created_at INTEGER NOT NULL, PRIMARY KEY(actor_id,scope_key,idempotency_key)
) STRICT;
CREATE TABLE events (
 seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE,
 actor_id TEXT NOT NULL REFERENCES actors, project_id TEXT REFERENCES projects,
 root_id TEXT REFERENCES work_items, operation_id TEXT REFERENCES operations,
 aggregate_kind TEXT NOT NULL, aggregate_id TEXT NOT NULL, aggregate_revision INTEGER NOT NULL,
 kind TEXT NOT NULL, created_at INTEGER NOT NULL,
 payload_json TEXT NOT NULL CHECK(json_valid(payload_json))
) STRICT;
CREATE INDEX event_work ON events(root_id,seq);
CREATE INDEX event_aggregate ON events(aggregate_kind,aggregate_id,seq);
CREATE TABLE queue_entries (
 id TEXT PRIMARY KEY, consumer TEXT NOT NULL, event_id TEXT NOT NULL REFERENCES events(id),
 root_id TEXT REFERENCES work_items, operation_id TEXT REFERENCES operations,
 priority INTEGER NOT NULL, ready_at INTEGER NOT NULL, state TEXT NOT NULL,
 owner_id TEXT, lease_until INTEGER, generation INTEGER NOT NULL DEFAULT 0,
 cancellation_epoch INTEGER NOT NULL DEFAULT 0, attempt_count INTEGER NOT NULL DEFAULT 0,
 payload_json TEXT NOT NULL CHECK(json_valid(payload_json)),
 UNIQUE(consumer,event_id), CHECK((owner_id IS NULL)=(lease_until IS NULL))
) STRICT;
CREATE INDEX queue_ready ON queue_entries(consumer,priority DESC,ready_at,id) WHERE state='Ready';
CREATE INDEX queue_expiry ON queue_entries(lease_until,id) WHERE state='Claimed';
CREATE INDEX queue_root ON queue_entries(root_id,state);
CREATE TABLE consumer_progress (
 consumer TEXT NOT NULL, partition_key TEXT NOT NULL, applied_seq INTEGER NOT NULL,
 revision INTEGER NOT NULL, PRIMARY KEY(consumer,partition_key)
) STRICT;
CREATE TABLE provider_connections (
 id TEXT PRIMARY KEY, kind TEXT NOT NULL, access_mode TEXT NOT NULL,
 endpoint_profile TEXT NOT NULL, revision INTEGER NOT NULL,
 credential_ref TEXT, credential_revision INTEGER NOT NULL, state TEXT NOT NULL,
 configuration_json TEXT NOT NULL CHECK(json_valid(configuration_json))
) STRICT;
CREATE TABLE provider_models (
 id TEXT PRIMARY KEY, connection_id TEXT NOT NULL REFERENCES provider_connections,
 upstream_id TEXT NOT NULL, revision INTEGER NOT NULL, enabled INTEGER NOT NULL CHECK(enabled IN (0,1)),
 capabilities_json TEXT NOT NULL CHECK(json_valid(capabilities_json)), UNIQUE(connection_id,upstream_id)
) STRICT;
CREATE TABLE routing_cursors (
 pool_id TEXT PRIMARY KEY, next_ordinal INTEGER NOT NULL, revision INTEGER NOT NULL
) STRICT;
CREATE TABLE router_availability (
 scope_key TEXT PRIMARY KEY, revision INTEGER NOT NULL, ready_at INTEGER,
 state TEXT NOT NULL, observation_json TEXT NOT NULL CHECK(json_valid(observation_json))
) STRICT;
CREATE TABLE price_snapshots (
 id TEXT PRIMARY KEY, provider_key TEXT NOT NULL, source_kind TEXT NOT NULL,
 source_url TEXT, source_digest TEXT NOT NULL, parser_version TEXT NOT NULL,
 fetched_at INTEGER NOT NULL, verified_at INTEGER NOT NULL,
 rates_json TEXT NOT NULL CHECK(json_valid(rates_json)), UNIQUE(provider_key,id)
) STRICT;
CREATE TABLE published_prices (
 provider_key TEXT PRIMARY KEY, snapshot_id TEXT NOT NULL, revision INTEGER NOT NULL,
 FOREIGN KEY(provider_key,snapshot_id) REFERENCES price_snapshots(provider_key,id)
) STRICT;
CREATE TABLE price_overrides (
 model_id TEXT PRIMARY KEY REFERENCES provider_models, revision INTEGER NOT NULL,
 rates_json TEXT NOT NULL CHECK(json_valid(rates_json))
) STRICT;
CREATE TABLE inference_requests (
 id TEXT PRIMARY KEY, purpose TEXT NOT NULL, project_id TEXT REFERENCES projects,
 root_id TEXT REFERENCES work_items, assignment_id TEXT REFERENCES assignments,
 session_id TEXT REFERENCES sessions, context_epoch INTEGER,
 state TEXT NOT NULL, revision INTEGER NOT NULL,
 request_json TEXT NOT NULL CHECK(json_valid(request_json))
) STRICT;
CREATE INDEX inference_work ON inference_requests(root_id,purpose,id);
CREATE TABLE provider_attempts (
 id TEXT PRIMARY KEY, request_id TEXT NOT NULL REFERENCES inference_requests,
 session_id TEXT REFERENCES sessions, turn_no INTEGER, attempt_no INTEGER NOT NULL, context_epoch INTEGER,
 model_id TEXT NOT NULL REFERENCES provider_models, price_snapshot_id TEXT REFERENCES price_snapshots,
 state TEXT NOT NULL, envelope_json TEXT NOT NULL CHECK(json_valid(envelope_json)),
 UNIQUE(request_id,attempt_no), UNIQUE(session_id,turn_no,attempt_no)
) STRICT;
CREATE TABLE transcript_chunks (
 session_id TEXT NOT NULL REFERENCES sessions, seq INTEGER NOT NULL,
 provider_attempt_id TEXT REFERENCES provider_attempts, kind TEXT NOT NULL,
 payload_json TEXT NOT NULL CHECK(json_valid(payload_json)),
 PRIMARY KEY(session_id,seq)
) STRICT;
CREATE TABLE tool_calls (
 id TEXT PRIMARY KEY, provider_attempt_id TEXT NOT NULL REFERENCES provider_attempts,
 native_call_id TEXT NOT NULL, ordinal INTEGER NOT NULL,
 operation_id TEXT NOT NULL REFERENCES operations, envelope_json TEXT NOT NULL CHECK(json_valid(envelope_json)),
 response_json TEXT CHECK(response_json IS NULL OR json_valid(response_json)),
 UNIQUE(provider_attempt_id,native_call_id), UNIQUE(provider_attempt_id,ordinal)
) STRICT;
CREATE TABLE session_events (
 session_id TEXT NOT NULL REFERENCES sessions, event_id TEXT NOT NULL REFERENCES events(id),
 consumed_epoch INTEGER, PRIMARY KEY(session_id,event_id)
) STRICT;
CREATE TABLE schedules (
 id TEXT PRIMARY KEY, project_id TEXT REFERENCES projects, enabled INTEGER NOT NULL CHECK(enabled IN (0,1)),
 revision INTEGER NOT NULL, timezone TEXT NOT NULL, next_at INTEGER,
 definition_json TEXT NOT NULL CHECK(json_valid(definition_json))
) STRICT;
CREATE INDEX schedule_next ON schedules(next_at,id) WHERE enabled=1;
CREATE TABLE job_runs (
 id TEXT PRIMARY KEY, schedule_id TEXT NOT NULL REFERENCES schedules, occurrence_key TEXT NOT NULL,
 state TEXT NOT NULL, revision INTEGER NOT NULL, checkpoint_json TEXT CHECK(checkpoint_json IS NULL OR json_valid(checkpoint_json)),
 UNIQUE(schedule_id,occurrence_key)
) STRICT;
CREATE TABLE resources (
 id TEXT PRIMARY KEY, canonical_identity TEXT NOT NULL UNIQUE, capacity INTEGER NOT NULL CHECK(capacity>0), revision INTEGER NOT NULL
) STRICT;
CREATE TABLE resource_grants (
 id TEXT PRIMARY KEY, resource_id TEXT NOT NULL REFERENCES resources,
 operation_id TEXT NOT NULL REFERENCES operations, generation INTEGER NOT NULL,
 units INTEGER NOT NULL CHECK(units>0), state TEXT NOT NULL, lease_until INTEGER,
 observation_json TEXT NOT NULL CHECK(json_valid(observation_json))
) STRICT;
CREATE INDEX resource_occupancy ON resource_grants(resource_id,state);
CREATE TABLE configuration_revisions (
 scope_key TEXT NOT NULL, namespace TEXT NOT NULL, revision INTEGER NOT NULL,
 schema_version INTEGER NOT NULL, body_json TEXT NOT NULL CHECK(json_valid(body_json)),
 PRIMARY KEY(scope_key,namespace,revision)
) STRICT;
CREATE TABLE effective_configuration (
 scope_key TEXT NOT NULL, namespace TEXT NOT NULL, revision INTEGER NOT NULL,
 PRIMARY KEY(scope_key,namespace), FOREIGN KEY(scope_key,namespace,revision)
 REFERENCES configuration_revisions(scope_key,namespace,revision)
) STRICT;
CREATE TABLE usage_entries (
 id TEXT PRIMARY KEY, attempt_id TEXT NOT NULL REFERENCES provider_attempts,
 observation_key TEXT NOT NULL, kind TEXT NOT NULL, cost_units INTEGER,
 currency TEXT, quantities_json TEXT NOT NULL CHECK(json_valid(quantities_json)),
 UNIQUE(attempt_id,observation_key)
) STRICT;
CREATE TABLE attempt_costs (
 attempt_id TEXT PRIMARY KEY REFERENCES provider_attempts, classification TEXT NOT NULL,
 amount_units INTEGER, currency TEXT, price_snapshot_id TEXT REFERENCES price_snapshots,
 completeness TEXT NOT NULL, revision INTEGER NOT NULL,
 CHECK(amount_units IS NULL OR amount_units>=0)
) STRICT;
CREATE TABLE budget_reservations (
 id TEXT PRIMARY KEY, root_id TEXT NOT NULL REFERENCES work_items,
 attempt_id TEXT NOT NULL UNIQUE REFERENCES provider_attempts,
 state TEXT NOT NULL, reserved_units INTEGER NOT NULL CHECK(reserved_units>=0), revision INTEGER NOT NULL
) STRICT;
CREATE TABLE artifacts (
 id TEXT PRIMARY KEY, origin_project_id TEXT NOT NULL REFERENCES projects,
 origin_work_id TEXT REFERENCES work_items, title TEXT NOT NULL, revision INTEGER NOT NULL,
 current_project_id TEXT NOT NULL REFERENCES projects, scope_revision INTEGER NOT NULL
) STRICT;
CREATE TABLE blobs (
 digest TEXT PRIMARY KEY, byte_count INTEGER NOT NULL CHECK(byte_count>=0),
 relative_path TEXT NOT NULL UNIQUE, state TEXT NOT NULL
) STRICT;
CREATE TABLE artifact_versions (
 id TEXT PRIMARY KEY, artifact_id TEXT NOT NULL REFERENCES artifacts,
 version_no INTEGER NOT NULL, blob_digest TEXT NOT NULL REFERENCES blobs,
 media_type TEXT NOT NULL, created_at INTEGER NOT NULL, deleted_at INTEGER,
 source_json TEXT NOT NULL CHECK(json_valid(source_json)), UNIQUE(artifact_id,version_no)
) STRICT;
CREATE TABLE artifact_tags (
 artifact_id TEXT NOT NULL REFERENCES artifacts, tag TEXT NOT NULL,
 PRIMARY KEY(artifact_id,tag)
) STRICT;
CREATE INDEX artifact_tag_lookup ON artifact_tags(tag,artifact_id);
CREATE TABLE artifact_shares (
 artifact_id TEXT NOT NULL REFERENCES artifacts, project_id TEXT NOT NULL REFERENCES projects,
 PRIMARY KEY(artifact_id,project_id)
) STRICT;
CREATE TABLE artifact_links (
 artifact_version_id TEXT NOT NULL REFERENCES artifact_versions, subject_kind TEXT NOT NULL,
 subject_id TEXT NOT NULL, purpose TEXT NOT NULL,
 PRIMARY KEY(artifact_version_id,subject_kind,subject_id,purpose)
) STRICT;
CREATE TABLE documents (
 artifact_id TEXT PRIMARY KEY REFERENCES artifacts,
 active_version_id TEXT NOT NULL REFERENCES artifact_versions,
 revision INTEGER NOT NULL, retrieval_enabled INTEGER NOT NULL CHECK(retrieval_enabled IN (0,1))
) STRICT;
CREATE TABLE extractions (
 id TEXT PRIMARY KEY, artifact_version_id TEXT NOT NULL REFERENCES artifact_versions,
 extractor_digest TEXT NOT NULL, state TEXT NOT NULL, extracted_blob_digest TEXT REFERENCES blobs,
 provenance_json TEXT NOT NULL CHECK(json_valid(provenance_json)),
 UNIQUE(artifact_version_id,extractor_digest)
) STRICT;
CREATE TABLE knowledge_entries (
 id TEXT PRIMARY KEY, project_id TEXT REFERENCES projects, kind TEXT NOT NULL,
 scope_kind TEXT NOT NULL CHECK(scope_kind IN ('Project','Workspace')),
 revision INTEGER NOT NULL, state TEXT NOT NULL,
 body_json TEXT NOT NULL CHECK(json_valid(body_json)), scope_revision INTEGER NOT NULL
) STRICT;
CREATE TABLE knowledge_links (
 from_id TEXT NOT NULL REFERENCES knowledge_entries, to_id TEXT NOT NULL REFERENCES knowledge_entries,
 kind TEXT NOT NULL, evidence_json TEXT NOT NULL CHECK(json_valid(evidence_json)),
 PRIMARY KEY(from_id,to_id,kind)
) STRICT;
CREATE TABLE knowledge_revisions (
 entry_id TEXT NOT NULL REFERENCES knowledge_entries, revision INTEGER NOT NULL,
 actor_id TEXT NOT NULL REFERENCES actors, body_json TEXT NOT NULL CHECK(json_valid(body_json)),
 PRIMARY KEY(entry_id,revision)
) STRICT;
CREATE TABLE knowledge_tags (
 entry_id TEXT NOT NULL REFERENCES knowledge_entries, tag TEXT NOT NULL,
 PRIMARY KEY(entry_id,tag)
) STRICT;
CREATE INDEX knowledge_tag_lookup ON knowledge_tags(tag,entry_id);
CREATE TABLE knowledge_shares (
 entry_id TEXT NOT NULL REFERENCES knowledge_entries, project_id TEXT NOT NULL REFERENCES projects,
 PRIMARY KEY(entry_id,project_id)
) STRICT;
CREATE TABLE index_generations (
 id TEXT PRIMARY KEY, source_key TEXT NOT NULL, kind TEXT NOT NULL,
 state TEXT NOT NULL, source_digest TEXT NOT NULL, watermark INTEGER NOT NULL,
 manifest_json TEXT NOT NULL CHECK(json_valid(manifest_json))
) STRICT;
CREATE TABLE source_units (
 generation_id TEXT NOT NULL REFERENCES index_generations, unit_key TEXT NOT NULL,
 content_digest TEXT NOT NULL, action TEXT NOT NULL CHECK(action IN ('Replace','Delete')),
 PRIMARY KEY(generation_id,unit_key)
) STRICT;
CREATE TABLE index_batches (
 generation_id TEXT NOT NULL, unit_key TEXT NOT NULL, batch_key TEXT NOT NULL,
 payload_digest TEXT NOT NULL, row_count INTEGER NOT NULL CHECK(row_count>=0),
 PRIMARY KEY(generation_id,unit_key,batch_key),
 FOREIGN KEY(generation_id,unit_key) REFERENCES source_units
) STRICT;
CREATE TABLE graph_nodes (
 generation_id TEXT NOT NULL, unit_key TEXT NOT NULL, node_id TEXT NOT NULL,
 kind TEXT NOT NULL, name TEXT NOT NULL, payload_json TEXT NOT NULL CHECK(json_valid(payload_json)),
 PRIMARY KEY(generation_id,unit_key,node_id),
 FOREIGN KEY(generation_id,unit_key) REFERENCES source_units
) STRICT;
CREATE INDEX graph_node_name ON graph_nodes(generation_id,kind,name,node_id);
CREATE TABLE graph_edges (
 generation_id TEXT NOT NULL, unit_key TEXT NOT NULL, edge_id TEXT NOT NULL,
 source_node_id TEXT NOT NULL, target_node_id TEXT NOT NULL, kind TEXT NOT NULL,
 PRIMARY KEY(generation_id,unit_key,edge_id),
 FOREIGN KEY(generation_id,unit_key) REFERENCES source_units
) STRICT;
CREATE INDEX graph_forward ON graph_edges(generation_id,source_node_id,kind,target_node_id);
CREATE INDEX graph_reverse ON graph_edges(generation_id,target_node_id,kind,source_node_id);
CREATE TABLE index_views (
 id TEXT PRIMARY KEY, source_key TEXT NOT NULL, view_digest TEXT NOT NULL UNIQUE,
 baseline_id TEXT NOT NULL REFERENCES index_generations,
 branch_id TEXT REFERENCES index_generations, worktree_id TEXT REFERENCES index_generations,
 coverage_json TEXT NOT NULL CHECK(json_valid(coverage_json)), UNIQUE(source_key,id)
) STRICT;
CREATE TABLE published_views (
 source_key TEXT PRIMARY KEY, view_id TEXT NOT NULL,
 revision INTEGER NOT NULL, FOREIGN KEY(source_key,view_id) REFERENCES index_views(source_key,id)
) STRICT;
CREATE TABLE index_pins (
 id TEXT PRIMARY KEY, view_id TEXT NOT NULL REFERENCES index_views,
 boot_id TEXT NOT NULL REFERENCES app_boots, lease_until INTEGER NOT NULL
) STRICT;
CREATE TABLE embedding_spaces (
 id TEXT PRIMARY KEY, model_digest TEXT NOT NULL, dimensions INTEGER NOT NULL CHECK(dimensions>0),
 metric TEXT NOT NULL, element_type TEXT NOT NULL, UNIQUE(id,dimensions)
) STRICT;
CREATE TABLE search_chunks (
 id TEXT PRIMARY KEY, generation_id TEXT NOT NULL, unit_key TEXT NOT NULL,
 source_version_id TEXT REFERENCES artifact_versions, locator_json TEXT NOT NULL CHECK(json_valid(locator_json)),
 text TEXT NOT NULL, content_digest TEXT NOT NULL,
 FOREIGN KEY(generation_id,unit_key) REFERENCES source_units
) STRICT;
CREATE INDEX chunks_source ON search_chunks(generation_id,unit_key,id);
CREATE TABLE chunk_vectors (
 chunk_id TEXT NOT NULL REFERENCES search_chunks, space_id TEXT NOT NULL,
 dimensions INTEGER NOT NULL, vector BLOB NOT NULL CHECK(length(vector)=dimensions*4),
 PRIMARY KEY(space_id,chunk_id), FOREIGN KEY(space_id,dimensions) REFERENCES embedding_spaces(id,dimensions)
) STRICT;
CREATE VIRTUAL TABLE search_fts USING fts5(chunk_id UNINDEXED, text, tokenize='unicode61');
-- FTS is an adapter-maintained derived table; row visibility is governed by published
-- generations and current eligibility, never by an FTS match alone.
CREATE TABLE integration_records (
 id TEXT PRIMARY KEY, project_id TEXT REFERENCES projects, repository_id TEXT REFERENCES repositories,
 root_id TEXT REFERENCES work_items, kind TEXT NOT NULL, external_key TEXT,
 revision INTEGER NOT NULL, state TEXT NOT NULL,
 body_json TEXT NOT NULL CHECK(json_valid(body_json)), UNIQUE(kind,external_key)
) STRICT;
CREATE INDEX integrations_work ON integration_records(root_id,kind,id);
CREATE INDEX integrations_project_state ON integration_records(project_id,kind,state,id);
CREATE TABLE integration_links (
 record_id TEXT NOT NULL REFERENCES integration_records, work_id TEXT NOT NULL REFERENCES work_items,
 purpose TEXT NOT NULL, PRIMARY KEY(record_id,work_id,purpose)
) STRICT;
CREATE INDEX integration_work_lookup ON integration_links(work_id,purpose,record_id);
CREATE TABLE external_cursors (
 source_id TEXT NOT NULL, consumer TEXT NOT NULL, revision INTEGER NOT NULL,
 cursor_json TEXT NOT NULL CHECK(json_valid(cursor_json)), PRIMARY KEY(source_id,consumer)
) STRICT;
CREATE TABLE import_mappings (
 import_id TEXT NOT NULL, source_kind TEXT NOT NULL, source_id TEXT NOT NULL,
 local_id TEXT NOT NULL, source_digest TEXT NOT NULL,
 PRIMARY KEY(import_id,source_kind,source_id)
) STRICT;
CREATE TABLE maintenance_operations (
 id TEXT PRIMARY KEY, kind TEXT NOT NULL, state TEXT NOT NULL,
 checkpoint_json TEXT NOT NULL CHECK(json_valid(checkpoint_json)), created_at INTEGER NOT NULL
) STRICT;

-- Local incoming capture can be unrouted without inventing a projectless task.
CREATE TABLE github_issue_intake (
 repository_id TEXT NOT NULL REFERENCES repositories, issue_number INTEGER NOT NULL CHECK(issue_number>0),
 revision INTEGER NOT NULL CHECK(revision>0), state TEXT NOT NULL,
 project_id TEXT REFERENCES projects, work_id TEXT REFERENCES work_items,
 remote_json TEXT NOT NULL CHECK(json_valid(remote_json)), operation_id TEXT REFERENCES operations,
 PRIMARY KEY(repository_id,issue_number)
) STRICT;
CREATE TABLE github_intake_clock (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), revision INTEGER NOT NULL CHECK(revision>0),
 interval_seconds INTEGER NOT NULL DEFAULT 1800 CHECK(interval_seconds>0),
 last_admitted_at INTEGER, next_admission_at INTEGER NOT NULL
) STRICT;
CREATE TABLE package_handler_commits (
 package_id TEXT NOT NULL, subscription_id TEXT NOT NULL, event_id TEXT NOT NULL REFERENCES events(id),
 receipt_id TEXT NOT NULL UNIQUE, payload_digest TEXT NOT NULL,
 state_generation INTEGER NOT NULL, state_revision INTEGER NOT NULL,
 checkpoint INTEGER NOT NULL, intents_json TEXT NOT NULL CHECK(json_valid(intents_json)),
 PRIMARY KEY(package_id,subscription_id,event_id)
) STRICT;
CREATE TABLE package_handler_state (
 package_id TEXT PRIMARY KEY, generation INTEGER NOT NULL, revision INTEGER NOT NULL,
 state_json TEXT NOT NULL CHECK(json_valid(state_json))
) STRICT;
CREATE TABLE review_source_evidence (
 repository_id TEXT NOT NULL REFERENCES repositories, pull_request_number INTEGER NOT NULL,
 head TEXT NOT NULL, policy_revision INTEGER NOT NULL, cycle INTEGER NOT NULL,
 source_id TEXT NOT NULL, run_id TEXT NOT NULL, state TEXT NOT NULL,
 fallback_operation_id TEXT REFERENCES operations,
 evidence_json TEXT NOT NULL CHECK(json_valid(evidence_json)),
 PRIMARY KEY(repository_id,pull_request_number,head,policy_revision,cycle,source_id,run_id)
) STRICT;
CREATE UNIQUE INDEX one_review_fallback ON review_source_evidence(
 repository_id,pull_request_number,head,policy_revision,cycle)
 WHERE fallback_operation_id IS NOT NULL;

COMMIT;
