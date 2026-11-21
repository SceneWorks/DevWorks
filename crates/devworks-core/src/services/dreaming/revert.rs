//! A run-wide revert walks the original journal backward. A conflict blocks its entire
//! causal component, while edits in other components remain independently reversible.
use super::{DreamingService, REVISION, authorize, human_or_host, invalid, rev};
use crate::context::InvocationContext;
use crate::operations::dreaming::RevertInput;
use crate::ports::storage::{
    DreamActionRow, DreamRunRow, DreamingRead, StorageError, StoredClaim, WriteChange,
    WriteTransaction,
};
use crate::protocol::provider::Digest;
use crate::protocol::records::{
    DreamRevertConflict, DreamRevertOutcome, DreamRevertResult, KnowledgeWriteReceipt,
};
use crate::protocol::results::OperationResult;
use crate::protocol::support::BoundedList;
use crate::protocol::{Id, RevisionDimension};
use crate::services::kernel::{Failure, counter, not_found};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

fn actions(repo: &dyn DreamingRead, run: &Id) -> Result<Vec<DreamActionRow>, StorageError> {
    let mut before = None;
    let mut result = Vec::new();
    loop {
        let page = repo.dream_actions(run, before, 100)?;
        let Some(last) = page.last() else { break };
        before = Some(last.position);
        result.extend(
            page.into_iter()
                .filter(|row| row.action["result"]["changed"] == true),
        );
    }
    Ok(result)
}

fn digest_rows(rows: &[DreamActionRow]) -> Result<Digest, StorageError> {
    let mut digest = sha2::Sha256::new();
    use sha2::Digest as _;
    for entry in rows {
        let canonical = crate::jcs::canonicalize(&json!({
            "id":entry.id,"position":entry.position,"record":entry.action,
        }))
        .map_err(|_| StorageError::InvalidRecord("invalid Dreaming journal".into()))?;
        digest.update((canonical.len() as u64).to_be_bytes());
        digest.update(canonical.as_bytes());
    }
    Digest::try_from(
        digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    )
    .map_err(|_| StorageError::InvalidRecord("invalid Dreaming journal digest".into()))
}

pub(super) fn journal_digest(repo: &dyn DreamingRead, run: &Id) -> Result<Digest, StorageError> {
    digest_rows(&actions(repo, run)?)
}

struct Intent {
    row: DreamActionRow,
    changes: Vec<WriteChange>,
    before: Vec<StoredClaim>,
    targets: BTreeMap<String, i64>,
}
impl Intent {
    fn decode(row: DreamActionRow) -> Result<Self, Failure> {
        let result = &row.action["result"];
        let changes: Vec<WriteChange> = serde_json::from_value(result["changes"].clone())
            .map_err(|_| invalid("dreaming_action_journal"))?;
        let before: Vec<StoredClaim> = serde_json::from_value(result["before"].clone())
            .map_err(|_| invalid("dreaming_action_journal"))?;
        let mut targets = changes
            .iter()
            .filter(|change| change.target_kind == "Claim")
            .filter_map(|change| Some((change.id.as_str().to_owned(), change.after?)))
            .collect::<BTreeMap<_, _>>();
        for row in &before {
            targets
                .entry(row.id.as_str().to_owned())
                .or_insert(row.revision);
        }
        Ok(Self {
            row,
            changes,
            before,
            targets,
        })
    }
}

