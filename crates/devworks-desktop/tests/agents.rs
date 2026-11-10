//! The native composition root admits agent operations through the shared gateway.
mod common;

use common::{Root, start};
use devworks_desktop::ops::{self, Failure};
use serde_json::json;

#[test]
fn native_host_registers_agent_coordination_owner() {
    let root = Root::new("agents-owner");
    let engine = start(&root);
    let backend = engine.backend();
    let reply = backend.query(
        "agents.inspect",
        &ops::query_frame(
            &*backend,
            "agents.inspect",
            json!({"assignmentId": "missing-assignment"}),
        ),
    );
    assert!(
        matches!(ops::query_result(reply), Err(Failure::Refused(_))),
        "the registered owner should reject the missing assignment"
    );
}
