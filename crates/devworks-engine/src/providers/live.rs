//! Host-only text previews. These are neither transcript entries nor provider-native state.
//! Only the owned sink publishes them; finalized transcripts remain the replay authority.
use devworks_core::{
    ports::storage::StorageError,
    protocol::{AttemptId, Id},
};
use std::{collections::BTreeMap, sync::Mutex};

const MAX_TURNS: usize = 128;
const MAX_TEXT_BYTES: usize = 32 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct LiveTurn {
    pub session_id: Id,
    pub attempt_id: AttemptId,
    pub context_epoch: i64,
    pub revision: u64,
    pub text: String,
    /// Hide the preview only once the matching canonical transcript has reached this position.
    pub finalized_through: Option<i64>,
}

#[derive(Default)]
pub(crate) struct LiveTurns(Mutex<BTreeMap<String, LiveTurn>>);
impl LiveTurns {
    pub fn clear(&self) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }
    pub fn snapshot(&self, session: &Id) -> Option<LiveTurn> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(session.as_str())
            .cloned()
    }
    pub fn text(
        &self,
        session: &Id,
        attempt: &AttemptId,
        epoch: i64,
        text: &str,
    ) -> Result<(), StorageError> {
        let mut turns = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(turn) = turns.get(session.as_str())
            && ((turn.attempt_id != *attempt && turn.finalized_through.is_none())
                || (turn.attempt_id == *attempt
                    && (turn.context_epoch != epoch || turn.finalized_through.is_some())))
        {
            return Err(StorageError::ScopeDenied);
        }
        let prior_bytes = turns
            .get(session.as_str())
            .filter(|turn| turn.attempt_id == *attempt)
            .map_or(0, |turn| turn.text.len());
        if prior_bytes.saturating_add(text.len()) > MAX_TEXT_BYTES {
            return Err(StorageError::InvalidRecord(
                "live text exceeds provider response bound".into(),
            ));
        }
        if !turns.contains_key(session.as_str()) && turns.len() >= MAX_TURNS {
            let old = turns
                .iter()
                .find(|(_, turn)| turn.finalized_through.is_some())
                .map(|(id, _)| id.clone());
            if let Some(old) = old {
                turns.remove(&old);
            } else {
                return Err(StorageError::InvalidRecord("live turn capacity".into()));
            }
        }
        while turns
            .values()
            .map(|turn| turn.text.len())
            .sum::<usize>()
            .saturating_add(text.len())
            > MAX_TOTAL_BYTES
        {
            let old = turns
                .iter()
                .find(|(_, turn)| turn.finalized_through.is_some())
                .map(|(id, _)| id.clone());
            if let Some(old) = old {
                turns.remove(&old);
            } else {
                return Err(StorageError::InvalidRecord("live text capacity".into()));
            }
        }
        let turn = turns
            .entry(session.as_str().to_owned())
            .or_insert_with(|| LiveTurn {
                session_id: session.clone(),
                attempt_id: attempt.clone(),
                context_epoch: epoch,
                revision: 0,
                text: String::new(),
                finalized_through: None,
            });
        if turn.attempt_id != *attempt {
            *turn = LiveTurn {
                session_id: session.clone(),
                attempt_id: attempt.clone(),
                context_epoch: epoch,
                revision: 0,
                text: String::new(),
                finalized_through: None,
            };
        }
        if turn.context_epoch != epoch || turn.finalized_through.is_some() {
            return Err(StorageError::ScopeDenied);
        }
        turn.text.push_str(text);
        turn.revision += 1;
        Ok(())
    }
    pub fn finalize(&self, session: &Id, attempt: &AttemptId, sequence: i64) {
        if let Some(turn) = self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_mut(session.as_str())
            && turn.attempt_id == *attempt
        {
            turn.finalized_through = Some(sequence);
            turn.revision += 1;
        }
    }
    pub fn discard(&self, session: &Id, attempt: &AttemptId) {
        let mut turns = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if turns
            .get(session.as_str())
            .is_some_and(|turn| turn.attempt_id == *attempt && turn.finalized_through.is_none())
        {
            turns.remove(session.as_str());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn id(value: &str) -> Id {
        Id::new(value).unwrap()
    }
    #[test]
    fn previews_are_owned_bounded_and_late_clear_cannot_remove_the_next_turn() {
        let live = LiveTurns::default();
        let session = id("session");
        let first = AttemptId(id("first"));
        let next = AttemptId(id("next"));
        live.text(&session, &first, 7, "visible").unwrap();
        assert!(live.text(&session, &next, 7, "foreign").is_err());
        assert!(live.text(&session, &first, 8, "foreign epoch").is_err());
        assert_eq!(live.snapshot(&session).unwrap().text, "visible");
        live.finalize(&session, &first, 11);
        assert!(live.text(&session, &first, 7, "late").is_err());
        live.text(&session, &next, 8, "next").unwrap();
        live.discard(&session, &first);
        live.finalize(&session, &first, 99);
        let current = live.snapshot(&session).unwrap();
        assert_eq!(current.attempt_id, next);
        assert_eq!(current.text, "next");
        assert!(current.finalized_through.is_none());
        live.discard(&session, &next);
        assert!(live.snapshot(&session).is_none());
        assert!(
            live.text(&session, &next, 8, &"x".repeat(MAX_TEXT_BYTES + 1))
                .is_err()
        );
        assert!(live.snapshot(&session).is_none());
        for n in 0..MAX_TURNS {
            live.text(&id(&format!("s{n}")), &first, 0, "x").unwrap();
        }
        assert!(live.text(&session, &next, 8, "x").is_err());
        assert!(live.snapshot(&session).is_none());
    }
}
