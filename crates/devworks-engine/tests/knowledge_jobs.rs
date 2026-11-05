//! A Knowledge delete by source too large for one short transaction continues as a durable job
//! through its consumer, one checkpointed batch per delivery; a job whose process stopped while
//! a batch was claimed resumes at the next boot and finishes the whole source.
mod common;

use common::*;
use devworks_core::authority::HostAuthority;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::gateway::Gateway;
use devworks_core::ports::retrieval::NoVectorIndex;
use devworks_core::ports::storage::*;
use devworks_core::protocol::records::KnowledgeWriteReceipt;
use devworks_core::protocol::results::{
    DomainCommandReply, DomainDecision, DomainQueryReply, OperationResult,
};
use devworks_core::protocol::{ActorId, Counter, Id, OperationState, Revision};
use devworks_core::services::UseCases;
use devworks_core::services::knowledge::{
    DELETION_BATCH, KnowledgeService, SOURCE_DELETION_CONSUMER,
};
use devworks_engine::knowledge::source_deletion_consumer;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

struct App<'h> {
    h: &'h Harness,
    store: Arc<dyn ApplicationStore>,
    cases: UseCases,
    knowledge: Arc<KnowledgeService>,
    n: AtomicU64,
}
impl<'h> App<'h> {
    fn boot(h: &'h Harness) -> Self {
        let store = h.open();
        let knowledge = Arc::new(KnowledgeService::new(
            h.ids.clone(),
            h.clock(),
            None,
            Arc::new(NoVectorIndex),
        ));
        Self {
            h,
            store,
            cases: UseCases::new().with_knowledge(knowledge.clone()),
            knowledge,
            n: AtomicU64::new(0),
        }
    }
    fn user(&self) -> InvocationContext {
        let info = self.store.info();
        HostIngress::new(&*self.store).context(
            ActorId(id("user-a")),
            PrincipalKind::Human,
            id("invocation-1"),
            id("ui-main"),
            Counter::new(info.restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
    }
    fn authority(&self) -> HostAuthority<'_> {
        HostAuthority {
            store: &*self.store,
            clock: &*self.h.clock,
            protected_branches: &[],
        }
    }
    fn cmd(&self, op: &str, revision: u32, mut body: Value) -> DomainCommandReply {
        body["kind"] = json!(op);
        let key = format!("key-{}", self.n.fetch_add(1, Ordering::SeqCst));
        let frame = json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": key,
            "schemaRevision": revision, "body": body});
        let authority = self.authority();
        let gateway = Gateway {
            authority: &authority,
            store: &*self.store,
            ids: &*self.h.ids,
        };
        self.cases
            .command(&gateway, &self.user(), op, frame.to_string().as_bytes())
            .expect("addressable")
    }
    fn q(&self, op: &str, mut body: Value) -> OperationResult {
        body["kind"] = json!(op);
        let frame = json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": "q-1",
            "schemaRevision": 2, "body": body});
        let authority = self.authority();
        let gateway = Gateway {
            authority: &authority,
            store: &*self.store,
            ids: &*self.h.ids,
        };
        match self
            .cases
            .query(&gateway, &self.user(), op, frame.to_string().as_bytes())
            .expect("addressable")
        {
            DomainQueryReply::Result { result, .. } => result,
            other => panic!("{other:?}"),
        }
    }
    fn status(&self, write: &Id) -> KnowledgeWriteReceipt {
        match self.q("knowledge.write_status", json!({"writeId": write})) {
            OperationResult::KnowledgeWriteStatus { value } => value,
            other => panic!("{other:?}"),
        }
    }
}

fn applied(reply: DomainCommandReply) -> OperationResult {
    match reply {
        devworks_core::protocol::CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied { result } => result,
            other => panic!("expected applied, got {other:?}"),
        },
        other => panic!("expected a recorded decision, got {other:?}"),
    }
}

