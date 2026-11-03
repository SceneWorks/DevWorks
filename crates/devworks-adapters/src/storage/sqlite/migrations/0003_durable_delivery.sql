-- DevWorks store migration 0003: durable event delivery.
-- Adds ordered partitions, the delivered event's committed sequence, a delivery revision and
-- the visible last failure cause to queue_entries, plus three durable delivery markers: a
-- cancellation requested while the delivery was claimed (it survives hold, requeue and claim
-- recovery), the time it last failed or was held (inspection order), and root-scoped work
-- recovered from an earlier boot that waits for root eligibility before it may run again.
-- Existing rows are backfilled from their events; consumer_progress (migration 0001) holds
-- per-partition checkpoints and, under the reserved partition key '', each consumer's settlement
-- counter and highest settled sequence. Never edit this file after release: add a new migration.
ALTER TABLE queue_entries ADD COLUMN partition_key TEXT;
ALTER TABLE queue_entries ADD COLUMN event_seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE queue_entries ADD COLUMN revision INTEGER NOT NULL DEFAULT 1 CHECK(revision>0);
ALTER TABLE queue_entries ADD COLUMN last_error TEXT;
ALTER TABLE queue_entries ADD COLUMN cancel_requested INTEGER NOT NULL DEFAULT 0 CHECK(cancel_requested IN (0,1));
ALTER TABLE queue_entries ADD COLUMN failed_at INTEGER;
ALTER TABLE queue_entries ADD COLUMN awaiting_eligibility INTEGER NOT NULL DEFAULT 0 CHECK(awaiting_eligibility IN (0,1));
UPDATE queue_entries SET event_seq=(SELECT seq FROM events WHERE events.id=queue_entries.event_id);
UPDATE queue_entries SET failed_at=ready_at WHERE state IN ('Failed','Reconciling');
CREATE INDEX queue_partition_head ON queue_entries(consumer,partition_key,event_seq)
 WHERE partition_key IS NOT NULL AND state NOT IN ('Completed','Cancelled','Dismissed','Compacted');
CREATE INDEX queue_unsettled ON queue_entries(consumer,state,event_seq)
 WHERE state NOT IN ('Completed','Cancelled','Dismissed','Compacted');
CREATE INDEX queue_operation ON queue_entries(operation_id) WHERE operation_id IS NOT NULL;
CREATE INDEX queue_awaiting_eligibility ON queue_entries(consumer,id) WHERE awaiting_eligibility=1;
