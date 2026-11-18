//! Native review and client coordination for the exact purge manifest. [`PurgeUi`] is the review
//! screen (its state and window are in [`crate::purge_screen`]); [`Shell`] publishes the client
//! state the review fences and removes what a completed purge owned.
use super::*;
use crate::project_purge::Live;
use crate::purge_ops::PurgeSlot;
pub use crate::purge_screen::PurgeUi;
use crate::screen::{Screen, ScreenContext, controller};
use devworks_core::{
    ports::storage::ProjectPurgeClientScope,
    protocol::records::{ProjectPurgeManifest, ProjectRecord, PurgePhase},
};
use serde_json::json;

controller!(PurgeCtl, purge: PurgeUi);

impl Screen for PurgeUi {
    type Slot = PurgeSlot;
    const ID: ScreenId = ScreenId::Purge;
    fn load(&mut self, _cx: &mut Cx, _route: Route) {}
    fn refresh(&mut self, _cx: &mut Cx, _route: Route) {}
    fn apply(&mut self, cx: &mut Cx, slot: PurgeSlot, reply: Reply) {
        if let Some((slot, reply)) = (PurgeCtl { purge: self, cx }).apply_purge(slot, reply) {
            cx.unhandled(&slot, reply);
        }
    }
    /// The review window, over whatever screen is shown.
    fn ui(&mut self, cx: &mut Cx, _route: Route, ui: &mut Ui) {
        PurgeCtl { purge: self, cx }.review_window(ui.ctx());
    }
    fn remove_purged(&mut self, scope: &ProjectPurgeClientScope) {
        if self
            .project
            .as_ref()
            .is_some_and(|r| r.id == scope.project_id)
        {
            self.cleanup_acknowledged = true;
        }
        if let Some(project) = &mut self.project
            && project.id == scope.project_id
        {
            project.name.0 = "Deleted project".into();
            project.description.0.clear();
            project.repository_ids.0.clear();
        }
    }
    fn context(&self, _route: Route, mem: &RouteMemory) -> ScreenContext {
        ScreenContext {
            tab: mem.tab,
            selection: mem.selection.clone(),
        }
    }
}

impl PurgeUi {
    /// Begins the review of `project`: its client state is already published. The review is
    /// fenced, then the manifest it recorded last is read again or a fresh one is prepared.
    pub(super) fn begin_review(&mut self, cx: &mut Cx, project: ProjectRecord) {
        PurgeCtl { purge: self, cx }.begin_review(project);
    }
    /// Submits the confirmed purge once its client state was published.
    pub(super) fn confirm(&mut self, cx: &mut Cx) {
        PurgeCtl { purge: self, cx }.confirm_purge();
    }
}

