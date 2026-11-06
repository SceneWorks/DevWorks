-- Canonical accounting is host-authored. Legacy facts are bootstrapped in bounded runtime
-- batches, never repriced or assigned fabricated historical attempt identities by migration.
CREATE TABLE usage_attempts (
 attempt_id TEXT PRIMARY KEY,
 request_id TEXT NOT NULL,
 operation_id TEXT NOT NULL,
 attempt_number INTEGER NOT NULL CHECK(attempt_number>0),
 project_id TEXT REFERENCES projects,
 feature_id TEXT REFERENCES work_items,
 work_id TEXT REFERENCES work_items,
 assignment_id TEXT REFERENCES assignments,
 session_id TEXT REFERENCES sessions,
 job_run_id TEXT REFERENCES job_runs,
 actor_id TEXT NOT NULL REFERENCES actors,
 role TEXT NOT NULL,
 purpose TEXT NOT NULL,
 catalog_model_id TEXT NOT NULL,
 connection_id TEXT NOT NULL,
 endpoint_profile TEXT NOT NULL,
 access_mode TEXT NOT NULL,
 provider_model_id TEXT,
 pricing_snapshot_json TEXT NOT NULL CHECK(json_valid(pricing_snapshot_json) AND json_type(pricing_snapshot_json)='object'),
 state TEXT NOT NULL CHECK(state IN ('Admitted','Dispatched','Settled','UnknownTail','CancelledBeforeDispatch')),
 revision INTEGER NOT NULL CHECK(revision>0),
 created_at INTEGER NOT NULL CHECK(created_at>=0),
 UNIQUE(request_id,attempt_number)
) STRICT;
CREATE INDEX usage_attempts_project ON usage_attempts(project_id,attempt_id);
CREATE INDEX usage_attempts_feature ON usage_attempts(feature_id,attempt_id);
CREATE INDEX usage_attempts_work ON usage_attempts(work_id,attempt_id);
CREATE INDEX usage_attempts_session ON usage_attempts(session_id,attempt_id);
CREATE INDEX usage_attempts_job ON usage_attempts(job_run_id,attempt_id);
CREATE TABLE usage_observations (
 attempt_id TEXT NOT NULL REFERENCES usage_attempts,
 observation_key TEXT NOT NULL,
 sequence INTEGER NOT NULL CHECK(sequence>0),
 digest TEXT NOT NULL CHECK(length(digest)=64),
 payload_json TEXT NOT NULL CHECK(json_valid(payload_json) AND json_type(payload_json)='object'),
 recorded_at INTEGER NOT NULL CHECK(recorded_at>=0),
 PRIMARY KEY(attempt_id,observation_key),
 UNIQUE(attempt_id,sequence)
) STRICT;
CREATE TRIGGER usage_observations_immutable_update BEFORE UPDATE ON usage_observations
 BEGIN SELECT RAISE(ABORT,'usage observations are immutable'); END;
CREATE TRIGGER usage_observations_immutable_delete BEFORE DELETE ON usage_observations
 BEGIN SELECT RAISE(ABORT,'usage observations are immutable'); END;
