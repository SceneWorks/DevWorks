//! Knowledge requests and replies: the same `knowledge.*` operations an agent tool submits,
//! with the import file stored as the user's artifact first. A write keeps what it took from
//! the forms until its outcome is recorded; an unknown outcome is settled by Confirm outcomes.
mod documents;

use crate::attachments;
use crate::backend::{Reply, Unresolved};
use crate::documents_ops::DocumentSlot;
use crate::documents_screen::{self, StagedImport, Upload};
use crate::knowledge_ops::KnowledgeSlot;
use crate::knowledge_screen::graph::GraphAction;
use crate::knowledge_screen::{self, KnowledgeAction, KnowledgeScreen};
use crate::memory::{RouteMemory, UnconfirmedImport};
use crate::project_purge::Live;
use crate::projects_ops::ProjectsSlot;
use crate::route::Route;
use crate::screen::{Cx, Screen, ScreenContext, ScreenId, controller};
use crate::widgets;
use devworks_core::ports::storage::ProjectPurgeClientScope;
use devworks_core::protocol::Id;
use devworks_core::protocol::provider::ArtifactRef;
use eframe::egui::Ui;
use serde_json::json;
use std::path::PathBuf;

controller!(KnowledgeCtl, knowledge: KnowledgeScreen);

/// Why a graph read or summary was not sent.
const OFFLINE: &str = "Not sent: DevWorks is offline or closing.";

