//! Native database ingress: exact shared frames, purpose-bound password intake and paging.
use crate::{
    backend::{Reply, TaskOutput, Unresolved},
    databases_ops::{self, DatabasesSlot, ObjectsRead, command, query},
    databases_screen::{Action, DatabasesScreen, Draft, ReadIntent},
    memory::RouteMemory,
    ops::{self, Failure},
    paged::{Applied, Fetch, Page},
    route::Route,
    screen::{Cx, Effect, Screen, ScreenContext, ScreenId, controller},
};
use devworks_core::{
    operations::OperationInput as In,
    ports::{secrets::Secret, storage::ProjectPurgeClientScope},
    protocol::{
        CommandReply,
        results::{DomainDecision, OperationResult},
    },
};
use eframe::egui::Ui;
use serde_json::{Value, json};

controller!(DatabasesCtl, databases: DatabasesScreen);

impl Screen for DatabasesScreen {
    type Slot = DatabasesSlot;
    const ID: ScreenId = ScreenId::Databases;
    fn load(&mut self, cx: &mut Cx, _route: Route) {
        DatabasesCtl {
            databases: self,
            cx,
        }
        .load_databases(false);
    }
    fn refresh(&mut self, cx: &mut Cx, _route: Route) {
        if !self.connections.loading() && !self.batch_scanning {
            DatabasesCtl {
                databases: self,
                cx,
            }
            .load_databases(false);
        }
    }
    fn apply(&mut self, cx: &mut Cx, slot: DatabasesSlot, reply: Reply) {
        let unhandled = DatabasesCtl {
            databases: self,
            cx,
        }
        .apply_databases(slot, reply);
        if let Some((slot, reply)) = unhandled {
            cx.unhandled(&slot, reply);
        }
    }
    fn ui(&mut self, cx: &mut Cx, _route: Route, ui: &mut Ui) {
        DatabasesCtl {
            databases: self,
            cx,
        }
        .databases_screen(ui);
    }
    fn pending_memory(&self, _unresolved: &[&Unresolved]) -> Option<Value> {
        Some(DatabasesScreen::pending_memory(self)).filter(|v| !v.is_null())
    }
    fn restore_pending(&mut self, saved: &Value) {
        DatabasesScreen::restore_pending(self, saved);
    }
    fn restore_in_flight(&mut self, slot: &DatabasesSlot, _u: &Unresolved) {
        if matches!(
            slot,
            DatabasesSlot::DatabaseCa | DatabasesSlot::DatabaseCommand(_)
        ) {
            self.busy = true;
        }
    }
    fn restored(&mut self, cx: &mut Cx) {
        DatabasesCtl {
            databases: self,
            cx,
        }
        .resume_database_batch();
    }
    fn remove_purged(&mut self, scope: &ProjectPurgeClientScope) {
        let p = scope.project_id.0.as_str();
        if self.project.as_deref() == Some(p) {
            self.project = None;
            self.clear_explorer();
            self.connections.reset();
        }
        if let Some(d) = &mut self.draft {
            if let Some(a) = d
                .config
                .get_mut("projectAccess")
                .and_then(|v| v.as_array_mut())
            {
                a.retain(|v| v.get("projectId").and_then(|v| v.as_str()) != Some(p));
            }
            if d.scope.as_deref() == Some(p) {
                d.scope = None;
            }
        }
    }
    /// The open tab, and the inspected schema object or else the selected source.
    fn context(&self, _route: Route, _mem: &RouteMemory) -> ScreenContext {
        ScreenContext {
            tab: self.tab,
            selection: self
                .selected_object
                .as_ref()
                .map(|object| object.id.as_str().to_owned())
                .or(self.selected.clone()),
        }
    }
}

