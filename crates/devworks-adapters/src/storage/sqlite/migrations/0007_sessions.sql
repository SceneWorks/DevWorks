-- DevWorks store migration 0007: sessions, transcript batches and work lineage.
-- Additive. Sessions (migration 1) gain their project, title, creator, lifecycle and append
-- bookkeeping (next per-session sequence, last checkpoint). transcript_chunks (migration 1)
-- holds the immutable transcript entries and gains their identity, batch, epoch and attribution.
-- New tables: batch receipts, durable tool calls with their single initial response, context
-- epochs and their recorded sources, checkpoints, attributed usage observations and the history
-- relationship graph. Transcript rows are never rewritten; compaction adds an epoch row and a
-- boundary entry, and a correction is a new link naming the link it supersedes.
-- Never edit this file after release: add a new migration.
ALTER TABLE sessions ADD COLUMN project_id TEXT REFERENCES projects;
ALTER TABLE sessions ADD COLUMN title TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN created_by TEXT REFERENCES actors;
ALTER TABLE sessions ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN closed_at INTEGER;
ALTER TABLE sessions ADD COLUMN closed_by TEXT REFERENCES actors;
ALTER TABLE sessions ADD COLUMN next_seq INTEGER NOT NULL DEFAULT 1 CHECK(next_seq>0);
ALTER TABLE sessions ADD COLUMN checkpoint_seq INTEGER NOT NULL DEFAULT 0 CHECK(checkpoint_seq>=0);
CREATE INDEX sessions_listing ON sessions(project_id,created_at DESC,id DESC);
ALTER TABLE transcript_chunks ADD COLUMN entry_id TEXT;
ALTER TABLE transcript_chunks ADD COLUMN batch_id TEXT;
ALTER TABLE transcript_chunks ADD COLUMN context_epoch INTEGER NOT NULL DEFAULT 0;
ALTER TABLE transcript_chunks ADD COLUMN actor_id TEXT REFERENCES actors;
ALTER TABLE transcript_chunks ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
CREATE UNIQUE INDEX transcript_entry_identity ON transcript_chunks(session_id,entry_id)
 WHERE entry_id IS NOT NULL;
CREATE TABLE transcript_batches (
 batch_id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions,
 payload_digest TEXT NOT NULL, actor_id TEXT NOT NULL REFERENCES actors,
 context_epoch INTEGER NOT NULL, first_seq INTEGER NOT NULL CHECK(first_seq>0),
 last_seq INTEGER NOT NULL, event_id TEXT NOT NULL REFERENCES events(id),
 committed_seq INTEGER NOT NULL, accepted_at INTEGER NOT NULL,
 CHECK(last_seq>=first_seq)
) STRICT;
CREATE TABLE session_tool_calls (
 session_id TEXT NOT NULL REFERENCES sessions, attempt_id TEXT NOT NULL,
 native_call_id TEXT NOT NULL, ordinal INTEGER NOT NULL CHECK(ordinal>=0),
 capability_id TEXT NOT NULL, call_seq INTEGER NOT NULL, response_seq INTEGER,
 outcome TEXT, operation_id TEXT REFERENCES operations,
 PRIMARY KEY(session_id,attempt_id,native_call_id), UNIQUE(session_id,attempt_id,ordinal),
 FOREIGN KEY(session_id,call_seq) REFERENCES transcript_chunks(session_id,seq),
 FOREIGN KEY(session_id,response_seq) REFERENCES transcript_chunks(session_id,seq),
 CHECK((response_seq IS NULL)=(outcome IS NULL))
) STRICT;
CREATE INDEX session_tool_call_order ON session_tool_calls(session_id,call_seq);
CREATE TABLE session_context_epochs (
 session_id TEXT NOT NULL REFERENCES sessions, epoch INTEGER NOT NULL CHECK(epoch>0),
 previous_epoch INTEGER NOT NULL, reason TEXT NOT NULL
  CHECK(reason IN ('Compaction','ProviderChange','InstructionAdoption')),
 context_json TEXT NOT NULL CHECK(json_valid(context_json)), cache_key TEXT NOT NULL,
 seq INTEGER NOT NULL, created_at INTEGER NOT NULL,
 PRIMARY KEY(session_id,epoch), CHECK(previous_epoch=epoch-1),
 FOREIGN KEY(session_id,seq) REFERENCES transcript_chunks(session_id,seq)
) STRICT;
CREATE TABLE session_context_sources (
 session_id TEXT NOT NULL REFERENCES sessions, epoch INTEGER NOT NULL CHECK(epoch>=0),
 ordinal INTEGER NOT NULL CHECK(ordinal>0), source_id TEXT NOT NULL, kind TEXT NOT NULL,
 source_revision INTEGER NOT NULL CHECK(source_revision>0),
 artifact_json TEXT CHECK(artifact_json IS NULL OR json_valid(artifact_json)),
 token_estimate INTEGER NOT NULL CHECK(token_estimate>=0),
 disposition TEXT NOT NULL CHECK(disposition IN ('Included','Truncated','Omitted')),
 seq INTEGER NOT NULL, PRIMARY KEY(session_id,epoch,ordinal)
) STRICT;
CREATE TABLE session_checkpoints (
 session_id TEXT NOT NULL REFERENCES sessions, seq INTEGER NOT NULL,
 batch_id TEXT NOT NULL REFERENCES transcript_batches,
 artifact_json TEXT NOT NULL CHECK(json_valid(artifact_json)), created_at INTEGER NOT NULL,
 PRIMARY KEY(session_id,seq), FOREIGN KEY(session_id,seq) REFERENCES transcript_chunks(session_id,seq)
) STRICT;
CREATE TABLE session_usage (
 batch_id TEXT NOT NULL REFERENCES transcript_batches, ordinal INTEGER NOT NULL CHECK(ordinal>0),
 session_id TEXT NOT NULL REFERENCES sessions, project_id TEXT REFERENCES projects,
 root_id TEXT REFERENCES work_items, committed_seq INTEGER NOT NULL,
 item_json TEXT NOT NULL CHECK(json_valid(item_json)), PRIMARY KEY(batch_id,ordinal)
) STRICT;
CREATE INDEX session_usage_root ON session_usage(root_id,committed_seq,batch_id,ordinal);
CREATE TABLE history_links (
 id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects,
 root_id TEXT NOT NULL REFERENCES work_items, kind TEXT NOT NULL,
 target_kind TEXT NOT NULL, target_id TEXT NOT NULL,
 parent_id TEXT REFERENCES history_links, supersedes_id TEXT REFERENCES history_links,
 actor_id TEXT NOT NULL REFERENCES actors, event_id TEXT NOT NULL REFERENCES events(id),
 seq INTEGER NOT NULL, repository_id TEXT REFERENCES repositories, revision TEXT,
 summary TEXT NOT NULL, body_json TEXT NOT NULL CHECK(json_valid(body_json)),
 created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX history_link_root ON history_links(root_id,seq,id);
CREATE INDEX history_link_kind ON history_links(root_id,kind,seq,id);
CREATE INDEX history_link_target ON history_links(target_kind,target_id,seq,id);
