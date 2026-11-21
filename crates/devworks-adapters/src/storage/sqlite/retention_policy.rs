//! Closed eligibility rules for operational records. These predicates never select payloads.
use devworks_core::ports::storage::RetentionKind;

pub(super) const DIAGNOSTIC_HOLD: &str = "
    CASE
      WHEN r.origin <> 'Host' OR EXISTS (
        SELECT 1 FROM actors a WHERE a.id=r.actor_id AND a.kind='human'
      ) THEN 'AuthoredHistory'
      WHEN r.evidence_json IS NOT NULL OR json_array_length(r.recovery_json)>0
        OR EXISTS (SELECT 1 FROM history_links h WHERE h.target_kind IN ('Diagnostic','DiagnosticRecord')
                   AND h.target_id=r.id) THEN 'PublishedEvidence'
      WHEN (r.outcome='Running' AND r.started_at IS NOT NULL AND r.finished_at IS NULL)
        OR (r.operation_id IS NOT NULL AND NOT EXISTS (
          SELECT 1 FROM operations op WHERE op.id=r.operation_id
            AND op.state IN ('ObservedComplete','CancelledBeforeDispatch')
        )) THEN 'LiveOperation'
    END";

const INDEX_HOLD: &str = "
    CASE
      WHEN r.state IN ('Queued','Running') OR r.owner IS NOT NULL THEN 'LiveOperation'
      WHEN EXISTS (
        SELECT 1 FROM index_overlays o
        WHERE o.run_id=r.id
          OR r.generation_id IN (o.base_generation_id,o.branch_generation_id,o.worktree_generation_id)
          OR o.view_id=r.output_view_id
      ) OR EXISTS (
        SELECT 1 FROM index_views v
        WHERE v.id=r.output_view_id OR r.generation_id IN (v.baseline_id,v.branch_id,v.worktree_id)
      ) OR EXISTS (
        SELECT 1 FROM index_batches b WHERE b.generation_id=r.generation_id
      ) OR EXISTS (
        SELECT 1 FROM history_links h WHERE h.target_kind='IndexRun' AND h.target_id=r.id
      ) THEN 'PublishedEvidence'
      WHEN NOT EXISTS (
        SELECT 1 FROM index_runs n WHERE n.repository_id=r.repository_id
          AND COALESCE(n.overlay_id,'')=COALESCE(r.overlay_id,'')
          AND (n.created_at,n.id)>(r.created_at,r.id)
      ) OR EXISTS (
        SELECT 1 FROM index_runs n WHERE n.repository_id=r.repository_id
          AND COALESCE(n.overlay_id,'')=COALESCE(r.overlay_id,'')
          AND n.state IN ('Queued','Running')
      ) THEN 'CurrentWitness'
      WHEN r.state='Failed' AND NOT EXISTS (
        SELECT 1 FROM index_runs n WHERE n.repository_id=r.repository_id
          AND COALESCE(n.overlay_id,'')=COALESCE(r.overlay_id,'')
          AND n.source_oid=r.source_oid AND n.config_digest=r.config_digest
          AND n.state='Succeeded' AND n.finished_at>COALESCE(r.finished_at,r.created_at)
          AND EXISTS (SELECT 1 FROM index_views v WHERE v.id=n.output_view_id)
      ) THEN 'UnresolvedFailure'
    END";

