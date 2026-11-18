//! The durable queue: reads shared by snapshots and the write unit, and the `DurableQueue` writes.
use super::*;

pub(super) const QUEUE_COLUMNS: &str = "id,consumer,event_id,root_id,operation_id,priority,ready_at,state,\
    owner_id,lease_until,generation,cancellation_epoch,attempt_count,\
    payload_json,partition_key,event_seq,revision,last_error,claim_boot_id,\
    cancel_requested,failed_at,awaiting_eligibility";

/// SQL list of settled queue states (they never block a partition or the watermark).
/// `Compacted` is a dedupe tombstone of a settled delivery.
pub(super) const SETTLED: &str = "('Completed','Cancelled','Dismissed','Compacted')";

pub(super) struct QueueRaw {
    id: String,
    consumer: String,
    event: String,
    root: Option<String>,
    op: Option<String>,
    priority: i64,
    ready: i64,
    state: String,
    owner: Option<String>,
    lease: Option<i64>,
    generation: i64,
    epoch: i64,
    attempts: i64,
    payload: String,
    partition: Option<String>,
    seq: i64,
    revision: i64,
    error: Option<String>,
    claim_boot: Option<String>,
    cancel_requested: bool,
    failed_at: Option<i64>,
    awaiting_eligibility: bool,
}

pub(super) fn queue_row(r: &Row<'_>) -> rusqlite::Result<QueueRaw> {
    Ok(QueueRaw {
        id: r.get(0)?,
        consumer: r.get(1)?,
        event: r.get(2)?,
        root: r.get(3)?,
        op: r.get(4)?,
        priority: r.get(5)?,
        ready: r.get(6)?,
        state: r.get(7)?,
        owner: r.get(8)?,
        lease: r.get(9)?,
        generation: r.get(10)?,
        epoch: r.get(11)?,
        attempts: r.get(12)?,
        payload: r.get(13)?,
        partition: r.get(14)?,
        seq: r.get(15)?,
        revision: r.get(16)?,
        error: r.get(17)?,
        claim_boot: r.get(18)?,
        cancel_requested: r.get(19)?,
        failed_at: r.get(20)?,
        awaiting_eligibility: r.get(21)?,
    })
}

pub(super) fn queue_of(r: QueueRaw) -> Result<QueueEntry, StorageError> {
    Ok(QueueEntry {
        id: id_of(r.id)?,
        consumer: r.consumer,
        event_id: EventId(id_of(r.event)?),
        root_id: opt_id(r.root)?.map(RootId),
        operation_id: opt_id(r.op)?.map(OperationId),
        priority: r.priority,
        ready_at_ms: r.ready,
        state: parse_queue_state(&r.state)?,
        owner: opt_id(r.owner)?,
        lease_until_ms: r.lease,
        generation: r.generation,
        cancellation_epoch: r.epoch,
        attempt_count: r.attempts,
        payload: json_of(&r.payload)?,
        partition: r.partition,
        event_sequence: r.seq,
        revision: r.revision,
        last_error: r.error,
        claim_boot_id: opt_id(r.claim_boot)?,
        cancel_requested: r.cancel_requested,
        failed_at_ms: r.failed_at,
        awaiting_eligibility: r.awaiting_eligibility,
    })
}

pub(super) fn entry(conn: &Connection, id: &Id) -> Result<Option<QueueEntry>, StorageError> {
    let raw = db(stmt(
        conn,
        &format!("SELECT {QUEUE_COLUMNS} FROM queue_entries WHERE id=?1 AND state<>'Compacted'"),
    )?
    .query_row([id.as_str()], queue_row)
    .optional())?;
    raw.map(queue_of).transpose()
}

pub(super) fn delivery(
    conn: &Connection,
    consumer: &str,
    event: &EventId,
) -> Result<Option<QueueEntry>, StorageError> {
    let raw = db(stmt(
        conn,
        &format!(
            "SELECT {QUEUE_COLUMNS} FROM queue_entries WHERE consumer=?1 AND event_id=?2 AND state<>'Compacted'"
        ),
    )?
    .query_row(params![consumer, event.0.as_str()], queue_row)
    .optional())?;
    raw.map(queue_of).transpose()
}

pub(super) fn status(conn: &Connection, consumer: &str) -> Result<QueueStatus, StorageError> {
    let mut s = stmt(
        conn,
        "SELECT state,count(*),min(ready_at) FROM queue_entries WHERE consumer=?1 AND state<>'Compacted' GROUP BY state",
    )?;
    let rows: Vec<(String, i64, Option<i64>)> =
        db(db(s.query_map([consumer], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))?.collect())?;
    let mut out = QueueStatus::default();
    for (state, count, oldest) in rows {
        match parse_queue_state(&state)? {
            QueueState::Ready => {
                out.ready = count;
                out.oldest_ready_at_ms = oldest;
            }
            QueueState::Claimed => out.claimed = count,
            QueueState::Reconciling => out.reconciling = count,
            QueueState::Failed => out.failed = count,
            QueueState::Completed => out.completed = count,
            QueueState::Cancelled => out.cancelled = count,
            QueueState::Dismissed => out.dismissed = count,
        }
    }
    Ok(out)
}

