//! Native review and client coordination for the exact purge manifest.
use super::*;
use crate::{conventions_screen::Scope, project_purge::Live};
use devworks_core::{
    ports::storage::ProjectPurgeClientScope,
    protocol::records::{ProjectPurgeEntry, ProjectPurgeManifest, ProjectRecord, PurgePhase},
};
use serde_json::json;

#[derive(Default)]
pub struct PurgeUi {
    pub project: Option<ProjectRecord>,
    pub manifest: Option<ProjectPurgeManifest>,
    pub entries: Vec<ProjectPurgeEntry>,
    pub next: Option<String>,
    pub loaded: bool,
    pub busy: bool,
    pub frozen: bool,
    pub acknowledged: bool,
    pub cleanup_acknowledged: bool,
    pub error: Option<String>,
    pub recovering: bool,
    pub last_poll: Option<std::time::Instant>,
}
impl Shell {
    fn purge_live(&self) -> Live {
        let mut live = Live {
            busy: self.client.as_ref().is_some_and(Client::has_local_tasks)
                || self
                    .knowledge
                    .import
                    .as_ref()
                    .is_some_and(crate::knowledge_screen::ImportStage::busy),
            ..Live::default()
        };
        let mut put = |project: &str, kind: &str, value: serde_json::Value| {
            live.projects
                .entry(project.into())
                .or_insert_with(|| json!({}))[kind] = value;
        };
        if let Some(p) = &self.work.of
            && let Some(f) = &self.work.create
        {
            put(
                p,
                "createWork",
                json!({"kind":f.kind,"title":f.title,"description":f.description,"labels":f.labels}),
            );
        }
        if let Some(f) = &self.work.edit {
            put(
                f.snapshot.project_id.0.as_str(),
                "editWork",
                json!({"snapshot":f.snapshot,"title":f.title,"description":f.description,"labels":f.labels}),
            );
        }
        if let Some(f) = &self.work.plan {
            put(f.snapshot.project_id.0.as_str(), "plan", json!(f));
        }
        for (p, f) in &self.work.allocation_filters {
            put(p, "allocation", json!(format!("{f:?}")));
        }
        if let Some(f) = &self.projects.membership {
            put(&f.project, "membership", json!(f.repositories));
        }
        for (p, w) in &self.work.workflows {
            if let Some(d) = &w.draft {
                put(p, "workflow", json!(format!("{d:?}")));
            }
        }
        if let Some(f) = &self.projects.edit {
            put(
                &f.project,
                "project",
                json!({"name":f.name,"description":f.description,"access":f.access}),
            );
        }
        for (p, f) in &self.projects.base {
            put(p, "directory", json!(f.buffer));
        }
        for (scope, state) in &self.conventions.states {
            let p = match scope {
                Scope::Project(p) => p.as_str(),
                Scope::Workspace => "",
            };
            {
                let draft = |d: &crate::conventions_screen::Draft| json!({"title":d.title,"slug":d.slug,"base":d.base,"anchor":d.anchor,"markdown":*d.markdown,"raw":*d.raw,"parent":d.parent,"path":d.parent_path,"applicability":d.applicability,"editorState":format!("{d:?}")});
                put(
                    p,
                    "conventions",
                    json!({"new":state.new_draft.as_ref().map(draft),"drafts":state.drafts.iter().map(|(id,d)|(id,draft(d))).collect::<BTreeMap<_,_>>(),"query":state.text,"slug":state.slug_input,"path":state.path_input,"upload":state.upload.as_ref().map(|d|json!({"base":d.base,"path":d.path.to_string_lossy(),"name":d.name,"mediaType":d.media_type})),"resources":state.resource_drafts.iter().map(|(k,d)|(k,json!({"record":d.resource,"base":d.base,"name":d.name,"mediaType":d.media_type,"retired":d.retired,"replacement":d.replacement.as_ref().map(|p|p.to_string_lossy())}))).collect::<BTreeMap<_,_>>(),"moves":state.move_drafts.iter().map(|(k,d)|(k,json!({"base":d.base,"anchor":d.anchor,"scope":d.scope,"parent":d.parent,"order":d.order}))).collect::<BTreeMap<_,_>>()}),
                );
            }
        }
        for (scope, state) in &self.conventions.states {
            let owner = match scope {
                Scope::Project(p) => p.as_str(),
                Scope::Workspace => "",
            };
            let paths = live.paths.entry(owner.into()).or_default();
            if let Some(upload) = &state.upload {
                paths.push(upload.path.clone());
            }
            paths.extend(
                state
                    .resource_drafts
                    .values()
                    .filter_map(|d| d.replacement.clone()),
            );
        }
        if let Some(p) = self.knowledge.purge_owner() {
            if let Some(d) = &self.knowledge.draft {
                put(
                    p,
                    "knowledge",
                    json!({"subject":d.subject,"value":d.value,"tags":d.tags,"activities":d.activities}),
                );
            }
            put(
                p,
                "knowledgeSearch",
                json!({"text":self.knowledge.text,"reason":self.knowledge.reason,"source":self.knowledge.cleanup_source}),
            );
        }
        if let Some(devworks_core::protocol::data::DataScope::Project { project_id }) = self
            .decisions
            .selected
            .as_ref()
            .and_then(|r| r.scope.as_ref())
        {
            put(
                project_id.0.as_str(),
                "decision",
                json!(self.decisions.rationale),
            );
        }
        if let Some((owner, draft)) = self.jobs.purge_draft() {
            put(&owner, "job", draft);
        }
        if let Some(f) = &self.projects.indexing.sources
            && let Some(p) = &f.project
        {
            put(
                p,
                "indexSources",
                json!({"languages":f.languages,"packages":f.packages,"excludes":f.excludes,"max":f.max_bytes}),
            );
        }
        for screen in self.screens() {
            screen.purge_live(&mut live);
        }
        fn refs(v: &serde_json::Value, out: &mut Vec<ArtifactRef>) {
            use devworks_core::protocol::support::Check;
            if let Ok(r) = serde_json::from_value::<ArtifactRef>(v.clone())
                && r.check().is_ok()
            {
                out.push(r);
                return;
            }
            match v {
                serde_json::Value::Object(m) => m.values().for_each(|v| refs(v, out)),
                serde_json::Value::Array(a) => a.iter().for_each(|v| refs(v, out)),
                _ => {}
            }
        }
        for (p, v) in &live.projects {
            let mut r = vec![];
            refs(v, &mut r);
            if !r.is_empty() {
                live.references.insert(p.clone(), r);
            }
        }
        live
    }
    pub(super) fn publish_purge_client(&mut self, save: bool) -> Result<bool, String> {
        let Some(b) = self.backend() else {
            return Ok(false);
        };
        let Some(cleaned) = b.purge_client_publish(self.memory(), self.purge_live(), save)? else {
            return Ok(false);
        };
        self.editors_changed = false;
        for scope in cleaned {
            self.remove_purged_client(&scope);
            b.purge_client_acknowledge(&scope);
        }
        Ok(true)
    }
    fn remove_purged_client(&mut self, scope: &ProjectPurgeClientScope) {
        let p = scope.project_id.0.as_str();
        if self
            .purge
            .project
            .as_ref()
            .is_some_and(|r| r.id == scope.project_id)
        {
            self.purge.cleanup_acknowledged = true;
        }

        let work = |id: &str| scope.work_ids.iter().any(|x| x.as_str() == id);
        let session = |id: &str| scope.session_ids.iter().any(|x| x.as_str() == id);
        if let Some(c) = &mut self.client {
            c.invalidate_project_queries(scope);
        }
        self.chat.text_preview = None;
        self.chat.routing.status.retain(|id, _| !session(id));
        self.chat.routing.rechecked.retain(|id, _| !session(id));
        self.chat.routing.first_in_flight.retain(|id| !session(id));
        self.chat.routing.pin_in_flight.retain(|id| !session(id));
        self.chat.routing.status_asked.retain(|id, _| !session(id));
        self.chat.sessions.retain(|r| !session(r.id.as_str()));
        self.chat.drafts.retain(|id, _| !session(id));
        self.chat.transcripts.retain(|id, _| !session(id));
        if self.chat.selected.as_deref().is_some_and(session) {
            self.chat.selected = None;
            self.chat.text_preview = None;
        }
        self.chat.routing.selections.retain(|id, _| !session(id));
        self.chat.routing.first_route.retain(|id| !session(id));
        if self.selected_project.as_deref() == Some(p) {
            self.selected_project = None;
        }
        self.work.remove_purged(scope);
        self.conventions.states.remove(&Scope::Project(p.into()));
        if self.conventions.scope == Scope::Project(p.into()) {
            self.conventions.scope = Scope::Workspace;
        }
        self.conventions.projects.retain(|(id, _)| id != p);
        if self.knowledge.purge_owner() == Some(p) {
            self.knowledge = KnowledgeScreen::new();
        }
        self.decisions.purge_owned(scope);
        self.projects
            .list
            .items
            .retain(|r| r.id != scope.project_id);
        self.projects
            .deleted
            .items
            .retain(|r| r.id != scope.project_id);
        self.projects.edit = self.projects.edit.take().filter(|f| f.project != p);
        self.projects.membership = self.projects.membership.take().filter(|f| f.project != p);
        self.projects.base.remove(p);
        if self
            .projects
            .indexing
            .sources
            .as_ref()
            .is_some_and(|f| f.project.as_deref() == Some(p))
        {
            self.projects.indexing.sources = None;
            self.projects.indexing.message = None;
        }
        self.projects.indexing.runs = Default::default();
        self.usage.data = None;
        self.usage.options.clear();
        self.usage.applied = None;
        self.usage.loading = false;
        if self.usage.draft.project.as_deref() == Some(p) {
            self.usage.draft.project = None;
        }

        self.knowledge.projects.retain(|(id, _)| id != p);
        if let Some(project) = &mut self.purge.project
            && project.id == scope.project_id
        {
            project.name.0 = "Deleted project".into();
            project.description.0.clear();
            project.repository_ids.0.clear();
        }

        if self.projects.members_of.as_deref() == Some(p) {
            self.projects.members = Default::default();
            self.projects.members_of = None;
        }
        let job_form = self
            .jobs
            .form
            .take()
            .filter(|f| f.purge_owner(&self.jobs.scope) != Some(p));
        if self.jobs.scope.as_deref() == Some(p) {
            self.jobs = Default::default();
        }
        self.jobs.form = job_form;
        if self.graph.project.as_deref() == Some(p) {
            self.graph = Default::default();
        }
        if self.databases.project.as_deref() == Some(p) {
            self.databases.project = None;
            self.databases.clear_explorer();
            self.databases.loaded = false;
        }
        if let Some(d) = &mut self.databases.draft {
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
        self.artifacts
            .items
            .retain(|r| !scope.deleted_artifact_ids.contains(&r.artifact.artifact_id));
        self.browser_parts.remove(&Some(p.into()));
        self.browser = Default::default();
        self.resources = Default::default();
        if self.work.board.as_deref().is_some_and(work) {
            self.work.board = None;
        }
        for screen in self.screens_mut() {
            screen.remove_purged(scope);
        }
        self.saved = None;
    }
    pub(super) fn start_purge_review(&mut self, project: ProjectRecord) {
        match self.publish_purge_client(true) {
            Ok(true) => {}
            Ok(false) => {
                self.announce("Native project purge is unavailable");
                return;
            }
            Err(e) => {
                self.announce(e);
                return;
            }
        }
        let Some(b) = self.backend() else {
            return;
        };
        if let Err(e) = b.purge_client_freeze(project.id.0.as_str(), true) {
            self.announce(e);
            return;
        }
        if let Some(client) = &mut self.client {
            client.set_purge_reviewing(true);
        }
        self.purge = PurgeUi {
            project: Some(project.clone()),
            frozen: true,
            busy: true,
            ..Default::default()
        };
        if let Some(review) = self
            .purge_reviews
            .get(project.id.0.as_str())
            .and_then(|v| serde_json::from_value::<ProjectPurgeManifest>(v.clone()).ok())
        {
            self.purge.recovering = true;
            self.purge.manifest = Some(review);
            self.refresh_purge_status();
        } else {
            self.prepare_purge_review(project);
        }
    }
    fn prepare_purge_review(&mut self, project: ProjectRecord) {
        self.purge.busy = true;
        self.command(
            Slot::PurgePrepare,
            "Preparing deletion manifest",
            "projects.prepare_purge",
            move |b| {
                purge_frame(
                    b,
                    "projects.prepare_purge",
                    json!({"projectId":project.id,"expectedRevision":project.revision}),
                    false,
                )
            },
        );
    }
    fn refresh_purge_status(&mut self) {
        if let Some(m) = self.purge.manifest.clone() {
            self.purge.busy = true;
            self.purge.last_poll = Some(std::time::Instant::now());
            self.query(
                Slot::PurgeStatus,
                "Reading purge status",
                "projects.purge_status",
                move |b| {
                    purge_frame(
                        b,
                        "projects.purge_status",
                        json!({"projectId":m.project_id,"manifestId":m.id}),
                        true,
                    )
                },
            );
        }
    }
    fn load_purge_page(&mut self) {
        let Some(m) = self.purge.manifest.clone() else {
            return;
        };
        let next = self.purge.next.clone();
        self.purge.busy = true;
        self.query(Slot::PurgePage,"Reading deletion manifest","projects.purge_manifest",move|b|{
            let mut page=json!({"limit":200});if let Some(c)=next{page["cursor"]=json!(c);}
            purge_frame(b,"projects.purge_manifest",json!({"projectId":m.project_id,"manifestId":m.id,"expectedRevision":m.revision,"page":page}),true)
        });
    }
    pub(super) fn apply_purge(&mut self, slot: Slot, reply: Reply) -> Option<(Slot, Reply)> {
        use devworks_core::protocol::results::OperationResult;
        let result = match (&slot, reply) {
            (Slot::PurgePrepare, Reply::Command(r)) | (Slot::PurgeConfirm, Reply::Command(r)) => {
                ops::command_result(r)
            }
            (Slot::PurgePage, Reply::Query(r)) | (Slot::PurgeStatus, Reply::Query(r)) => {
                ops::query_result(r).map(|(r, _)| r)
            }
            (_, reply) => return Some((slot, reply)),
        };
        self.purge.busy = false;
        match result {
            Ok(OperationResult::ProjectsPreparePurge { value }) => {
                self.purge_reviews
                    .insert(value.project_id.0.as_str().into(), json!(value));
                self.purge.manifest = Some(value);
                self.purge.entries.clear();
                self.purge.next = None;
                self.purge.loaded = false;
                self.load_purge_page();
            }
            Ok(OperationResult::ProjectsPurgeManifest { value }) => {
                if self.purge.manifest.as_ref().is_none_or(|m| {
                    m.digest != value.manifest.digest || m.revision != value.manifest.revision
                }) {
                    self.purge.error = Some("The manifest changed. Prepare it again.".into());
                    return None;
                }
                self.purge.manifest = Some(value.manifest);
                self.purge.entries.extend(value.entries.items.0);
                self.purge.next = value.entries.next_cursor.map(|c| c.0);
                self.purge.loaded = !value.entries.has_more;
            }
            Ok(OperationResult::ProjectsPurge { value })
            | Ok(OperationResult::ProjectsPurgeStatus { value }) => {
                self.purge_reviews
                    .insert(value.project_id.0.as_str().into(), json!(value));
                let prepared = self.purge.recovering && matches!(value.phase, PurgePhase::Prepared);
                self.purge.recovering = false;
                self.purge.manifest = Some(value);
                if prepared && let Some(p) = self.purge.project.clone() {
                    self.prepare_purge_review(p);
                }
            }
            Ok(r) => self.purge.error = Some(format!("Unexpected result from {}", r.operation())),
            Err(e) => self.purge.error = Some(e.describe()),
        }
        None
    }
    pub(super) fn purge_overlay(&mut self, ctx: &egui::Context) {
        if !self.purge.frozen {
            return;
        }
        let mut confirm = false;
        let mut close = false;
        let mut more = false;
        let mut retry = false;
        let mut prepare = false;
        egui::Window::new("Permanent project purge").collapsible(false).resizable(true).show(ctx,|ui|{
            ui.label("Review the complete deletion manifest. This permanently removes owned DevWorks data and local drafts. Shared records remain. Git and repository files are preserved.");
            if let Some(m)=&self.purge.manifest{
                ui.label(format!("Manifest {} · revision {} · {:?}",m.id.as_str(),m.revision.as_str(),m.phase));
                ui.label(format!("Remove {} records and {} bytes; keep {} records and {} bytes.",m.record_count.get(),m.managed_byte_count.get(),m.kept_record_count.get(),m.kept_byte_count.get()));
                egui::ScrollArea::vertical().max_height(300.0).show(ui,|ui|for e in &self.purge.entries{ui.label(format!("{:?} · {} · {} records · {} bytes · {}",e.disposition,e.record_class.0,e.record_count.get(),e.byte_count.get(),e.reason.0));});
                if matches!(m.phase,PurgePhase::Prepared){
                    if !self.purge.loaded{more=ui.add_enabled(!self.purge.busy,egui::Button::new("Read next manifest page")).clicked();}
                    ui.checkbox(&mut self.purge.acknowledged,"I reviewed every entry and want to permanently purge this project");
                    confirm=ui.add_enabled(self.purge.loaded&&self.purge.acknowledged&&!self.purge.busy,egui::Button::new("Permanently purge")).clicked();
                }else{ui.label(format!("{} physical effects remain. Held effects retry with their original reviewed identities.",m.pending_effects.get()));retry=ui.add_enabled(!self.purge.busy,egui::Button::new("Refresh purge status")).clicked();}
            }
            if let Some(e)=&self.purge.error{ui.colored_label(theme::RED,e);if self.purge.manifest.as_ref().is_none_or(|m|matches!(m.phase,PurgePhase::Prepared)){prepare=ui.add_enabled(!self.purge.busy,egui::Button::new("Prepare a fresh manifest")).clicked();}}
            if self.purge.busy{ui.spinner();}
            let can_close=self.purge.manifest.as_ref().is_none_or(|m|matches!(m.phase,PurgePhase::Prepared|PurgePhase::Complete))||self.purge.cleanup_acknowledged;
            close=ui.add_enabled(can_close&&!self.purge.busy,egui::Button::new("Close review")).clicked();
        });
        if more {
            self.load_purge_page();
        }
        if prepare && let Some(p) = self.purge.project.clone() {
            self.purge.error = None;
            self.purge.acknowledged = false;
            self.prepare_purge_review(p);
        }
        if confirm {
            if let Err(e) = self.publish_purge_client(true) {
                self.purge.error = Some(e);
                return;
            }
            if let (Some(b), Some(m)) = (self.backend(), self.purge.manifest.clone()) {
                let frame = purge_frame(
                    &*b,
                    "projects.purge",
                    json!({"projectId":m.project_id,"manifestId":m.id,"expectedRevision":m.revision,"expectedDigest":m.digest,"expectedProjectRevision":m.project_revision,"confirm":true}),
                    false,
                );
                self.purge.busy = self.client.as_mut().is_some_and(|c| {
                    c.confirm_project_purge(Slot::PurgeConfirm, "Purging reviewed project", frame)
                });
            }
        }
        let active = self
            .purge
            .manifest
            .as_ref()
            .is_some_and(|m| matches!(m.phase, PurgePhase::RemovingBytes | PurgePhase::Held));
        if (retry
            || (active
                && self
                    .purge
                    .last_poll
                    .is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(1))))
            && !self.purge.busy
        {
            self.refresh_purge_status();
        }
        if close && !self.purge.busy {
            if let (Some(b), Some(p)) = (self.backend(), &self.purge.project) {
                let _ = b.purge_client_freeze(p.id.0.as_str(), false);
            }
            if let Some(client) = &mut self.client {
                client.set_purge_reviewing(false);
            }
            self.purge = Default::default();
        }
    }
}
fn purge_frame(b: &dyn Backend, op: &str, body: serde_json::Value, query: bool) -> Vec<u8> {
    let bytes = if query {
        ops::query_frame(b, op, body)
    } else {
        ops::command_frame(b, op, body)
    };
    let mut frame: serde_json::Value = serde_json::from_slice(&bytes).expect("native frame");
    frame["schemaRevision"] = json!(6);
    serde_json::to_vec(&frame).expect("native frame")
}
