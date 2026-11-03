//! Deterministic port substitutes for gateway tests. They only store and count; every admission,
//! replay and conflict decision under test is made by the production gateway.
#![allow(dead_code, clippy::result_large_err)]

use devworks_core::context::{InvocationContext, PrincipalKind, ReceiptIdentity};
use devworks_core::gateway::{AdmittedCommand, AdmittedQuery, Gateway};
use devworks_core::ports::*;
use devworks_core::protocol::results::{DomainDecision, OperationResult};
use devworks_core::protocol::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;

pub fn id(s: &str) -> Id {
    Id::new(s).expect("valid fixture id")
}
pub fn revision(n: i64) -> Revision {
    Revision::new(n).expect("positive")
}

pub fn context(actor: &str) -> InvocationContext {
    context_in(actor, "ui-main", 0)
}
pub fn context_in(actor: &str, namespace: &str, epoch: i64) -> InvocationContext {
    InvocationContext::new(
        ActorId(id(actor)),
        PrincipalKind::Human,
        id("invocation-1"),
        id(namespace),
        StoreId(id("store-a")),
        Counter::new(epoch).expect("epoch"),
        revision(1),
    )
}

pub struct FixedClock;
impl Clock for FixedClock {
    fn now_ms(&self) -> Counter {
        Counter::new(1_700_000_000_000).expect("time")
    }
}

#[derive(Default)]
pub struct SeqIds(Cell<u64>);
impl IdSource for SeqIds {
    fn next_id(&self, kind: IdKind) -> Id {
        self.0.set(self.0.get() + 1);
        id(&format!("{kind:?}-{}", self.0.get()).to_lowercase())
    }
}

#[derive(Default)]
pub struct Authority {
    pub deny: Cell<Option<AuthorityDenial>>,
    pub calls: Cell<usize>,
}
impl AuthorityPort for Authority {
    fn authorize(
        &self,
        _: &InvocationContext,
        _: &AuthorityRequest<'_>,
    ) -> Result<(), AuthorityDenial> {
        self.calls.set(self.calls.get() + 1);
        self.deny.get().map_or(Ok(()), Err)
    }
}

/// In-memory receipt table keyed by receipt identity, with failure injection.
#[derive(Default)]
pub struct Receipts {
    pub rows: RefCell<HashMap<ReceiptIdentity, StoredReceipt>>,
    pub lookups: Cell<usize>,
    pub commits: Cell<usize>,
    pub lookup_error: Cell<Option<ReceiptStoreError>>,
    pub commit_error: Cell<Option<ReceiptStoreError>>,
    /// Simulates a concurrent writer that committed this receipt between lookup and commit.
    pub race: RefCell<Option<StoredReceipt>>,
}
impl Receipts {
    pub fn touched(&self) -> usize {
        self.lookups.get() + self.commits.get()
    }
}
impl ReceiptStore for Receipts {
    fn lookup(
        &self,
        identity: &ReceiptIdentity,
    ) -> Result<Option<StoredReceipt>, ReceiptStoreError> {
        self.lookups.set(self.lookups.get() + 1);
        if let Some(e) = self.lookup_error.get() {
            return Err(e);
        }
        Ok(self.rows.borrow().get(identity).cloned())
    }
    fn commit(
        &self,
        identity: &ReceiptIdentity,
        new: NewReceipt,
    ) -> Result<CommitOutcome, ReceiptStoreError> {
        self.commits.set(self.commits.get() + 1);
        if let Some(e) = self.commit_error.get() {
            return Err(e);
        }
        if let Some(winner) = self.race.borrow_mut().take() {
            self.rows
                .borrow_mut()
                .insert(identity.clone(), winner.clone());
            return Ok(CommitOutcome::AlreadyRecorded(winner));
        }
        let mut rows = self.rows.borrow_mut();
        if let Some(existing) = rows.get(identity) {
            return Ok(CommitOutcome::AlreadyRecorded(existing.clone()));
        }
        let committed_sequence = revision(rows.len() as i64 + 1);
        rows.insert(
            identity.clone(),
            StoredReceipt {
                operation: new.operation.to_owned(),
                contract: new.contract.to_owned(),
                schema_revision: new.schema_revision,
                fingerprint: new.fingerprint.clone(),
                receipt: Receipt {
                    command_id: new.command_id,
                    committed_sequence: committed_sequence.clone(),
                    accepted_at_ms: new.accepted_at_ms,
                    request_digest: new.fingerprint.as_str().to_owned(),
                    operation_id: new.operation_id,
                    decision: new.decision,
                },
            },
        );
        Ok(CommitOutcome::Committed { committed_sequence })
    }
}