impl PurgeCtl<'_, '_> {
    fn begin_review(&mut self, project: ProjectRecord) {
        let Some(b) = self.backend() else {
            return;
        };
        if let Err(e) = b.purge_client_freeze(project.id.0.as_str(), true) {
            self.announce(e);
            return;
        }
        if let Some(client) = self.client.as_deref_mut() {
            client.set_purge_reviewing(true);
        }
        self.purge.reset();
        self.purge.project = Some(project.clone());
        self.purge.frozen = true;
        self.purge.busy = true;
        if let Some(review) = self
            .purge
            .reviews
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
            PurgeSlot::PurgePrepare,
            "Preparing deletion manifest",
            "projects.prepare_purge",
            move |b| ops::purge_prepare(b, &project),
        );
    }
    fn refresh_purge_status(&mut self) {
        if let Some(m) = self.purge.manifest.clone() {
            self.purge.busy = true;
            self.purge.last_poll = Some(std::time::Instant::now());
            self.query(
                PurgeSlot::PurgeStatus,
                "Reading purge status",
                "projects.purge_status",
                move |b| ops::purge_status(b, &m),
            );
        }
    }
    fn load_purge_page(&mut self) {
        let Some(m) = self.purge.manifest.clone() else {
            return;
        };
        let next = self.purge.next.clone();
        self.purge.busy = true;
        self.query(
            PurgeSlot::PurgePage,
            "Reading deletion manifest",
            "projects.purge_manifest",
            move |b| ops::purge_manifest_page(b, &m, next.as_deref()),
        );
    }
    fn apply_purge(&mut self, slot: PurgeSlot, reply: Reply) -> Option<(PurgeSlot, Reply)> {
        use devworks_core::protocol::results::OperationResult;
        let result = match (&slot, reply) {
            (PurgeSlot::PurgePrepare, Reply::Command(r))
            | (PurgeSlot::PurgeConfirm, Reply::Command(r)) => ops::command_result(r),
            (PurgeSlot::PurgePage, Reply::Query(r)) | (PurgeSlot::PurgeStatus, Reply::Query(r)) => {
                ops::query_result(r).map(|(r, _)| r)
            }
            (_, reply) => return Some((slot, reply)),
        };
        self.purge.busy = false;
        match result {
            Ok(OperationResult::ProjectsPreparePurge { value }) => {
                self.purge
                    .reviews
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
                self.purge
                    .reviews
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
    /// Submits the purge the user confirmed, with the exact manifest reviewed.
    fn confirm_purge(&mut self) {
        if let (Some(b), Some(m)) = (self.backend(), self.purge.manifest.clone()) {
            let frame = ops::purge_confirm(&*b, &m);
            self.purge.busy = self.client.as_deref_mut().is_some_and(|c| {
                c.confirm_project_purge(
                    PurgeSlot::PurgeConfirm.into(),
                    "Purging reviewed project",
                    frame,
                )
            });
        }
    }
    fn review_window(&mut self, ctx: &egui::Context) {
        if !self.purge.frozen {
            return;
        }
        let intent = self.purge.window(ctx);
        if intent.more {
            self.load_purge_page();
        }
        if intent.prepare
            && let Some(p) = self.purge.project.clone()
        {
            self.purge.error = None;
            self.purge.acknowledged = false;
            self.prepare_purge_review(p);
        }
        if intent.confirm {
            // The shell publishes the client state first, then submits the purge.
            self.effects.push(Effect::ConfirmPurge);
        }
        let active = self
            .purge
            .manifest
            .as_ref()
            .is_some_and(|m| matches!(m.phase, PurgePhase::RemovingBytes | PurgePhase::Held));
        // A close in this frame wins: a poll started now would set busy and drop the close.
        if !intent.close
            && (intent.retry
                || (active
                    && self
                        .purge
                        .last_poll
                        .is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(1))))
            && !self.purge.busy
        {
            self.refresh_purge_status();
        }
        if intent.close && !self.purge.busy {
            if let (Some(b), Some(p)) = (self.backend(), &self.purge.project) {
                let _ = b.purge_client_freeze(p.id.0.as_str(), false);
            }
            if let Some(client) = self.client.as_deref_mut() {
                client.set_purge_reviewing(false);
            }
            self.purge.reset();
        }
    }
}

impl Shell {
    fn purge_live(&self) -> Live {
        let mut live = Live {
            busy: self.client.as_ref().is_some_and(Client::has_local_tasks),
            ..Live::default()
        };
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
    /// What a completed purge removes from the shell's own state; each screen drops its own.
    fn remove_purged_client(&mut self, scope: &ProjectPurgeClientScope) {
        let p = scope.project_id.0.as_str();
        if let Some(c) = &mut self.client {
            c.invalidate_project_queries(scope);
        }
        if self.selected_project.as_deref() == Some(p) {
            self.selected_project = None;
        }
        self.projects
            .list
            .items
            .retain(|r| r.id != scope.project_id);
        if self.projects.members_of.as_deref() == Some(p) {
            self.projects.members = Default::default();
            self.projects.members_of = None;
        }
        for screen in self.screens_mut() {
            screen.remove_purged(scope);
        }
        self.saved = None;
    }
    /// Begins the permanent purge review of a deleted project: the client state is published
    /// and fenced first.
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
        self.with_typed(|p: &mut PurgeUi, cx| p.begin_review(cx, project));
    }
    /// The confirmed purge: the client state is published, then the purge is submitted.
    pub(super) fn confirm_purge(&mut self) {
        if let Err(e) = self.publish_purge_client(true) {
            self.purge.error = Some(e);
            return;
        }
        self.with_typed(|p: &mut PurgeUi, cx| p.confirm(cx));
    }
}
