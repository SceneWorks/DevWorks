-- A recipient copy may be inherited by one successor session. Keep the original recipient and
-- its delivery state unchanged; the successor receives a separate handed-over delivery.
ALTER TABLE agent_message_deliveries ADD COLUMN handed_to_session_id TEXT REFERENCES sessions(id);
CREATE INDEX agent_deliveries_handed_to ON agent_message_deliveries(project_id,handed_to_session_id,sequence);