#[test]
fn an_interrupted_source_deletion_resumes_at_boot_and_finishes_the_whole_source() {
    let h = Harness::new();
    let claims = DELETION_BATCH * 2 + 100;
    let (write, job_entry) = {
        let app = App::boot(&h);
        let registered = ActorId(id("user-a"));
        write(&*app.store, move |tx| {
            tx.actors()
                .register(&registered, PrincipalKind::Human, "user-a")
        })
        .unwrap();
        for b in 0..claims / 100 {
            let drafts: Vec<Value> = (0..100)
                .map(|i| {
                    json!({"kind": "Fact", "subjectId": "s", "predicate": "states",
                        "value": format!("fact {b}-{i}"), "confidence": 0.7, "evidenceIds": [],
                        "tags": [], "source": "run-9"})
                })
                .collect();
            applied(app.cmd(
                "knowledge.record",
                2,
                json!({"scope": {"kind": "Workspace"}, "batch": {
                "entities": [{"clientId": "s", "label": format!("Run {b}"), "entityType": "t",
                    "aliases": [], "source": "run-9"}],
                "evidence": [], "claims": drafts}}),
            ));
        }
        let (digest, revision) = match app.q(
            "knowledge.diagnostics",
            json!({"scope": {"kind": "Workspace"}, "sourceId": "run-9"}),
        ) {
            OperationResult::KnowledgeDiagnostics { value } => {
                let d = value
                    .items
                    .0
                    .iter()
                    .find(|d| d.component.0 == "knowledge.preview")
                    .unwrap()
                    .id
                    .as_str()
                    .to_owned();
                let rev = read(&*app.store, |r| {
                    r.knowledge()
                        .knowledge_scope_revision(&KnowledgeScope::Workspace)
                });
                (d, rev)
            }
            other => panic!("{other:?}"),
        };
        let summary = match applied(app.cmd(
            "knowledge.delete_by_source",
            1,
            json!({"scope": {"kind": "Workspace"}, "sourceId": "run-9",
            "expectedScopeRevision": revision.to_string(), "previewDigest": digest}),
        )) {
            OperationResult::KnowledgeDeleteBySource { value } => value,
            other => panic!("{other:?}"),
        };
        let r = app.status(&summary.id);
        assert!(matches!(r.state, OperationState::Dispatching));
        assert_eq!(r.claim_count.unwrap().get(), DELETION_BATCH as i64);
        // The process stops while a dispatcher holds the job's next batch: its transaction
        // never committed, so only the claim remains.
        let ghost = ClaimRequest {
            consumer: SOURCE_DELETION_CONSUMER.into(),
            owner: id("ghost"),
            lease_ms: 3_600_000,
            limit: 1,
        };
        let entry = write(&*app.store, move |tx| tx.queue().claim_ready(&ghost))
            .unwrap()
            .remove(0);
        (summary.id, entry.id)
    };

    // Next boot: the dispatcher recovers the stranded claim and the consumer finishes the job.
    let app = App::boot(&h);
    let d = dispatcher(
        &h,
        app.store.clone(),
        vec![source_deletion_consumer(app.knowledge.clone())],
    );
    let rt = runtime();
    rt.block_on(async {
        within("the deletion job", async {
            d.tick().await.unwrap();
            d.run_until_idle().await.unwrap();
        })
        .await;
    });
    let r = app.status(&write);
    assert!(matches!(r.state, OperationState::ObservedComplete));
    assert_eq!(r.claim_count.unwrap().get(), claims as i64);
    assert_eq!(r.entity_count.unwrap().get(), (claims / 100) as i64);
    assert!(matches!(
        entry(&*app.store, &job_entry).state,
        QueueState::Completed
    ));
    // Every claim of the source is a tombstone; the source is writable again.
    let left = read(&*app.store, |r| {
        r.knowledge()
            .knowledge_source_claims(&KnowledgeScope::Workspace, "run-9", None, 500)
    });
    assert!(left.is_empty());
}