/// Inspection ranks: (rank, state predicate, keyset predicate, order).
pub(super) const INSPECTION_RANKS: [(u8, &str, &str, &str); 3] = [
    (
        0,
        "state='Claimed'",
        "(event_seq,id)>(?3,?5)",
        "event_seq,id",
    ),
    (
        1,
        "state IN ('Failed','Reconciling')",
        "(coalesce(failed_at,0),id)<(?3,?5)",
        "coalesce(failed_at,0) DESC,id DESC",
    ),
    (
        2,
        "state='Ready'",
        "(-priority,ready_at,id)>(-?3,?4,?5)",
        "priority DESC,ready_at,id",
    ),
];

pub(super) fn deliveries(
    conn: &Connection,
    filter: &DeliveryFilter,
    after: Option<&DeliveryKey>,
    max: u16,
) -> Result<DeliveryPage, StorageError> {
    let n = page_limit(max)?;
    // One extra row tells whether more follow. Ranks are read in order, each with its own
    // keyset predicate, so the combined order is total and stable across pages.
    let mut entries = Vec::new();
    for (rank, predicate, keyset, order) in INSPECTION_RANKS {
        let start = match after {
            Some(k) if k.rank > rank => continue,
            Some(k) if k.rank == rank => Some(k),
            _ => None,
        };
        let want = n + 1 - entries.len() as i64;
        if want <= 0 {
            break;
        }
        let sql = format!(
            "SELECT {QUEUE_COLUMNS} FROM queue_entries WHERE {predicate} AND (?1 IS \
                NULL OR consumer=?1) AND (?2 IS NULL OR operation_id=?2) AND ({}) ORDER \
                BY {order} LIMIT ?6",
            if start.is_some() {
                keyset
            } else {
                "?3 IS NULL AND ?4 IS NULL AND ?5 IS NULL"
            }
        );
        let (a, b, id) = match start {
            Some(k) => (Some(k.a), Some(k.b), Some(k.id.as_str().to_owned())),
            None => (None, None, None),
        };
        let mut s = stmt(conn, &sql)?;
        let raws: Vec<QueueRaw> = db(db(s.query_map(
            params![
                filter.consumer,
                filter.operation_id.as_ref().map(|o| o.0.as_str()),
                a,
                b,
                id,
                want
            ],
            queue_row,
        ))?
        .collect())?;
        for raw in raws {
            entries.push(queue_of(raw)?);
        }
    }
    let next = if entries.len() as i64 > n {
        entries.truncate(n as usize);
        entries.last().and_then(QueueEntry::inspection_key)
    } else {
        None
    };
    Ok(DeliveryPage { entries, next })
}

pub(super) fn progress_row(
    conn: &Connection,
    consumer: &str,
    partition: &str,
) -> Result<(i64, i64), StorageError> {
    Ok(db(stmt(
        conn,
        "SELECT applied_seq,revision FROM consumer_progress WHERE consumer=?1 AND partition_key=?2",
    )?
    .query_row(params![consumer, partition], |r| Ok((r.get(0)?, r.get(1)?)))
    .optional())?
    .unwrap_or((0, 0)))
}

pub(super) fn progress(
    conn: &Connection,
    consumer: &str,
) -> Result<ConsumerProgress, StorageError> {
    let (highest_settled, revision) = progress_row(conn, consumer, "")?;
    let (lowest_unsettled, next_retry): (Option<i64>, Option<i64>) = db(stmt(
        conn,
        &format!(
            "SELECT min(event_seq),min(CASE WHEN state='Ready' AND attempt_count>0 \
                THEN ready_at END) FROM queue_entries WHERE consumer=?1 AND state NOT \
                IN {SETTLED}"
        ),
    )?
    .query_row(params![consumer], |r| Ok((r.get(0)?, r.get(1)?))))?;
    let watermark = match lowest_unsettled {
        Some(low) => highest_settled.min(low - 1).max(0),
        None => highest_settled,
    };
    Ok(ConsumerProgress {
        watermark,
        highest_settled,
        revision,
        next_retry_at_ms: next_retry,
    })
}

pub(super) fn checkpoint(
    conn: &Connection,
    consumer: &str,
    partition: Option<&str>,
) -> Result<i64, StorageError> {
    match partition {
        Some(p) => Ok(progress_row(conn, consumer, p)?.0),
        None => Ok(progress(conn, consumer)?.watermark),
    }
}