impl DatabasesCtl<'_, '_> {
    fn load_databases(&mut self, append: bool) {
        if self.databases.connections.loading() || self.databases.scan.loading() {
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
            // The lists keep their request counters, so a reply to the old scope stays refused.
            let mut lists = std::mem::take(&mut *self.databases);
            lists.connections.reset();
            lists.scan.reset();
            lists.objects.reset();
            lists.relations.reset();
            lists.code_references.reset();
            *self.databases = Default::default();
            self.databases.connections = lists.connections;
            self.databases.scan = lists.scan;
            self.databases.objects = lists.objects;
            self.databases.relations = lists.relations;
            self.databases.code_references = lists.code_references;
            self.databases.sequence = sequence;
            self.databases.draft = draft;
            self.databases.project = self.selected_project.clone();
        }
        self.databases.sequence = self.databases.sequence.wrapping_add(1);
        let scanning = self.databases.batch_scanning;
        let list = if scanning {
            &mut self.databases.scan
        } else {
            &mut self.databases.connections
        };
        // A sync-all scan always starts at the first page (its list was reset). The sources
        // list belongs to `databases.project`, which only changes where the lists are reset,
        // so a re-read of it covers every page shown.
        let Some(fetch) = (if append {
            list.more_page()
        } else if scanning {
            Some(list.reload())
        } else {
            Some(list.reread())
        }) else {
            return;
        };
        self.send_database_list(fetch);
    }
    /// Submits the inventory request `fetch` names, for the scan or the shown sources.
    fn send_database_list(&mut self, fetch: Fetch) {
        let scanning = self.databases.batch_scanning;
        let project = if scanning {
            self.databases.batch_scope.clone()
        } else {
            self.databases.project.clone()
        };
        let slot = DatabasesSlot::DatabaseList(fetch.generation, fetch.append());
        let cursor = fetch.cursor.clone();
        let generation = fetch.generation;
        if !self.query(
            slot,
            "Loading database sources",
            "schemas.connections",
            move |b| {
                query(
                    b,
                    databases_ops::connections(project.as_deref(), cursor.as_deref()),
                )
            },
        ) {
            let list = if scanning {
                &mut self.databases.scan
            } else {
                &mut self.databases.connections
            };
            list.abandon(generation);
        }
    }
    fn database_read_pin(
        &mut self,
        source: &devworks_core::protocol::records::SchemaConnection,
        intent: ReadIntent,
    ) -> Option<String> {
        if self.databases.view.is_none()
            && let Some(captured) = &source.read_view
        {
            self.databases.view = captured
                .schema_views
                .first()
                .map(|v| v.snapshot_id.as_str().into());
            self.databases.read_pin = Some(captured.pin_id.as_str().into());
        }
        let Some(pin) = self.databases.read_pin.clone() else {
            if self.databases.pending_read.is_some() {
                self.databases.pending_read = None;
                self.databases.error = Some("No retained schema view is available. Sync this source before reading its metadata.".into());
                return None;
            }
            // A fresh status can see a publication that changed during preparation. Ask
            // the owner once for a matching lease, then resume this same source's intent.
            self.databases.pending_read = Some((source.id.as_str().into(), intent));
            self.databases.error = None;
            self.load_databases(false);
            if !self.databases.connections.loading() {
                self.databases.pending_read = None;
                self.databases.error =
                    Some("Could not refresh the source's retained schema view.".into());
            }
            return None;
        };
        self.databases.pending_read = None;
        Some(pin)
    }
    fn load_database_objects(&mut self, append: bool) {
        let Some(source) = self.databases.source().cloned() else {
            return;
        };
        // Invalidate any earlier object/relationship reply when a new read intent starts,
        // including one that first needs a fresh source capture.
        self.databases.supersede_reads();
        let Some(pin) = self.database_read_pin(&source, ReadIntent::Objects(append)) else {
            return;
        };
        if !append {
            self.databases.objects.reset();
        }
        let Some(fetch) = (if append {
            self.databases.objects.more_page()
        } else {
            Some(self.databases.objects.reload())
        }) else {
            return;
        };
        let read = ObjectsRead {
            project: self.databases.project.clone(),
            pin,
            connection: source.id.clone(),
            kind: self.databases.kind.clone(),
            text: self.databases.filter.clone(),
            object: self.databases.object_id.clone(),
            parent: self.databases.parent.clone(),
            cursor: fetch.cursor.clone(),
        };
        let slot = DatabasesSlot::DatabaseObjects(fetch.generation, append);
        self.cx.paged(
            &mut self.databases.objects,
            fetch,
            slot,
            "Reading schema objects",
            "schemas.objects",
            move |b, _| query(b, read.input()),
        );
    }
    fn resume_database_read(&mut self) {
        if self.databases.batch_scanning {
            return;
        }
        let Some((id, intent)) = self.databases.pending_read.clone() else {
            return;
        };
        if self.databases.selected.as_deref() != Some(id.as_str()) {
            self.databases.pending_read = None;
        } else if self
            .databases
            .connections
            .items
            .iter()
            .any(|source| source.id.as_str() == id)
        {
            match intent {
                ReadIntent::Objects(append) => self.load_database_objects(append),
                ReadIntent::Relationships { ids, append } => {
                    self.load_database_relations(ids, append)
                }
                ReadIntent::CodeReferences { id, append } => {
                    self.load_database_code_references(id, append)
                }
            }
        } else if self.databases.connections.more() {
            // The selected source can be on a later inventory page. Never substitute the
            // first source just because a fresh first page does not contain its identity.
            self.load_databases(true);
        } else {
            self.databases.pending_read = None;
            self.databases.error =
                Some("The selected database source is no longer visible in this scope.".into());
        }
    }
    fn load_database_relations(&mut self, ids: Vec<String>, append: bool) {
        let Some(source) = self.databases.source().cloned() else {
            return;
        };
        self.databases.supersede_reads();
        let Some(pin) = self.database_read_pin(
            &source,
            ReadIntent::Relationships {
                ids: ids.clone(),
                append,
            },
        ) else {
            return;
        };
        if !append {
            self.databases.relations.reset();
            self.databases.relation_ids = ids.clone();
        }
        let Some(fetch) = (if append {
            self.databases.relations.more_page()
        } else {
            Some(self.databases.relations.reload())
        }) else {
            return;
        };
        let project = self.databases.project.clone();
        let slot = DatabasesSlot::DatabaseRelations(fetch.generation, append);
        self.cx.paged(
            &mut self.databases.relations,
            fetch,
            slot,
            "Reading schema relationships",
            "schemas.relationships",
            move |b, cursor| {
                query(
                    b,
                    databases_ops::relationships(
                        project.as_deref(),
                        &pin,
                        &source.id,
                        &ids,
                        cursor,
                    ),
                )
            },
        );
    }
    fn load_database_code_references(&mut self, id: String, append: bool) {
        let Some(source) = self.databases.source().cloned() else {
            return;
        };
        self.databases.supersede_reads();
        let Some(pin) = self.database_read_pin(
            &source,
            ReadIntent::CodeReferences {
                id: id.clone(),
                append,
            },
        ) else {
            return;
        };
        if !append || self.databases.code_object.as_deref() != Some(id.as_str()) {
            self.databases.code_references.reset();
            self.databases.code_object = Some(id.clone());
        }
        let Some(fetch) = (if append {
            self.databases.code_references.more_page()
        } else {
            Some(self.databases.code_references.reload())
        }) else {
            return;
        };
        let project = self.databases.project.clone();
        let connection = source.id.clone();
        let slot = DatabasesSlot::DatabaseCodeReferences(fetch.generation, append);
        self.cx.paged(
            &mut self.databases.code_references,
            fetch,
            slot,
            "Reading the code that uses a schema object",
            "schemas.code_references",
            move |b, cursor| {
                query(
                    b,
                    databases_ops::code_references(
                        project.as_deref(),
                        &pin,
                        &connection,
                        &id,
                        cursor,
                    ),
                )
            },
        );
    }
    fn database_command(&mut self, input: Option<In>) {
        let Some(input) = input else {
            self.databases.error = Some("The database request could not be formed.".into());
            return;
        };
        let operation = input.operation();
        self.databases.busy = true;
        if !self.command(
            DatabasesSlot::DatabaseCommand(operation.into()),
            "Updating database source",
            operation,
            move |b| command(b, input),
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
                    DatabasesSlot::DatabaseCaPrepare.into(),
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
        let project = draft.scope.clone();
        let existing = draft.id.clone().map(|id| (id, draft.revision.unwrap_or(1)));
        let mut config = draft.config.clone();
        if databases_ops::save(
            project.as_deref(),
            existing
                .as_ref()
                .map(|(id, revision)| (id.as_str(), *revision)),
            &config,
        )
        .is_none()
        {
            self.databases.error = Some(
                "Check the source settings: the name, endpoint and other values must be filled in and within their allowed length."
                    .into(),
            );
            return;
        }
        let password = Secret::new(std::mem::take(&mut draft.password));
        self.databases.busy = true;
        let submitted = self.client.as_mut().is_some_and(|client| {
            client.task(
                DatabasesSlot::DatabasePrepare.into(),
                "Preparing database source",
                move |b| {
                    if !password.is_empty() {
                        config["credentialRef"] = json!(b.stage_database_password(password)?);
                    }
                    let existing = existing
                        .as_ref()
                        .map(|(id, revision)| (id.as_str(), *revision));
                    Ok(TaskOutput::Prepared {
                        frame: command(
                            b,
                            databases_ops::save(project.as_deref(), existing, &config),
                        ),
                    })
                },
            )
        });
        if !submitted {
            self.databases.busy = false;
        }
    }
    fn databases_screen(&mut self, ui: &mut Ui) {
        if self.databases.project != self.selected_project
            || (!self.databases.connections.loaded && !self.databases.connections.loading())
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
        self.databases
            .ui(ui, self.cx.online(), &projects, &mut actions);
        for action in actions {
            self.database_action(action);
        }
    }
    fn database_action(&mut self, action: Action) {
        match action {
            Action::GenerateKey => match self
                .backend()
                .ok_or_else(|| "Application is offline".to_owned())
                .and_then(|b| b.generate_database_encryption_key())
            {
                Ok(key) => {
                    self.databases.generated_key = Some(key);
                    self.databases.error = None;
                }
                Err(error) => self.databases.error = Some(error),
            },
            Action::ClearKey => self.databases.generated_key = None,
            Action::Refresh => {
                self.databases.stop_reads();
                self.databases.view = None;
                self.databases.read_pin = None;
                self.databases.objects.reset();
                self.databases.relations.reset();
                self.databases.selected_object = None;
                self.load_databases(false);
            }
            Action::More => self.load_databases(true),
            Action::New => {
                self.databases.draft = Some(Draft::new(self.databases.project.clone()));
                self.databases.tab = 2;
            }
            Action::Edit(id) => {
                self.database_action(Action::Select(id.clone()));
                self.databases.tab = 2;
                self.databases.draft = self
                    .databases
                    .connections
                    .items
                    .iter()
                    .find(|s| s.id.as_str() == id)
                    .and_then(|s| Draft::edit(s, self.databases.project.clone()))
            }
            Action::Select(id) => {
                self.databases.tab = 0;
                self.databases.stop_reads();
                self.databases.selected = Some(id);
                self.databases.view = None;
                self.databases.read_pin = None;
                self.databases.parent = None;
                self.databases.object_id = None;
                self.databases.relation_ids.clear();
                self.databases.objects.reset();
                self.databases.relations.reset();
                self.databases.selected_object = None;
                self.databases.error = None;
                self.load_database_objects(false);
            }
            Action::Save => self.save_database(),
            Action::CancelDraft => {
                self.databases.draft = None;
                self.databases.tab = 0;
            }
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
                    .items
                    .iter()
                    .find(|s| s.id.as_str() == id)
                {
                    let input = databases_ops::refresh(
                        s.id.as_str(),
                        s.revision.as_str(),
                        self.databases.project.as_deref(),
                        Some(test),
                    );
                    self.database_command(input);
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
                        self.database_command(databases_ops::remove_source(
                            self.databases.project.as_deref(),
                            &source,
                        ));
                    } else {
                        self.database_command(Some(databases_ops::remove_index(&source)));
                    }
                }
            }
            Action::Objects(append) => {
                self.databases.tab = 0;
                self.databases.object_id = None;
                self.load_database_objects(append);
            }
            Action::Endpoint(id) => {
                self.databases.tab = 0;
                self.databases.object_id = Some(id);
                self.databases.parent = None;
                self.databases.kind.clear();
                self.databases.filter.clear();
                self.load_database_objects(false);
            }
            Action::Inspect(object) => {
                self.databases.selected_object = Some(object);
                self.databases.focus_object = true;
            }
            Action::Children(id) => {
                self.databases.tab = 0;
                self.databases.object_id = None;
                self.databases.parent = Some(id);
                self.load_database_objects(false);
            }
            Action::Relations(ids, append) => {
                self.databases.tab = 1;
                self.load_database_relations(ids, append);
            }
            Action::CodeReferences(id, append) => self.load_database_code_references(id, append),
            Action::OpenCode(r) => self.effects.push(Effect::OpenGraphNode {
                repository: r.repository_id.as_str().into(),
                node: r.node_id.as_str().into(),
            }),
            Action::ConfirmOutcome => self.confirm_outcomes(),
        }
    }
    fn apply_databases(
        &mut self,
        slot: DatabasesSlot,
        reply: Reply,
    ) -> Option<(DatabasesSlot, Reply)> {
        match (slot, reply) {
            (DatabasesSlot::DatabaseList(g, _), Reply::Query(result)) => {
                let scanning = self.databases.batch_scanning;
                match ops::query_result(result) {
                    Ok((OperationResult::SchemasConnections { value }, _)) => {
                        let next = value
                            .next_cursor
                            .clone()
                            .filter(|_| value.has_more)
                            .map(|s| s.0);
                        if scanning {
                            // A sync-all scan reads every page but shows none of them.
                            let applied = self
                                .databases
                                .scan
                                .apply(g, Ok(Page::new(Vec::new(), next)));
                            if applied == Applied::Stale {
                                return None;
                            }
                            self.scan_database_batch(value);
                            self.resume_database_read();
                            return None;
                        }
                        let rows = Page::new(value.items.0, next);
                        match self.databases.connections.apply(g, Ok(rows)) {
                            Applied::Stale => return None,
                            Applied::Continue(fetch) => {
                                self.send_database_list(fetch);
                                return None;
                            }
                            _ => {}
                        }
                        self.resume_database_read();
                    }
                    Ok((other, _)) => {
                        let error = ops::unexpected("schemas.connections", &other).describe();
                        if self.fail_database_list(scanning, g, error.clone()) {
                            self.databases.error = Some(error);
                        }
                    }
                    Err(e) => {
                        if !self.fail_database_list(scanning, g, e.describe()) {
                            return None;
                        }
                        self.databases.error = Some(e.describe());
                        if self.databases.pending_read.take().is_some() {
                            self.databases.stop_reads();
                        }
                        // Only the inventory scan owns batch cancellation. An unrelated
                        // refresh may fail while a captured batch command still awaits its
                        // receipt; retain that command and every remaining captured source.
                        if self.databases.batch_scanning {
                            self.databases.batch_scanning = false;
                            self.databases.batch_active = false;
                            self.databases.batch_remaining.clear();
                            self.databases.busy = false;
                        }
                    }
                }
            }
            (DatabasesSlot::DatabaseCodeReferences(g, _), Reply::Query(result)) => {
                match ops::query_result(result) {
                    Ok((OperationResult::SchemasCodeReferences { value }, _)) => {
                        let next = value.next_cursor.filter(|_| value.has_more).map(|c| c.0);
                        let rows = Page::new(value.items.0, next);
                        if self.databases.code_references.apply(g, Ok(rows)) == Applied::Stale {
                            return None;
                        }
                    }
                    Ok((o, _)) => {
                        let e = ops::unexpected("schemas.code_references", &o).describe();
                        if self.databases.code_references.apply(g, Err(e.clone())) != Applied::Stale
                        {
                            self.databases.error = Some(e);
                        }
                    }
                    Err(e) => {
                        if self.databases.code_references.apply(g, Err(e.describe()))
                            != Applied::Stale
                        {
                            self.databases.error = Some(e.describe());
                        }
                    }
                }
            }
            (DatabasesSlot::DatabaseObjects(g, _), Reply::Query(result)) => {
                match ops::query_result(result) {
                    Ok((OperationResult::SchemasObjects { value }, _)) => {
                        let first = value.items.0.first().cloned();
                        let next = value.next_cursor.filter(|_| value.has_more).map(|c| c.0);
                        let rows = Page::new(value.items.0, next);
                        if self.databases.objects.apply(g, Ok(rows)) == Applied::Stale {
                            return None;
                        }
                        if self.databases.object_id.is_some() {
                            self.databases.selected_object = first;
                        }
                        self.databases.error = None;
                    }
                    Ok((o, _)) => {
                        let e = ops::unexpected("schemas.objects", &o).describe();
                        if self.databases.objects.apply(g, Err(e.clone())) != Applied::Stale {
                            self.databases.error = Some(e);
                        }
                    }
                    Err(e) => {
                        if self.databases.objects.apply(g, Err(e.describe())) != Applied::Stale {
                            self.databases.error = Some(e.describe());
                        }
                    }
                }
            }
            (DatabasesSlot::DatabaseRelations(g, _), Reply::Query(result)) => {
                match ops::query_result(result) {
                    Ok((OperationResult::SchemasRelationships { value }, _)) => {
                        let next = value.next_cursor.filter(|_| value.has_more).map(|c| c.0);
                        let rows = Page::new(value.items.0, next);
                        if self.databases.relations.apply(g, Ok(rows)) == Applied::Stale {
                            return None;
                        }
                    }
                    Ok((o, _)) => {
                        let e = ops::unexpected("schemas.relationships", &o).describe();
                        if self.databases.relations.apply(g, Err(e.clone())) != Applied::Stale {
                            self.databases.error = Some(e);
                        }
                    }
                    Err(e) => {
                        if self.databases.relations.apply(g, Err(e.describe())) != Applied::Stale {
                            self.databases.error = Some(e.describe());
                        }
                    }
                }
            }
            (DatabasesSlot::DatabasePrepare, Reply::Task(result)) => match result {
                Ok(TaskOutput::Prepared { frame }) => {
                    // Keep the host-staged reference for corrected retries; plaintext was
                    // consumed during preparation and never returns to persisted form state.
                    if let Ok(prepared) = serde_json::from_slice::<Value>(&frame)
                        && let Some(handle) = prepared
                            .pointer("/body/config/credentialRef")
                            .and_then(Value::as_str)
                        && let Some(draft) = self.databases.draft.as_mut()
                    {
                        draft.config["credentialRef"] = json!(handle);
                    }
                    if !self.command(
                        DatabasesSlot::DatabaseCommand("schemas.configure".into()),
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
            (DatabasesSlot::DatabaseCaPrepare, Reply::Task(result)) => match result {
                Ok(TaskOutput::Prepared { frame }) => {
                    if !self.command(
                        DatabasesSlot::DatabaseCa,
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
            (DatabasesSlot::DatabaseCa, Reply::Command(outcome)) => {
                match ops::saved_artifact(outcome) {
                    Ok(artifact) => {
                        if let Some(draft) = self.databases.draft.as_mut() {
                            draft.config["caCertificate"] = json!(artifact.artifact);
                            self.save_database();
                        } else {
                            self.databases.busy = false;
                            self.databases.notice = Some(
                                "Certificate saved. Open the source editor to attach it.".into(),
                            );
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
                }
            }
            (DatabasesSlot::DatabaseCommand(_), Reply::Command(outcome)) => {
                self.databases.busy = false;
                match outcome {
                    Ok(CommandReply::Recorded { receipt, .. }) => match receipt.decision {
                        DomainDecision::Applied { result }
                        | DomainDecision::NoChange { result } => {
                            match result {
                                OperationResult::SchemasConfigure { value } => {
                                    self.databases.selected = (!matches!(
                                        value.state,
                                        devworks_core::protocol::records::ItemState::Disabled
                                    ))
                                    .then(|| value.id.as_str().into());
                                    self.databases.draft = None;
                                    self.databases.tab = 0;
                                    self.databases.clear_explorer();
                                }
                                OperationResult::SchemasRemoveIndex { .. } => {
                                    self.databases.clear_explorer()
                                }
                                _ => {}
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
                    Ok(CommandReply::ReceiptPurged { outcome, .. }) => {
                        self.databases.clear_explorer();
                        self.databases.error = Some(Failure::Purged(outcome).describe());
                    }
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
impl DatabasesCtl<'_, '_> {
    fn begin_database_sync_all(&mut self) {
        if self.databases.connections.loading() {
            // The scan and the sources list are read one at a time.
            self.databases.notice =
                Some("Wait for the database sources to finish loading, then sync all.".into());
            return;
        }
        self.databases.batch_remaining.clear();
        self.databases.batch_scanning = true;
        self.databases.batch_active = true;
        self.databases.batch_scope = self.databases.project.clone();
        self.databases.batch_submitted = 0;
        self.databases.batch_failures = 0;
        self.databases.busy = true;
        self.databases.scan.reset();
        self.load_databases(false);
    }
    /// Records that the inventory read `generation` failed; `false` when it was superseded.
    fn fail_database_list(&mut self, scanning: bool, generation: u64, error: String) -> bool {
        let list = if scanning {
            &mut self.databases.scan
        } else {
            &mut self.databases.connections
        };
        list.apply(generation, Err(error)) != Applied::Stale
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
        if self.databases.scan.more() {
            self.load_databases(true);
        } else {
            self.databases.batch_scanning = false;
            self.next_database_batch();
        }
    }
    fn next_database_batch(&mut self) {
        if let Some((id, revision)) = self.databases.batch_remaining.pop_front() {
            self.databases.batch_submitted += 1;
            let input = databases_ops::refresh(
                &id,
                &revision.to_string(),
                self.databases.batch_scope.as_deref(),
                None,
            );
            self.database_command(input);
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

impl DatabasesCtl<'_, '_> {
    fn resume_database_batch(&mut self) {
        if self.databases.batch_scanning {
            self.databases.batch_remaining.clear();
            self.databases.scan.reset();
            self.databases.busy = true;
            self.load_databases(false);
        } else if self.databases.batch_active && !self.databases.busy {
            self.next_database_batch();
        }
    }
}
