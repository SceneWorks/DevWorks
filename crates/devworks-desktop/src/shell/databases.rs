//! Native database ingress: exact shared frames, purpose-bound password intake and paging.
use super::Shell;
use crate::{
    backend::{Reply, Slot, TaskOutput},
    databases_screen::{Action, Draft},
    ops::{self, Failure},
};
use devworks_core::{
    ports::secrets::Secret,
    protocol::{
        CommandReply,
        results::{DomainDecision, OperationResult},
    },
};
use eframe::egui::Ui;
use serde_json::{Value, json};
fn query(b: &dyn crate::backend::Backend, op: &str, body: Value) -> Vec<u8> {
    ops::at_revision(ops::query_frame(b, op, body), 5)
}
fn command(b: &dyn crate::backend::Backend, op: &str, body: Value) -> Vec<u8> {
    ops::at_revision(ops::command_frame(b, op, body), 5)
}
impl Shell {
    pub(super) fn load_databases(&mut self, append: bool) {
        if self.databases.loading {
            return;
        }
        if self.databases.project != self.selected_project
            && self.databases.busy
            && !self.databases.batch_active
        {
            return;
        }
        if !self.databases.batch_active
            && !self.databases.batch_scanning
            && !append
            && self.databases.project != self.selected_project
        {
            let draft = self.databases.draft.take();
            let sequence = self.databases.sequence.wrapping_add(1);
            let read_sequence = self.databases.read_sequence.wrapping_add(1);
            self.databases = Default::default();
            self.databases.sequence = sequence;
            self.databases.read_sequence = read_sequence;
            self.databases.draft = draft;
            self.databases.project = self.selected_project.clone();
        }
        self.databases.sequence = self.databases.sequence.wrapping_add(1);
        let n = self.databases.sequence;
        let body = json!({"scope":if self.databases.batch_scanning{crate::databases_screen::scope(self.databases.batch_scope.as_deref())}else{self.databases.scope()},"page":{"limit":100,"cursor":self.databases.next.clone().filter(|_|append)}});
        let mut body = body;
        if !append || self.databases.next.is_none() {
            body["page"].as_object_mut().unwrap().remove("cursor");
        }
        self.databases.loading = true;
        if !self.query(
            Slot::DatabaseList(n, append),
            "Loading database sources",
            "schemas.connections",
            move |b| query(b, "schemas.connections", body),
        ) {
            self.databases.loading = false;
        }
    }
    fn load_database_objects(&mut self, append: bool) {
        let Some(source) = self.databases.source().cloned() else {
            return;
        };
        if self.databases.view.is_none() {
            self.databases.view = source.current_view_id.map(|id| id.as_str().into());
        }
        let Some(view) = self.databases.view.clone() else {
            self.databases.error =
                Some("No published schema view is available. Sync this source first.".into());
            return;
        };
        if !append {
            self.databases.objects.clear();
            self.databases.object_next = None;
        }
        self.databases.read_sequence = self.databases.read_sequence.wrapping_add(1);
        let n = self.databases.read_sequence;
        let mut body = json!({"scope":self.databases.scope(),"views":{"kind":"Exact","viewIds":[view]},"connectionId":source.id,"kinds":if self.databases.kind.is_empty(){vec![]}else{vec![self.databases.kind.clone()]},"page":{"limit":100}});
        if let Some(cursor) = self.databases.object_next.clone().filter(|_| append) {
            body["page"]["cursor"] = json!(cursor);
        }
        if !self.databases.filter.is_empty() {
            body["text"] = json!(self.databases.filter);
        }
        if let Some(id) = &self.databases.object_id {
            body["objectId"] = json!(id);
        }
        if let Some(parent) = &self.databases.parent {
            body["parentId"] = json!(parent);
        }
        self.databases.read_loading = true;
        if !self.query(
            Slot::DatabaseObjects(n, append),
            "Reading schema objects",
            "schemas.objects",
            move |b| query(b, "schemas.objects", body),
        ) {
            self.databases.read_loading = false;
        }
    }
    fn load_database_relations(&mut self, ids: Vec<String>, append: bool) {
        let (Some(source), Some(view)) = (
            self.databases.source().cloned(),
            self.databases.view.clone(),
        ) else {
            return;
        };
        if !append {
            self.databases.relations.clear();
            self.databases.relation_next = None;
            self.databases.relation_ids = ids.clone();
        }
        self.databases.read_sequence = self.databases.read_sequence.wrapping_add(1);
        let n = self.databases.read_sequence;
        let mut body = json!({"scope":self.databases.scope(),"views":{"kind":"Exact","viewIds":[view]},"connectionId":source.id,"objectIds":ids,"page":{"limit":100}});
        if let Some(cursor) = self.databases.relation_next.clone().filter(|_| append) {
            body["page"]["cursor"] = json!(cursor);
        }
        self.databases.read_loading = true;
        if !self.query(
            Slot::DatabaseRelations(n, append),
            "Reading schema relationships",
            "schemas.relationships",
            move |b| query(b, "schemas.relationships", body),
        ) {
            self.databases.read_loading = false;
        }
    }
    fn database_command(&mut self, operation: &'static str, body: Value) {
        self.databases.busy = true;
        if !self.command(
            Slot::DatabaseCommand(operation.into()),
            "Updating database source",
            operation,
            move |b| command(b, operation, body),
        ) {
            self.databases.busy = false;
        }
    }
    fn save_database(&mut self) {
        let Some(draft) = self.databases.draft.as_mut() else {
            return;
        };
        if let Some(path) = draft.ca_file.take() {
            self.databases.busy = true;
            let submitted = self.client.as_mut().is_some_and(|client| {
                client.task(
                    Slot::DatabaseCaPrepare,
                    "Preparing database certificate",
                    move |b| {
                        let (_, bytes) = crate::files::open_regular(&path)
                            .map_err(|_| "Cannot read the selected certificate".to_owned())?;
                        if bytes > 1024 * 1024 {
                            return Err("Certificate exceeds1MiB".into());
                        }
                        let name = path
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("database-ca.pem");
                        crate::attachments::prepare_file(
                            b,
                            &path,
                            name,
                            "application/x-pem-file",
                            "database-ca",
                        )
                    },
                )
            });
            if !submitted {
                self.databases.busy = false;
            }
            return;
        }
        let mut body = json!({"scope":crate::databases_screen::scope(draft.scope.as_deref()),"action":"Save","config":draft.config});
        if let Some(id) = &draft.id {
            body["connectionId"] = json!(id);
            body["expectedRevision"] = json!(draft.revision.unwrap_or(1).to_string());
        }
        let password = Secret::new(std::mem::take(&mut draft.password));
        self.databases.busy = true;
        let submitted = self.client.as_mut().is_some_and(|client| {
            client.task(
                Slot::DatabasePrepare,
                "Preparing database source",
                move |b| {
                    if !password.is_empty() {
                        body["config"]["credentialRef"] =
                            json!(b.stage_database_password(password)?);
                    }
                    Ok(TaskOutput::Prepared {
                        frame: command(b, "schemas.configure", body),
                    })
                },
            )
        });
        if !submitted {
            self.databases.busy = false;
        }
    }
    pub(super) fn databases_screen(&mut self, ui: &mut Ui) {
        if self.databases.project != self.selected_project
            || (!self.databases.loaded && !self.databases.loading)
        {
            self.load_databases(false);
        }
        if !self.projects.list.loaded {
            self.load_projects();
        }
        let projects = self
            .projects
            .list
            .items
            .iter()
            .map(|p| (p.id.0.as_str().into(), p.name.0.clone()))
            .collect::<Vec<_>>();
        let mut actions = Vec::new();
        let online = self.online().is_ok();
        eframe::egui::ScrollArea::vertical()
            .id_salt(("database-screen", self.databases.project.clone()))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.databases.ui(ui, online, &projects, &mut actions)
            });
        for action in actions {
            self.database_action(action);
        }
    }
    fn database_action(&mut self, action: Action) {
        match action {
            Action::Refresh => {
                self.databases.view = None;
                self.databases.objects.clear();
                self.databases.relations.clear();
                self.databases.selected_object = None;
                self.load_databases(false);
            }
            Action::More => self.load_databases(true),
            Action::New => self.databases.draft = Some(Draft::new(self.databases.project.clone())),
            Action::Edit(id) => {
                self.databases.draft = self
                    .databases
                    .connections
                    .iter()
                    .find(|s| s.id.as_str() == id)
                    .and_then(|s| Draft::edit(s, self.databases.project.clone()))
            }
            Action::Select(id) => {
                self.databases.selected = Some(id);
                self.databases.view = None;
                self.databases.parent = None;
                self.databases.objects.clear();
                self.databases.relations.clear();
                self.databases.selected_object = None;
                self.databases.error = None;
                self.load_database_objects(false);
            }
            Action::Save => self.save_database(),
            Action::CancelDraft => self.databases.draft = None,
            Action::PickDatabase => {
                if let Some(path) = self.dialogs.pick_files().first()
                    && let Some(d) = self.databases.draft.as_mut()
                {
                    d.config["endpoint"] = json!(path.to_string_lossy());
                }
            }
            Action::PickCa => {
                if let Some(path) = self.dialogs.pick_files().first()
                    && let Some(d) = self.databases.draft.as_mut()
                {
                    d.ca_file = Some(path.clone());
                }
            }
            Action::Index(id, test) => {
                if let Some(s) = self
                    .databases
                    .connections
                    .iter()
                    .find(|s| s.id.as_str() == id)
                {
                    let body = json!({"connectionId":s.id,"expectedRevision":s.revision,"scope":self.databases.scope(),"testOnly":test});
                    self.database_command("schemas.refresh", body);
                }
            }
            Action::SyncAll => self.begin_database_sync_all(),
            Action::Remove(kind) => {
                self.databases.confirm = Some(kind);
            }
            Action::CancelRemove => self.databases.confirm = None,
            Action::ConfirmRemove => {
                if let Some(source) = self.databases.source().cloned() {
                    let kind = self.databases.confirm.take();
                    if kind == Some("source") {
                        self.database_command("schemas.configure",json!({"scope":self.databases.scope(),"connectionId":source.id,"expectedRevision":source.revision,"action":"Remove","config":source.config}));
                    } else {
                        self.database_command(
                            "schemas.remove_index",
                            json!({"connectionId":source.id,"expectedRevision":source.revision}),
                        );
                    }
                }
            }
            Action::Objects(append) => {
                self.databases.object_id = None;
                self.load_database_objects(append);
            }
            Action::Endpoint(id) => {
                self.databases.object_id = Some(id);
                self.databases.parent = None;
                self.databases.kind.clear();
                self.databases.filter.clear();
                self.load_database_objects(false);
            }
            Action::Inspect(object) => self.databases.selected_object = Some(object),
            Action::Children(id) => {
                self.databases.parent = Some(id);
                self.load_database_objects(false);
            }
            Action::Relations(ids, append) => self.load_database_relations(ids, append),
            Action::ConfirmOutcome => self.confirm_outcomes(),
        }
    }
    pub(super) fn apply_databases(&mut self, slot: Slot, reply: Reply) -> Option<(Slot, Reply)> {
        match (slot, reply) {
            (Slot::DatabaseList(n, _), _) if n != self.databases.sequence => {}
            (Slot::DatabaseList(_, append), Reply::Query(result)) => {
                self.databases.loading = false;
                match ops::query_result(result) {
                    Ok((OperationResult::SchemasConnections { value }, _)) => {
                        if self.databases.batch_scanning {
                            self.scan_database_batch(value);
                            return None;
                        }
                        if !append {
                            self.databases.connections.clear();
                        }
                        self.databases.connections.extend(value.items.0);
                        self.databases.next =
                            value.next_cursor.filter(|_| value.has_more).map(|s| s.0);
                        self.databases.loaded = true;
                    }
                    Ok((other, _)) => {
                        self.databases.error =
                            Some(ops::unexpected("schemas.connections", &other).describe())
                    }
                    Err(e) => {
                        self.databases.error = Some(e.describe());
                        self.databases.batch_scanning = false;
                        self.databases.batch_active = false;
                        self.databases.batch_remaining.clear();
                        self.databases.busy = false;
                    }
                }
            }
            (Slot::DatabaseObjects(n, _) | Slot::DatabaseRelations(n, _), _)
                if n != self.databases.read_sequence => {}
            (Slot::DatabaseObjects(_, append), Reply::Query(result)) => {
                self.databases.read_loading = false;
                match ops::query_result(result) {
                    Ok((OperationResult::SchemasObjects { value }, _)) => {
                        if !append {
                            self.databases.objects.clear();
                        }
                        if self.databases.object_id.is_some() {
                            self.databases.selected_object = value.items.0.first().cloned();
                        }
                        self.databases.objects.extend(value.items.0);
                        self.databases.object_next =
                            value.next_cursor.filter(|_| value.has_more).map(|c| c.0);
                        self.databases.error = None;
                    }
                    Ok((o, _)) => {
                        self.databases.error =
                            Some(ops::unexpected("schemas.objects", &o).describe())
                    }
                    Err(e) => self.databases.error = Some(e.describe()),
                }
            }
            (Slot::DatabaseRelations(_, append), Reply::Query(result)) => {
                self.databases.read_loading = false;
                match ops::query_result(result) {
                    Ok((OperationResult::SchemasRelationships { value }, _)) => {
                        if !append {
                            self.databases.relations.clear();
                        }
                        self.databases.relations.extend(value.items.0);
                        self.databases.relation_next =
                            value.next_cursor.filter(|_| value.has_more).map(|c| c.0);
                    }
                    Ok((o, _)) => {
                        self.databases.error =
                            Some(ops::unexpected("schemas.relationships", &o).describe())
                    }
                    Err(e) => self.databases.error = Some(e.describe()),
                }
            }
            (Slot::DatabasePrepare, Reply::Task(result)) => match result {
                Ok(TaskOutput::Prepared { frame }) => {
                    if !self.command(
                        Slot::DatabaseCommand("schemas.configure".into()),
                        "Saving database source",
                        "schemas.configure",
                        move |_| frame,
                    ) {
                        self.databases.busy = false;
                    }
                }
                Ok(_) => {
                    self.databases.busy = false;
                    self.databases.error = Some("Unexpected source preparation result".into());
                }
                Err(error) => {
                    self.databases.busy = false;
                    self.databases.error = Some(error);
                }
            },
            (Slot::DatabaseCaPrepare, Reply::Task(result)) => match result {
                Ok(TaskOutput::Prepared { frame }) => {
                    if !self.command(
                        Slot::DatabaseCa,
                        "Saving database certificate",
                        "artifacts.save",
                        move |_| frame,
                    ) {
                        self.databases.busy = false;
                    }
                }
                Ok(_) => {
                    self.databases.busy = false;
                    self.databases.error = Some("Unexpected certificate preparation result".into());
                }
                Err(error) => {
                    self.databases.busy = false;
                    self.databases.error = Some(error);
                }
            },
            (Slot::DatabaseCa, Reply::Command(outcome)) => match ops::saved_artifact(outcome) {
                Ok(artifact) => {
                    if let Some(draft) = self.databases.draft.as_mut() {
                        draft.config["caCertificate"] = json!(artifact.artifact);
                        self.save_database();
                    } else {
                        self.databases.busy = false;
                        self.databases.notice =
                            Some("Certificate saved. Open the source editor to attach it.".into());
                    }
                }
                Err(Failure::Unknown) => {
                    self.databases.busy = true;
                    self.databases.error=Some("Certificate outcome is unconfirmed. Confirm the original outcome before saving.".into());
                }
                Err(e) => {
                    self.databases.busy = false;
                    self.databases.error = Some(e.describe());
                }
            },
            (Slot::DatabaseCommand(_), Reply::Command(outcome)) => {
                self.databases.busy = false;
                match outcome {
                    Ok(CommandReply::Recorded { receipt, .. }) => match receipt.decision {
                        DomainDecision::Applied { result }
                        | DomainDecision::NoChange { result } => {
                            if let OperationResult::SchemasConfigure { value } = result {
                                self.databases.selected = (!matches!(
                                    value.state,
                                    devworks_core::protocol::records::ItemState::Disabled
                                ))
                                .then(|| value.id.as_str().into());
                                self.databases.draft = None;
                            }
                            self.databases.notice = Some("Database source change recorded.".into());
                            self.databases.error = None;
                        }
                        DomainDecision::Accepted { operation_id } => {
                            self.databases.notice = Some(format!(
                                "Metadata work accepted as {}. Refresh to inspect its actual outcome.",
                                operation_id.0.as_str()
                            ));
                            self.databases.error = None;
                        }
                        DomainDecision::Refused { fault } => {
                            self.databases.error = Some(Failure::Refused(fault).describe())
                        }
                    },
                    Ok(CommandReply::CommandStatusUnknown { .. }) => {
                        self.databases.busy = true;
                        self.databases.error = Some(Failure::Unknown.describe());
                    }
                    Ok(other) => {
                        self.databases.error =
                            Some(format!("Database command was rejected: {other:?}"))
                    }
                    Err(error) => self.databases.error = Some(error.to_string()),
                }
                if !self.databases.busy {
                    if self.databases.batch_active {
                        if self.databases.error.is_some() {
                            self.databases.batch_failures += 1;
                        }
                        self.next_database_batch();
                    } else if !self.databases.batch_remaining.is_empty() {
                        self.next_database_batch();
                    } else {
                        self.load_databases(false);
                    }
                }
            }
            (slot, reply) => return Some((slot, reply)),
        }
        None
    }
}
impl Shell {
    fn begin_database_sync_all(&mut self) {
        self.databases.batch_remaining.clear();
        self.databases.batch_scanning = true;
        self.databases.batch_active = true;
        self.databases.batch_scope = self.databases.project.clone();
        self.databases.batch_submitted = 0;
        self.databases.batch_failures = 0;
        self.databases.busy = true;
        self.databases.next = None;
        self.load_databases(false);
    }
    fn scan_database_batch(
        &mut self,
        page: devworks_core::protocol::support::Page<
            devworks_core::protocol::records::SchemaConnection,
        >,
    ) {
        for source in page.items.0 {
            if !matches!(
                source.state,
                devworks_core::protocol::records::ItemState::Disabled
            ) && source.active_operation_id.is_none()
            {
                self.databases
                    .batch_remaining
                    .push_back((source.id.as_str().into(), source.revision.get()));
            }
        }
        self.databases.next = page.next_cursor.filter(|_| page.has_more).map(|c| c.0);
        if self.databases.next.is_some() {
            self.load_databases(true);
        } else {
            self.databases.batch_scanning = false;
            self.next_database_batch();
        }
    }
    pub(super) fn next_database_batch(&mut self) {
        if let Some((id, revision)) = self.databases.batch_remaining.pop_front() {
            self.databases.batch_submitted += 1;
            let body = json!({"connectionId":id,"expectedRevision":revision.to_string(),"scope":crate::databases_screen::scope(self.databases.batch_scope.as_deref())});
            self.database_command("schemas.refresh", body);
        } else {
            self.databases.busy = false;
            self.databases.batch_active = false;
            self.databases.notice = Some(format!(
                "Sync-all admission finished for {} sources ({} refusals). Refresh to inspect actual metadata outcomes.",
                self.databases.batch_submitted, self.databases.batch_failures
            ));
            self.load_databases(false);
        }
    }
}

impl Shell {
    pub(super) fn resume_database_batch(&mut self) {
        if self.databases.batch_scanning {
            self.databases.batch_remaining.clear();
            self.databases.next = None;
            self.databases.busy = true;
            self.load_databases(false);
        } else if self.databases.batch_active && !self.databases.busy {
            self.next_database_batch();
        }
    }
}