pub(super) fn awaiting_eligibility(
    conn: &Connection,
    consumer: &str,
    after: Option<&Id>,
    max: u16,
) -> Result<Vec<QueueEntry>, StorageError> {
    let n = page_limit(max)?;
    let mut s = stmt(
        conn,
        &format!(
            "SELECT {QUEUE_COLUMNS} FROM queue_entries WHERE consumer=?1 AND \
                awaiting_eligibility=1 AND state='Reconciling' AND (?2 IS NULL OR \
                id>?2) ORDER BY id LIMIT ?3"
        ),
    )?;
    let raws: Vec<QueueRaw> =
        db(db(s.query_map(params![consumer, after.map(Id::as_str), n], queue_row))?.collect())?;
    raws.into_iter().map(queue_of).collect()
}

pub(super) fn next_due(conn: &Connection, now: i64) -> Result<Option<i64>, StorageError> {
    db(stmt(
        conn,
        "SELECT min(t) FROM (SELECT min(ready_at) AS t FROM queue_entries WHERE \
            state='Ready' AND ready_at>?1 UNION ALL SELECT min(lease_until) FROM \
            queue_entries WHERE state='Claimed')",
    )?
    .query_row([now], |r| r.get(0)))
}

pub(super) fn queue_state_name(s: QueueState) -> &'static str {
    match s {
        QueueState::Ready => "Ready",
        QueueState::Claimed => "Claimed",
        QueueState::Completed => "Completed",
        QueueState::Failed => "Failed",
        QueueState::Cancelled => "Cancelled",
        QueueState::Reconciling => "Reconciling",
        QueueState::Dismissed => "Dismissed",
    }
}

pub(super) fn parse_queue_state(s: &str) -> Result<QueueState, StorageError> {
    Ok(match s {
        "Ready" => QueueState::Ready,
        "Claimed" => QueueState::Claimed,
        "Completed" => QueueState::Completed,
        "Failed" => QueueState::Failed,
        "Cancelled" => QueueState::Cancelled,
        "Reconciling" => QueueState::Reconciling,
        "Dismissed" => QueueState::Dismissed,
        other => return Err(corrupt(format!("unknown queue state {other}"))),
    })
}

