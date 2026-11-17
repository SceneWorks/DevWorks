//! The production composition builds every store-polling background loop on the shared commit
//! wake, never on a private timer. A loop that regresses to its own sleep would drop out of the
//! runtime's registered list.
mod common;
use common::*;

#[test]
fn every_store_polling_loop_waits_on_store_commits() {
    let _case = native_runtime_case();
    let root = Root::new("polling-loops");
    let engine = start(&root);
    let mut loops = engine.host().runtime().polling_loops();
    loops.sort_unstable();
    assert_eq!(
        loops,
        [
            "container reconciliation",
            "convention imports",
            "git",
            "help maintenance",
            "project purge",
            "schema view cleanup",
        ]
    );
}