impl Screen for KnowledgeScreen {
    type Slot = KnowledgeSlot;
    const ID: ScreenId = ScreenId::Knowledge;
    fn load(&mut self, cx: &mut Cx, _route: Route) {
        let mut ctl = KnowledgeCtl {
            knowledge: self,
            cx,
        };
        // Scope choices are the projects the user can see.
        if !ctl.projects.list.loaded && !ctl.pending(&ProjectsSlot::ProjectsList) {
            ctl.load_projects();
        }
        if ctl
            .routes
            .get(&Route::Knowledge)
            .is_some_and(|r| r.tab == documents_screen::TAB)
        {
            if !ctl.knowledge.documents.list.loaded {
                ctl.load_documents(false);
            }
            return;
        }
        if ctl.routes.get(&Route::Knowledge).is_none_or(|r| r.tab != 2)
            && !ctl.knowledge.claims.loaded
            && !ctl.knowledge.claims.loading()
        {
            ctl.load_knowledge();
        }
    }
    fn refresh(&mut self, cx: &mut Cx, _route: Route) {
        if cx
            .routes
            .get(&Route::Knowledge)
            .is_some_and(|r| r.tab == documents_screen::TAB)
        {
            let mut ctl = KnowledgeCtl {
                knowledge: self,
                cx,
            };
            ctl.refresh_documents();
            return;
        }
        // A re-read covers every page the user loaded.
        if cx.routes.get(&Route::Knowledge).is_none_or(|r| r.tab != 2)
            && self.claims.loaded
            && !self.claims.loading()
        {
            let running = self.last_write.as_ref().is_some_and(|w| w.running);
            let mut ctl = KnowledgeCtl {
                knowledge: self,
                cx,
            };
            ctl.refresh_knowledge();
            // A write still running (a delete by source continuing as a job) is read again
            // until its receipt says it is done.
            if running {
                ctl.read_write_status();
            }
        }
    }
    fn apply(&mut self, cx: &mut Cx, slot: KnowledgeSlot, reply: Reply) {
        if let KnowledgeSlot::Documents(slot) = slot {
            KnowledgeCtl {
                knowledge: self,
                cx,
            }
            .apply_documents(slot, reply);
            return;
        }
        if let Some((slot, reply)) = (KnowledgeCtl {
            knowledge: self,
            cx,
        })
        .apply_knowledge(slot, reply)
        {
            cx.unhandled(&slot, reply);
        }
    }
    fn ui(&mut self, cx: &mut Cx, _route: Route, ui: &mut Ui) {
        let online = cx.online().map_err(str::to_owned);
        let mut actions = Vec::new();
        let mut document_actions = Vec::new();
        let mut ctl = KnowledgeCtl {
            knowledge: self,
            cx,
        };
        ctl.sync_knowledge_scope(&mut actions);
        let project = ctl.knowledge.project.clone();
        ctl.knowledge.documents.scope(project.as_deref());
        let previous_tab = ctl.routes.entry(Route::Knowledge).or_default().tab;
        ctl.knowledge.tab = previous_tab;
        KnowledgeScreen::ui(
            ctl.knowledge,
            ui,
            online.as_ref().map(|_| ()).map_err(String::as_str),
            &mut actions,
            &mut document_actions,
        );
        let tab = ctl.knowledge.tab;
        ctl.routes.entry(Route::Knowledge).or_default().tab = tab;
        if tab == documents_screen::TAB {
            if !ctl.knowledge.documents.list.loaded && ctl.knowledge.documents.list.error.is_none()
            {
                ctl.load_documents(false);
            }
            ctl.document_actions(document_actions);
        } else {
            if tab == 0
                && previous_tab == documents_screen::TAB
                && !actions
                    .iter()
                    .any(|action| matches!(action, KnowledgeAction::Search))
            {
                actions.push(KnowledgeAction::Search);
            }
            ctl.knowledge_actions(actions);
        }
    }
    /// Source uploads persist their identical staged frames through shutdown and recovery.
    fn pending_memory(&self, _unresolved: &[&Unresolved]) -> Option<serde_json::Value> {
        let entry = self
            .import_frame(false)
            .map(|(name, frame)| UnconfirmedImport { name, frame });
        let document = self.documents.staged_import();
        match document {
            Some(document) => Some(json!({"entry":entry,"document":document})),
            None => entry.and_then(|entry| serde_json::to_value(entry).ok()),
        }
    }
    fn restore_pending(&mut self, saved: &serde_json::Value) {
        if let Ok(upload) = serde_json::from_value::<StagedImport>(saved["document"].clone()) {
            self.documents.upload = Some(Upload::Unconfirmed(upload));
        }
        let entry = saved.get("entry").unwrap_or(saved);
        if let Ok(i) = serde_json::from_value::<UnconfirmedImport>(entry.clone()) {
            self.import = Some(knowledge_screen::ImportStage::Unconfirmed {
                name: i.name,
                frame: i.frame,
            });
        }
    }
    /// Knowledge changes stay blocked until the earlier write's outcome is known.
    fn restore_in_flight(&mut self, slot: &KnowledgeSlot, u: &Unresolved) {
        if matches!(slot, KnowledgeSlot::KnowledgeWrite) {
            self.writing = Some(u.label.clone());
        }
        if matches!(slot, KnowledgeSlot::Documents(DocumentSlot::Write)) {
            self.documents.writing = true;
        }
    }
    fn unconfirmed_count(&self) -> usize {
        usize::from(self.import_frame(true).is_some())
            + usize::from(matches!(
                self.documents.upload,
                Some(Upload::Unconfirmed(_))
            ))
    }
    fn scope(&self) -> Option<String> {
        Some(self.project.clone().unwrap_or_default())
    }
    fn purge_live(&self, out: &mut Live) {
        out.busy |= self.documents.busy()
            || self
                .import
                .as_ref()
                .is_some_and(knowledge_screen::ImportStage::busy);
        let Some(p) = self.purge_owner() else {
            return;
        };
        let mut put = |kind: &str, value: serde_json::Value| {
            out.projects.entry(p.into()).or_insert_with(|| json!({}))[kind] = value;
        };
        if let Some(d) = &self.draft {
            put(
                "knowledge",
                json!({"subject":d.subject,"value":d.value,"tags":d.tags,"activities":d.activities}),
            );
        }
        put(
            "knowledgeSearch",
            json!({"text":self.text,"reason":self.reason,"source":self.cleanup_source,
                "graphQuery":self.graph.query,"lessonActivity":self.lesson_activity}),
        );
    }
    fn remove_purged(&mut self, scope: &ProjectPurgeClientScope) {
        let p = scope.project_id.0.as_str();
        if self.documents.project.as_deref() == Some(p) {
            self.documents.scope(None);
        } else {
            // Workspace views may contain results from a source whose previous ownership
            // was just retired. Re-read visible document and citation data on demand.
            self.documents.list.reset();
            self.documents.select(None);
            self.documents.results = None;
            self.documents.search_generation += 1;
        }
        if self.purge_owner() == Some(p) {
            *self = KnowledgeScreen::new();
        }
        self.projects.retain(|(id, _)| id != p);
    }
    fn context(&self, _route: Route, mem: &RouteMemory) -> ScreenContext {
        ScreenContext {
            tab: mem.tab,
            selection: if mem.tab == documents_screen::TAB {
                self.documents
                    .selected
                    .as_ref()
                    .map(|source| source.artifact_id.as_str().to_owned())
            } else {
                self.selected.as_ref().map(|id| id.as_str().to_owned())
            },
        }
    }
}