impl DurableQueue for Unit<'_> {
    fn enqueue(&mut self, e: NewQueueEntry) -> Result<QueueEntry, StorageError> {
        bounded("consumer", &e.consumer, 128)?;
        object("queue payload", &e.payload)?;
        if let Some(p) = &e.partition {
            bounded("partition", p, 256)?;
        }
        if e.ready_at_ms < 0 {
            return Err(invalid("ready time must be non-negative"));
        }
        let seq: i64 = db(stmt(self.conn, "SELECT seq FROM events WHERE id=?1")?
            .query_row([e.event_id.0.as_str()], |r| r.get(0))
            .optional())?
        .ok_or_else(|| StorageError::NotFound {
            kind: "event",
            id: e.event_id.0.as_str().to_owned(),
        })?;
        require(
            self.conn,
            "work_items",
            "root",
            e.root_id.as_ref().map(|r| r.0.as_str()),
        )?;
        require(
            self.conn,
            "operations",
            "operation",
            e.operation_id.as_ref().map(|o| o.0.as_str()),
        )?;
        let duplicate: bool = db(stmt(
            self.conn,
            "SELECT 1 FROM queue_entries WHERE consumer=?1 AND event_id=?2",
        )?
        .exists(params![e.consumer, e.event_id.0.as_str()]))?;
        if duplicate
            || exists(
                self.conn,
                "SELECT 1 FROM queue_entries WHERE id=?1",
                e.id.as_str(),
            )?
        {
            return Err(StorageError::Duplicate {
                kind: "queue_delivery",
                id: format!("{}/{}", e.consumer, e.event_id.0.as_str()),
            });
        }
        if let Some(p) = &e.partition {
            // An ordered partition is appended in event order: a delivery for an older event
            // than one already enqueued or applied there would be delivered out of order.
            let later: bool = db(stmt(
                self.conn,
                "SELECT 1 FROM queue_entries WHERE consumer=?1 AND partition_key=?2 AND event_seq>?3",
            )?
            .exists(params![e.consumer, p, seq]))?;
            if later || progress_row(self.conn, &e.consumer, p)?.0 >= seq {
                return Err(invalid(
                    "ordered partition deliveries must be enqueued in event order",
                ));
            }
        }
        db(stmt(
            self.conn,
            "INSERT INTO queue_entries(id,consumer,event_id,root_id,operation_id,\
                priority,ready_at,state,payload_json,partition_key,event_seq) VALUES(?1,\
                ?2,?3,?4,?5,?6,?7,'Ready',?8,?9,?10)",
        )?
        .execute(params![
            e.id.as_str(),
            e.consumer,
            e.event_id.0.as_str(),
            e.root_id.as_ref().map(|r| r.0.as_str()),
            e.operation_id.as_ref().map(|o| o.0.as_str()),
            e.priority,
            e.ready_at_ms,
            text(&e.payload),
            e.partition,
            seq
        ]))?;
        self.reload(&e.id)
    }

    fn claim_ready(&mut self, req: &ClaimRequest) -> Result<Vec<QueueEntry>, StorageError> {
        let n = page_limit(req.limit)?;
        bounded("consumer", &req.consumer, 128)?;
        if req.lease_ms <= 0 {
            return Err(invalid("lease must be positive"));
        }
        let lease_until = self
            .now
            .checked_add(req.lease_ms)
            .ok_or_else(|| invalid("lease overflows"))?;
        // Eligible: ready now; the head of its ordered partition (no older unsettled delivery);
        // and its operation is not possibly dispatched (uncertain effects pause dependents).
        let candidates: Vec<(String, i64)> = {
            let mut s = stmt(
                self.conn,
                &format!(
                    "SELECT q.id,q.generation FROM queue_entries q WHERE q.consumer=?1 AND \
                        q.state='Ready' AND q.ready_at<=?2 \
                        AND (q.partition_key IS NULL OR NOT EXISTS (SELECT 1 FROM queue_entries \
                        p WHERE p.consumer=q.consumer AND p.partition_key=q.partition_key AND \
                        p.event_seq<q.event_seq AND p.state NOT IN {SETTLED})) \
                        AND (q.operation_id IS NULL OR NOT EXISTS (SELECT 1 FROM operations o \
                        WHERE o.id=q.operation_id AND o.state IN ('Dispatching',\
                        'OutcomeUnknown','NeedsInput'))) \
                        ORDER BY q.priority DESC,q.ready_at,q.id LIMIT ?3"
                ),
            )?;
            db(db(s.query_map(params![req.consumer, self.now, n], |r| {
                Ok((r.get(0)?, r.get(1)?))
            }))?
            .collect())?
        };
        let boot = self.boot()?.clone();
        let mut claimed = Vec::new();
        for (id, generation) in candidates {
            let changed = db(stmt(
                self.conn,
                "UPDATE queue_entries SET state='Claimed',owner_id=?1,lease_until=?2,\
                    generation=generation+1,attempt_count=attempt_count+1,\
                    revision=revision+1,claim_boot_id=?5 WHERE id=?3 AND state='Ready' AND \
                    generation=?4",
            )?
            .execute(params![
                req.owner.as_str(),
                lease_until,
                id,
                generation,
                boot.as_str()
            ]))?;
            if changed != 1 {
                continue; // lost race or no longer eligible
            }
            let e = self.reload(&id_of(id)?)?;
            self.observe_queue(
                &e,
                "QueueClaimed",
                json!({"owner": req.owner.as_str(), "generation": e.generation, "leaseUntil": lease_until, "attempt": e.attempt_count, "boot": boot.as_str()}),
            )?;
            claimed.push(e);
        }
        Ok(claimed)
    }

    fn renew(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        lease_ms: i64,
    ) -> Result<QueueEntry, StorageError> {
        let e = self.fenced(id, fence)?;
        if lease_ms <= 0 {
            return Err(invalid("lease must be positive"));
        }
        if e.lease_until_ms.is_some_and(|l| l < self.now) {
            // An expired lease is not renewable; recovery decides what happens next.
            return Err(StorageError::LeaseLost {
                kind: "queue_entry",
                id: id.as_str().to_owned(),
            });
        }
        let until = self
            .now
            .checked_add(lease_ms)
            .ok_or_else(|| invalid("lease overflows"))?;
        db(stmt(
            self.conn,
            "UPDATE queue_entries SET lease_until=?1,revision=revision+1 WHERE id=?2 AND generation=?3",
        )?
        .execute(params![until, id.as_str(), fence.generation]))?;
        self.reload(id)
    }

    fn settle(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        settlement: Settlement,
    ) -> Result<QueueEntry, StorageError> {
        let e = self.fenced(id, fence)?;
        let state = match settlement {
            Settlement::Completed => QueueState::Completed,
            Settlement::Cancelled => {
                self.cancel_operation(&e)?;
                QueueState::Cancelled
            }
        };
        // A completion clears the visible failure; a cancellation keeps it as evidence.
        db(stmt(
            self.conn,
            "UPDATE queue_entries SET state=?1,owner_id=NULL,lease_until=NULL,\
                claim_boot_id=NULL,revision=revision+1,last_error=CASE WHEN \
                ?1='Completed' THEN NULL ELSE last_error END WHERE id=?2 AND \
                generation=?3",
        )?
        .execute(params![
            queue_state_name(state),
            id.as_str(),
            fence.generation
        ]))?;
        let after = self.reload(id)?;
        self.settled(&after)?;
        Ok(after)
    }

    fn fail(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        cause: &str,
        disposition: FailureDisposition,
    ) -> Result<QueueEntry, StorageError> {
        bounded("failure cause", cause, 1024)?;
        let e = self.fenced(id, fence)?;
        let dispatched = match &e.operation_id {
            Some(op) => possibly_dispatched(self.load_op(op)?.state),
            None => false,
        };
        let (state, ready_at) = match disposition {
            FailureDisposition::Retry {
                delay_ms,
                max_attempts,
            } => {
                if delay_ms < 0 || max_attempts <= 0 {
                    return Err(invalid("retry delay/attempt bounds"));
                }
                if dispatched {
                    // A retry hint never authorizes replaying an effect that may have happened,
                    // and an exhausted budget never turns it into poison: hold the delivery for
                    // reconciliation, exactly as claim recovery does.
                    (QueueState::Reconciling, e.ready_at_ms)
                } else if self.cancel_applies(&e)? {
                    // A cancellation requested during the attempt wins over its retry.
                    return self.settle_cancelled(&e, cause);
                } else if e.attempt_count >= max_attempts {
                    (QueueState::Failed, e.ready_at_ms)
                } else {
                    (
                        QueueState::Ready,
                        self.now
                            .checked_add(delay_ms)
                            .ok_or_else(|| invalid("retry delay overflows"))?,
                    )
                }
            }
            // A possibly dispatched effect is held, never poison.
            FailureDisposition::Terminal if dispatched => (QueueState::Reconciling, e.ready_at_ms),
            FailureDisposition::Terminal => (QueueState::Failed, e.ready_at_ms),
        };
        let failed_at = (state != QueueState::Ready).then_some(self.now);
        db(stmt(
            self.conn,
            "UPDATE queue_entries SET state=?1,ready_at=?2,owner_id=NULL,\
                lease_until=NULL,claim_boot_id=NULL,revision=revision+1,last_error=?5,\
                failed_at=coalesce(?6,failed_at) WHERE id=?3 AND generation=?4",
        )?
        .execute(params![
            queue_state_name(state),
            ready_at,
            id.as_str(),
            fence.generation,
            cause,
            failed_at
        ]))?;
        let after = self.reload(id)?;
        self.observe_queue(
            &after,
            "QueueAttemptFailed",
            json!({
                "cause": cause,
                "attempt": after.attempt_count,
                "outcome": queue_state_name(state),
                "readyAt": ready_at,
                "terminal": state == QueueState::Failed,
                "held": state == QueueState::Reconciling,
            }),
        )?;
        Ok(after)
    }

    fn cancel(&mut self, id: &Id) -> Result<QueueEntry, StorageError> {
        let e = self.queue_entry(id)?;
        let op_state = match &e.operation_id {
            Some(op) => Some(self.load_op(op)?.state),
            None => None,
        };
        let request = |me: &mut Self| -> Result<QueueEntry, StorageError> {
            db(stmt(
                me.conn,
                "UPDATE queue_entries SET cancellation_epoch=cancellation_epoch+1,cancel_requested=1,revision=revision+1 WHERE id=?1",
            )?
            .execute([id.as_str()]))?;
            let after = me.reload(id)?;
            me.observe_queue(
                &after,
                "QueueCancellationRequested",
                json!({"state": queue_state_name(after.state)}),
            )?;
            Ok(after)
        };
        match e.state {
            // The effect may have happened: record the request; reconciliation decides.
            QueueState::Reconciling if op_state.is_some_and(possibly_dispatched) => request(self),
            QueueState::Ready | QueueState::Reconciling => {
                // The same rule as the dispatcher: an operation already cancelled before
                // dispatch is an idempotent no-op, an established outcome is refused.
                if !cancellation_applies(self, &e)? {
                    return Err(invalid(
                        "the delivery's operation already has an outcome; it cannot be cancelled",
                    ));
                }
                db(stmt(
                    self.conn,
                    "UPDATE queue_entries SET cancellation_epoch=cancellation_epoch+1,revision=revision+1 WHERE id=?1",
                )?
                .execute([id.as_str()]))?;
                let e = self.reload(id)?;
                self.settle_cancelled(&e, "cancelled before dispatch")
            }
            QueueState::Claimed => request(self),
            QueueState::Cancelled => Ok(e),
            QueueState::Completed | QueueState::Failed | QueueState::Dismissed => {
                Err(invalid("a settled delivery cannot be cancelled"))
            }
        }
    }

    fn recover_claims(
        &mut self,
        mode: ClaimRecovery,
        max: u16,
    ) -> Result<RecoveryOutcome, StorageError> {
        let n = page_limit(max)?;
        let stale: Vec<String> = {
            let sql = match mode {
                ClaimRecovery::Expired => {
                    "SELECT id FROM queue_entries WHERE state='Claimed' AND lease_until<?1 ORDER BY lease_until,id LIMIT ?2"
                }
                ClaimRecovery::PriorBoots => {
                    "SELECT id FROM queue_entries WHERE state='Claimed' AND (claim_boot_id IS NULL OR claim_boot_id<>?1) ORDER BY lease_until,id LIMIT ?2"
                }
            };
            let mut s = stmt(self.conn, sql)?;
            match mode {
                ClaimRecovery::Expired => {
                    db(db(s.query_map(params![self.now, n], |r| r.get(0)))?.collect())?
                }
                ClaimRecovery::PriorBoots => {
                    let boot = self.boot()?.clone();
                    db(db(s.query_map(params![boot.as_str(), n], |r| r.get(0)))?.collect())?
                }
            }
        };
        let mut out = RecoveryOutcome::default();
        for raw in stale {
            let id = id_of(raw)?;
            let e = self.reload(&id)?;
            let dispatched = match &e.operation_id {
                Some(op) => possibly_dispatched(self.load_op(op)?.state),
                // Handler database effects commit atomically with completion, so redelivering an
                // entry without an external operation is safe (at-least-once).
                None => false,
            };
            if !dispatched && self.cancel_applies(&e)? {
                self.settle_cancelled(&e, "cancellation requested before claim recovery")?;
                out.cancelled.push(id);
                continue;
            }
            // Root-scoped work of a dead process is not continued automatically: the root may
            // have been paused or held meanwhile, so it waits until its eligibility is confirmed.
            let awaiting = mode == ClaimRecovery::PriorBoots && e.root_id.is_some();
            let state = if dispatched || awaiting {
                QueueState::Reconciling
            } else {
                QueueState::Ready
            };
            let failed_at = (state == QueueState::Reconciling).then_some(self.now);
            db(stmt(
                self.conn,
                "UPDATE queue_entries SET state=?1,owner_id=NULL,lease_until=NULL,\
                    claim_boot_id=NULL,generation=generation+1,revision=revision+1,\
                    awaiting_eligibility=?4,failed_at=coalesce(?5,failed_at) WHERE id=?2 \
                    AND state='Claimed' AND generation=?3",
            )?
            .execute(params![
                queue_state_name(state),
                id.as_str(),
                e.generation,
                awaiting,
                failed_at
            ]))?;
            let after = self.reload(&id)?;
            self.observe_queue(
                &after,
                "QueueClaimRecovered",
                json!({
                    "previousOwner": e.owner.as_ref().map(Id::as_str),
                    "previousGeneration": e.generation,
                    "outcome": queue_state_name(state),
                    "mode": format!("{mode:?}"),
                    "awaitingEligibility": awaiting,
                }),
            )?;
            if state == QueueState::Reconciling {
                out.held.push(id);
            } else {
                out.requeued.push(id);
            }
        }
        Ok(out)
    }

    fn requeue(&mut self, id: &Id, delay_ms: i64) -> Result<QueueEntry, StorageError> {
        let e = self.queue_entry(id)?;
        if !matches!(e.state, QueueState::Reconciling | QueueState::Failed) {
            return Err(invalid("only held or failed deliveries can be requeued"));
        }
        if delay_ms < 0 {
            return Err(invalid("delay must be non-negative"));
        }
        self.not_possibly_dispatched(&e, "a retry")?;
        if self.cancel_applies(&e)? {
            return self.settle_cancelled(&e, "cancellation requested before the retry");
        }
        let ready = self
            .now
            .checked_add(delay_ms)
            .ok_or_else(|| invalid("delay overflows"))?;
        // From Failed the retry budget restarts; the failure cause stays visible until the
        // delivery completes.
        let attempts = if e.state == QueueState::Failed {
            0
        } else {
            e.attempt_count
        };
        db(stmt(
            self.conn,
            "UPDATE queue_entries SET state='Ready',ready_at=?1,attempt_count=?4,\
                generation=generation+1,revision=revision+1,awaiting_eligibility=0 \
                WHERE id=?2 AND generation=?3",
        )?
        .execute(params![ready, id.as_str(), e.generation, attempts]))?;
        let after = self.reload(id)?;
        self.observe_queue(
            &after,
            "QueueRequeued",
            json!({"readyAt": ready, "from": queue_state_name(e.state), "previousAttempts": e.attempt_count}),
        )?;
        Ok(after)
    }

    fn hold(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        cause: &str,
    ) -> Result<QueueEntry, StorageError> {
        bounded("hold cause", cause, 1024)?;
        self.fenced(id, fence)?;
        db(stmt(
            self.conn,
            "UPDATE queue_entries SET state='Reconciling',owner_id=NULL,\
                lease_until=NULL,claim_boot_id=NULL,generation=generation+1,\
                revision=revision+1,last_error=?1,failed_at=?4 WHERE id=?2 AND \
                generation=?3",
        )?
        .execute(params![cause, id.as_str(), fence.generation, self.now]))?;
        let after = self.reload(id)?;
        self.observe_queue(&after, "QueueDeliveryHeld", json!({"cause": cause}))?;
        Ok(after)
    }

    fn hold_for_eligibility(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        cause: &str,
    ) -> Result<QueueEntry, StorageError> {
        bounded("hold cause", cause, 1024)?;
        let e = self.fenced(id, fence)?;
        if e.root_id.is_none() {
            return Err(invalid(
                "only root-scoped deliveries wait for root eligibility",
            ));
        }
        db(stmt(
            self.conn,
            "UPDATE queue_entries SET state='Reconciling',owner_id=NULL,\
                lease_until=NULL,claim_boot_id=NULL,generation=generation+1,\
                revision=revision+1,last_error=?1,failed_at=?4,awaiting_eligibility=1 \
                WHERE id=?2 AND generation=?3",
        )?
        .execute(params![cause, id.as_str(), fence.generation, self.now]))?;
        let after = self.reload(id)?;
        self.observe_queue(
            &after,
            "QueueDeliveryAwaitingEligibility",
            json!({"cause": cause}),
        )?;
        Ok(after)
    }

    fn dismiss(&mut self, id: &Id, reason: &str) -> Result<QueueEntry, StorageError> {
        if reason.len() > 65_536 || reason.contains('\0') {
            return Err(invalid("dismissal reason bounds"));
        }
        let e = self.queue_entry(id)?;
        if e.state != QueueState::Failed {
            return Err(invalid("only failed deliveries can be dismissed"));
        }
        self.not_possibly_dispatched(&e, "dismissal")?;
        db(stmt(self.conn,
            "UPDATE queue_entries SET state='Dismissed',revision=revision+1 WHERE id=?1 AND state='Failed' AND generation=?2",
        )?
        .execute(params![id.as_str(), e.generation]))?;
        let after = self.reload(id)?;
        self.settled(&after)?;
        self.observe_queue(
            &after,
            "QueueDeliveryDismissed",
            json!({"reason": reason, "cause": after.last_error}),
        )?;
        Ok(after)
    }

    fn reassert(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        lease_ms: i64,
    ) -> Result<QueueEntry, StorageError> {
        let e = self.fenced(id, fence)?;
        if lease_ms <= 0 {
            return Err(invalid("lease must be positive"));
        }
        let until = self
            .now
            .checked_add(lease_ms)
            .ok_or_else(|| invalid("lease overflows"))?;
        db(stmt(
            self.conn,
            "UPDATE queue_entries SET lease_until=?1,revision=revision+1 WHERE id=?2 AND generation=?3",
        )?
        .execute(params![until, id.as_str(), fence.generation]))?;
        let after = self.reload(id)?;
        if e.lease_until_ms.is_some_and(|l| l < self.now) {
            self.observe_queue(
                &after,
                "QueueLeaseReasserted",
                json!({"owner": fence.owner.as_str(), "expiredAt": e.lease_until_ms, "leaseUntil": until}),
            )?;
        }
        Ok(after)
    }

    fn compact(&mut self, id: &Id) -> Result<(), StorageError> {
        let e = self.queue_entry(id)?;
        if !e.state.is_settled() {
            return Err(invalid(
                "only completed, cancelled or dismissed deliveries can be compacted",
            ));
        }
        // Tombstone: the row (and its UNIQUE(consumer, event_id)) stays; its bookkeeping goes.
        db(stmt(self.conn,
            "UPDATE queue_entries SET state='Compacted',payload_json='{}',owner_id=NULL,lease_until=NULL,claim_boot_id=NULL WHERE id=?1",
        )?
        .execute([id.as_str()]))?;
        Ok(())
    }
}

