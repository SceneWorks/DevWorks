-- DevWorks store migration 0009: indexed history feeds and tool invocation facts.
-- Additive. The project change feed reads a project's command decisions in committed order
-- through a partial index, so a page and its one-row has-more probe touch only matching rows.
-- tool_invocations records every completed tool invocation (a durable call and its single
-- initial response) of every session, with or without a project or work root, at the committed
-- sequence of the batch that recorded the response; it is indexed per root and per project
-- (the workspace scope is the NULL project). Every response recorded by an earlier build is
-- copied in from its call, its two transcript entries and its batch. session_usage gains the
-- matching per-project index. Never edit this file after release: add a new migration.
CREATE INDEX event_project_decisions ON events(project_id,seq) WHERE kind='CommandDecided';
CREATE TABLE tool_invocations (
 session_id TEXT NOT NULL REFERENCES sessions, response_seq INTEGER NOT NULL,
 call_seq INTEGER NOT NULL, committed_seq INTEGER NOT NULL,
 project_id TEXT REFERENCES projects, root_id TEXT REFERENCES work_items,
 call_entry_id TEXT NOT NULL, capability_id TEXT NOT NULL, outcome TEXT NOT NULL,
 error_code TEXT, operation_id TEXT REFERENCES operations,
 actor_id TEXT NOT NULL REFERENCES actors, called_at INTEGER NOT NULL,
 responded_at INTEGER NOT NULL,
 PRIMARY KEY(session_id,response_seq),
 FOREIGN KEY(session_id,response_seq) REFERENCES transcript_chunks(session_id,seq),
 FOREIGN KEY(session_id,call_seq) REFERENCES transcript_chunks(session_id,seq)
) STRICT;
CREATE INDEX tool_invocation_root ON tool_invocations(root_id,committed_seq,response_seq);
CREATE INDEX tool_invocation_project ON tool_invocations(project_id,committed_seq,response_seq);
INSERT INTO tool_invocations(session_id,response_seq,call_seq,committed_seq,project_id,root_id,
 call_entry_id,capability_id,outcome,error_code,operation_id,actor_id,called_at,responded_at)
 SELECT c.session_id,c.response_seq,c.call_seq,b.committed_seq,s.project_id,s.root_id,
  q.entry_id,c.capability_id,c.outcome,
  CASE WHEN c.outcome='Failed' THEN json_extract(r.payload_json,'$.outcome.code') END,
  c.operation_id,b.actor_id,q.created_at,r.created_at
 FROM session_tool_calls c
 JOIN sessions s ON s.id=c.session_id
 JOIN transcript_chunks r ON r.session_id=c.session_id AND r.seq=c.response_seq
 JOIN transcript_batches b ON b.session_id=r.session_id AND b.batch_id=r.batch_id
 JOIN transcript_chunks q ON q.session_id=c.session_id AND q.seq=c.call_seq
 WHERE c.response_seq IS NOT NULL;
CREATE INDEX session_usage_project ON session_usage(project_id,committed_seq,ordinal);
