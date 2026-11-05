//! Custom project workflows through the shared gateway and real SQLite store. No inference.
mod common;
use common::*;
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::gateway::Gateway;
use devworks_core::ports::storage::*;
use devworks_core::protocol::records::StatusRecord;
use devworks_core::protocol::results::{DomainDecision, DomainQueryReply, OperationResult};
use devworks_core::protocol::{CommandReply, ProjectId, RootId};
use devworks_core::services::{UseCases, runs::ControlService, work::WorkService};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
struct Fx {
    h: Harness,
    store: Arc<dyn ApplicationStore>,
    cases: UseCases,
    user: InvocationContext,
    keys: AtomicUsize,
}
impl Fx {
    fn new() -> Self {
        let h = Harness::new();
        let store = h.open();
        let db = rusqlite::Connection::open(h.root.join("workspace.sqlite")).unwrap();
        db.execute_batch("INSERT INTO projects(id,title,lifecycle,revision,scope_revision,created_at) VALUES('project','P','Active',1,1,0),('other','O','Active',1,1,0);").unwrap();
        let user = principal(&*store, "user", PrincipalKind::Human);
        let cases = UseCases::new().with_work(Arc::new(WorkService::new(
            h.ids.clone(),
            Arc::new(ControlService::new(h.ids.clone())),
        )));
        Self {
            h,
            store,
            cases,
            user,
            keys: AtomicUsize::new(0),
        }
    }
    fn gateway(&self) -> Gateway<'_> {
        Gateway {
            authority: &Authority,
            store: &*self.store,
            ids: &*self.h.ids,
        }
    }
    fn command(&self, op: &str, body: Value) -> CommandReply {
        let revision = if op.starts_with("workflows.") && op != "workflows.reassign_and_remove" {
            3
        } else {
            1
        };
        let frame = json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":revision,"requestKey":format!("key-{}",self.keys.fetch_add(1,Ordering::SeqCst)),"body":body});
        self.cases
            .command(
                &self.gateway(),
                &self.user,
                op,
                frame.to_string().as_bytes(),
            )
            .unwrap()
    }
    fn statuses(&self) -> Vec<StatusRecord> {
        let frame = json!({"mode":"Query","protocol":"1.0-draft.1","schemaRevision":3,"queryId":"statuses","body":{"kind":"workflows.list","projectId":"project","page":{"limit":500}}});
        match self
            .cases
            .query(
                &self.gateway(),
                &self.user,
                "workflows.list",
                frame.to_string().as_bytes(),
            )
            .unwrap()
        {
            DomainQueryReply::Result {
                result: OperationResult::WorkflowsList { value },
                ..
            } => value.items.0,
            other => panic!("{other:?}"),
        }
    }
    fn create(&self, label: &str, category: &str) -> StatusRecord {
        let revision = self.statuses()[0].workflow_revision.clone().unwrap();
        match applied(self.command("workflows.create_status",json!({"kind":"workflows.create_status","projectId":"project","expectedWorkflowRevision":revision,"label":label,"category":category}))){
            OperationResult::WorkflowsCreateStatus{value}=>value,other=>panic!("{other:?}")}
    }
    fn remove(&self, source: &StatusRecord, destination: &StatusRecord) -> CommandReply {
        self.command("workflows.reassign_and_remove",json!({"kind":"workflows.reassign_and_remove","projectId":"project","expectedWorkflowRevision":source.workflow_revision,"removeStatusId":source.id,"destinationStatusId":destination.id,"expectedAffectedWorkDigest":source.affected_work_digest}))
    }
    fn issue(&self, status: &StatusRecord) -> devworks_core::protocol::Id {
        match applied(self.command("issues.create",json!({"kind":"issues.create","projectId":"project","title":"Issue","description":"","repositoryIds":[],"statusId":status.id,"priority":1}))){
            OperationResult::IssuesCreate{value}=>value.id,other=>panic!("{other:?}")}
    }
}
fn applied(reply: CommandReply) -> OperationResult {
    match reply {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied { result } | DomainDecision::NoChange { result } => result,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}
fn refused(reply: CommandReply) {
    assert!(
        !matches!(reply,CommandReply::Recorded{receipt,..}if matches!(receipt.decision,DomainDecision::Applied{..}|DomainDecision::NoChange{..})),
        "operation unexpectedly changed data"
    );
}

#[test]
fn virtual_defaults_and_custom_labels_keep_six_explicit_semantics_and_revisioned_order() {
    let f = Fx::new();
    let defaults = f.statuses();
    assert_eq!(defaults.len(), 6);
    assert!(
        read(&*f.store, |s| s.work().statuses(&ProjectId(id("project")))).is_empty(),
        "query is read-only"
    );
    let added = f.create("Ready, despite its label", "Backlog");
    assert_eq!(added.category.0, "Backlog");
    assert!(added.workflow_revision.is_some());
    let renamed=applied(f.command("workflows.rename_status",json!({"kind":"workflows.rename_status","projectId":"project","statusId":added.id,"expectedRevision":added.revision,"label":"Ship it"})));
    let OperationResult::WorkflowsRenameStatus { value: renamed } = renamed else {
        panic!()
    };
    assert_eq!(renamed.category.0, "Backlog");
    let current = f.statuses();
    let mut order: Vec<_> = current.iter().map(|s| s.id.clone()).collect();
    order.reverse();
    applied(f.command("workflows.order",json!({"kind":"workflows.order","projectId":"project","expectedWorkflowRevision":current[0].workflow_revision,"orderedStatusIds":order})));
    assert_eq!(
        f.statuses()
            .iter()
            .map(|s| s.id.clone())
            .collect::<Vec<_>>(),
        order
    );
    refused(f.command("workflows.rename_status",json!({"kind":"workflows.rename_status","projectId":"project","statusId":added.id,"expectedRevision":added.revision,"label":"Stale"})));
    assert_eq!(
        f.statuses()
            .iter()
            .find(|s| s.id == added.id)
            .unwrap()
            .label
            .0,
        "Ship it"
    );
}

#[test]
fn invalid_category_partial_order_and_cross_project_status_are_refused_without_writes() {
    let f = Fx::new();
    let defaults = f.statuses();
    refused(f.command("workflows.create_status",json!({"kind":"workflows.create_status","projectId":"project","expectedWorkflowRevision":"1","label":"Custom","category":"Magic"})));
    assert!(read(&*f.store, |s| s.work().statuses(&ProjectId(id("project")))).is_empty());
    refused(f.command("workflows.order",json!({"kind":"workflows.order","projectId":"project","expectedWorkflowRevision":"1","orderedStatusIds":[defaults[0].id]})));
    refused(f.command("workflows.order",json!({"kind":"workflows.order","projectId":"project","expectedWorkflowRevision":"1","orderedStatusIds":[defaults[0].id,defaults[0].id]})));
    refused(f.command("workflows.rename_status",json!({"kind":"workflows.rename_status","projectId":"other","statusId":defaults[0].id,"expectedRevision":"1","label":"Foreign"})));
    assert!(read(&*f.store, |s| s.work().statuses(&ProjectId(id("project")))).is_empty());
    assert_eq!(f.statuses().len(), 6);
}

#[test]
fn occupied_removal_fences_preview_and_reassigns_history_revision_and_queue_atomically() {
    let f = Fx::new();
    let destination = f.create("New backlog", "Backlog");
    let source = f
        .statuses()
        .into_iter()
        .find(|s| s.id.as_str() == "project-backlog")
        .unwrap();
    let work = f.issue(&source);
    refused(f.remove(&source, &destination)); // occupancy changed after the preview
    let current = f.statuses();
    let source = current
        .iter()
        .find(|s| s.id.as_str() == "project-backlog")
        .unwrap();
    let destination = current.iter().find(|s| s.id == destination.id).unwrap();
    assert_eq!(source.affected_work_count.as_ref().unwrap().get(), 1);
    let old = read(&*f.store, |s| s.work().work(&work)).unwrap();
    let queue = read(&*f.store, |s| {
        s.work()
            .queue_revision(&ProjectId(id("project")), PROJECT_QUEUE)
    });
    applied(f.remove(source, destination));
    let new = read(&*f.store, |s| s.work().work(&work)).unwrap();
    assert_eq!(new.revision, old.revision + 1);
    assert_eq!(new.status_id.as_ref(), Some(&destination.id));
    assert_eq!(new.completion, old.completion);
    assert_eq!(new.brief, old.brief);
    assert!(
        read(&*f.store, |s| s
            .work()
            .queue_revision(&ProjectId(id("project")), PROJECT_QUEUE))
            > queue
    );
    let events = read(&*f.store, |s| {
        s.history()
            .page(&EventFilter::Project(ProjectId(id("project"))), 0, 500)
    });
    assert!(
        events
            .events
            .iter()
            .any(|e| e.kind == "WorkStatusReassigned")
    );
    assert!(
        events
            .events
            .iter()
            .any(|e| e.kind == "WorkflowStatusRemoved")
    );
    assert!(!f.statuses().iter().any(|s| s.id == source.id));
    // Ordinary issue creation can address the surviving custom Backlog identity.
    let mut body = json!({"kind":"issues.create","projectId":"project","title":"After","description":"","repositoryIds":[],"priority":1});
    body["statusId"] = json!(destination.id);
    applied(f.command("issues.create", body));
}

#[test]
fn status_removal_cannot_promote_unverified_work_to_done_or_change_execution_holds() {
    let f = Fx::new();
    let extra = f.create("Another Done", "Done");
    let current = f.statuses();
    let backlog = current.iter().find(|s| s.category.0 == "Backlog").unwrap();
    let done = current.iter().find(|s| s.category.0 == "Done").unwrap();
    let work = f.issue(backlog);
    let control = StoredRootControl {
        desired: DesiredExecution::Paused,
        ..StoredRootControl::initial(RootId(work.clone()))
    };
    write(&*f.store, {
        let c = control.clone();
        move |tx| tx.runs().insert_root_control(&c)
    })
    .unwrap();
    refused(f.remove(backlog, done));
    let before = read(&*f.store, |s| s.work().work(&work)).unwrap();
    assert_eq!(before.completion, None);
    assert_eq!(before.status_id.as_ref(), Some(&backlog.id));
    // A legitimate manual Done record remains manual after merging two Done columns.
    write(&*f.store, {
        let work = work.clone();
        let done = done.id.clone();
        move |tx| {
            let mut w = tx.work().work(&work)?.unwrap();
            let rev = w.revision;
            w.status_id = Some(done);
            w.completion = Some(MANUAL.into());
            w.revision += 1;
            tx.work().update_work(&w, rev)
        }
    })
    .unwrap();
    let current = f.statuses();
    let source = current.iter().find(|s| s.id == done.id).unwrap();
    let destination = current.iter().find(|s| s.id == extra.id).unwrap();
    applied(f.remove(source, destination));
    assert_eq!(
        read(&*f.store, |s| s.work().work(&work))
            .unwrap()
            .completion
            .as_deref(),
        Some(MANUAL)
    );
    assert_eq!(
        read(&*f.store, |s| s.runs().root_control(&RootId(work.clone()))),
        Some(control)
    );
}