/// What the shell asks of the Knowledge controller.
impl KnowledgeScreen {
    /// Selects an entry and reads it.
    pub(super) fn select(&mut self, cx: &mut Cx, id: Id) {
        KnowledgeCtl {
            knowledge: self,
            cx,
        }
        .knowledge_actions(vec![KnowledgeAction::Select(id)]);
    }
    /// A file dropped on the entry view is previewed before import.
    pub(super) fn import_files(&mut self, cx: &mut Cx, paths: Vec<PathBuf>) {
        KnowledgeCtl {
            knowledge: self,
            cx,
        }
        .knowledge_actions(vec![KnowledgeAction::ImportFiles(paths)]);
    }
    pub(super) fn import_documents(&mut self, cx: &mut Cx, paths: Vec<PathBuf>) {
        let mut ctl = KnowledgeCtl {
            knowledge: self,
            cx,
        };
        let mut actions = Vec::new();
        ctl.sync_knowledge_scope(&mut actions);
        let project = ctl.knowledge.project.clone();
        ctl.knowledge.documents.scope(project.as_deref());
        ctl.document_actions(vec![documents_screen::Action::Drop(paths)]);
    }

    /// Confirm outcomes: the import file's save is submitted again with its identical frame
    /// (not while closing: it stays unconfirmed, its frame kept).
    pub(super) fn confirm_import(&mut self, cx: &mut Cx) {
        KnowledgeCtl {
            knowledge: self,
            cx,
        }
        .confirm_document_source();
        let Some((name, frame)) = self.import_frame(true) else {
            return;
        };
        if (KnowledgeCtl {
            knowledge: self,
            cx,
        })
        .submit_import_save(frame.clone())
        {
            self.import = Some(knowledge_screen::ImportStage::Storing { name, frame });
        }
    }
}

