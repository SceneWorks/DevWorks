//! Real qualified route and shared configuration gateway. Fanout itself performs no inference.
use super::*;
use devworks_core::{
    gateway::Gateway,
    protocol::{
        OperationId, OperationState, ProjectId, records::RouterConfigAction,
        results::DomainDecision,
    },
    services::{
        conventions::indexing::{
            IndexService, commands::JOB_KIND, publisher::IndexOutcome, selection::SELECTION_KIND,
        },
        router::embedding_spaces::EmbeddingSpaceService,
    },
};
fn project(f: &Fx, name: &str) {
    let name = name.to_owned();
    write(&*f.store, move |tx| {
        tx.projects().insert_project(&StoredProject {
            id: ProjectId(id(&name)),
            name: name.clone(),
            description: String::new(),
            lifecycle: "Active".into(),
            revision: 1,
            scope_revision: 1,
            created_at_ms: T0,
            updated_at_ms: T0,
            archived_at_ms: None,
            deleted_at_ms: None,
            archive_epoch: 0,
            archive_stop_pending: false,
        })
    })
    .unwrap();
}
fn operations(f: &Fx) -> Vec<OperationRecord> {
    read(&*f.store, |r| {
        r.operations().operations_for_capability(
            "conventions.index",
            &[
                OperationState::IntentRecorded,
                OperationState::Dispatching,
                OperationState::OutcomeUnknown,
                OperationState::ObservedComplete,
            ],
            None,
            500,
        )
    })
}
fn children(f: &Fx) -> Vec<OperationRecord> {
    operations(f)
        .into_iter()
        .filter(|op| op.payload["kind"] == JOB_KIND && op.payload["selectionEvent"].is_string())
        .collect()
}
fn dispatch(f: &Fx, operation: &OperationId) -> OperationRecord {
    let key = operation.clone();
    write(&*f.store, move |tx| {
        let op = tx.operations().operation(&key)?.unwrap();
        let attempt =
            tx.operations()
                .begin_attempt(&key, op.revision, AttemptBinding::default())?;
        let op = tx.operations().operation(&key)?.unwrap();
        tx.operations().transition(
            &key,
            op.revision,
            Some(&attempt.fence()),
            OperationState::Dispatching,
            None,
        )
    })
    .unwrap()
}
fn clear(f: &Fx, index: Arc<IndexService>, ctx: &InvocationContext) -> OperationId {
    let owner = Arc::new(EmbeddingSpaceService::new(f.store.clone()));
    assert!(owner.install_selection_indexer(Arc::downgrade(&index)));
    let gateway = Gateway {
        authority: &Authority,
        store: &*f.store,
        ids: f.h.ids.as_ref(),
    };
    let action = RouterConfigAction::ClearEmbeddingSpace {
        expected_selection_revision: Revision::new(2).unwrap(),
    };
    let frame = json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,"requestKey":"actual-selection-clear","body":{"kind":"router.configure","action":action}});
    let first = gateway
        .command(
            ctx,
            "router.configure",
            frame.to_string().as_bytes(),
            owner.clone(),
        )
        .unwrap();
    let replay = gateway
        .command(ctx, "router.configure", frame.to_string().as_bytes(), owner)
        .unwrap();
    assert!(matches!(
        first,
        devworks_core::protocol::CommandReply::Recorded {
            receipt: devworks_core::protocol::Receipt {
                decision: DomainDecision::Applied { .. },
                ..
            },
            replayed: false,
            ..
        }
    ));
    assert!(matches!(
        replay,
        devworks_core::protocol::CommandReply::Recorded { replayed: true, .. }
    ));
    let parents = operations(f)
        .into_iter()
        .filter(|op| op.payload["kind"] == SELECTION_KIND)
        .collect::<Vec<_>>();
    assert_eq!(
        parents.len(),
        1,
        "selection transaction enqueues ONE bounded parent, not every project"
    );
    assert!(children(f).is_empty());
    parents[0].id.clone()
}
struct LostPage {
    inner: Arc<dyn ApplicationStore>,
    target: Arc<Mutex<Option<OperationId>>>,
    armed: Arc<AtomicBool>,
}
struct PageJob {
    inner: Box<dyn WriteJob>,
    target: Arc<Mutex<Option<OperationId>>>,
    armed: Arc<AtomicBool>,
    hide: bool,
}
impl WriteJob for PageJob {
    fn run(&mut self, tx: &mut dyn WriteTransaction) -> Result<(), StorageError> {
        self.inner.run(tx)?;
        let key = self.target.lock().unwrap().clone();
        if let Some(key) = key
            && tx.operations().operation(&key)?.is_some_and(|op| {
                op.attempts
                    .iter()
                    .flat_map(|a| &a.observations)
                    .any(|v| v.get("selectionFanoutPage").is_some())
            })
            && self.armed.swap(false, Ordering::SeqCst)
        {
            self.hide = true;
        }
        Ok(())
    }
    fn finish(self: Box<Self>, outcome: Result<(), StorageError>) {
        self.inner.finish(if self.hide && outcome.is_ok() {
            Err(StorageError::CommitUnknown)
        } else {
            outcome
        });
    }
}
impl ApplicationStore for LostPage {
    fn info(&self) -> StoreInfo {
        self.inner.info()
    }
    fn read(
        &self,
        o: &ReadOptions,
        w: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.inner.read(o, w)
    }
    fn submit_write(&self, o: WriteOptions, j: Box<dyn WriteJob>) -> Result<(), StorageError> {
        self.inner.submit_write(
            o,
            Box::new(PageJob {
                inner: j,
                target: self.target.clone(),
                armed: self.armed.clone(),
                hide: false,
            }),
        )
    }
}
#[test]
fn real_selection_clear_queues_one_parent_then_recovers_all_65_project_pages_without_duplicate_children()
 {
    let f = Fx::new(
        "Embedding",
        Script::aggregate(super::convention_search::VECTOR),
        true,
    );
    let (ctx, native, _) = super::convention_search::qualify(&f);
    for n in 0..65 {
        project(&f, &format!("project-{n:03}"));
    }
    let target = Arc::new(Mutex::new(None));
    let wrapped: Arc<dyn ApplicationStore> = Arc::new(LostPage {
        inner: f.store.clone(),
        target: target.clone(),
        armed: Arc::new(AtomicBool::new(true)),
    });
    let index = Arc::new(IndexService::new(
        wrapped,
        f.artifacts.clone(),
        f.h.ids.clone(),
        f.h.clock(),
    ));
    index.set_embeddings(Some(native));
    let parent = clear(&f, index.clone(), &ctx);
    *target.lock().unwrap() = Some(parent.clone());
    let op = dispatch(&f, &parent);
    assert!(matches!(
        index.run_index(&op, &Cancellation::new(), None),
        Err(StorageError::CommitUnknown)
    ));
    assert_eq!(
        children(&f).len(),
        33,
        "one committed page is Workspace +32 actual projects"
    );
    let key = parent.clone();
    let unknown = write(&*f.store, move |tx| {
        let op = tx.operations().operation(&key)?.unwrap();
        tx.operations().transition(
            &key,
            op.revision,
            op.latest_attempt().map(|a| a.fence()).as_ref(),
            OperationState::OutcomeUnknown,
            None,
        )
    })
    .unwrap();
    let IndexOutcome::Completed(result) = index
        .run_index(&unknown, &Cancellation::new(), None)
        .unwrap()
    else {
        panic!("reconcilefanout")
    };
    assert_eq!(result["scopes"], 66);
    let jobs = children(&f);
    assert_eq!(jobs.len(), 66);
    let ids = jobs
        .iter()
        .map(|op| op.id.0.as_str().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids.len(), 66);
    for child in &jobs {
        assert_eq!(child.payload["actor"], json!(ctx.principal()));
        assert_eq!(
            child.payload["sourceContext"]["invocationContext"]["kind"],
            "Human"
        );
        let project = child.payload["sourceContext"]["invocationContext"]["project"].clone();
        if child.payload["scope"]["kind"] == "Project" {
            assert_eq!(project, child.payload["scope"]["projectId"]);
        } else {
            assert!(project.is_null());
        }
        assert!(child.root_id.is_none());
        assert!(child.assignment_id.is_none());
    }
    assert!(matches!(
        index
            .run_index(&unknown, &Cancellation::new(), None)
            .unwrap(),
        IndexOutcome::Completed(_)
    ));
    assert_eq!(children(&f).len(), 66);
    assert_eq!(
        f.http.lock().unwrap().requests.lock().unwrap().len(),
        1,
        "only fixed qualification probe; selection fanout never calls provider"
    );
}
#[test]
fn project_and_session_bound_human_keeps_same_lineage_for_shared_and_own_scope_without_siblings() {
    let f = Fx::new(
        "Embedding",
        Script::aggregate(super::convention_search::VECTOR),
        true,
    );
    let (ctx, _, index) = super::convention_search::qualify(&f);
    project(&f, "project-a");
    project(&f, "project-b");
    let actor = ctx.principal().clone();
    write(&*f.store, move |tx| {
        let host = tx.store().host_actor.clone();
        tx.sessions().insert_session(&StoredSession {
            id: id("bound-session"),
            conversation_id: id("bound-conversation"),
            agent_id: host,
            project_id: Some(ProjectId(id("project-a"))),
            root_id: None,
            title: "Bound session".into(),
            context_epoch: 0,
            revision: 1,
            created_by: actor,
            created_at_ms: T0,
            closed_at_ms: None,
            closed_by: None,
            next_sequence: 1,
            checkpoint_sequence: 0,
        })
    })
    .unwrap();
    let bound = ctx
        .with_lineage(Some(ProjectId(id("project-a"))), None, None)
        .with_session(id("bound-session"));
    let parent = clear(&f, index.clone(), &bound);
    let op = dispatch(&f, &parent);
    assert!(matches!(
        index.run_index(&op, &Cancellation::new(), None).unwrap(),
        IndexOutcome::Completed(_)
    ));
    let jobs = children(&f);
    assert_eq!(jobs.len(), 2);
    assert!(
        jobs.iter()
            .any(|j| j.payload["scope"]["kind"] == "Workspace")
    );
    assert!(
        jobs.iter()
            .any(|j| j.payload["scope"]["projectId"] == "project-a")
    );
    for job in jobs {
        assert_eq!(
            job.payload["sourceContext"]["invocationContext"]["project"],
            "project-a"
        );
        assert_eq!(
            job.payload["sourceContext"]["invocationContext"]["session"],
            "bound-session"
        );
        assert_eq!(job.payload["actor"], json!(bound.principal()));
        let actual = dispatch(&f, &job.id);
        assert!(matches!(
            index
                .run_index(&actual, &Cancellation::new(), None)
                .unwrap(),
            IndexOutcome::Completed(_)
        ));
    }
    assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);
}
