-- Drop the tables of migration 1 that no build ever wrote, and index the operations read by
-- capability. Each dropped table has no INSERT, UPDATE or REPLACE in any build and no repository
-- method; later migrations replaced their purpose (knowledge in 0018, router connections in
-- 0015, usage in 0028, integration records for GitHub intake). The migration refuses to run if
-- any of them holds a row, so it never discards data.
CREATE TEMP TABLE migration_52_guard (
 row_count INTEGER NOT NULL CONSTRAINT unused_tables_are_empty CHECK(row_count=0)
);
INSERT INTO migration_52_guard SELECT
 (SELECT count(*) FROM approvals) + (SELECT count(*) FROM provider_connections)
 + (SELECT count(*) FROM provider_models) + (SELECT count(*) FROM routing_cursors)
 + (SELECT count(*) FROM router_availability) + (SELECT count(*) FROM price_snapshots)
 + (SELECT count(*) FROM published_prices) + (SELECT count(*) FROM price_overrides)
 + (SELECT count(*) FROM inference_requests) + (SELECT count(*) FROM provider_attempts)
 + (SELECT count(*) FROM tool_calls) + (SELECT count(*) FROM session_events)
 + (SELECT count(*) FROM resource_grants) + (SELECT count(*) FROM usage_entries)
 + (SELECT count(*) FROM attempt_costs) + (SELECT count(*) FROM budget_reservations)
 + (SELECT count(*) FROM documents) + (SELECT count(*) FROM extractions)
 + (SELECT count(*) FROM knowledge_entries) + (SELECT count(*) FROM knowledge_links)
 + (SELECT count(*) FROM knowledge_revisions) + (SELECT count(*) FROM knowledge_tags)
 + (SELECT count(*) FROM knowledge_shares) + (SELECT count(*) FROM search_chunks)
 + (SELECT count(*) FROM chunk_vectors) + (SELECT count(*) FROM search_fts)
 + (SELECT count(*) FROM import_mappings) + (SELECT count(*) FROM maintenance_operations)
 + (SELECT count(*) FROM github_issue_intake) + (SELECT count(*) FROM review_source_evidence);
DROP TABLE migration_52_guard;

-- feature_plans.pending_proposal_id and transcript_chunks.provider_attempt_id referenced
-- approvals and provider_attempts. Both were always NULL (their parents never held a row), so
-- the two tables are rebuilt without them.
CREATE TABLE feature_plans_next (
 feature_id TEXT PRIMARY KEY REFERENCES work_items,
 revision INTEGER NOT NULL CHECK(revision>0),
 approved_plan_json TEXT CHECK(approved_plan_json IS NULL OR json_valid(approved_plan_json)),
 approved_by TEXT REFERENCES actors, approved_at INTEGER,
 pending_plan_json TEXT CHECK(pending_plan_json IS NULL OR json_valid(pending_plan_json)),
 pending_revision INTEGER CHECK(pending_revision IS NULL OR pending_revision>0),
 CHECK((approved_plan_json IS NULL)=(approved_by IS NULL)),
 CHECK((approved_plan_json IS NULL)=(approved_at IS NULL)),
 CHECK((pending_plan_json IS NULL)=(pending_revision IS NULL))
) STRICT;
INSERT INTO feature_plans_next
 (feature_id,revision,approved_plan_json,approved_by,approved_at,pending_plan_json,pending_revision)
 SELECT feature_id,revision,approved_plan_json,approved_by,approved_at,pending_plan_json,pending_revision
 FROM feature_plans;
DROP TABLE feature_plans;
ALTER TABLE feature_plans_next RENAME TO feature_plans;

CREATE TABLE transcript_chunks_next (
 session_id TEXT NOT NULL REFERENCES sessions, seq INTEGER NOT NULL, kind TEXT NOT NULL,
 payload_json TEXT NOT NULL CHECK(json_valid(payload_json)), entry_id TEXT, batch_id TEXT,
 context_epoch INTEGER NOT NULL DEFAULT 0, actor_id TEXT REFERENCES actors,
 created_at INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(session_id,seq)
) STRICT;
INSERT INTO transcript_chunks_next
 (session_id,seq,kind,payload_json,entry_id,batch_id,context_epoch,actor_id,created_at)
 SELECT session_id,seq,kind,payload_json,entry_id,batch_id,context_epoch,actor_id,created_at
 FROM transcript_chunks;
DROP TABLE transcript_chunks;
ALTER TABLE transcript_chunks_next RENAME TO transcript_chunks;
CREATE UNIQUE INDEX transcript_entry_identity ON transcript_chunks(session_id,entry_id)
 WHERE entry_id IS NOT NULL;

DROP TABLE tool_calls;
DROP TABLE usage_entries;
DROP TABLE attempt_costs;
DROP TABLE budget_reservations;
DROP TABLE provider_attempts;
DROP TABLE inference_requests;
DROP TABLE published_prices;
DROP TABLE price_overrides;
DROP TABLE price_snapshots;
DROP TABLE provider_models;
DROP TABLE provider_connections;
DROP TABLE routing_cursors;
DROP TABLE router_availability;
DROP TABLE approvals;
DROP TABLE session_events;
DROP TABLE resource_grants;
DROP TABLE documents;
DROP TABLE extractions;
DROP TABLE knowledge_links;
DROP TABLE knowledge_revisions;
DROP TABLE knowledge_tags;
DROP TABLE knowledge_shares;
DROP TABLE knowledge_entries;
DROP TABLE chunk_vectors;
DROP TABLE search_fts;
DROP TABLE search_chunks;
DROP TABLE import_mappings;
DROP TABLE maintenance_operations;
DROP TABLE github_issue_intake;
DROP TABLE review_source_evidence;

-- operations_for_capability: capability=? AND state IN (...) ORDER BY id.
CREATE INDEX operations_capability_state ON operations(capability,state,id);