/// Scripted command use case: returns its configured decision and counts calls.
type DecideFn = dyn Fn(&AdmittedCommand) -> Result<Decided, HandlerFailure>;
type AnswerFn = dyn Fn(&AdmittedQuery) -> Result<QueryAnswer, DomainFault>;

pub struct Commands {
    pub decide: Box<DecideFn>,
    pub calls: Cell<usize>,
}
impl Commands {
    pub fn new(f: impl Fn(&AdmittedCommand) -> Result<Decided, HandlerFailure> + 'static) -> Self {
        Self {
            decide: Box::new(f),
            calls: Cell::new(0),
        }
    }
    /// `Applied` with a mutation summary for operations whose result is `MutationSummary`.
    pub fn no_op_refusal() -> Self {
        Self::new(|_| {
            Ok(Decided {
                decision: DomainDecision::Refused {
                    fault: fault(DomainErrorCode::DependencyBlocked),
                },
                operation_id: None,
            })
        })
    }
}
impl CommandHandler for Commands {
    fn decide(
        &self,
        _: &InvocationContext,
        command: &AdmittedCommand,
    ) -> Result<Decided, HandlerFailure> {
        self.calls.set(self.calls.get() + 1);
        (self.decide)(command)
    }
}

pub struct Queries {
    pub answer: Box<AnswerFn>,
    pub calls: Cell<usize>,
}
impl Queries {
    pub fn new(f: impl Fn(&AdmittedQuery) -> Result<QueryAnswer, DomainFault> + 'static) -> Self {
        Self {
            answer: Box::new(f),
            calls: Cell::new(0),
        }
    }
}
impl QueryHandler for Queries {
    fn query(
        &self,
        _: &InvocationContext,
        query: &AdmittedQuery,
    ) -> Result<QueryAnswer, DomainFault> {
        self.calls.set(self.calls.get() + 1);
        (self.answer)(query)
    }
}

pub fn fault(code: DomainErrorCode) -> DomainFault {
    serde_json::from_value(serde_json::json!({
        "code": code_name(code), "retry": "AfterBlockerClears", "correlationId": "corr-1", "blockerIds": ["hold-a"]
    }))
    .expect("fault")
}
fn code_name(code: DomainErrorCode) -> String {
    serde_json::to_value(code)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}

pub fn stamp() -> ReadStamp {
    serde_json::from_value(serde_json::json!({
        "storeId": "store-a", "restoreEpoch": "0", "sequence": "7", "policyRevision": "1", "scopeRevision": "1"
    }))
    .expect("stamp")
}

pub fn result(operation: &str, value: serde_json::Value) -> OperationResult {
    serde_json::from_value(serde_json::json!({"kind": operation, "value": value})).expect("result")
}

pub struct Harness {
    pub authority: Authority,
    pub receipts: Receipts,
    pub clock: FixedClock,
    pub ids: SeqIds,
}
impl Harness {
    pub fn new() -> Self {
        Self {
            authority: Authority::default(),
            receipts: Receipts::default(),
            clock: FixedClock,
            ids: SeqIds::default(),
        }
    }
    pub fn gateway(&self) -> Gateway<'_> {
        Gateway {
            authority: &self.authority,
            receipts: &self.receipts,
            clock: &self.clock,
            ids: &self.ids,
        }
    }
}

pub fn contracts_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/architecture/contracts")
}
pub fn repo_file(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
}
