//! Receipt reply publication through the durable delivery dispatcher on the real SQLite store
//! and real managed files: commands through the gateway, their receipts' replies published by
//! the dispatcher's asynchronous consumer after commit, and recovery after a restart.
mod common;

use common::*;
use devworks_adapters::fs::NativeFilesystem;
use devworks_adapters::fs::artifacts::NativeArtifactFiles;
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::gateway::Gateway;
use devworks_core::ports::files::{ArtifactSources, ResolvedSource, SourceRefusal};
use devworks_core::ports::storage::ApplicationStore;
use devworks_core::protocol::data::ArtifactSource;
use devworks_core::protocol::records::{ReceiptLookupState, ReceiptObservation};
use devworks_core::protocol::results::{DomainCommandReply, DomainQueryReply, OperationResult};
use devworks_core::protocol::{Check, CommandReply, ProjectId};
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::artifacts::replies::{REPLY_CONSUMER, ReceiptReplies};
use devworks_core::services::history::HistoryService;
use devworks_core::services::sessions::SessionService;
use devworks_engine::RetryPolicy;
use devworks_engine::replies::reply_consumer;
use serde_json::json;
use std::sync::Arc;

struct NoSources;
impl ArtifactSources for NoSources {
    fn resolve(
        &self,
        _: &InvocationContext,
        _: &ArtifactSource,
        _: &ProjectId,
    ) -> Result<ResolvedSource, SourceRefusal> {
        Err(SourceRefusal::NotFound)
    }
}

struct Boot {
    store: Arc<dyn ApplicationStore>,
    cases: UseCases,
    replies: Arc<ReceiptReplies>,
}
fn boot(h: &Harness) -> Boot {
    let store = h.open();
    let files = Arc::new(NativeArtifactFiles::new(
        h.root.clone(),
        h.root.with_extension("cache"),
    ));
    let svc = Arc::new(ArtifactService::new(
        files,
        Arc::new(NoSources),
        Arc::new(NativeFilesystem::new()),
        h.clock(),
        h.ids.clone(),
    ));
    let cases = UseCases::new()
        .with_artifacts(svc.clone())
        .with_sessions(Arc::new(SessionService::new(h.ids.clone(), store.clone())))
        .with_history(Arc::new(HistoryService::new(h.ids.clone())));
    Boot {
        store,
        cases,
        replies: Arc::new(ReceiptReplies::new(svc)),
    }
}
impl Boot {
    fn start(&self, h: &Harness, ctx: &InvocationContext, key: &str) -> DomainCommandReply {
        let gateway = Gateway {
            authority: &Authority,
            store: &*self.store,
            ids: &*h.ids,
        };
        let frame = json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": key,
            "body": {"kind": "sessions.start", "scope": {"kind": "Workspace"}, "title": "Home"}});
        self.cases
            .command(
                &gateway,
                ctx,
                "sessions.start",
                frame.to_string().as_bytes(),
            )
            .expect("addressable")
    }
    fn receipt(&self, h: &Harness, ctx: &InvocationContext, key: &str) -> DomainQueryReply {
        let gateway = Gateway {
            authority: &Authority,
            store: &*self.store,
            ids: &*h.ids,
        };
        let frame = json!({"protocol": "1.0-draft.1", "queryId": "q-r",
            "query": {"kind": "history.command_receipt", "requestKey": key}});
        self.cases
            .query(
                &gateway,
                ctx,
                "history.command_receipt",
                frame.to_string().as_bytes(),
            )
            .expect("addressable")
    }
    fn observation(&self, h: &Harness, ctx: &InvocationContext, key: &str) -> ReceiptObservation {
        match self.receipt(h, ctx, key) {
            DomainQueryReply::Result {
                result: OperationResult::HistoryCommandReceipt { value },
                ..
            } => value,
            other => panic!("{other:?}"),
        }
    }
    fn dispatcher(&self, h: &Harness) -> devworks_engine::Dispatcher {
        dispatcher(
            h,
            self.store.clone(),
            vec![reply_consumer(
                self.replies.clone(),
                self.store.clone(),
                RetryPolicy::default(),
            )],
        )
    }
}

#[test]
fn the_dispatcher_publishes_every_recorded_receipt_reply() {
    let h = Harness::new();
    let rt = runtime();
    let b = boot(&h);
    let user = principal(&*b.store, "user-a", PrincipalKind::Human);
    let first = b.start(&h, &user, "start-1");
    let second = b.start(&h, &user, "start-2");
    assert!(matches!(first, CommandReply::Recorded { .. }));
    assert!(matches!(
        b.receipt(&h, &user, "start-1"),
        DomainQueryReply::Rejected { .. }
    ));
    let d = b.dispatcher(&h);
    let claimed = rt.block_on(async {
        within("publication", async {
            d.recover().await.unwrap();
            d.run_until_idle().await.unwrap()
        })
        .await
    });
    assert_eq!(claimed, 2);
    let status = read(&*b.store, |s| s.queue().status(REPLY_CONSUMER));
    assert_eq!((status.completed, status.ready, status.failed), (2, 0, 0));
    for (key, reply) in [("start-1", &first), ("start-2", &second)] {
        let o = b.observation(&h, &user, key);
        o.check().unwrap();
        assert!(matches!(o.state, ReceiptLookupState::Recorded));
        let CommandReply::Recorded { receipt, .. } = reply else {
            panic!()
        };
        assert_eq!(o.command_id.as_ref(), Some(&receipt.command_id));
        assert!(o.reply_artifact.is_some() && o.reply_schema.is_some());
    }
    // Nothing is left to deliver, and a further pass publishes nothing again.
    let again = rt.block_on(async { d.run_until_idle().await.unwrap() });
    assert_eq!(again, 0);
}

#[test]
fn an_unpublished_reply_is_published_by_the_next_boot() {
    let h = Harness::new();
    let rt = runtime();
    let user;
    {
        let b = boot(&h);
        user = principal(&*b.store, "user-a", PrincipalKind::Human);
        assert!(matches!(
            b.start(&h, &user, "start-1"),
            CommandReply::Recorded { .. }
        ));
        // The process stops before any dispatcher ran.
    }
    let b = boot(&h);
    let user = principal(&*b.store, user.principal().0.as_str(), PrincipalKind::Human);
    let d = b.dispatcher(&h);
    let claimed = rt.block_on(async {
        within("recovery", async {
            d.recover().await.unwrap();
            d.run_until_idle().await.unwrap()
        })
        .await
    });
    assert_eq!(claimed, 1);
    b.observation(&h, &user, "start-1").check().unwrap();
}
