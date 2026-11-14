-- Bounded read and admission paths (DW-229): indexes and maintained summaries that let a request
-- read what it asked for instead of paging history.

-- Usage recovery visits only attempts whose balance still holds local custody.
CREATE INDEX usage_attempt_balances_open ON usage_attempt_balances(attempt_id)
 WHERE local_quiescent=0;

-- A Knowledge claim carries tallies of its deliveries, lesson events and open challenges, so a
-- claim read costs the same however long its history is. Triggers keep them current for every
-- writer, and the existing rows are counted once here.
ALTER TABLE knowledge_claims ADD COLUMN delivered_sessions INTEGER NOT NULL DEFAULT 0
 CHECK (delivered_sessions >= 0);
ALTER TABLE knowledge_claims ADD COLUMN last_delivered_at INTEGER;
ALTER TABLE knowledge_claims ADD COLUMN lesson_events INTEGER NOT NULL DEFAULT 0
 CHECK (lesson_events >= 0);
ALTER TABLE knowledge_claims ADD COLUMN open_challenges INTEGER NOT NULL DEFAULT 0
 CHECK (open_challenges >= 0);
UPDATE knowledge_claims SET
 delivered_sessions = (SELECT count(DISTINCT d.session_id) FROM knowledge_deliveries d
  WHERE d.claim_id = knowledge_claims.id),
 last_delivered_at = (SELECT max(d.delivered_at) FROM knowledge_deliveries d
  WHERE d.claim_id = knowledge_claims.id),
 lesson_events = (SELECT count(*) FROM knowledge_lesson_events e
  WHERE e.claim_id = knowledge_claims.id),
 open_challenges = (SELECT count(*) FROM knowledge_challenges h
  WHERE h.claim_id = knowledge_claims.id AND h.state = 'Open');
CREATE TRIGGER knowledge_deliveries_tally_insert AFTER INSERT ON knowledge_deliveries BEGIN
 UPDATE knowledge_claims SET
  delivered_sessions = delivered_sessions + NOT EXISTS(SELECT 1 FROM knowledge_deliveries d
   WHERE d.session_id = NEW.session_id AND d.claim_id = NEW.claim_id
   AND d.revision <> NEW.revision),
  last_delivered_at = max(coalesce(last_delivered_at, NEW.delivered_at), NEW.delivered_at)
 WHERE id = NEW.claim_id;
END;
CREATE TRIGGER knowledge_deliveries_tally_delete AFTER DELETE ON knowledge_deliveries BEGIN
 UPDATE knowledge_claims SET
  delivered_sessions = delivered_sessions - NOT EXISTS(SELECT 1 FROM knowledge_deliveries d
   WHERE d.session_id = OLD.session_id AND d.claim_id = OLD.claim_id),
  last_delivered_at = (SELECT max(d.delivered_at) FROM knowledge_deliveries d
   WHERE d.claim_id = OLD.claim_id)
 WHERE id = OLD.claim_id;
END;
CREATE TRIGGER knowledge_lesson_events_tally_insert AFTER INSERT ON knowledge_lesson_events BEGIN
 UPDATE knowledge_claims SET lesson_events = lesson_events + 1 WHERE id = NEW.claim_id;
END;
CREATE TRIGGER knowledge_lesson_events_tally_delete AFTER DELETE ON knowledge_lesson_events BEGIN
 UPDATE knowledge_claims SET lesson_events = lesson_events - 1 WHERE id = OLD.claim_id;
END;
CREATE TRIGGER knowledge_lesson_events_tally_move AFTER UPDATE OF claim_id
 ON knowledge_lesson_events WHEN OLD.claim_id IS NOT NEW.claim_id BEGIN
 UPDATE knowledge_claims SET lesson_events = lesson_events - 1 WHERE id = OLD.claim_id;
 UPDATE knowledge_claims SET lesson_events = lesson_events + 1 WHERE id = NEW.claim_id;
END;
CREATE TRIGGER knowledge_challenges_tally_insert AFTER INSERT ON knowledge_challenges
 WHEN NEW.state = 'Open' BEGIN
 UPDATE knowledge_claims SET open_challenges = open_challenges + 1 WHERE id = NEW.claim_id;
