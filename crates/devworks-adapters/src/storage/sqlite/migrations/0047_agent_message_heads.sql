CREATE TABLE agent_message_sequence_heads (
 kind TEXT PRIMARY KEY CHECK(kind IN ('delivery','event')),
 head INTEGER NOT NULL CHECK(head>=0)
) STRICT;
INSERT INTO agent_message_sequence_heads(kind,head)
 VALUES('delivery',(SELECT coalesce(max(sequence),0) FROM agent_message_deliveries));
INSERT INTO agent_message_sequence_heads(kind,head)
 VALUES('event',(SELECT coalesce(max(sequence),0) FROM agent_message_events));