CREATE TABLE usage_attempt_balances (
 attempt_id TEXT PRIMARY KEY REFERENCES usage_attempts,
 revision INTEGER NOT NULL CHECK(revision>0),
 priced_micro_usd INTEGER CHECK(priced_micro_usd IS NULL OR priced_micro_usd>=0),
 reserved_micro_usd INTEGER CHECK(reserved_micro_usd IS NULL OR reserved_micro_usd>=0),
 classification TEXT NOT NULL,
 monetary_complete INTEGER NOT NULL CHECK(monetary_complete IN (0,1)),
 incomplete INTEGER NOT NULL CHECK(incomplete IN (0,1)),
 unknown_tail INTEGER NOT NULL CHECK(unknown_tail IN (0,1)),
 summary_json TEXT NOT NULL CHECK(json_valid(summary_json) AND json_type(summary_json)='object')
) STRICT;
CREATE TABLE usage_feature_budgets (
 feature_id TEXT PRIMARY KEY REFERENCES work_items,
 revision INTEGER NOT NULL CHECK(revision>0),
 limit_micro_usd INTEGER CHECK(limit_micro_usd IS NULL OR limit_micro_usd>=0),
 warning_revision INTEGER CHECK(warning_revision IS NULL OR (warning_revision>0 AND warning_revision<=revision))
) STRICT;
CREATE TABLE usage_origins (
 source_kind TEXT NOT NULL,
 source_id TEXT NOT NULL,
 attempt_id TEXT REFERENCES usage_attempts,
 request_id TEXT,
 project_id TEXT REFERENCES projects,
 feature_id TEXT REFERENCES work_items,
 work_id TEXT REFERENCES work_items,
 session_id TEXT REFERENCES sessions,
 job_run_id TEXT REFERENCES job_runs,
 payload_json TEXT NOT NULL CHECK(json_valid(payload_json) AND json_type(payload_json)='object'),
 recorded_at INTEGER NOT NULL CHECK(recorded_at>=0),
 priced_micro_usd INTEGER CHECK(priced_micro_usd IS NULL OR priced_micro_usd>=0),
 classification TEXT NOT NULL,
 monetary_complete INTEGER NOT NULL CHECK(monetary_complete IN (0,1)),
 incomplete INTEGER NOT NULL CHECK(incomplete IN (0,1)),
 unknown_tail INTEGER NOT NULL CHECK(unknown_tail IN (0,1)),
 PRIMARY KEY(source_kind,source_id)
) STRICT;
CREATE INDEX usage_origins_project ON usage_origins(project_id,source_kind,source_id);
CREATE INDEX usage_origins_feature ON usage_origins(feature_id,source_kind,source_id);
CREATE TRIGGER usage_origins_immutable_update BEFORE UPDATE ON usage_origins
 WHEN OLD.attempt_id IS NOT NULL OR NEW.attempt_id IS NULL
 OR OLD.source_kind IS NOT NEW.source_kind OR OLD.source_id IS NOT NEW.source_id
 OR OLD.request_id IS NOT NEW.request_id OR OLD.project_id IS NOT NEW.project_id
 OR OLD.feature_id IS NOT NEW.feature_id OR OLD.work_id IS NOT NEW.work_id
 OR OLD.session_id IS NOT NEW.session_id OR OLD.job_run_id IS NOT NEW.job_run_id
 OR OLD.payload_json IS NOT NEW.payload_json OR OLD.recorded_at IS NOT NEW.recorded_at
 OR OLD.priced_micro_usd IS NOT NEW.priced_micro_usd OR OLD.classification IS NOT NEW.classification
 OR OLD.monetary_complete IS NOT NEW.monetary_complete OR OLD.incomplete IS NOT NEW.incomplete
 OR OLD.unknown_tail IS NOT NEW.unknown_tail
 BEGIN SELECT RAISE(ABORT,'usage origins are immutable except their first explicit attempt binding'); END;
CREATE TRIGGER usage_origins_immutable_delete BEFORE DELETE ON usage_origins
 BEGIN SELECT RAISE(ABORT,'usage origins are immutable'); END;
CREATE TABLE usage_changes (
 event_id TEXT PRIMARY KEY,
 attempt_id TEXT REFERENCES usage_attempts,
 feature_id TEXT REFERENCES work_items,
 kind TEXT NOT NULL,
 delta_micro_usd INTEGER,
 payload_json TEXT NOT NULL CHECK(json_valid(payload_json) AND json_type(payload_json)='object'),
 created_at INTEGER NOT NULL CHECK(created_at>=0)
) STRICT;
CREATE INDEX usage_changes_feature ON usage_changes(feature_id,event_id);
CREATE TRIGGER usage_changes_immutable_update BEFORE UPDATE ON usage_changes
 BEGIN SELECT RAISE(ABORT,'usage changes are immutable'); END;
CREATE TRIGGER usage_changes_immutable_delete BEFORE DELETE ON usage_changes
 BEGIN SELECT RAISE(ABORT,'usage changes are immutable'); END;