END;
CREATE TRIGGER knowledge_challenges_tally_delete AFTER DELETE ON knowledge_challenges
 WHEN OLD.state = 'Open' BEGIN
 UPDATE knowledge_claims SET open_challenges = open_challenges - 1 WHERE id = OLD.claim_id;
END;
CREATE TRIGGER knowledge_challenges_tally_update AFTER UPDATE OF state, claim_id
 ON knowledge_challenges BEGIN
 UPDATE knowledge_claims SET open_challenges = open_challenges - 1
  WHERE id = OLD.claim_id AND OLD.state = 'Open';
 UPDATE knowledge_claims SET open_challenges = open_challenges + 1
  WHERE id = NEW.claim_id AND NEW.state = 'Open';
END;

-- A usage report reads the attempts and origins of its date range, and the late attempts of the
-- requests made in it, through time indexes instead of paging all usage history. Late attempts
-- page in (request time, request, attempt) order, which these indexes serve without a sort.
CREATE INDEX usage_attempts_created ON usage_attempts(created_at,attempt_id);
CREATE INDEX usage_attempts_project_created ON usage_attempts(project_id,created_at,attempt_id);
CREATE INDEX usage_attempts_request ON usage_attempts(request_id,attempt_id);
CREATE INDEX router_requests_created ON router_requests(created_at,request_id);
CREATE INDEX usage_origins_recorded ON usage_origins(recorded_at,source_kind,source_id);
CREATE INDEX usage_origins_project_recorded
 ON usage_origins(project_id,recorded_at,source_kind,source_id);

-- A next_work claim reads the project's live claims and the work items in an actionable status,
-- not released claims or completed work.
CREATE INDEX allocation_claims_live_project ON allocation_claims(project_id,id)
 WHERE state IN ('Active','Transferred','Yielding');
CREATE INDEX work_items_by_status
 ON work_items(project_id,status_id,priority DESC,manual_order,created_at,id);

-- Outgoing MCP: each server's catalog moves out of its scope's configuration into its own
-- namespace, outgoing_mcp/<server id>, so reading one server reads only its own catalog. Every
-- current configuration gets a new revision without the catalogs; history keeps its revisions.
INSERT INTO configuration_revisions(scope_key,namespace,revision,schema_version,body_json)
 SELECT r.scope_key,'outgoing_mcp/'||json_extract(s.value,'$.public.id'),2,r.schema_version,
  json_object(
   'capabilities',json(coalesce(json_extract(s.value,'$.capabilities'),'[]')),
   'schemas',json(coalesce(json_extract(s.value,'$.schemas'),'{}')),
   'resource_uris',json(coalesce(json_extract(s.value,'$.resource_uris'),'{}')),
   'retired',json(coalesce(json_extract(s.value,'$.retired'),'{}')))
 FROM effective_configuration e
 JOIN configuration_revisions r USING(scope_key,namespace,revision),
 json_each(r.body_json,'$.servers') s
 WHERE e.namespace='outgoing_mcp';
INSERT INTO effective_configuration(scope_key,namespace,revision)
 SELECT scope_key,namespace,revision FROM configuration_revisions
 WHERE substr(namespace,1,13)='outgoing_mcp/';
INSERT INTO configuration_revisions(scope_key,namespace,revision,schema_version,body_json)
 SELECT r.scope_key,'outgoing_mcp',r.revision+1,r.schema_version,
  json_set(r.body_json,'$.servers',json((SELECT json_group_array(
   json(json_remove(s.value,'$.capabilities','$.schemas','$.resource_uris','$.retired'))
   ORDER BY CAST(s.key AS INTEGER)) FROM json_each(r.body_json,'$.servers') s)))
 FROM effective_configuration e
 JOIN configuration_revisions r USING(scope_key,namespace,revision)
 WHERE e.namespace='outgoing_mcp';
UPDATE effective_configuration SET revision=revision+1 WHERE namespace='outgoing_mcp';
