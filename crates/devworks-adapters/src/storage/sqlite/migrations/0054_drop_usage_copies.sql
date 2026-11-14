-- Drop session_usage and job_run_usage. Since migration 28 every session and job usage item is
-- recorded twice: in its source table and, in the same transaction, as a usage_origins row
-- (SessionUsage or JobUsage) holding the same item. Rows written before migration 28 reach
-- usage_origins only through the bounded runtime bootstrap, which may not have finished. This
-- migration copies every source row that has no origin yet, computed exactly as the ingestion
-- code computes it, and discards the rest, whose origin already holds the same data. Readers
-- page session usage by committed sequence, so usage_origins gains committed_seq (set for
-- SessionUsage rows only) and the per-scope indexes the dropped table had.
ALTER TABLE usage_origins ADD COLUMN committed_seq INTEGER;
DROP TRIGGER usage_origins_immutable_update;
UPDATE usage_origins SET committed_seq=(
 SELECT b.committed_seq FROM transcript_batches b
 WHERE b.session_id=usage_origins.session_id AND b.batch_id=json_extract(usage_origins.source_id,'$[1]'))
 WHERE source_kind='SessionUsage';

-- Whole micro-USD of each canonical decimal amount, rounded half up (raw_amount in
-- services/usage/ingest.rs); an amount it cannot price stays unpriced.
CREATE TEMP TABLE migration_54_amounts (amount TEXT PRIMARY KEY, micro INTEGER) STRICT;
INSERT INTO migration_54_amounts(amount,micro)
 SELECT amount,CASE WHEN length(amount)<=64 AND length(w)>0 AND w NOT GLOB '*[^0-9]*'
   AND length(ltrim(w,'0'))<=12
   AND (NOT dotted OR (length(f) BETWEEN 1 AND 12 AND f NOT GLOB '*[^0-9]*'))
  THEN CAST(w AS INTEGER)*1000000 + CAST(substr(f||'000000',1,6) AS INTEGER)
   + (substr(f||'0000000',7,1)>='5') END
 FROM (SELECT amount,instr(amount,'.')>0 AS dotted,
   CASE WHEN instr(amount,'.')>0 THEN substr(amount,1,instr(amount,'.')-1) ELSE amount END AS w,
   CASE WHEN instr(amount,'.')>0 THEN substr(amount,instr(amount,'.')+1) ELSE '' END AS f
  FROM (SELECT json_extract(item_json,'$.amountUsd') AS amount FROM session_usage
   WHERE json_type(item_json,'$.amountUsd')='text'
   UNION SELECT amount_usd FROM job_run_usage WHERE amount_usd IS NOT NULL));

INSERT INTO usage_origins(source_kind,source_id,attempt_id,request_id,project_id,feature_id,
 work_id,session_id,job_run_id,payload_json,recorded_at,priced_micro_usd,classification,
 monetary_complete,incomplete,unknown_tail,committed_seq)
 SELECT 'SessionUsage',json_array(u.session_id,u.batch_id,u.ordinal),NULL,
  json_extract(u.item_json,'$.requestId'),u.project_id,
  (SELECT w.id FROM work_items w WHERE w.id=u.root_id AND w.kind='feature'),
  u.root_id,u.session_id,NULL,json_object('version',1,'item',json(u.item_json)),b.accepted_at,
  (SELECT a.micro FROM migration_54_amounts a WHERE a.amount=json_extract(u.item_json,'$.amountUsd')),
  CASE json_extract(u.item_json,'$.priceKind')
   WHEN 'Reported' THEN 'ReportedCharge'
   WHEN 'Estimated' THEN CASE json_extract(u.item_json,'$.priceOrigin')
    WHEN 'SubscriptionApiEquivalent' THEN 'SubscriptionApiEquivalent'
    WHEN 'UserConfigured' THEN 'ConfiguredEstimate' ELSE 'OfficialEstimate' END
   WHEN 'Free' THEN 'LocalFree' ELSE 'Untracked' END,
  0,ifnull(json_extract(u.item_json,'$.incomplete'),0),ifnull(json_extract(u.item_json,'$.incomplete'),0),
  u.committed_seq
 FROM session_usage u
 JOIN transcript_batches b ON b.session_id=u.session_id AND b.batch_id=u.batch_id
 WHERE NOT EXISTS(SELECT 1 FROM usage_origins o WHERE o.source_kind='SessionUsage'
  AND o.source_id=json_array(u.session_id,u.batch_id,u.ordinal));

