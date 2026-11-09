-- The exact frozen continuation used to resume a native provider session after restart.
-- Its project follows the owning session; the immutable artifact retains its own custody.
CREATE TABLE session_provider_context_heads (
 session_id TEXT PRIMARY KEY REFERENCES sessions(id),
 context_epoch INTEGER NOT NULL CHECK(context_epoch>=0),
 observed_sequence INTEGER NOT NULL CHECK(observed_sequence>=0),
 kind TEXT NOT NULL CHECK(kind IN ('ProviderTurnContinuation','ProviderOperationUpdateContext')),
 attempt_id TEXT,
 binding_json TEXT NOT NULL CHECK(json_valid(binding_json)),
 artifact_json TEXT NOT NULL CHECK(json_valid(artifact_json)),
 revision INTEGER NOT NULL CHECK(revision>0)
) STRICT;
