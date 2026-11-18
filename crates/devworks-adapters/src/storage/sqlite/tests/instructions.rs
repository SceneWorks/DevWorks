//! Instruction revision state transitions on the real SQLite store.
use super::Harness;
use devworks_core::ports::storage::*;
use devworks_core::protocol::Id;

#[test]
fn a_non_positive_revision_number_is_invalid_input_not_a_corrupt_store() {
    let h = Harness::new();
    let s = h.store();
    for number in [0, -1] {
        let result = run_write(&s, WriteOptions::fixture(), move |tx| {
            Ok(tx.instructions().set_instruction_revision_state(
                &Id::new("instruction-a").unwrap(),
                number,
                InstructionRevisionState::Draft,
                InstructionRevisionState::Proposed,
                None,
                None,
            ))
        })
        .unwrap();
        assert!(
            matches!(result, Err(StorageError::InvalidRecord(_))),
            "{number}: {result:?}"
        );
    }
}
