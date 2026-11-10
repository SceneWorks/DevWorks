-- Agent messages outlive one activation or session. Recipient delivery and stream events keep
-- independent cursors; every write is part of the application's single-writer transaction.
CREATE TABLE agent_messages (
 id TEXT PRIMARY KEY,
 project_id TEXT NOT NULL REFERENCES projects,
 root_id TEXT REFERENCES work_items,
 channel TEXT NOT NULL CHECK(channel IN ('Assignment','ProjectBoard','Item','Project','Feature','Resource','Session','Human')),
 from_assignment_id TEXT REFERENCES assignments,
 to_assignment_id TEXT REFERENCES assignments,
 message_class TEXT NOT NULL CHECK(message_class IN ('Instruction','Status','Question','Evidence','Info','Request','Blocker','Announcement')),
 body TEXT NOT NULL CHECK(length(body) BETWEEN 1 AND 65536),
 attachments_json TEXT NOT NULL CHECK(json_valid(attachments_json)),
 in_reply_to_id TEXT REFERENCES agent_messages,
 state TEXT CHECK(state IN ('Open','Answered','Declined')),
 blocking INTEGER NOT NULL CHECK(blocking IN (0,1)),
 idempotency_key TEXT,
 revision INTEGER NOT NULL CHECK(revision>=1),
 created_at INTEGER NOT NULL,
 resolved_at INTEGER,
 from_actor_id TEXT NOT NULL REFERENCES actors,
 target_item_id TEXT REFERENCES work_items,
 target_feature_id TEXT REFERENCES work_items,
 resource_name TEXT CHECK(resource_name IS NULL OR length(resource_name) BETWEEN 1 AND 200),
 target_session_id TEXT REFERENCES sessions,
 to_human INTEGER NOT NULL DEFAULT 0 CHECK(to_human IN (0,1)),
 sender_session_id TEXT REFERENCES sessions,
 resolved_by_actor_id TEXT REFERENCES actors,
 resolved_by_session_id TEXT REFERENCES sessions,
 CHECK(channel<>'Assignment' OR to_assignment_id IS NOT NULL),
 CHECK((message_class IN ('Question','Request') AND state IS NOT NULL)
    OR (message_class NOT IN ('Question','Request') AND state IS NULL))
) STRICT;
CREATE INDEX agent_messages_project_created ON agent_messages(project_id,created_at,id);
CREATE INDEX agent_messages_root_created ON agent_messages(root_id,created_at,id);
CREATE UNIQUE INDEX agent_messages_idempotent ON agent_messages(project_id,from_actor_id,idempotency_key)
 WHERE idempotency_key IS NOT NULL;

CREATE TABLE agent_message_deliveries (
 sequence INTEGER PRIMARY KEY,
 message_id TEXT NOT NULL REFERENCES agent_messages,
 project_id TEXT NOT NULL REFERENCES projects,
 recipient_assignment_id TEXT REFERENCES assignments,
 recipient_session_id TEXT REFERENCES sessions,
 recipient_human INTEGER NOT NULL CHECK(recipient_human IN (0,1)),
 state TEXT NOT NULL CHECK(state IN ('Pending','Delivered','Read','Answered','Declined')),
 revision INTEGER NOT NULL CHECK(revision>=1),
 delivered_at INTEGER,
 read_at INTEGER,
 resolved_at INTEGER,
 handed_over INTEGER NOT NULL DEFAULT 0 CHECK(handed_over IN (0,1)),
 CHECK((recipient_assignment_id IS NOT NULL) + (recipient_session_id IS NOT NULL) + recipient_human = 1)
) STRICT;
CREATE UNIQUE INDEX agent_deliveries_recipient ON agent_message_deliveries(message_id,coalesce(recipient_assignment_id,''),coalesce(recipient_session_id,''),recipient_human);
CREATE INDEX agent_deliveries_assignment ON agent_message_deliveries(project_id,recipient_assignment_id,sequence);
CREATE INDEX agent_deliveries_unread ON agent_message_deliveries(recipient_assignment_id,state,sequence);
CREATE UNIQUE INDEX agent_deliveries_human ON agent_message_deliveries(message_id)
 WHERE recipient_human=1;

CREATE TABLE agent_message_events (
 sequence INTEGER PRIMARY KEY,
 project_id TEXT NOT NULL REFERENCES projects,
 message_id TEXT NOT NULL REFERENCES agent_messages,
 kind TEXT NOT NULL CHECK(kind IN ('Posted','HandedOver','Read','Answered','Declined')),
 actor_id TEXT NOT NULL REFERENCES actors,
 recipient_assignment_id TEXT REFERENCES assignments,
 recipient_session_id TEXT REFERENCES sessions,
 recipient_human INTEGER NOT NULL DEFAULT 0 CHECK(recipient_human IN (0,1)),
 created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX agent_message_events_project ON agent_message_events(project_id,sequence);