const MESSAGE_HOLD: &str = "
    CASE
      WHEN r.channel<>'Session' OR r.to_human=1 OR r.root_id IS NOT NULL
        OR r.target_item_id IS NOT NULL OR r.target_feature_id IS NOT NULL
        OR EXISTS (SELECT 1 FROM actors a WHERE a.id=r.from_actor_id AND a.kind NOT IN ('agent','host','schedule'))
        OR EXISTS (SELECT 1 FROM actors a WHERE a.id=r.resolved_by_actor_id AND a.kind='human')
        OR EXISTS (
          SELECT 1 FROM sessions s JOIN actors a ON a.id=s.created_by
          WHERE s.id=r.target_session_id AND a.kind NOT IN ('agent','host','schedule')
        ) OR EXISTS (
          SELECT 1 FROM sessions s JOIN actors a ON a.id=s.agent_id
          WHERE s.id=r.target_session_id AND a.kind NOT IN ('agent','host','schedule')
        ) THEN 'AuthoredHistory'
      WHEN r.blocking=1 OR r.state='Open' THEN 'OpenConversation'
      WHEN r.message_class NOT IN ('Info','Status','Announcement')
        OR json_array_length(r.attachments_json) IS NOT 0
        OR r.in_reply_to_id IS NOT NULL
        OR EXISTS (SELECT 1 FROM agent_messages m WHERE m.in_reply_to_id=r.id)
        OR EXISTS (SELECT 1 FROM history_links h
                   WHERE h.target_kind IN ('Message','AgentMessage') AND h.target_id=r.id)
        THEN 'Referenced'
      WHEN r.idempotency_key IS NOT NULL OR EXISTS (
        SELECT 1 FROM agent_message_deliveries d
        WHERE d.message_id=r.id AND d.sequence=(SELECT head FROM agent_message_sequence_heads WHERE kind='delivery')
      ) OR EXISTS (
        SELECT 1 FROM agent_message_events e
        WHERE e.message_id=r.id AND e.sequence=(SELECT head FROM agent_message_sequence_heads WHERE kind='event')
      ) THEN 'ReplayHead'
      WHEN r.target_session_id IS NULL OR NOT EXISTS (
        SELECT 1 FROM agent_message_deliveries d WHERE d.message_id=r.id
      ) OR EXISTS (
        SELECT 1 FROM agent_message_deliveries d WHERE d.message_id=r.id
          AND (d.state IN ('Pending','Delivered') OR d.recipient_human=1
               OR d.recipient_assignment_id IS NOT NULL OR d.read_at IS NULL)
      ) THEN 'PendingDelivery'
      WHEN EXISTS (
        SELECT 1 FROM agent_message_deliveries d
        LEFT JOIN sessions s ON s.id=d.recipient_session_id
        LEFT JOIN sessions handover ON handover.id=d.handed_to_session_id
        WHERE d.message_id=r.id AND (
          s.closed_at IS NULL OR s.closed_at>=:floor
          OR (d.handed_to_session_id IS NOT NULL AND
              (handover.closed_at IS NULL OR handover.closed_at>=:floor))
          OR max(COALESCE(d.delivered_at,0),COALESCE(d.read_at,0),COALESCE(d.resolved_at,0))>=:cutoff
        )
      ) OR EXISTS (
        SELECT 1 FROM agent_message_events e
        JOIN actors a ON a.id=e.actor_id
        WHERE e.message_id=r.id AND (a.kind='human' OR e.recipient_human=1 OR e.created_at>=:cutoff)
      ) OR COALESCE(r.resolved_at,0)>=:cutoff THEN 'HandoverWindow'
    END";

// These rows belong to their existing owners. Keep the session while any refers to it.
pub(super) const SESSION_FOREIGN_REFERENCES: &[(&str, &str)] = &[
    ("agent_message_deliveries", "handed_to_session_id"),
    ("agent_message_deliveries", "recipient_session_id"),
    ("agent_message_events", "recipient_session_id"),
    ("agent_messages", "resolved_by_session_id"),
    ("agent_messages", "sender_session_id"),
    ("agent_messages", "target_session_id"),
    ("allocation_claims", "session_id"),
    ("assignments", "session_id"),
    ("eval_runs", "session_id"),
    ("knowledge_deliveries", "session_id"),
    ("router_auto_sessions", "session_id"),
    ("run_instructions", "session_id"),
    ("session_checkpoints", "session_id"),
    ("session_context_epochs", "session_id"),
    ("session_context_sources", "session_id"),
    ("session_provider_context_heads", "session_id"),
    ("session_tool_calls", "session_id"),
    ("session_turns", "session_id"),
    ("tool_invocations", "session_id"),
    ("transcript_batches", "session_id"),
    ("transcript_chunks", "session_id"),
    ("usage_attempts", "session_id"),
    ("usage_origins", "session_id"),
    ("watcher_events", "session_id"),
    ("watchers", "session_id"),
];