impl Unit<'_> {
    /// Store-owned bookkeeping event attributed to the host actor.
    pub(super) fn observe_queue(
        &mut self,
        entry: &QueueEntry,
        kind: &str,
        payload: Value,
    ) -> Result<i64, StorageError> {
        let id = EventId(self.ids.next_id(IdKind::Event));
        self.append(NewEvent {
            id,
            actor: self.info.host_actor.clone(),
            project_id: None,
            root_id: entry.root_id.clone(),
            operation_id: entry.operation_id.clone(),
            aggregate_kind: "queue_entry".into(),
            aggregate_id: entry.id.as_str().to_owned(),
            aggregate_revision: entry.revision.max(1),
            kind: kind.into(),
            payload,
        })
    }

    pub(super) fn fenced(&self, id: &Id, fence: &QueueFence) -> Result<QueueEntry, StorageError> {
        let e = entry(self.conn, id)?.ok_or_else(|| StorageError::NotFound {
            kind: "queue_entry",
            id: id.as_str().to_owned(),
        })?;
        if e.state != QueueState::Claimed
            || e.owner.as_ref() != Some(&fence.owner)
            || e.generation != fence.generation
            || e.claim_boot_id.as_ref() != self.info.boot_id.as_ref()
        {
            return Err(StorageError::LeaseLost {
                kind: "queue_entry",
                id: id.as_str().to_owned(),
            });
        }
        Ok(e)
    }

    pub(super) fn reload(&self, id: &Id) -> Result<QueueEntry, StorageError> {
        entry(self.conn, id)?.ok_or_else(|| corrupt("queue entry vanished inside its transaction"))
    }
}