impl DreamingService {
    pub(super) fn revert(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        revision: u32,
        input: &RevertInput,
    ) -> Result<OperationResult, Failure> {
        let run = tx
            .dreaming()?
            .dream_run(&input.run_id)?
            .filter(|run| authorize(ctx, &run.scope).is_ok())
            .ok_or_else(|| not_found("dreaming_run", &input.run_id))?;
        human_or_host(ctx)?;
        if run.phase.owns_scope() {
            return Err(StorageError::Busy.into());
        }
        if run.revision != input.expected_revision.get() {
            return Err(StorageError::conflict(
                RevisionDimension::Operation,
                "dreaming_run",
                run.id,
                input.expected_revision.clone(),
                rev(run.revision),
            )
            .into());
        }
        let journal = actions(tx.dreaming()?, &run.id)?;
        let selected = if let Some(id) = &input.proposal_id {
            let one = journal
                .into_iter()
                .filter(|row| {
                    row.id == *id
                        || row.action["result"]["proposalId"].as_str() == Some(id.as_str())
                })
                .collect::<Vec<_>>();
            if one.is_empty() {
                return Err(not_found("dreaming_action", id).into());
            }
            one
        } else {
            journal
        };
        if digest_rows(&selected)? != input.expected_change_digest {
            return Err(StorageError::conflict(
                RevisionDimension::Operation,
                "dreaming_journal",
                run.id,
                input.expected_revision.clone(),
                rev(run.revision),
            )
            .into());
        }
        let intents = selected
            .into_iter()
            .map(Intent::decode)
            .collect::<Result<Vec<_>, _>>()?;
        let mut applied = Vec::new();
        let mut pending = Vec::new();
        let mut legacy_receipt = None;
        for action in intents {
            if let Some(receipt) = tx.dreaming()?.dream_reversion(&run.id, &action.row.id)? {
                if revision < REVISION {
                    legacy_receipt = Some(
                        serde_json::from_value(receipt)
                            .map_err(|_| invalid("dreaming_reversion_receipt"))?,
                    );
                }
                applied.push(action.row.id);
            } else {
                pending.push(action);
            }
        }
        let mut conflicts = Vec::new();
        for component in components(&pending) {
            let component = component.iter().map(|index| &pending[*index]);
            match reverse_component(tx, ctx, &run, component)? {
                Component::Reverted(receipts) => {
                    for (id, receipt) in receipts {
                        applied.push(id);
                        if revision < REVISION {
                            legacy_receipt = Some(receipt);
                        }
                    }
                }
                Component::Conflict(conflict) => conflicts.push(conflict),
            }
        }
        let value = if revision < REVISION {
            DreamRevertResult::Legacy(
                legacy_receipt.ok_or_else(|| invalid("dreaming_reversion_conflict"))?,
            )
        } else {
            DreamRevertResult::Current(DreamRevertOutcome {
                run_id: run.id,
                remaining: counter(conflicts.len() as i64),
                applied: BoundedList(applied),
                conflicts: BoundedList(conflicts),
            })
        };
        Ok(OperationResult::DreamingRevert { value })
    }
}

/// Assemble causal components using all of the claims an action read or changed.
/// The input is newest first, which is also the order within each component.
fn components(actions: &[Intent]) -> Vec<BTreeSet<usize>> {
    let mut occurrences = BTreeMap::<&str, Vec<usize>>::new();
    for (index, action) in actions.iter().enumerate() {
        for target in action.targets.keys() {
            occurrences.entry(target).or_default().push(index);
        }
    }
    let mut remaining: BTreeSet<_> = (0..actions.len()).collect();
    let mut result = Vec::new();
    while let Some(first) = remaining.pop_first() {
        let mut group = BTreeSet::from([first]);
        let mut visit = VecDeque::from([first]);
        while let Some(index) = visit.pop_front() {
            for target in actions[index].targets.keys() {
                for neighbor in occurrences.remove(target.as_str()).unwrap_or_default() {
                    if remaining.remove(&neighbor) {
                        group.insert(neighbor);
                        visit.push_back(neighbor);
                    }
                }
            }
        }
        result.push(group);
    }
    result
}

enum Component {
    Reverted(Vec<(Id, KnowledgeWriteReceipt)>),
    Conflict(DreamRevertConflict),
}

fn causal_conflict<'a>(
    actions: impl Iterator<Item = &'a Intent>,
    mut current_revision: impl FnMut(&Id) -> Result<Option<i64>, StorageError>,
    current: &mut BTreeMap<String, i64>,
) -> Result<Option<DreamRevertConflict>, Failure> {
    // At each step the older action's output must be the exact input consumed by the next
    // action in this component. The current head alone cannot prove there was no intervening edit.
    let mut required_older_output = BTreeMap::new();
    for action in actions {
        for (id, expected) in &action.targets {
            let target = wire_id(id)?;
            let actual = if let Some(older_output) = required_older_output.get(id) {
                *older_output
            } else {
                let actual = current_revision(&target)?;
                if let Some(actual) = actual {
                    current.insert(id.clone(), actual);
                }
                actual
            };
            if actual != Some(*expected) {
                return Ok(Some(DreamRevertConflict {
                    action_id: action.row.id.clone(),
                    target_id: target,
                    expected_revision: rev(*expected),
                    current_revision: current.get(id).copied().map(rev),
                }));
            }
            let prior = action
                .changes
                .iter()
                .find(|change| change.target_kind == "Claim" && change.id == target)
                .map_or(Some(*expected), |change| change.before);
            required_older_output.insert(id.clone(), prior);
        }
    }
    Ok(None)
}