CREATE TABLE usage_bootstrap (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 revision INTEGER NOT NULL CHECK(revision>0),
 phase TEXT NOT NULL CHECK(phase IN ('Sessions','Jobs','RouterRequests','Complete')),
 session_sequence INTEGER,
 session_source INTEGER,
 session_id TEXT,
 job_run_id TEXT,
 job_seq INTEGER,
 router_after TEXT,
 source_sequence INTEGER NOT NULL CHECK(source_sequence>=0),
 processed INTEGER NOT NULL CHECK(processed>=0),
 completed_at INTEGER,
 CHECK((session_sequence IS NULL)=(session_source IS NULL) AND (session_source IS NULL)=(session_id IS NULL)),
 CHECK((job_run_id IS NULL)=(job_seq IS NULL)),
 CHECK((phase='Complete')=(completed_at IS NOT NULL))
) STRICT;
INSERT INTO usage_bootstrap(singleton,revision,phase,source_sequence,processed) VALUES(1,1,'Sessions',0,0);
CREATE TABLE usage_price_revision (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 revision INTEGER NOT NULL CHECK(revision>0)
) STRICT;
INSERT INTO usage_price_revision(singleton,revision) VALUES(1,1);
CREATE TABLE usage_price_snapshots (
 id TEXT PRIMARY KEY,
 provider TEXT NOT NULL,
 revision INTEGER NOT NULL CHECK(revision>0),
 source_url TEXT NOT NULL,
 source_digest TEXT NOT NULL CHECK(length(source_digest)=64),
 parser_version TEXT NOT NULL,
 fetched_at INTEGER NOT NULL CHECK(fetched_at>=0),
 verified_at INTEGER NOT NULL CHECK(verified_at>=0),
 effective_date TEXT,
 tariffs_json TEXT NOT NULL CHECK(json_valid(tariffs_json) AND json_type(tariffs_json)='array'),
 UNIQUE(provider,revision)
) STRICT;
CREATE TRIGGER usage_price_snapshots_immutable_update BEFORE UPDATE ON usage_price_snapshots
 BEGIN SELECT RAISE(ABORT,'price snapshots are immutable'); END;
CREATE TRIGGER usage_price_snapshots_immutable_delete BEFORE DELETE ON usage_price_snapshots
 BEGIN SELECT RAISE(ABORT,'price snapshots are immutable'); END;
CREATE TABLE usage_price_status (
 provider TEXT PRIMARY KEY,
 revision INTEGER NOT NULL CHECK(revision>0),
 current_snapshot_id TEXT REFERENCES usage_price_snapshots,
 last_attempt INTEGER,
 last_success INTEGER,
 refresh_pending INTEGER NOT NULL CHECK(refresh_pending IN (0,1)),
 last_error TEXT,
 next_refresh INTEGER
) STRICT;
CREATE TABLE usage_price_overrides (
 model_id TEXT NOT NULL,
 revision INTEGER NOT NULL CHECK(revision>=2),
 mode TEXT NOT NULL CHECK(mode IN ('Set','Clear')),
 tariffs_json TEXT NOT NULL CHECK(json_valid(tariffs_json) AND json_type(tariffs_json)='array'),
 updated_by TEXT NOT NULL REFERENCES actors,
 updated_at INTEGER NOT NULL CHECK(updated_at>=0),
 PRIMARY KEY(model_id,revision)
) STRICT;
CREATE TRIGGER usage_price_overrides_immutable_update BEFORE UPDATE ON usage_price_overrides
 BEGIN SELECT RAISE(ABORT,'price override revisions are immutable'); END;
CREATE TRIGGER usage_price_overrides_immutable_delete BEFORE DELETE ON usage_price_overrides
 BEGIN SELECT RAISE(ABORT,'price override revisions are immutable'); END;

-- Explicit billing policy is revision-fenced with the owning connection.
ALTER TABLE router_connections ADD COLUMN local_free INTEGER NOT NULL DEFAULT 0 CHECK(local_free IN (0,1));

-- Host bootstrap seeks committed facts across every scope without broad per-page scans.
CREATE INDEX session_usage_global ON session_usage(committed_seq,ordinal);
CREATE INDEX tool_invocation_global ON tool_invocations(committed_seq,response_seq);