const SESSION_LOGICAL_REFERENCES: &[(&str, &str)] = &[
    ("browser_sessions", "agent_session_id"),
    ("job_runs", "session_id"),
    ("knowledge_claims", "session_id"),
    ("knowledge_lesson_events", "session_id"),
    ("knowledge_source_deletions", "session_id"),
    ("knowledge_writes", "session_id"),
    ("owned_containers", "session_id"),
    ("owned_processes", "session_id"),
    ("resource_leases", "session_id"),
    ("router_pins", "session_id"),
    ("router_requests", "session_id"),
    ("usage_bootstrap", "session_id"),
];

fn session_hold() -> String {
    let references: String = SESSION_FOREIGN_REFERENCES
        .iter()
        .chain(SESSION_LOGICAL_REFERENCES)
        .map(|(table, column)| {
            format!(" OR EXISTS (SELECT 1 FROM {table} q WHERE q.{column}=r.id)")
        })
        .collect();
    format!(
        "CASE
          WHEN r.closed_at IS NULL OR r.closed_by IS NULL THEN 'LiveOperation'
          WHEN r.closed_at>=:floor THEN 'HandoverWindow'
          WHEN r.root_id IS NOT NULL OR r.pin_json IS NOT NULL
            OR NOT EXISTS (SELECT 1 FROM actors a WHERE a.id=r.created_by
                           AND a.kind IN ('agent','host','schedule'))
            OR NOT EXISTS (SELECT 1 FROM actors a WHERE a.id=r.agent_id
                           AND a.kind IN ('agent','host','schedule')) THEN 'AuthoredHistory'
          WHEN r.checkpoint_seq>0 OR r.next_seq>1
            OR EXISTS (SELECT 1 FROM history_links h WHERE h.target_kind='Session' AND h.target_id=r.id)
            OR EXISTS (SELECT 1 FROM conversations c WHERE c.id=r.conversation_id AND c.root_id IS NOT NULL)
            {references} THEN 'Referenced'
        END"
    )
}

pub(super) fn table(kind: RetentionKind) -> &'static str {
    match kind {
        RetentionKind::Diagnostics => "diagnostic_records",
        RetentionKind::IndexRuns => "index_runs",
        RetentionKind::Messages => "agent_messages",
        RetentionKind::Sessions => "sessions",
    }
}

/// The coarse cutoff uses an indexed column where available. The exact activity date
/// can be later, in which case the record cannot make the approved candidate list.
pub(super) fn select(kind: RetentionKind, predicate: &str) -> String {
    let table = table(kind);
    let (revision, age, activity, hold) = match kind {
        RetentionKind::Diagnostics => (
            "r.sequence",
            "r.recorded_at",
            "r.recorded_at",
            DIAGNOSTIC_HOLD.to_owned(),
        ),
        RetentionKind::IndexRuns => (
            "r.revision",
            "COALESCE(r.finished_at,r.created_at)",
            "COALESCE(r.finished_at,r.created_at)",
            INDEX_HOLD.to_owned(),
        ),
        RetentionKind::Messages => (
            "r.revision",
            "r.created_at",
            "r.created_at",
            MESSAGE_HOLD.to_owned(),
        ),
        RetentionKind::Sessions => (
            "r.revision",
            "max(r.created_at,COALESCE(r.last_seen_at,0),COALESCE(r.closed_at,0))",
            "max(r.created_at,COALESCE(r.last_seen_at,0),COALESCE(r.closed_at,0))",
            session_hold(),
        ),
    };
    // Common named bindings are present for all kinds, including the exact-ID reads.
    format!(
        "SELECT r.id, {revision} AS revision, {activity} AS activity,
                CASE WHEN {activity}>=:cutoff THEN 'HandoverWindow' ELSE {hold} END AS hold
         FROM {table} r WHERE {age}<=:cutoff AND :floor>=0 AND {predicate}"
    )
}