impl Unit<'_> {
    pub(super) fn queue_entry(&self, id: &Id) -> Result<QueueEntry, StorageError> {
        entry(self.conn, id)?.ok_or_else(|| StorageError::NotFound {
            kind: "queue_entry",
            id: id.as_str().to_owned(),
        })
    }

    /// Records a settlement in the consumer's progress rows: the reserved `''` row counts
    /// settlements and keeps the highest settled sequence; an ordered partition's row is its
    /// contiguous checkpoint. A delivery behind the partition's head can be settled first (an
    /// operator cancels a queued delivery), so the checkpoint is recomputed as the settled prefix:
    /// the highest settled event sequence below the lowest unsettled delivery of the partition
    /// (or the highest settled one when nothing is unsettled). It is always the sequence of a
    /// settled delivery of this partition, never a sequence in between, so a checkpoint an
    /// in-flight invocation read stays valid while deliveries behind it settle.
    pub(super) fn settled(&self, e: &QueueEntry) -> Result<(), StorageError> {
        db(stmt(
            self.conn,
            "INSERT INTO consumer_progress(consumer,partition_key,applied_seq,\
                revision) VALUES(?1,'',?2,1) ON CONFLICT(consumer,partition_key) DO \
                UPDATE SET applied_seq=max(applied_seq,excluded.applied_seq),\
                revision=revision+1",
        )?
        .execute(params![e.consumer, e.event_sequence]))?;
        if let Some(p) = e.partition.as_deref() {
            let prefix: Option<i64> = db(stmt(
                self.conn,
                &format!(
                    "SELECT max(event_seq) FROM queue_entries WHERE consumer=?1 AND \
                        partition_key=?2 AND state IN {SETTLED} AND event_seq<coalesce((SELECT \
                        min(event_seq) FROM queue_entries WHERE consumer=?1 AND \
                        partition_key=?2 AND state NOT IN {SETTLED}),9223372036854775807)"
                ),
            )?
            .query_row(params![e.consumer, p], |r| r.get(0)))?;
            db(stmt(
                self.conn,
                "INSERT INTO consumer_progress(consumer,partition_key,applied_seq,\
                    revision) VALUES(?1,?2,?3,1) ON CONFLICT(consumer,partition_key) DO \
                    UPDATE SET applied_seq=max(applied_seq,excluded.applied_seq),\
                    revision=revision+1",
            )?
            .execute(params![e.consumer, p, prefix.unwrap_or(0)]))?;
        }
        Ok(())
    }

    /// Cancels the delivery's operation before dispatch. Refused when the effect may have started
    /// or already has an outcome; an operation already cancelled before dispatch is left as is.
    pub(super) fn cancel_operation(&mut self, e: &QueueEntry) -> Result<(), StorageError> {
        let Some(op_id) = &e.operation_id else {
            return Ok(());
        };
        let op = self.load_op(op_id)?;
        if op.state == OperationState::CancelledBeforeDispatch {
            return Ok(());
        }
        if !cancellable_before_dispatch(op.state) {
            return Err(invalid(format!(
                "the delivery's operation is {}; only an undispatched operation can be cancelled",
                state_name(op.state)
            )));
        }
        // A preflighted attempt is closed under its own fence, with the reason as its
        // observation; an operation without a live attempt has nothing to attribute it to.
        let fence = match op.state {
            OperationState::Preflighted => op.latest_attempt().map(AttemptRecord::fence),
            _ => None,
        };
        let note = fence
            .as_ref()
            .map(|_| json!({"cancelledWithDelivery": e.id.as_str()}));
        OperationJournal::transition(
            self,
            op_id,
            op.revision,
            fence.as_ref(),
            OperationState::CancelledBeforeDispatch,
            note,
        )?;
        Ok(())
    }

    /// Whether a requested cancellation applies instead of making `e` ready again: there is no
    /// operation, or it has certainly not dispatched. An established outcome (`ObservedComplete`,
    /// `Failed`) must still be recorded by redelivery, so the request cannot apply to it.
    pub(super) fn cancel_applies(&self, e: &QueueEntry) -> Result<bool, StorageError> {
        Ok(e.cancel_requested && cancellation_applies(self, e)?)
    }

    /// Settles an unclaimed or claimed delivery `Cancelled` with its undispatched operation.
    pub(super) fn settle_cancelled(
        &mut self,
        e: &QueueEntry,
        cause: &str,
    ) -> Result<QueueEntry, StorageError> {
        self.cancel_operation(e)?;
        db(stmt(
            self.conn,
            "UPDATE queue_entries SET state='Cancelled',owner_id=NULL,\
                lease_until=NULL,claim_boot_id=NULL,awaiting_eligibility=0,\
                revision=revision+1 WHERE id=?1 AND generation=?2",
        )?
        .execute(params![e.id.as_str(), e.generation]))?;
        let after = self.reload(&e.id)?;
        self.settled(&after)?;
        self.observe_queue(&after, "QueueDeliveryCancelled", json!({"cause": cause}))?;
        Ok(after)
    }

    pub(super) fn not_possibly_dispatched(
        &self,
        e: &QueueEntry,
        action: &str,
    ) -> Result<(), StorageError> {
        if let Some(op) = &e.operation_id
            && possibly_dispatched(self.load_op(op)?.state)
        {
            return Err(invalid(format!(
                "the delivery's operation outcome is unresolved; reconcile it before {action}"
            )));
        }
        Ok(())
    }
}
