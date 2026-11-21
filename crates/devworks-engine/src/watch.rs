//! Watcher supervision (DW-94). Watched processes run on their own process-effect consumer
//! ([`crate::process_tools::watch_consumer`]); this supervisor's pass settles each watcher whose
//! process outcome is observed and delivers its event into the owning session's input. A pass
//! reads only durable state, so a restart between an event's commit and its delivery delivers it
//! on the next pass, once.
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::ports::storage::{
    ApplicationStore, MAX_PAGE, ReadOptions, ReadSnapshot, StorageError, WatchEventState,
    WatcherFilter, WatcherPhase, read_snapshot,
};
use devworks_core::ports::{IdKind, IdSource};
use devworks_core::protocol::provider::TranscriptBody;
use devworks_core::protocol::{Counter, Id, Revision};
use devworks_core::services::watch::{WATCHER_NOTICE_MEDIA_TYPE, WatchService, WatchTick};
use std::sync::Arc;

pub struct WatchSupervisor {
    store: Arc<dyn ApplicationStore>,
    ids: Arc<dyn IdSource>,
    service: Arc<WatchService>,
}

impl WatchSupervisor {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        ids: Arc<dyn IdSource>,
        service: Arc<WatchService>,
    ) -> Self {
        Self {
            store,
            ids,
            service,
        }
    }

    pub fn service(&self) -> &Arc<WatchService> {
        &self.service
    }

    fn host(&self) -> InvocationContext {
        let info = self.store.info();
        HostIngress::new(&*self.store).context(
            info.host_actor,
            PrincipalKind::Host,
            self.ids.next_id(IdKind::Invocation),
            Id::new("watch-supervisor").expect("static ingress identity"),
            Counter::new(info.restore_epoch).expect("nonnegative restore epoch"),
            Revision::new(1).expect("positive policy revision"),
        )
    }

    /// One blocking pass: settle observed outcomes, then deliver pending events.
    pub fn tick(&self) -> Result<WatchTick, StorageError> {
        self.service.tick(&self.host())
    }
    /// The settling half of a pass alone: events are recorded, not yet delivered.
    pub fn settle(&self) -> Result<usize, StorageError> {
        self.service.settle_observed()
    }
    /// The last pass of an orderly shutdown, after owned trees were stopped.
    pub fn close_at_shutdown(
        &self,
        stopped: &std::collections::HashSet<Id>,
        deadline: std::time::Instant,
    ) -> Result<usize, StorageError> {
        self.service.close_at_shutdown(stopped, deadline)
    }
    /// The watcher backup check when it is due (its setting, interval and unfinished watchers).
    pub fn backup_if_due(
        &self,
    ) -> Result<Option<devworks_core::services::watch::BackupPass>, StorageError> {
        self.service.backup_if_due(&self.host())
    }
    /// The delivering half of a pass alone.
    pub fn deliver(&self) -> Result<usize, StorageError> {
        self.service.deliver_pending(&self.host())
    }
}

pub(crate) enum AgentInput {
    Ready,
    Waiting,
    Finished,
}

pub(crate) fn is_notice(body: &TranscriptBody) -> bool {
    matches!(body, TranscriptBody::HostNotice { artifact, .. }
        if artifact.media_type == WATCHER_NOTICE_MEDIA_TYPE)
}

fn has_events(
    snapshot: &dyn ReadSnapshot,
    session: &Id,
    states: &[WatchEventState],
) -> Result<bool, StorageError> {
    Ok(!snapshot
        .watchers()
        .watch_events(session, states, 1)?
        .is_empty())
}

/// A summary may omit or paraphrase notices. Carry their exact, host-authored transcript input
/// into the new epoch while they still await a completed generation turn.
pub(crate) fn unacknowledged_notices(
    store: &dyn ApplicationStore,
    session: &Id,
    through: i64,
) -> Result<Vec<TranscriptBody>, StorageError> {
    read_snapshot(store, &ReadOptions::default(), |snapshot| {
        let mut notices = Vec::new();
        let mut position = 0;
        loop {
            let page = snapshot
                .watchers()
                .delivered_watch_events(session, position, through, MAX_PAGE)?;
            for event in &page {
                let sequence = event
                    .transcript_sequence
                    .filter(|sequence| *sequence > position)
                    .ok_or_else(|| StorageError::InvalidRecord("watcher notice sequence".into()))?;
                let entry = snapshot
                    .sessions()
                    .entries(session, sequence - 1, 1)?
                    .pop()
                    .filter(|entry| entry.sequence == sequence)
                    .ok_or_else(|| {
                        StorageError::InvalidRecord("watcher notice transcript gap".into())
                    })?;
                let body = serde_json::from_value::<TranscriptBody>(entry.body).map_err(|_| {
                    StorageError::InvalidRecord("watcher notice transcript body".into())
                })?;
                if !matches!(&body, TranscriptBody::HostNotice { source, artifact }
                    if source == &event.id && artifact.media_type == WATCHER_NOTICE_MEDIA_TYPE)
                {
                    return Err(StorageError::InvalidRecord(
                        "watcher notice identity".into(),
                    ));
                }
                notices.push(body);
                position = sequence;
            }
            if page.len() < usize::from(MAX_PAGE) {
                return Ok(notices);
            }
        }
    })
}

fn notice_since(
    snapshot: &dyn ReadSnapshot,
    owner: &devworks_core::ports::storage::StoredSession,
    mut position: i64,
) -> Result<bool, StorageError> {
    while position < owner.last_durable_sequence() {
        let entries = snapshot.sessions().entries(&owner.id, position, 500)?;
        if entries.is_empty() {
            return Err(StorageError::InvalidRecord(
                "watcher input transcript gap".into(),
            ));
        }
        for entry in entries {
            position = entry.sequence;
            if entry.kind == "HostNotice"
                && serde_json::from_value::<TranscriptBody>(entry.body)
                    .is_ok_and(|body| is_notice(&body))
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// A coordinator draining children can still accept input after its last provider response.
pub(crate) async fn notice_after_input(
    engine: &crate::turn_engine::TurnEngine,
    session: &Id,
    after: i64,
) -> Result<bool, StorageError> {
    let session = session.clone();
    crate::store::read(engine.store(), move |snapshot| {
        let owner = snapshot
            .sessions()
            .session(&session)?
            .ok_or(StorageError::ScopeDenied)?;
        notice_since(snapshot, &owner, after)
    })
    .await
}

/// An idle assignment stays live while its watchers can still produce input. The turn runner
/// waits without an in-flight turn and starts another one only for a delivered event or reminder.
pub(crate) async fn agent_input(
    engine: &crate::turn_engine::TurnEngine,
    session: &Id,
    after: i64,
) -> Result<AgentInput, StorageError> {
    let session = session.clone();
    crate::store::read(engine.store(), move |snapshot| {
        let Some(owner) = snapshot
            .sessions()
            .session(&session)?
            .filter(|row| !row.closed())
        else {
            return Ok(AgentInput::Finished);
        };
        if has_events(snapshot, &session, &[WatchEventState::Delivered])? {
            return Ok(AgentInput::Ready);
        }
        let watching = !snapshot
            .watchers()
            .watchers(
                &WatcherFilter::Session(session.clone()),
                &[WatcherPhase::Active, WatcherPhase::Stopping],
                None,
                1,
            )?
            .is_empty();
        if watching && notice_since(snapshot, &owner, after)? {
            return Ok(AgentInput::Ready);
        }
        if watching || has_events(snapshot, &session, &[WatchEventState::Pending])? {
            Ok(AgentInput::Waiting)
        } else {
            Ok(AgentInput::Finished)
        }
    })
    .await
}
