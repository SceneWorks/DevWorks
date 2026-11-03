//! Admission order, authority placement and receipt replay/conflict semantics of the gateway.
#![allow(clippy::result_large_err)]
mod common;

use common::*;
use devworks_core::admission::{self, AdmissionError, Selection};
use devworks_core::framing::FrameError;
use devworks_core::gateway::GatewayFailure;
use devworks_core::operations::{OperationInput, runs};
use devworks_core::ports::storage::{StorageError, Unavailable};
use devworks_core::ports::{AuthorityDenial, Decided, HandlerFailure, QueryAnswer};
use devworks_core::protocol::results::{DomainCommandReply, DomainDecision, DomainQueryReply};
use devworks_core::protocol::*;
use devworks_core::registry::{EffectClass, Mode};
use serde_json::json;
use std::sync::Arc;

const EDIT: &str = r#"{"mode":"Command","protocol":"1.0-draft.1","requestKey":"key-1","body":{"kind":"projects.edit","projectId":"project-a","expectedRevision":"1","changes":{}}}"#;
const START: &str = r#"{"protocol":"1.0-draft.1","requestKey":"key-start","command":{"kind":"runs.start","rootId":"root-a","expectedWorkRevision":"1","expectedControlRevision":"1"}}"#;

fn refusal(operation: &str, mode: Mode, raw: &str) -> AdmissionError {
    admission::decode(Selection { operation, mode }, raw.as_bytes())
        .expect_err("must be refused")
        .error
}

fn applied_edit() -> Arc<Commands> {
    Commands::new(|_| {
        Ok(Decided {
            decision: DomainDecision::Applied {
                result: result(
                    "projects.edit",
                    json!({"id":"project-a","revision":"2","name":"n","description":"d","repositoryIds":[],"lifecycle":"Active","accessMode":"Full"}),
                ),
            },
            operation_id: None,
        })
    })
}

fn rejected_code(reply: &DomainCommandReply) -> (&Rejection, &RetryAdvice) {
    match reply {
        CommandReply::Rejected { code, retry, .. } => (code, retry),
        other => panic!("expected rejection, got {other:?}"),
    }
}

