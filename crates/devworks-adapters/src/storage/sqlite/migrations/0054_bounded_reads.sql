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