INSERT INTO usage_origins(source_kind,source_id,attempt_id,request_id,project_id,feature_id,
 work_id,session_id,job_run_id,payload_json,recorded_at,priced_micro_usd,classification,
 monetary_complete,incomplete,unknown_tail,committed_seq)
 SELECT 'JobUsage',json_array(u.run_id,u.seq),NULL,u.request_id,r.project_id,NULL,NULL,NULL,
  u.run_id,
  json_object('version',1,'purpose',u.purpose,'modelId',u.model_id,'inputTokens',u.input_tokens,
   'outputTokens',u.output_tokens,'cacheReadTokens',u.cache_read_tokens,
   'cacheWriteTokens',u.cache_write_tokens,'amountUsd',u.amount_usd,'priceKind',u.price_kind,
   'priceOrigin',u.price_origin,'priceRevision',u.price_revision,
   'incomplete',json(CASE u.incomplete WHEN 1 THEN 'true' ELSE 'false' END)),
  u.recorded_at,(SELECT a.micro FROM migration_54_amounts a WHERE a.amount=u.amount_usd),
  CASE u.price_kind
   WHEN 'Reported' THEN 'ReportedCharge'
   WHEN 'Estimated' THEN CASE u.price_origin
    WHEN 'SubscriptionApiEquivalent' THEN 'SubscriptionApiEquivalent'
    WHEN 'UserConfigured' THEN 'ConfiguredEstimate' ELSE 'OfficialEstimate' END
   WHEN 'Free' THEN 'LocalFree' ELSE 'Untracked' END,
  0,u.incomplete,u.incomplete,NULL
 FROM job_run_usage u JOIN job_runs r ON r.id=u.run_id
 WHERE NOT EXISTS(SELECT 1 FROM usage_origins o WHERE o.source_kind='JobUsage'
  AND o.source_id=json_array(u.run_id,u.seq));
DROP TABLE migration_54_amounts;

DROP TABLE session_usage;
DROP TABLE job_run_usage;

CREATE TRIGGER usage_origins_immutable_update BEFORE UPDATE ON usage_origins
 WHEN OLD.attempt_id IS NOT NULL OR NEW.attempt_id IS NULL
 OR OLD.source_kind IS NOT NEW.source_kind OR OLD.source_id IS NOT NEW.source_id
 OR OLD.request_id IS NOT NEW.request_id OR OLD.project_id IS NOT NEW.project_id
 OR OLD.feature_id IS NOT NEW.feature_id OR OLD.work_id IS NOT NEW.work_id
 OR OLD.session_id IS NOT NEW.session_id OR OLD.job_run_id IS NOT NEW.job_run_id
 OR OLD.payload_json IS NOT NEW.payload_json OR OLD.recorded_at IS NOT NEW.recorded_at
 OR OLD.priced_micro_usd IS NOT NEW.priced_micro_usd OR OLD.classification IS NOT NEW.classification
 OR OLD.monetary_complete IS NOT NEW.monetary_complete OR OLD.incomplete IS NOT NEW.incomplete
 OR OLD.unknown_tail IS NOT NEW.unknown_tail OR OLD.committed_seq IS NOT NEW.committed_seq
 BEGIN SELECT RAISE(ABORT,'usage origins are immutable except their first explicit attempt binding'); END;
CREATE INDEX usage_origins_session_root ON usage_origins(source_kind,work_id,committed_seq);
CREATE INDEX usage_origins_session_project ON usage_origins(source_kind,project_id,committed_seq);
CREATE INDEX usage_origins_session_global ON usage_origins(source_kind,committed_seq);
CREATE UNIQUE INDEX usage_origins_job_request ON usage_origins(source_kind,job_run_id,request_id)
 WHERE source_kind='JobUsage';