#[test]
fn unknown_contract_version_is_rejected_before_authority_or_receipts() {
    let h = Harness::new();
    let forged = json!({
        "mode": "Command", "protocol": "2.0", "requestKey": "key-1",
        "actorId": "admin", "scopeKey": "store-a/0/ui-main", "approved": true,
        "body": {"kind": "projects.edit", "projectId": "project-a", "expectedRevision": "1", "changes": {}}
    })
    .to_string();
    let handler = applied_edit();
    let reply = h
        .gateway()
        .command(
            &context("user-a"),
            "projects.edit",
            forged.as_bytes(),
            handler.clone(),
        )
        .unwrap();
    assert_eq!(
        rejected_code(&reply),
        (&Rejection::UnsupportedSchema, &RetryAdvice::Never)
    );
    assert_eq!(
        (
            h.authority.calls.get(),
            h.store.touched(),
            handler.calls.get()
        ),
        (0, 0, 0)
    );

    assert_eq!(
        refusal("projects.edit", Mode::Command, &forged),
        AdmissionError::UnsupportedProtocol("2.0".into())
    );
    let missing = EDIT.replace(r#""protocol":"1.0-draft.1","#, "");
    assert_eq!(
        refusal("projects.edit", Mode::Command, &missing),
        AdmissionError::MissingProtocol
    );
    let numeric = EDIT.replace(r#""1.0-draft.1""#, "1");
    assert_eq!(
        refusal("projects.edit", Mode::Command, &numeric),
        AdmissionError::MissingProtocol
    );
    let near = EDIT.replace("1.0-draft.1", "1.0-draft.1 ");
    assert!(matches!(
        refusal("projects.edit", Mode::Command, &near),
        AdmissionError::UnsupportedProtocol(_)
    ));
}

#[test]
fn caller_supplied_authority_fields_are_unknown_members() {
    let injected = EDIT.replace(r#""changes":{}"#, r#""changes":{},"actorId":"admin""#);
    assert!(matches!(
        refusal("projects.edit", Mode::Command, &injected),
        AdmissionError::Decode(_)
    ));
    let envelope = EDIT.replace(
        r#""mode":"Command","#,
        r#""mode":"Command","policyRevision":"9","#,
    );
    assert!(matches!(
        refusal("projects.edit", Mode::Command, &envelope),
        AdmissionError::InvalidEnvelope(_)
    ));
}

#[test]
fn selected_operation_and_mode_are_enforced_before_decoding() {
    // Unknown selection.
    assert_eq!(
        refusal("projects.explode", Mode::Command, EDIT),
        AdmissionError::UnknownOperation("projects.explode".into())
    );
    // A command operation selected under a read entitlement.
    assert!(matches!(
        refusal(
            "projects.edit",
            Mode::Query,
            &EDIT
                .replace("requestKey", "queryId")
                .replace(r#""Command""#, r#""Query""#)
        ),
        AdmissionError::ModeNotAllowed {
            operation: "projects.edit",
            mode: Mode::Query
        }
    ));
    // Envelope claims a different mode than the host selected.
    let hidden = EDIT.replace(r#""mode":"Command""#, r#""mode":"Query""#);
    assert_eq!(
        refusal("projects.edit", Mode::Command, &hidden),
        AdmissionError::EnvelopeModeMismatch {
            selected: Mode::Command
        }
    );
    // Body discriminator names another (also valid) operation.
    let archive = r#"{"mode":"Command","protocol":"1.0-draft.1","requestKey":"key-1","body":{"kind":"projects.archive","projectId":"project-a","expectedRevision":"1"}}"#;
    assert!(
        admission::decode(
            Selection {
                operation: "projects.archive",
                mode: Mode::Command
            },
            archive.as_bytes()
        )
        .is_ok()
    );
    assert_eq!(
        refusal("projects.edit", Mode::Command, archive),
        AdmissionError::SelectorMismatch {
            pointer: "/body/kind",
            expected: "projects.edit"
        }
    );
    // Specialized envelope: a pause body cannot be admitted as start.
    let pause = r#"{"protocol":"1.0-draft.1","requestKey":"k","command":{"kind":"runs.pause","rootId":"root-a","expectedControlRevision":"1"}}"#;
    assert!(matches!(
        refusal("runs.start", Mode::Command, pause),
        AdmissionError::SelectorMismatch { .. }
    ));
    // Through the gateway, none of these reach authority or receipts.
    let h = Harness::new();
    let handler = applied_edit();
    for (op, raw) in [
        ("projects.edit", hidden.as_str()),
        ("projects.edit", archive),
        ("projects.explode", EDIT),
    ] {
        let reply = h
            .gateway()
            .command(&context("user-a"), op, raw.as_bytes(), handler.clone())
            .unwrap();
        assert_eq!(rejected_code(&reply).0, &Rejection::InvalidRequest);
    }
    assert_eq!(
        (
            h.authority.calls.get(),
            h.store.touched(),
            handler.calls.get()
        ),
        (0, 0, 0)
    );
}

#[test]
fn hybrid_names_decode_each_mode_into_its_own_input() {
    let read = r#"{"mode":"Query","protocol":"1.0-draft.1","queryId":"q-1","body":{"kind":"notifications.settings"}}"#;
    let decoded = admission::decode(
        Selection {
            operation: "notifications.settings",
            mode: Mode::Query,
        },
        read.as_bytes(),
    )
    .unwrap_or_else(|r| panic!("{}", r.error));
    assert_eq!(
        (decoded.input.operation(), decoded.input.mode()),
        ("notifications.settings", Mode::Query)
    );
    // The read body cannot be admitted as the configuration write, and vice versa.
    let as_command = read
        .replace(r#""mode":"Query""#, r#""mode":"Command""#)
        .replace("queryId", "requestKey");
    assert!(matches!(
        refusal("notifications.settings", Mode::Command, &as_command),
        AdmissionError::Decode(_)
    ));
}

#[test]
fn aliased_inputs_keep_separate_entitlements_and_fingerprints() {
    let traverse = r#"{"protocol":"1.0-draft.1","queryId":"query-a","scope":{"kind":"Project","projectId":"project-a"},"view":{"kind":"Exact","viewIds":["view-a"]},"query":{"kind":"Traverse","startIds":["node-a"],"direction":"Forward","edgeKinds":[],"maxDepth":5},"limits":{"maxResults":100,"maxVisited":10000,"deadlineMs":2000}}"#;
    let dependency = admission::decode(
        Selection {
            operation: "code.dependency",
            mode: Mode::Query,
        },
        traverse.as_bytes(),
    );
    let traversal = admission::decode(
        Selection {
            operation: "code.traverse",
            mode: Mode::Query,
        },
        traverse.as_bytes(),
    );
    match (dependency, traversal) {
        (Ok(a), Ok(b)) => {
            assert_eq!(
                (a.descriptor.name, b.descriptor.name),
                ("code.dependency", "code.traverse")
            );
            assert_ne!(a.fingerprint, b.fingerprint);
        }
        (a, b) => {
            // Shape drifted from the recorded candidate: report what the native decoder says.
            panic!(
                "traverse fixture not admitted: {:?} / {:?}",
                a.err().map(|r| r.error),
                b.err().map(|r| r.error)
            );
        }
    }
    assert!(matches!(
        refusal("code.search", Mode::Query, traverse),
        AdmissionError::SelectorMismatch { .. }
    ));
}

#[test]
fn scalar_rules_are_enforced_by_typed_decoding() {
    let with_revision = |r: &str| {
        EDIT.replace(
            r#""expectedRevision":"1""#,
            &format!(r#""expectedRevision":{r}"#),
        )
    };
    let decode = |raw: &str| {
        admission::decode(
            Selection {
                operation: "projects.edit",
                mode: Mode::Command,
            },
            raw.as_bytes(),
        )
    };
    assert!(
        decode(&with_revision(r#""9223372036854775807""#)).is_ok(),
        "i64::MAX is a valid revision"
    );
    for bad in [
        r#""9223372036854775808""#, // i64 overflow
        r#""01""#,                  // leading zero
        r#""0""#,                   // revisions are positive
        r#""+1""#,
        r#""-1""#,
        r#""1e3""#,
        r#"" 1""#,
        r#""1.0""#,
        "1", // JSON number instead of canonical string
    ] {
        assert!(
            matches!(
                decode(&with_revision(bad)).map_err(|r| r.error),
                Err(AdmissionError::Decode(_))
            ),
            "{bad}"
        );
    }
    let long_key = EDIT.replace("key-1", &"k".repeat(129));
    assert!(matches!(
        decode(&long_key).map_err(|r| r.error),
        Err(AdmissionError::InvalidEnvelope(_))
    ));
    assert!(decode(&EDIT.replace("key-1", &"k".repeat(128))).is_ok());
    assert!(matches!(
        decode(&EDIT.replace("key-1", "key/1")).map_err(|r| r.error),
        Err(AdmissionError::InvalidEnvelope(_))
    ));
}

#[test]
fn framing_rejects_hostile_frames_before_any_interpretation() {
    let frame = |raw: &[u8]| match admission::decode(
        Selection {
            operation: "projects.edit",
            mode: Mode::Command,
        },
        raw,
    ) {
        Err(r) => match r.error {
            AdmissionError::Frame(e) => e,
            other => panic!("expected a frame error, got {other}"),
        },
        Ok(_) => panic!("accepted"),
    };
    let duplicate = EDIT.replace(
        r#""expectedRevision":"1""#,
        r#""expectedRevision":"1","expectedRevision":"2""#,
    );
    assert_eq!(
        frame(duplicate.as_bytes()),
        FrameError::DuplicateMember("expectedRevision".into())
    );
    let dup_envelope = EDIT.replace(
        r#""protocol":"1.0-draft.1""#,
        r#""protocol":"1.0-draft.1","protocol":"2.0""#,
    );
    assert_eq!(
        frame(dup_envelope.as_bytes()),
        FrameError::DuplicateMember("protocol".into())
    );
    assert!(matches!(
        frame(" ".repeat(1024 * 1024 + 1).as_bytes()),
        FrameError::TooLarge { .. }
    ));
    let mut invalid = EDIT.as_bytes().to_vec();
    invalid[60] = 0xff;
    assert_eq!(frame(&invalid), FrameError::InvalidUtf8);
    assert!(matches!(
        frame(format!("{EDIT} {{}}").as_bytes()),
        FrameError::Syntax(_)
    ));
    assert!(matches!(
        frame(&EDIT.as_bytes()[..EDIT.len() - 1]),
        FrameError::Syntax(_)
    ));
    assert!(matches!(frame(br#"{"a":NaN}"#), FrameError::Syntax(_)));
    assert!(matches!(frame(br#"{"a":1e400}"#), FrameError::Syntax(_)));
    let deep = format!("{}{}", "[".repeat(101), "]".repeat(101));
    assert_eq!(frame(deep.as_bytes()), FrameError::TooDeep);
    let ok_depth = format!("{}{}", "[".repeat(100), "]".repeat(100));
    assert!(devworks_core::framing::parse(ok_depth.as_bytes()).is_ok());

    // Through the gateway a duplicate member with a valid key is still refused with no effects.
    let h = Harness::new();
    let handler = applied_edit();
    let reply = h.gateway().command(
        &context("user-a"),
        "projects.edit",
        duplicate.as_bytes(),
        handler.clone(),
    );
    assert!(matches!(
        reply,
        Err(GatewayFailure::Unaddressable {
            error: AdmissionError::Frame(_),
            ..
        })
    ));
    assert_eq!(
        (
            h.authority.calls.get(),
            h.store.touched(),
            handler.calls.get()
        ),
        (0, 0, 0)
    );
}

#[test]
fn permission_denial_precedes_receipt_access() {
    let h = Harness::new();
    h.authority
        .deny
        .set(Some(AuthorityDenial::PermissionDenied));
    let handler = applied_edit();
    let reply = h
        .gateway()
        .command(
            &context("agent-a"),
            "projects.edit",
            EDIT.as_bytes(),
            handler.clone(),
        )
        .unwrap();
    assert_eq!(
        rejected_code(&reply),
        (&Rejection::PermissionDenied, &RetryAdvice::Never)
    );
    assert_eq!(
        (
            h.authority.calls.get(),
            h.store.touched(),
            handler.calls.get()
        ),
        (1, 0, 0)
    );
    h.authority.deny.set(Some(AuthorityDenial::InvalidTarget));
    let reply = h
        .gateway()
        .command(
            &context("agent-a"),
            "runs.start",
            START.as_bytes(),
            handler.clone(),
        )
        .unwrap();
    assert_eq!(rejected_code(&reply).0, &Rejection::InvalidTarget);
    assert_eq!(h.store.touched(), 0);
}

#[test]
fn same_command_replays_and_changed_intent_conflicts_without_effects() {
    let h = Harness::new();
    let handler = applied_edit();
    let ctx = context("user-a");
    let first = h
        .gateway()
        .command(&ctx, "projects.edit", EDIT.as_bytes(), handler.clone())
        .unwrap();
    let CommandReply::Recorded {
        receipt,
        replayed: false,
        ..
    } = &first
    else {
        panic!("{first:?}")
    };

    // Reordered members and whitespace: same intent, same key → historical replay, no redispatch.
    let reordered = r#"{ "body": {"changes": {}, "expectedRevision": "1", "projectId": "project-a", "kind": "projects.edit"},
        "requestKey": "key-1", "protocol": "1.0-draft.1", "mode": "Command" }"#;
    let second = h
        .gateway()
        .command(&ctx, "projects.edit", reordered.as_bytes(), handler.clone())
        .unwrap();
    let CommandReply::Recorded {
        receipt: replayed,
        replayed: true,
        ..
    } = &second
    else {
        panic!("{second:?}")
    };
    assert_eq!(replayed.command_id, receipt.command_id);
    assert_eq!(replayed.request_digest, receipt.request_digest);
    assert_eq!(
        handler.calls.get(),
        1,
        "a replay must not redispatch the use case"
    );

    // Changed expected revision under the same key is a different intent.
    let changed = EDIT.replace(r#""expectedRevision":"1""#, r#""expectedRevision":"2""#);
    let third = h
        .gateway()
        .command(&ctx, "projects.edit", changed.as_bytes(), handler.clone())
        .unwrap();
    assert_eq!(
        rejected_code(&third),
        (
            &Rejection::IdempotencyConflict,
            &RetryAdvice::RefreshAndNewRequest
        )
    );
    assert_eq!(
        (
            handler.calls.get(),
            h.store.recorded.get(),
            h.store.receipt_count()
        ),
        (1, 1, 1)
    );

    // A fresh key is a new command.
    let fresh = EDIT.replace("key-1", "key-2");
    assert!(matches!(
        h.gateway()
            .command(&ctx, "projects.edit", fresh.as_bytes(), handler.clone())
            .unwrap(),
        CommandReply::Recorded {
            replayed: false,
            ..
        }
    ));
    assert_eq!(handler.calls.get(), 2);
}

#[test]
fn receipt_identity_is_scoped_to_actor_and_host_namespace() {
    let h = Harness::new();
    let handler = applied_edit();
    for ctx in [
        context("user-a"),
        context("user-b"),
        context_in("user-a", "tool-call-7", 0),
    ] {
        let reply = h
            .gateway()
            .command(&ctx, "projects.edit", EDIT.as_bytes(), handler.clone())
            .unwrap();
        assert!(
            matches!(
                reply,
                CommandReply::Recorded {
                    replayed: false,
                    ..
                }
            ),
            "{ctx:?}"
        );
    }
    // Another principal and another tool namespace never see the first receipt.
    assert_eq!((handler.calls.get(), h.store.receipt_count()), (3, 3));

    // An invocation bound to an earlier/other restore epoch is not current authority: it is
    // refused on the write owner without replaying or minting anything.
    let reply = h
        .gateway()
        .command(
            &context_in("user-a", "ui-main", 1),
            "projects.edit",
            EDIT.as_bytes(),
            handler.clone(),
        )
        .unwrap();
    assert_eq!(
        rejected_code(&reply),
        (&Rejection::PermissionDenied, &RetryAdvice::Never)
    );
    // An unregistered principal cannot attribute a receipt.
    let reply = h
        .gateway()
        .command(
            &context("stranger"),
            "projects.edit",
            EDIT.as_bytes(),
            handler.clone(),
        )
        .unwrap();
    assert_eq!(rejected_code(&reply).0, &Rejection::PermissionDenied);
    assert_eq!((handler.calls.get(), h.store.receipt_count()), (3, 3));
}

#[test]
fn storage_failures_and_lost_acknowledgments_have_typed_outcomes() {
    let h = Harness::new();
    let handler = applied_edit();
    let ctx = context("user-a");
    // Not admitted by the write owner: nothing ran, the same command may be retried.
    h.store.refuse_next.set(Some(StorageError::Busy));
    let reply = h
        .gateway()
        .command(&ctx, "projects.edit", EDIT.as_bytes(), handler.clone())
        .unwrap();
    assert_eq!(
        rejected_code(&reply),
        (&Rejection::StorageUnavailable, &RetryAdvice::SameCommand)
    );
    assert_eq!(handler.calls.get(), 0);

    // A storage failure inside the use case rolls back the whole command: no receipt or history.
    let failing = Commands::new(|_| {
        Err(HandlerFailure::Storage(StorageError::unavailable(
            Unavailable::DiskFull,
            "injected",
        )))
    });
    let reply = h
        .gateway()
        .command(&ctx, "projects.edit", EDIT.as_bytes(), failing.clone())
        .unwrap();
    assert_eq!(
        rejected_code(&reply),
        (&Rejection::StorageUnavailable, &RetryAdvice::SameCommand)
    );
    assert_eq!(
        (h.store.receipt_count(), h.store.event_kinds().len()),
        (0, 0)
    );

    // The commit happened but its acknowledgment was lost.
    h.store
        .lose_next_ack
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let reply = h
        .gateway()
        .command(&ctx, "projects.edit", EDIT.as_bytes(), handler.clone())
        .unwrap();
    assert!(
        matches!(&reply, CommandReply::CommandStatusUnknown { request_key, .. } if request_key.0.as_str() == "key-1")
    );
    // Resolution: resubmitting the exact command/key finds the committed receipt.
    let reply = h
        .gateway()
        .command(&ctx, "projects.edit", EDIT.as_bytes(), handler.clone())
        .unwrap();
    assert!(matches!(
        reply,
        CommandReply::Recorded { replayed: true, .. }
    ));
    assert_eq!((handler.calls.get(), h.store.receipt_count()), (1, 1));
}

#[test]
fn a_losing_duplicate_never_runs_the_use_case() {
    // Receipt lookup, decision and commit form one serialized unit of work, so of two identical
    // concurrent commands exactly one decides; the other replays the winner's receipt.
    let h = Arc::new(Harness::new());
    let handler = applied_edit();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let (h, handler, barrier) = (h.clone(), handler.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                h.gateway()
                    .command(
                        &context("user-a"),
                        "projects.edit",
                        EDIT.as_bytes(),
                        handler,
                    )
                    .unwrap()
            })
        })
        .collect();
    let replies: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(
        handler.calls.get(),
        1,
        "the losing duplicate must not decide"
    );
    let ids: Vec<_> = replies
        .iter()
        .map(|r| match r {
            CommandReply::Recorded {
                receipt, replayed, ..
            } => (receipt.command_id.clone(), *replayed),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(ids[0].0, ids[1].0);
    assert_ne!(ids[0].1, ids[1].1);
    assert_eq!(h.store.receipt_count(), 1);
}

#[test]
fn a_stored_receipt_is_not_disclosed_once_authority_denies() {
    let h = Harness::new();
    let handler = applied_edit();
    let ctx = context("user-a");
    h.gateway()
        .command(&ctx, "projects.edit", EDIT.as_bytes(), handler.clone())
        .unwrap();
    let touched = h.store.touched();
    h.authority
        .deny
        .set(Some(AuthorityDenial::PermissionDenied));
    let reply = h
        .gateway()
        .command(&ctx, "projects.edit", EDIT.as_bytes(), handler.clone())
        .unwrap();
    // A denial, not the historical receipt: replay revalidates current access first.
    assert_eq!(
        rejected_code(&reply),
        (&Rejection::PermissionDenied, &RetryAdvice::Never)
    );
    assert!(!format!("{reply:?}").contains("command_id"));
    assert_eq!(h.store.touched(), touched, "the store was not consulted");
}

#[test]
fn incompatible_stored_receipts_are_a_visible_hold_not_a_conflict() {
    let agers: [fn(&MemoryStore); 2] = [|s| s.age_receipts("0.9-draft"), |s| s.rebind_receipts(2)];
    for age in agers {
        let h = Harness::new();
        let handler = applied_edit();
        let ctx = context("user-a");
        h.gateway()
            .command(&ctx, "projects.edit", EDIT.as_bytes(), handler.clone())
            .unwrap();
        age(&h.store);
        for _ in 0..2 {
            let reply = h
                .gateway()
                .command(&ctx, "projects.edit", EDIT.as_bytes(), handler.clone())
                .unwrap();
            assert_eq!(
                rejected_code(&reply),
                (&Rejection::UnsupportedSchema, &RetryAdvice::ResolveReceipt)
            );
        }
        assert_eq!(
            handler.calls.get(),
            1,
            "an incompatible record never redispatches"
        );
        // One durable, diagnosable hold record per receipt identity.
        let holds = h
            .store
            .event_kinds()
            .iter()
            .filter(|k| *k == "ReceiptIncompatible")
            .count();
        assert_eq!(holds, 1);
    }
}

#[test]
fn a_revision_conflict_undoes_partial_writes_and_is_a_durable_refusal() {
    let h = Harness::new();
    let ctx = context("user-a");
    let conflicting = Commands::new(|_| {
        Err(HandlerFailure::Storage(StorageError::conflict(
            RevisionDimension::Work,
            "project",
            id("project-a"),
            revision(1),
            revision(3),
        )))
    });
    let reply = h
        .gateway()
        .command(&ctx, "projects.edit", EDIT.as_bytes(), conflicting.clone())
        .unwrap();
    let CommandReply::Recorded {
        receipt,
        replayed: false,
        ..
    } = &reply
    else {
        panic!("{reply:?}")
    };
    let DomainDecision::Refused { fault } = &receipt.decision else {
        panic!("{receipt:?}")
    };
    assert!(matches!(fault.code, DomainErrorCode::RevisionConflict));
    assert_eq!(fault.current_revision, Some(revision(3)));
    // Exact retry replays the committed conflict decision.
    let again = h
        .gateway()
        .command(&ctx, "projects.edit", EDIT.as_bytes(), conflicting.clone())
        .unwrap();
    assert!(matches!(
        again,
        CommandReply::Recorded { replayed: true, .. }
    ));
    assert_eq!(conflicting.calls.get(), 1);
    assert_eq!(h.store.event_kinds(), vec!["CommandDecided".to_owned()]);
}

#[test]
fn action_dependent_commands_reach_authority_with_their_effective_effect() {
    let h = Harness::new();
    let sign_in = r#"{"mode":"Command","protocol":"1.0-draft.1","requestKey":"k-gh","body":{"kind":"github.account","action":"BeginSignIn"}}"#;
    let refresh = sign_in.replace("BeginSignIn", "Refresh");
    let close = r#"{"mode":"Command","protocol":"1.0-draft.1","requestKey":"k-br","body":{"kind":"browser.sessions","action":{"kind":"Close","sessionId":"s-1","expectedRevision":"1"}}}"#;
    h.authority
        .deny
        .set(Some(AuthorityDenial::PermissionDenied));
    let handler = applied_edit();
    for (op, raw, effect, action) in [
        (
            "github.account",
            sign_in.to_owned(),
            EffectClass::External,
            "BeginSignIn",
        ),
        ("github.account", refresh, EffectClass::External, "Refresh"),
        (
            "browser.sessions",
            close.to_owned(),
            EffectClass::Journaled,
            "Close",
        ),
    ] {
        let decoded = admission::decode(
            Selection {
                operation: op,
                mode: Mode::Command,
            },
            raw.as_bytes(),
        )
        .unwrap_or_else(|e| panic!("{op}: {e:?}"));
        assert_eq!((decoded.effect, decoded.action), (effect, Some(action)));
        h.gateway()
            .command(&context("user-a"), op, raw.as_bytes(), handler.clone())
            .unwrap();
        assert_eq!(h.authority.last_effect.get(), Some((effect, Some(action))));
    }
    assert_eq!(h.store.touched(), 0);
}

#[test]
fn accepted_requires_an_asynchronous_completion_contract() {
    // projects.edit is a Local command: there is no asynchronous operation to report.
    let h = Harness::new();
    let accepted = Commands::new(|_| {
        Ok(Decided {
            decision: DomainDecision::Accepted {
                operation_id: OperationId(id("op-1")),
            },
            operation_id: Some(OperationId(id("op-1"))),
        })
    });
    let outcome = h.gateway().command(
        &context("user-a"),
        "projects.edit",
        EDIT.as_bytes(),
        accepted.clone(),
    );
    assert!(
        matches!(&outcome, Err(GatewayFailure::HandlerContract(e)) if e.contains("Accepted")),
        "{outcome:?}"
    );
    assert_eq!(h.store.receipt_count(), 0);
}

#[test]
fn query_stamps_must_match_the_invocation_snapshot() {
    let h = Harness::new();
    let get = r#"{"mode":"Query","protocol":"1.0-draft.1","queryId":"q-1","body":{"kind":"projects.get","projectId":"project-a"}}"#;
    let project = json!({"id":"project-a","revision":"2","name":"n","description":"d","repositoryIds":[],"lifecycle":"Active","accessMode":"Full"});
    for (field, value) in [
        ("storeId", json!("store-b")),
        ("restoreEpoch", json!("1")),
        ("sequence", json!("9")),
    ] {
        let mut s = serde_json::to_value(stamp()).unwrap();
        s[field] = value;
        let bad: ReadStamp = serde_json::from_value(s).unwrap();
        let p = project.clone();
        let handler = Queries::new(move |_| {
            Ok(QueryAnswer {
                stamp: bad.clone(),
                result: result("projects.get", p.clone()),
            })
        });
        let outcome =
            h.gateway()
                .query(&context("user-a"), "projects.get", get.as_bytes(), &handler);
        assert!(
            matches!(outcome, Err(GatewayFailure::HandlerContract(_))),
            "{field}: {outcome:?}"
        );
    }
    // A caller bound to another restore epoch is refused before the use case runs.
    let handler = Queries::new(move |_| unreachable!("stale epoch must not reach the use case"));
    let reply = h
        .gateway()
        .query(
            &context_in("user-a", "ui-main", 3),
            "projects.get",
            get.as_bytes(),
            &handler,
        )
        .unwrap();
    assert!(
        matches!(&reply, DomainQueryReply::Rejected { fault, .. } if matches!(fault.code, DomainErrorCode::PermissionDenied))
    );
}

#[test]
fn use_cases_cannot_record_another_operations_result() {
    let h = Harness::new();
    let wrong = Commands::new(|_| {
        Ok(Decided {
            decision: DomainDecision::Applied {
                result: result(
                    "projects.archive",
                    json!({"id":"project-a","revision":"2","name":"n","description":"d","repositoryIds":[],"lifecycle":"Archived","accessMode":"Full"}),
                ),
            },
            operation_id: None,
        })
    });
    let outcome = h.gateway().command(
        &context("user-a"),
        "projects.edit",
        EDIT.as_bytes(),
        wrong.clone(),
    );
    assert!(
        matches!(outcome, Err(GatewayFailure::HandlerContract(_))),
        "{outcome:?}"
    );
    assert_eq!(h.store.recorded.get(), 0);

    let unknown_without_operation = Commands::new(|_| {
        let mut f = fault(DomainErrorCode::OutcomeUnknown);
        f.retry = RetryAdvice::ReconcileOperation;
        Ok(Decided {
            decision: DomainDecision::Refused { fault: f },
            operation_id: None,
        })
    });
    let outcome = h.gateway().command(
        &context("user-a"),
        "projects.edit",
        EDIT.as_bytes(),
        unknown_without_operation.clone(),
    );
    assert!(matches!(outcome, Err(GatewayFailure::HandlerContract(_))));
    assert_eq!(h.store.recorded.get(), 0);
}

#[test]
fn root_control_commands_reach_the_use_case_as_their_selected_typed_input() {
    let h = Harness::new();
    let seen = Commands::new(|cmd| {
        let OperationInput::Runs(runs::Input::Start(request)) = cmd.input() else {
            panic!("runs.start delivered as {:?}", cmd.input());
        };
        assert_eq!(request.request_key.0.as_str(), "key-start");
        Ok(Decided {
            decision: DomainDecision::Accepted {
                operation_id: OperationId(id("op-1")),
            },
            operation_id: Some(OperationId(id("op-1"))),
        })
    });
    let reply = h
        .gateway()
        .command(
            &context("user-a"),
            "runs.start",
            START.as_bytes(),
            seen.clone(),
        )
        .unwrap();
    let CommandReply::Recorded { receipt, .. } = reply else {
        panic!()
    };
    assert_eq!(receipt.operation_id, Some(OperationId(id("op-1"))));
    assert_eq!(receipt.committed_sequence, revision(1));
    assert_eq!(receipt.accepted_at_ms.as_str(), "1700000000000");
    assert_eq!(
        receipt.request_digest,
        "b0944a8d96d87e894c041b08ffbfd4d21c83a5de42c0f7bf856aebab6005ab22"
    );
}

#[test]
fn queries_never_touch_receipts_and_publish_only_their_own_result() {
    let h = Harness::new();
    let get = r#"{"mode":"Query","protocol":"1.0-draft.1","queryId":"q-1","body":{"kind":"projects.get","projectId":"project-a"}}"#;
    let project = json!({"id":"project-a","revision":"2","name":"n","description":"d","repositoryIds":[],"lifecycle":"Active","accessMode":"Full"});
    let answer = project.clone();
    let handler = Queries::new(move |_| {
        Ok(QueryAnswer {
            stamp: stamp(),
            result: result("projects.get", answer.clone()),
        })
    });
    let reply = h
        .gateway()
        .query(&context("user-a"), "projects.get", get.as_bytes(), &handler)
        .unwrap();
    assert!(
        matches!(&reply, DomainQueryReply::Result { query_id, stamp, .. } if query_id.0.as_str() == "q-1" && stamp.sequence.as_str() == "0")
    );
    // Queries read one snapshot and never submit a write or form a receipt.
    assert_eq!((h.store.submits.get(), h.store.reads.get()), (0, 1));

    let mislabeled = Queries::new(move |_| {
        Ok(QueryAnswer {
            stamp: stamp(),
            result: result("projects.restore", project.clone()),
        })
    });
    assert!(matches!(
        h.gateway().query(
            &context("user-a"),
            "projects.get",
            get.as_bytes(),
            &mislabeled
        ),
        Err(GatewayFailure::HandlerContract(_))
    ));

    h.authority.deny.set(Some(AuthorityDenial::NotFound));
    let reply = h
        .gateway()
        .query(&context("user-b"), "projects.get", get.as_bytes(), &handler)
        .unwrap();
    assert!(
        matches!(&reply, DomainQueryReply::Rejected { fault, .. } if matches!(fault.code, DomainErrorCode::NotFound))
    );
    assert_eq!(handler.calls.get(), 1);

    // A command body submitted as a query is refused before authority.
    h.authority.deny.set(None);
    let before = h.authority.calls.get();
    let reply = h.gateway().query(
        &context("user-a"),
        "projects.edit",
        EDIT.as_bytes(),
        &handler,
    );
    assert!(matches!(
        reply,
        Err(GatewayFailure::Unaddressable {
            error: AdmissionError::ModeNotAllowed { .. },
            ..
        })
    ));
    assert_eq!(h.authority.calls.get(), before);
}

#[test]
fn checked_increment_never_wraps() {
    let max = Revision::new(i64::MAX).unwrap();
    assert!(max.checked_next().is_none());
    assert_eq!(
        Revision::new(41).unwrap().checked_next().unwrap().as_str(),
        "42"
    );
    assert_eq!(
        Counter::new(0).unwrap().checked_next().unwrap().as_str(),
        "1"
    );
    assert!(Counter::new(i64::MAX).unwrap().checked_next().is_none());
    assert!(Revision::new(0).is_err() && Counter::new(-1).is_err());
}
