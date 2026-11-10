-- A transferred child resumes under a new root at a new provider context epoch.
CREATE TABLE session_context_epochs_next (
 session_id TEXT NOT NULL REFERENCES sessions, epoch INTEGER NOT NULL CHECK(epoch>0),
 previous_epoch INTEGER NOT NULL, reason TEXT NOT NULL
  CHECK(reason IN ('Compaction','ProviderChange','InstructionAdoption','MembershipHandoff')),
 context_json TEXT NOT NULL CHECK(json_valid(context_json)), identity TEXT NOT NULL,
 seq INTEGER NOT NULL, created_at INTEGER NOT NULL,
 PRIMARY KEY(session_id,epoch), CHECK(previous_epoch=epoch-1),
 FOREIGN KEY(session_id,seq) REFERENCES transcript_chunks(session_id,seq)
) STRICT;
INSERT INTO session_context_epochs_next
 (session_id,epoch,previous_epoch,reason,context_json,identity,seq,created_at)
 SELECT session_id,epoch,previous_epoch,reason,context_json,identity,seq,created_at
 FROM session_context_epochs;
DROP TABLE session_context_epochs;
ALTER TABLE session_context_epochs_next RENAME TO session_context_epochs;
