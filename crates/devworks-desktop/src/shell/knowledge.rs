//! Knowledge requests and replies: the same `knowledge.*` operations an agent tool submits,
//! with the import file stored as the user's artifact first. A write keeps what it took from
//! the forms until its outcome is recorded; an unknown outcome is settled by Confirm outcomes.
use crate::attachments;
use crate::backend::{Reply, Unresolved};
use crate::knowledge_ops::KnowledgeSlot;
use crate::knowledge_screen::{self, KnowledgeAction, KnowledgeScreen};
use crate::memory::{RouteMemory, UnconfirmedImport};
use crate::project_purge::Live;
use crate::projects_ops::ProjectsSlot;
use crate::route::Route;
use crate::screen::{Cx, Screen, ScreenContext, ScreenId, controller};
use crate::widgets;
use devworks_core::ports::storage::ProjectPurgeClientScope;
use devworks_core::protocol::Id;
use eframe::egui::Ui;
use serde_json::json;
use std::path::PathBuf;

controller!(KnowledgeCtl, knowledge: KnowledgeScreen);

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
        if ctl.routes.get(&Route::Knowledge).is_none_or(|r| r.tab != 2)
            && !ctl.knowledge.loaded
            && ctl.knowledge.listing.is_none()
        {
            ctl.load_knowledge();
        }
    }
    fn refresh(&mut self, cx: &mut Cx, _route: Route) {
        // Only an unpaged listing is re-read in place: a re-read of a list the user paged
        // through would drop the pages they loaded.
        if cx.routes.get(&Route::Knowledge).is_none_or(|r| r.tab != 2)
            && self.loaded
            && self.listing.is_none()
            && self.items.len() <= knowledge_screen::PAGE as usize
        {
            KnowledgeCtl {
                knowledge: self,
                cx,
            }
            .load_knowledge();
        }
    }
    fn apply(&mut self, cx: &mut Cx, slot: KnowledgeSlot, reply: Reply) {
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
        KnowledgeCtl {
            knowledge: self,
            cx,
        }
        .sync_knowledge_scope(&mut actions);
        // The selected subview is route memory, kept across navigation and restarts.
        self.tab = cx.routes.entry(Route::Knowledge).or_default().tab;
        KnowledgeScreen::ui(
            self,
            ui,
            online.as_ref().map(|_| ()).map_err(String::as_str),
            &mut actions,
        );
        cx.routes.entry(Route::Knowledge).or_default().tab = self.tab;
        KnowledgeCtl {
            knowledge: self,
            cx,
        }
        .knowledge_actions(actions);
    }
    /// An import file whose save outcome is not confirmed, by its exact frame.
    fn pending_memory(&self, _unresolved: &[&Unresolved]) -> Option<serde_json::Value> {
        self.import_frame(false)
            .and_then(|(name, frame)| serde_json::to_value(UnconfirmedImport { name, frame }).ok())
    }
    fn restore_pending(&mut self, saved: &serde_json::Value) {
        if let Ok(i) = serde_json::from_value::<UnconfirmedImport>(saved.clone()) {
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
    }
    fn unconfirmed_count(&self) -> usize {
        usize::from(self.import_frame(true).is_some())
    }
    fn scope(&self) -> Option<String> {
        Some(self.project.clone().unwrap_or_default())
    }
    fn purge_live(&self, out: &mut Live) {
        out.busy |= self
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
            json!({"text":self.text,"reason":self.reason,"source":self.cleanup_source}),
        );
    }
    fn remove_purged(&mut self, scope: &ProjectPurgeClientScope) {
        let p = scope.project_id.0.as_str();
        if self.purge_owner() == Some(p) {
            *self = KnowledgeScreen::new();
        }
        self.projects.retain(|(id, _)| id != p);
    }
    fn context(&self, _route: Route, mem: &RouteMemory) -> ScreenContext {
        ScreenContext {
            tab: mem.tab,
            selection: self.selected.as_ref().map(|id| id.as_str().to_owned()),
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
    /// Confirm outcomes: the import file's save is submitted again with its identical frame
    /// (not while closing: it stays unconfirmed, its frame kept).
    pub(super) fn confirm_import(&mut self, cx: &mut Cx) {
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
        let generation = self.knowledge.generation;
        if !self.query(
            KnowledgeSlot::KnowledgeList(generation),
            "Loading Knowledge",
            op,
            move |b| knowledge_screen::query(b, op, body),
        ) {
            self.knowledge.listing = None;
        }
        let body = self.knowledge.diagnostics_body();
        self.query(
            KnowledgeSlot::KnowledgeDiagnostics,
            "Checking Knowledge",
            "knowledge.diagnostics",
            move |b| knowledge_screen::query(b, "knowledge.diagnostics", body),
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
        if changed && self.knowledge.loaded {
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
        let body = self.knowledge.read_body(&claim);
        self.query(
            KnowledgeSlot::KnowledgeRead(claim.as_str().to_owned()),
            "Loading a Knowledge entry",
            "knowledge.read",
            move |b| knowledge_screen::query(b, "knowledge.read", body),
        );
    }
    /// Submits one Knowledge write at a time; what it took from the screen's forms
    /// (`submitted`) stays until its outcome is recorded.
    fn knowledge_write(
        &mut self,
        label: &str,
        op: &'static str,
        body: serde_json::Value,
        submitted: knowledge_screen::Submitted,
    ) {
        if self.knowledge.writing.is_some() {
            return;
        }
        self.knowledge.writing = Some(label.to_owned());
        self.knowledge.submitted = Some(submitted);
        if !self.command(KnowledgeSlot::KnowledgeWrite, label, op, move |b| {
            knowledge_screen::command(b, op, body)
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
                    if self.knowledge.listing.is_some() || self.knowledge.cursor.is_none() {
                        continue;
                    }
                    let (op, body) = self.knowledge.begin_listing(true);
                    let generation = self.knowledge.generation;
                    if !self.query(
                        KnowledgeSlot::KnowledgeList(generation),
                        "Loading more Knowledge",
                        op,
                        move |b| knowledge_screen::query(b, op, body),
                    ) {
                        self.knowledge.listing = None;
                    }
                }
                KnowledgeAction::Select(id) => {
                    self.knowledge.selected = Some(id.clone());
                    self.knowledge.detail = None;
                    self.knowledge.detail_error = None;
                    self.read_knowledge(id);
                }
                KnowledgeAction::Save(draft) => match self.knowledge.save_body(&draft) {
                    Ok((op, body)) => self.knowledge_write(
                        "Entry",
                        op,
                        body,
                        knowledge_screen::Submitted::Draft(draft),
                    ),
                    Err(m) => self.knowledge.message = Some((widgets::Tone::Attention, m)),
                },
                KnowledgeAction::PreviewSource => match self.knowledge.preview_body() {
                    Ok((source, body)) => {
                        self.knowledge.source_preview = None;
                        self.query(
                            KnowledgeSlot::KnowledgePreview(source),
                            "Previewing a cleanup",
                            "knowledge.diagnostics",
                            move |b| knowledge_screen::query(b, "knowledge.diagnostics", body),
                        );
                    }
                    Err(m) => self.knowledge.message = Some((widgets::Tone::Attention, m)),
                },
                KnowledgeAction::ImportDialog => {
                    let picked = self.dialogs.pick_files();
                    self.import_knowledge_file(picked);
                }
                KnowledgeAction::ImportFiles(paths) => self.import_knowledge_file(paths),
                KnowledgeAction::ConfirmImport => match self.knowledge.import_body() {
                    Ok(body) => {
                        if self.command(
                            KnowledgeSlot::KnowledgeImport,
                            "Importing Knowledge",
                            "knowledge.import_user_data",
                            move |b| {
                                knowledge_screen::command(b, "knowledge.import_user_data", body)
                            },
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
                KnowledgeAction::DeleteSource => match self.knowledge.delete_source_body() {
                    Ok((op, body)) => self.knowledge_write(
                        "Delete by source",
                        op,
                        body,
                        knowledge_screen::Submitted::Nothing,
                    ),
                    Err(m) => self.knowledge.message = Some((widgets::Tone::Attention, m)),
                },
                KnowledgeAction::Change(change) => match self.knowledge.change_body(change) {
                    Ok((op, body)) => {
                        let submitted = self.knowledge.change_submission(change);
                        self.knowledge_write(change.label(), op, body, submitted)
                    }
                    Err(m) => self.knowledge.message = Some((widgets::Tone::Attention, m)),
                },
            }
        }
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
                if self.knowledge.listed(generation, q) {
                    self.load_knowledge();
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
                            move |b| knowledge_screen::query(b, "knowledge.diagnostics", body),
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
                }
            }
            other => return Some(other),
        }
        None
    }
}