impl KnowledgeCtl<'_, '_> {
    /// Starts a new Knowledge listing (first page) and refreshes the retrieval mode. A listing
    /// that could not be submitted is not left in progress.
    fn load_knowledge(&mut self) {
        let (op, body) = self.knowledge.begin_listing(false);
        self.send_listing(op, body, "Loading Knowledge");
        self.check_knowledge();
    }
    /// Re-reads the same listing (the periodic refresh): every page shown is read again.
    fn refresh_knowledge(&mut self) {
        let (op, body) = self.knowledge.begin_refresh();
        self.send_listing(op, body, "Loading Knowledge");
        self.check_knowledge();
    }
    /// Submits the listing request just started; one nothing took is not left in progress.
    fn send_listing(
        &mut self,
        op: &'static str,
        body: Option<devworks_core::operations::OperationInput>,
        label: &str,
    ) {
        let generation = self.knowledge.claims.generation();
        if !self.query(
            KnowledgeSlot::KnowledgeList(generation),
            label,
            op,
            move |b| knowledge_screen::query(b, body),
        ) {
            self.knowledge.claims.abandon(generation);
        }
    }
    fn check_knowledge(&mut self) {
        let body = self.knowledge.diagnostics_input();
        self.query(
            KnowledgeSlot::KnowledgeDiagnostics,
            "Checking Knowledge",
            "knowledge.diagnostics",
            move |b| knowledge_screen::query(b, body),
        );
    }
    /// Gives the Knowledge screen the projects the user can see (active and archived, from
    /// `projects.list`) and follows the navigation's project selection; a changed scope lists
    /// again.
    fn sync_knowledge_scope(&mut self, actions: &mut Vec<KnowledgeAction>) {
        use devworks_core::protocol::records::LifeState;
        if !self.projects.list.loaded {
            return;
        }
        let projects: Vec<(String, String)> = self
            .projects
            .list
            .items
            .iter()
            .filter(|p| matches!(p.lifecycle, LifeState::Active | LifeState::Archived))
            .map(|p| {
                let name = match p.lifecycle {
                    LifeState::Archived => format!("{} (archived)", p.name.0),
                    _ => p.name.0.clone(),
                };
                (p.id.0.as_str().to_owned(), name)
            })
            .collect();
        let mut changed = self.knowledge.set_projects(projects);
        let nav = self.selected_project().map(str::to_owned);
        if self.knowledge.followed.as_ref() != Some(&nav) {
            self.knowledge.followed = Some(nav.clone());
            changed |= self.knowledge.follow_navigation(nav.as_deref());
        }
        if changed && self.knowledge.claims.loaded {
            actions.push(KnowledgeAction::Search);
        }
    }
    /// Stores the first of `paths` as the user's artifact (like a chat attachment), then
    /// previews its import. One import at a time; nothing is imported before Confirm.
    fn import_knowledge_file(&mut self, paths: Vec<PathBuf>) {
        let Some(path) = paths.into_iter().next() else {
            return;
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "import.json".into());
        if !self.knowledge.begin_import(&name) {
            self.knowledge.message = Some((
                widgets::Tone::Attention,
                "An import is already in progress.".into(),
            ));
            return;
        }
        let media = if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("json"))
        {
            "application/json"
        } else {
            attachments::media_type(&path)
        };
        let online = self.online().is_ok();
        let submitted = online
            && self.client.as_deref_mut().is_some_and(|c| {
                c.task(
                    KnowledgeSlot::KnowledgeImportSave.into(),
                    "Storing the import file",
                    move |b| attachments::prepare_file(b, &path, &name, media, "knowledge-import"),
                )
            });
        if !submitted {
            self.knowledge.import = Some(knowledge_screen::ImportStage::Failed {
                name: "the file".into(),
                error: "DevWorks is offline or closing.".into(),
            });
        }
    }
    fn read_knowledge(&mut self, claim: Id) {
        let body = self.knowledge.read_input(&claim);
        self.query(
            KnowledgeSlot::KnowledgeRead(claim.as_str().to_owned()),
            "Loading a Knowledge entry",
            "knowledge.read",
            move |b| knowledge_screen::query(b, body),
        );
    }
    /// Submits one Knowledge write at a time; what it took from the screen's forms
    /// (`submitted`) stays until its outcome is recorded.
    fn knowledge_write(
        &mut self,
        label: &str,
        op: &'static str,
        body: Option<devworks_core::operations::OperationInput>,
        submitted: knowledge_screen::Submitted,
    ) {
        if self.knowledge.writing.is_some() {
            return;
        }
        self.knowledge.writing = Some(label.to_owned());
        self.knowledge.submitted = Some(submitted);
        if !self.command(KnowledgeSlot::KnowledgeWrite, label, op, move |b| {
            knowledge_screen::command(b, body)
        }) {
            // Refused because an earlier write is unresolved: that one stays the write in
            // flight until Confirm outcomes settles it.
            let write: crate::backend::Slot = KnowledgeSlot::KnowledgeWrite.into();
            let held = self.client.as_deref().and_then(|c| {
                c.unresolved()
                    .iter()
                    .find(|u| u.slot == write)
                    .map(|u| u.label.clone())
            });
            self.knowledge.message = Some((
                widgets::Tone::Attention,
                match &held {
                    Some(earlier) => format!(
                        "{earlier}: the outcome is not confirmed yet. Use Confirm outcomes \
                         first; Knowledge changes wait until it is known."
                    ),
                    None => format!("{label} was not sent: DevWorks is offline or closing."),
                },
            ));
            self.knowledge.writing = held;
            self.knowledge.submitted = None;
        }
    }
    fn knowledge_actions(&mut self, actions: Vec<KnowledgeAction>) {
        for a in actions {
            match a {
                KnowledgeAction::Search => self.load_knowledge(),
                KnowledgeAction::More => {
                    if self.knowledge.claims.loading() || !self.knowledge.claims.more() {
                        continue;
                    }
                    let (op, body) = self.knowledge.begin_listing(true);
                    self.send_listing(op, body, "Loading more Knowledge");
                }
                KnowledgeAction::Select(id) => {
                    self.knowledge.selected = Some(id.clone());
                    self.knowledge.detail = None;
                    self.knowledge.detail_error = None;
                    self.read_knowledge(id);
                }
                KnowledgeAction::Save(draft) => match self.knowledge.save_input(&draft) {
                    Ok((op, body)) => self.knowledge_write(
                        "Entry",
                        op,
                        body,
                        knowledge_screen::Submitted::Draft(draft),
                    ),
                    Err(m) => self.knowledge.message = Some((widgets::Tone::Attention, m)),
                },
                KnowledgeAction::PreviewSource => match self.knowledge.preview_input() {
                    Ok((source, body)) => {
                        self.knowledge.source_preview = None;
                        self.query(
                            KnowledgeSlot::KnowledgePreview(source),
                            "Previewing a cleanup",
                            "knowledge.diagnostics",
                            move |b| knowledge_screen::query(b, body),
                        );
                    }
                    Err(m) => self.knowledge.message = Some((widgets::Tone::Attention, m)),
                },
                KnowledgeAction::ImportDialog => {
                    let picked = self.dialogs.pick_files();
                    self.import_knowledge_file(picked);
                }
                KnowledgeAction::ImportFiles(paths) => self.import_knowledge_file(paths),
                KnowledgeAction::ConfirmImport => match self.knowledge.import_input() {
                    Ok(body) => {
                        if self.command(
                            KnowledgeSlot::KnowledgeImport,
                            "Importing Knowledge",
                            "knowledge.import_user_data",
                            move |b| knowledge_screen::command(b, body),
                        ) {
                            self.knowledge.import_started();
                        } else {
                            self.knowledge.message = Some((
                                widgets::Tone::Attention,
                                "The import was not sent: DevWorks is offline or closing.".into(),
                            ));
                        }
                    }
                    Err(m) => self.knowledge.message = Some((widgets::Tone::Attention, m)),
                },
                KnowledgeAction::CancelImport => {
                    if !self.knowledge.import.as_ref().is_some_and(|s| s.busy()) {
                        self.knowledge.import = None;
                    }
                }
                KnowledgeAction::DeleteSource => match self.knowledge.delete_source_input() {
                    Ok((op, body)) => self.knowledge_write(
                        "Delete by source",
                        op,
                        body,
                        knowledge_screen::Submitted::Nothing,
                    ),
                    Err(m) => self.knowledge.message = Some((widgets::Tone::Attention, m)),
                },
                KnowledgeAction::Change(change) => match self.knowledge.change_input(change) {
                    Ok((op, body)) => {
                        let submitted = self.knowledge.change_submission(change);
                        self.knowledge_write(change.label(), op, body, submitted)
                    }
                    Err(m) => self.knowledge.message = Some((widgets::Tone::Attention, m)),
                },
                KnowledgeAction::Relate => match self.knowledge.relate_input() {
                    Ok(body) => {
                        let submitted = knowledge_screen::Submitted::Relation {
                            object: self.knowledge.relate_object.clone(),
                            predicate: self.knowledge.relate_predicate_wire(),
                        };
                        self.knowledge_write("Relate", "knowledge.relate", body, submitted)
                    }
                    Err(m) => self.knowledge.message = Some((widgets::Tone::Attention, m)),
                },
                KnowledgeAction::Graph(action) => self.graph_action(action),
            }
        }
    }

    /// A graph view request, in the listed scope.
    fn graph_action(&mut self, action: GraphAction) {
        let scope = self.knowledge.graph_scope();
        let request = match action {
            GraphAction::Open(read) => Ok(self.knowledge.graph.begin(scope, read)),
            GraphAction::Expand => self.knowledge.graph.begin_expand(scope),
            GraphAction::Summary => {
                match self.knowledge.graph.begin_summary(scope) {
                    Ok((generation, body)) => {
                        if !self.query(
                            KnowledgeSlot::KnowledgeSummary(generation),
                            "Rendering a Knowledge summary",
                            "knowledge.render_summary",
                            move |b| knowledge_screen::query(b, body),
                        ) {
                            self.knowledge.graph.abandon_summary(generation, OFFLINE);
                        }
                    }
                    Err(m) => self.knowledge.graph.error = Some(m),
                }
                return;
            }
            GraphAction::Select(id) => {
                self.knowledge_actions(vec![KnowledgeAction::Select(id)]);
                return;
            }
            GraphAction::Close => {
                self.knowledge.graph.close();
                return;
            }
        };
        match request {
            Ok((generation, op, body)) => self.send_graph(generation, op, body),
            Err(m) => self.knowledge.graph.error = Some(m),
        }
    }
    /// Submits a graph read just started; one nothing took is not left in flight.
    fn send_graph(
        &mut self,
        generation: u64,
        op: &'static str,
        body: Option<devworks_core::operations::OperationInput>,
    ) {
        if !self.query(
            KnowledgeSlot::KnowledgeGraph(generation),
            "Reading the Knowledge graph",
            op,
            move |b| knowledge_screen::query(b, body),
        ) {
            self.knowledge.graph.abandon(generation, OFFLINE);
        }
    }
    /// Reads one page of a published summary's text.
    fn read_summary(&mut self, generation: u64, artifact: ArtifactRef, cursor: Option<String>) {
        if !self.query(
            KnowledgeSlot::KnowledgeSummaryText(generation),
            "Reading a Knowledge summary",
            "artifacts.read",
            move |b| crate::ops::artifact_read_after(b, &artifact, cursor.as_deref()),
        ) {
            self.knowledge.graph.abandon_summary(generation, OFFLINE);
        }
    }
    /// Reads back the last recorded write's receipt.
    fn read_write_status(&mut self) {
        let Some((write_id, body)) = self.knowledge.write_status_input() else {
            return;
        };
        let slot = KnowledgeSlot::KnowledgeWriteStatus(write_id);
        if self.pending(&slot) {
            return;
        }
        self.query(
            slot,
            "Checking a Knowledge write",
            "knowledge.write_status",
            move |b| knowledge_screen::query(b, body),
        );
    }

    /// Admits a Knowledge import file's exact save frame (prepared, or unconfirmed).
    fn submit_import_save(&mut self, frame: Vec<u8>) -> bool {
        self.client.as_deref_mut().is_some_and(|c| {
            c.task(
                KnowledgeSlot::KnowledgeImportSave.into(),
                "Storing the import file",
                move |b| attachments::save(b, frame, None),
            )
        })
    }

    fn apply_knowledge(
        &mut self,
        slot: KnowledgeSlot,
        reply: Reply,
    ) -> Option<(KnowledgeSlot, Reply)> {
        match (slot, reply) {
            (KnowledgeSlot::KnowledgeList(generation), Reply::Query(q)) => {
                match self.knowledge.listed(generation, q) {
                    knowledge_screen::Listed::Restart => self.load_knowledge(),
                    knowledge_screen::Listed::Continue(fetch) => {
                        let (op, body) = self.knowledge.continue_listing(&fetch);
                        self.send_listing(op, body, "Loading Knowledge");
                    }
                    knowledge_screen::Listed::Done => {}
                }
            }
            (KnowledgeSlot::KnowledgeRead(claim), Reply::Query(q)) => {
                self.knowledge.read_reply(&claim, q)
            }
            (KnowledgeSlot::KnowledgeDiagnostics, Reply::Query(q)) => {
                self.knowledge.diagnostics_reply(q)
            }
            (KnowledgeSlot::KnowledgeImportSave, Reply::Task(r)) => {
                match self.knowledge.import_saved(r) {
                    knowledge_screen::ImportNext::Store(frame) => {
                        if !self.submit_import_save(frame) {
                            // Not admitted: nothing was stored.
                            self.knowledge.import = Some(knowledge_screen::ImportStage::Failed {
                                name: "the file".into(),
                                error: "DevWorks is offline or closing.".into(),
                            });
                        }
                    }
                    knowledge_screen::ImportNext::Preview(body) => {
                        let submitted = self.query(
                            KnowledgeSlot::KnowledgeImportPreview,
                            "Previewing an import",
                            "knowledge.diagnostics",
                            move |b| knowledge_screen::query(b, body),
                        );
                        if !submitted {
                            self.knowledge.import = None;
                        }
                    }
                    knowledge_screen::ImportNext::Nothing => {}
                }
            }
            (KnowledgeSlot::KnowledgeImportPreview, Reply::Query(q)) => {
                self.knowledge.import_previewed(q)
            }
            (KnowledgeSlot::KnowledgeImport, Reply::Command(c)) => {
                if self.knowledge.import_done(c) {
                    self.load_knowledge();
                }
            }
            (KnowledgeSlot::KnowledgePreview(source), Reply::Query(q)) => {
                self.knowledge.preview_reply(&source, q)
            }
            (KnowledgeSlot::KnowledgeWrite, Reply::Command(c)) => {
                if self.knowledge.written(c) {
                    self.load_knowledge();
                    if let Some(id) = self.knowledge.selected.clone() {
                        self.read_knowledge(id);
                    }
                    // The graph shown is read again; it stays on screen until the reply.
                    let scope = self.knowledge.graph_scope();
                    if let Some((generation, op, body)) = self.knowledge.graph.begin_refresh(scope)
                    {
                        self.send_graph(generation, op, body);
                    }
                    self.read_write_status();
                }
            }
            (KnowledgeSlot::KnowledgeGraph(generation), Reply::Query(q)) => {
                self.knowledge.graph.reply(generation, q)
            }
            (KnowledgeSlot::KnowledgeSummary(generation), Reply::Query(q)) => {
                if let Some(artifact) = self.knowledge.graph.summary_published(generation, q) {
                    self.read_summary(generation, artifact, None);
                }
            }
            (KnowledgeSlot::KnowledgeSummaryText(generation), Reply::Query(q)) => {
                if let Some((artifact, cursor)) = self.knowledge.graph.summary_text(generation, q) {
                    self.read_summary(generation, artifact, Some(cursor));
                }
            }
            (KnowledgeSlot::KnowledgeWriteStatus(write_id), Reply::Query(q)) => {
                self.knowledge.write_status_reply(&write_id, q)
            }
            other => return Some(other),
        }
        None
    }
}