fn reverse_component<'a>(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    run: &DreamRunRow,
    actions: impl Iterator<Item = &'a Intent> + Clone,
) -> Result<Component, Failure> {
    let mut current = BTreeMap::new();
    if let Some(conflict) = causal_conflict(
        actions.clone(),
        |target| {
            Ok(tx
                .knowledge()
                .knowledge_claim(target)?
                .filter(|claim| claim.scope == run.scope && claim.lifecycle != "Deleted")
                .map(|claim| claim.revision))
        },
        &mut current,
    )? {
        return Ok(Component::Conflict(conflict));
    }
    let point = tx.savepoint()?;
    let result = (|| {
        let mut receipts = Vec::new();
        for action in actions {
            let reversed = crate::services::knowledge::reverse_dream_changes(
                tx,
                ctx,
                &run.scope,
                &action.row.action,
                &action.changes,
                &action.before,
                &current,
            );
            let receipt = match reversed {
                Ok(receipt) => receipt,
                Err(
                    Failure::Refused(_)
                    | Failure::Storage(
                        StorageError::RevisionConflict(_)
                        | StorageError::ViewUnavailable
                        | StorageError::ScopeDenied,
                    ),
                ) => {
                    let (id, expected) = action
                        .targets
                        .iter()
                        .next()
                        .ok_or_else(|| invalid("dreaming_reversion_target"))?;
                    let target_id = wire_id(id)?;
                    let terminal = tx.knowledge().knowledge_claim(&target_id)?;
                    return Ok(Component::Conflict(DreamRevertConflict {
                        action_id: action.row.id.clone(),
                        target_id,
                        expected_revision: rev(*expected),
                        current_revision: terminal.map(|claim| rev(claim.revision)),
                    }));
                }
                Err(Failure::Storage(error)) => return Err(error.into()),
            };
            let written = tx
                .knowledge()
                .knowledge_write(&receipt.write_id)?
                .ok_or_else(|| invalid("dreaming_reversion_receipt"))?;
            for change in written.changes {
                if let Some(revision) = change.after
                    && let Some(current_revision) = current.get_mut(change.id.as_str())
                {
                    *current_revision = revision;
                }
            }
            let stored = serde_json::to_value(&receipt)
                .map_err(|_| invalid("dreaming_reversion_receipt"))?;
            tx.dreaming()?.put_dream_reversion(
                &run.id,
                &action.row.id,
                ctx.principal(),
                &stored,
            )?;
            receipts.push((action.row.id.clone(), receipt));
        }
        Ok(Component::Reverted(receipts))
    })();
    if matches!(&result, Ok(Component::Reverted(_))) {
        tx.release(point)?;
    } else {
        tx.rollback_to(point)?;
    }
    result
}

fn wire_id(id: &str) -> Result<Id, Failure> {
    Id::new(id).map_err(|_| invalid("dreaming_journal_target").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(action: &str, before: i64, after: i64) -> Intent {
        let target = Id::new("lesson-1").unwrap();
        Intent {
            row: DreamActionRow {
                id: Id::new(action).unwrap(),
                position: after,
                committed_at_ms: 0,
                action: json!({}),
            },
            changes: vec![WriteChange {
                id: target.clone(),
                target_kind: "Claim".into(),
                before: Some(before),
                after: Some(after),
            }],
            before: Vec::new(),
            targets: BTreeMap::from([(target.as_str().to_owned(), after)]),
        }
    }

    #[test]
    fn revert_rejects_intervening_human_edit_between_two_dream_actions() {
        let separated = [edit("dream-second", 3, 4), edit("dream-first", 1, 2)];
        let conflict = causal_conflict(separated.iter(), |_| Ok(Some(4)), &mut BTreeMap::new())
            .unwrap()
            .unwrap();
        assert_eq!(conflict.action_id.as_str(), "dream-first");
        assert_eq!(conflict.expected_revision.get(), 2);
        let contiguous = [edit("dream-second", 2, 3), edit("dream-first", 1, 2)];
        assert!(
            causal_conflict(contiguous.iter(), |_| Ok(Some(3)), &mut BTreeMap::new())
                .unwrap()
                .is_none()
        );
    }
}
