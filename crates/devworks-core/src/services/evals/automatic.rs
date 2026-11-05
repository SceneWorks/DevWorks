//! Automatic evaluation is authorized only at a durable terminal boundary. These checks also
//! run when persisting a captured report, so a concurrent Continue cannot grade a live session.
use crate::ports::storage::{
    ActivationPhase, AssignmentState, RunRead, StorageError, StoredSession,
};

const PAGE: u16 = 64;

/// Closing a conversation and stopping an activation are distinct terminal boundaries. An
/// open coordinator or worker conversation is eligible when its assignment's activation has
/// stopped. Reusing that conversation for any live activation makes it ineligible again.
pub fn automatically_eligible(
    runs: &dyn RunRead,
    session: &StoredSession,
) -> Result<bool, StorageError> {
    let Some(root) = &session.root_id else {
        return Ok(session.closed());
    };
    let mut after = None;
    let mut stopped = false;
    loop {
        let page = runs.assignments(root, false, after.as_ref(), PAGE)?;
        let more = page.len() == usize::from(PAGE);
        after = page.last().map(|a| a.id.clone());
        for a in page.into_iter().filter(|a| a.session_id == session.id) {
            if a.state == AssignmentState::Active {
                return Ok(false);
            }
            if let Some(execution) = a.execution_id {
                match runs.activation(&execution)? {
                    Some(a) if a.phase == ActivationPhase::Stopped => stopped = true,
                    Some(_) => return Ok(false),
                    None => {}
                }
            }
        }
        if !more {
            return Ok(session.closed() || stopped);
        }
    }
}
