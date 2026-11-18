//! Settings → Roles and skills: the `skills.*` / `roles.*` gateway operations, text reads
//! through `artifacts.read`, and draft text stored through `artifacts.save` before the draft
//! command. Commands keep their exact frames; an unknown outcome is settled by Confirm outcomes.
use crate::{
    attachments,
    backend::{Reply, TaskOutput, is_unknown},
    instructions_ops::{self as iop, InstructionsSlot, Kind, TaskResult},
    instructions_screen::{InstructionAction, InstructionsPanel, Pending, Stage},
    memory::RouteMemory,
    ops,
    paged::{Applied, Fetch, Page},
    projects_ops::ProjectsSlot,
    route::Route,
    screen::{Cx, Screen, ScreenContext, ScreenId, controller},
    widgets::Tone,
};
use devworks_core::protocol::records::LifeState;

/// The cursor that ends one source's pages and starts the next project's discovery.
const NEXT_SOURCE: &str = "\u{0}next-source";
use devworks_core::settings::catalog::AgentRole;
use eframe::egui::Ui;

const UNKNOWN: &str = "The outcome is not confirmed yet. Use Confirm outcomes; the exact \
                       request is resubmitted and nothing is applied twice.";

controller!(InstructionsCtl, instructions: InstructionsPanel);

/// Settings draws the panel among its items by calling [`Screen::ui`] with its own context; the
/// panel runs its own actions.
impl Screen for InstructionsPanel {
    type Slot = InstructionsSlot;
    const ID: ScreenId = ScreenId::Instructions;
    fn load(&mut self, _cx: &mut Cx, _route: Route) {}
    fn refresh(&mut self, _cx: &mut Cx, _route: Route) {}
    fn apply(&mut self, cx: &mut Cx, slot: InstructionsSlot, reply: Reply) {
        let unhandled = InstructionsCtl {
            instructions: self,
            cx,
        }
        .apply_instructions(slot, reply);
        if let Some((slot, reply)) = unhandled {
            cx.unhandled(&slot, reply);
        }
    }
    fn ui(&mut self, cx: &mut Cx, _route: Route, ui: &mut Ui) {
        // The projects a skill can be scoped to are the active ones the shell has read.
        self.projects_loaded = cx.projects.list.loaded;
        self.projects = cx
            .projects
            .list
            .items
            .iter()
            .filter(|p| {
                matches!(
                    p.lifecycle,
                    devworks_core::protocol::records::LifeState::Active
                )
            })
            .map(|p| (p.id.0.as_str().to_owned(), p.name.0.clone()))
            .collect();
        let mut actions = Vec::new();
        InstructionsPanel::ui(self, ui, cx.online().is_ok(), &mut actions);
        for action in actions {
            self.act(cx, action);
        }
    }
    fn context(&self, _route: Route, mem: &RouteMemory) -> ScreenContext {
        ScreenContext {
            tab: mem.tab,
            selection: mem.selection.clone(),
        }
    }
}

impl InstructionsPanel {
    pub(super) fn act(&mut self, cx: &mut Cx, action: InstructionAction) {
        InstructionsCtl {
            instructions: self,
            cx,
        }
        .instruction_action(action);
    }
}

impl InstructionsCtl<'_, '_> {
    fn instruction_action(&mut self, action: InstructionAction) {
        match action {
            InstructionAction::Load => self.load_instructions(),
            InstructionAction::LoadDetail => self.load_instruction_detail(),
            InstructionAction::View(revision) => self.view_instruction_revision(revision),
            InstructionAction::Save {
                kind,
                project,
                target,
                name,
                text,
            } => self.save_instruction(kind, project, target, name, text),
            InstructionAction::Restore {
                kind,
                id,
                expected,
                target,
            } => self.restore_instruction(kind, id, expected, target),
            InstructionAction::SetEnabled {
                id,
                expected,
                enabled,
            } => self.set_skill_enabled(id, expected, enabled),
            InstructionAction::OpenApprovals => self.navigate(Route::Approvals),
        }
    }

    fn panel(&mut self) -> &mut InstructionsPanel {
        self.instructions
    }

    /// Removes a file the shell created (recorded as owned when it was created).
    fn remove_owned(&self, path: Option<&std::path::Path>) {
        if let (Some(p), Some(b)) = (path, self.backend()) {
            attachments::remove_owned(p, &b.scratch_dir());
        }
    }

    fn load_instructions(&mut self) {
        let p = self.panel();
        p.requested = true;
        // A failed earlier read shows as loading again; rows read earlier stay until the new
        // ones are complete.
        p.skills.error = None;
        let fetch = p.skills.reload_all();
        // The workspace's skills are read first, then each active project's own, one after
        // another. A load before the project list is read reloads once it is.
        let loaded = self.projects.list.loaded;
        if !loaded && !self.pending(&ProjectsSlot::ProjectsList) {
            self.load_projects();
        }
        let projects: Vec<String> = self
            .projects
            .list
            .items
            .iter()
            .filter(|p| matches!(p.lifecycle, LifeState::Active))
            .map(|p| p.id.0.as_str().to_owned())
            .collect();
        let p = self.panel();
        p.awaiting_projects = !loaded;
        p.discover_queue = projects;
        p.discover_queue.reverse();
        p.discover_project = None;
        self.discover_page(fetch);
        for role in AgentRole::ALL {
            let id = iop::role_id(role);
            let submitted = self.query(
                InstructionsSlot::InstructionRole(id.clone()),
                "Reading role instructions",
                "roles.read",
                {
                    let id = id.clone();
                    move |b| iop::query(b, iop::read(Kind::Role, &id, None))
                },
            );
            if !submitted {
                self.panel()
                    .roles
                    .insert(id, Err("Not requested: DevWorks is offline.".into()));
            }
        }
        let p = self.panel();
        if p.selected.is_some() {
            // Fence the old details now; they reload with the refreshed record.
            p.reload_selected = true;
            p.detail.text_generation = p.detail.text_generation.wrapping_add(1);
            p.detail.history.reset();
            p.detail.resources.reset();
            p.detail.text = None;
        }
    }

    /// Reads one page of the discovery. A cursor that names the next source moves on to the next
    /// project's skills instead of naming a page.
    fn discover_page(&mut self, fetch: Fetch) {
        let p = self.panel();
        let cursor = match fetch.cursor.as_deref() {
            Some(NEXT_SOURCE) => {
                p.discover_project = p.discover_queue.pop();
                None
            }
            other => other.map(str::to_owned),
        };
        let project = p.discover_project.clone();
        let slot = InstructionsSlot::InstructionSkills(fetch.generation);
        let sent = self.cx.paged(
            &mut self.instructions.skills,
            fetch,
            slot,
            "Discovering skills",
            "skills.discover",
            move |b, _| iop::query(b, iop::discover(project.as_deref(), cursor.as_deref())),
        );
        if !sent {
            self.panel().skills.error = Some("Not requested: DevWorks is offline.".into());
        }
    }

    fn load_instruction_detail(&mut self) {
        let Some(sel) = self.panel().selected.clone() else {
            return;
        };
        let p = self.panel();
        p.detail.text_generation = p.detail.text_generation.wrapping_add(1);
        p.detail.history.reset();
        p.detail.resources.reset();
        let fetch = p.detail.history.reload_all();
        p.detail.resources_revision = None;
        p.detail.text = None;
        p.detail.shown = Some(sel.record.revision.get());
        // The listed record may predate the history (which observes sources first): the text
        // and resources load from the history's current revision once it arrives.
        p.detail.follow_history = true;
        self.history_page(sel.kind, sel.id.clone(), fetch);
    }

    /// Loads the text (and a skill's resources) of the history's current revision.
    fn load_current_from_history(&mut self) {
        let p = self.panel();
        if !std::mem::take(&mut p.detail.follow_history) {
            return;
        }
        let Some(sel) = p.selected.clone() else {
            return;
        };
        let current = match p.detail.history.result() {
            Some(Ok(h)) => iop::current_in(h).cloned(),
            Some(Err(e)) => {
                let e = e.clone();
                p.detail.text = Some(Err(e.clone()));
                p.detail.resources.error = Some(e);
                return;
            }
            None => return,
        };
        let Some(current) = current else {
            p.detail.text = Some(Err("This item has no revisions.".into()));
            return;
        };
        let revision = current.revision.get();
        p.detail.shown = Some(revision);
        p.detail.resources_revision = Some(revision);
        let text_generation = p.detail.text_generation;
        self.load_instruction_text(text_generation, current.content.clone());
        if sel.kind == Kind::Skill {
            let fetch = self.panel().detail.resources.reload_all();
            self.resources_page(sel.id, revision, fetch);
        }
    }

    fn history_page(&mut self, kind: Kind, id: String, fetch: Fetch) {
        let slot = InstructionsSlot::InstructionHistory(fetch.generation);
        let sent = self.cx.paged(
            &mut self.instructions.detail.history,
            fetch,
            slot,
            "Reading instruction history",
            kind.history_op(),
            move |b, cursor| iop::query(b, iop::history(kind, &id, cursor)),
        );
        if !sent {
            self.panel().detail.history.error = Some("Not requested: DevWorks is offline.".into());
        }
    }

    fn resources_page(&mut self, id: String, revision: i64, fetch: Fetch) {
        let slot = InstructionsSlot::InstructionResources(fetch.generation);
        let sent = self.cx.paged(
            &mut self.instructions.detail.resources,
            fetch,
            slot,
            "Reading skill resources",
            "skills.resources",
            move |b, cursor| iop::query(b, iop::read_resources(&id, revision, cursor)),
        );
        if !sent {
            self.panel().detail.resources.error =
                Some("Not requested: DevWorks is offline.".into());
        }
    }

    fn load_instruction_text(
        &mut self,
        generation: u64,
        content: devworks_core::protocol::provider::ArtifactRef,
    ) {
        let submitted = self.online().is_ok()
            && self.client.as_mut().is_some_and(|c| {
                c.task(
                    InstructionsSlot::InstructionText(generation).into(),
                    "Reading instruction text",
                    move |b| iop::load_text(b, &content),
                )
            });
        if !submitted {
            self.panel().detail.text = Some(Err("Not requested: DevWorks is offline.".into()));
        }
    }

    fn view_instruction_revision(&mut self, revision: i64) {
        let record = match self.panel().detail.history.result() {
            Some(Ok(h)) => h.iter().find(|r| r.revision.get() == revision).cloned(),
            _ => None,
        };
        let Some(record) = record else {
            return;
        };
        let skill = self
            .panel()
            .selected
            .as_ref()
            .is_some_and(|s| s.kind == Kind::Skill);
        let id = self.panel().selected.as_ref().map(|s| s.id.clone());
        let p = self.panel();
        // History stays as loaded; text (and a skill's resources) follow the viewed revision
        // under their own generations, so nothing in flight is stranded.
        p.detail.text_generation = p.detail.text_generation.wrapping_add(1);
        let text_generation = p.detail.text_generation;
        p.detail.shown = Some(revision);
        p.detail.text = None;
        p.editor = None;
        self.load_instruction_text(text_generation, record.content);
        if skill && let Some(id) = id {
            let d = &mut self.panel().detail;
            d.resources.reset();
            d.resources_revision = Some(revision);
            let fetch = d.resources.reload_all();
            self.resources_page(id, revision, fetch);
        }
    }

    fn save_instruction(
        &mut self,
        kind: Kind,
        project: Option<String>,
        target: Option<(String, i64)>,
        name: String,
        text: String,
    ) {
        if self.panel().pending.is_some() || !iop::savable_text(&text) {
            return;
        }
        let token = self.panel().next_token();
        let title = format!("{name} instructions");
        let submitted = self.online().is_ok()
            && self.client.as_mut().is_some_and(|c| {
                c.task(
                    InstructionsSlot::InstructionPrepare(token).into(),
                    "Preparing instruction text",
                    move |b| iop::prepare_save(b, &title, &text),
                )
            });
        let p = self.panel();
        if submitted {
            p.pending = Some(Pending {
                token,
                kind,
                draft: Some((target, name)),
                project,
                stage: Stage::Preparing,
                toggle: None,
                staged: None,
            });
            p.message = None;
        } else {
            p.message = Some((Tone::Attention, "Not saved: DevWorks is offline.".into()));
        }
    }

    fn restore_instruction(&mut self, kind: Kind, id: String, expected: i64, target: i64) {
        if self.panel().pending.is_some() {
            return;
        }
        let token = self.panel().next_token();
        let op = kind.rollback_op();
        let submitted = self.command(
            InstructionsSlot::InstructionCommand(token),
            "Restoring an instruction revision",
            op,
            move |b| iop::command(b, iop::rollback(kind, &id, expected, target)),
        );
        let p = self.panel();
        if submitted {
            p.pending = Some(Pending {
                token,
                kind,
                draft: None,
                project: None,
                stage: Stage::Saving,
                toggle: None,
                staged: None,
            });
            p.message = None;
        } else {
            p.message = Some((Tone::Attention, "Not restored: DevWorks is offline.".into()));
        }
    }

    fn set_skill_enabled(&mut self, id: String, expected: i64, enabled: bool) {
        if self.panel().pending.is_some() {
            return;
        }
        let token = self.panel().next_token();
        let submitted = self.command(
            InstructionsSlot::InstructionCommand(token),
            if enabled {
                "Enabling a skill"
            } else {
                "Disabling a skill"
            },
            "skills.set_enabled",
            move |b| iop::command(b, iop::set_enabled(&id, expected, enabled)),
        );
        let p = self.panel();
        if submitted {
            p.pending = Some(Pending {
                token,
                kind: Kind::Skill,
                draft: None,
                project: None,
                stage: Stage::Saving,
                toggle: Some(enabled),
                staged: None,
            });
            p.message = None;
        } else {
            p.message = Some((Tone::Attention, "Not changed: DevWorks is offline.".into()));
        }
    }

    fn pending_for(&mut self, token: u64) -> Option<Pending> {
        self.panel().pending.clone().filter(|p| p.token == token)
    }

    fn fail_instruction(&mut self, token: u64, text: String) {
        if let Some(p) = self.pending_for(token) {
            self.remove_owned(p.staged.as_deref());
            self.panel().pending = None;
        }
        self.panel().message = Some((Tone::Bad, text));
    }

    fn apply_instructions(
        &mut self,
        slot: InstructionsSlot,
        reply: Reply,
    ) -> Option<(InstructionsSlot, Reply)> {
        match (slot, reply) {
            (InstructionsSlot::InstructionSkills(generation), Reply::Query(q)) => {
                match iop::records_page(q) {
                    Ok(page) => {
                        let p = self.panel();
                        // A project's discovery lists the workspace's skills too, already read:
                        // keep only that project's own.
                        let reading = p.discover_project.clone();
                        let items: Vec<_> = page
                            .items
                            .into_iter()
                            .filter(|r| reading.is_none() || iop::record_project(r) == reading)
                            .collect();
                        // After a source's last page the discovery moves on to the next one.
                        let next = page
                            .next
                            .or_else(|| (!p.discover_queue.is_empty()).then(|| NEXT_SOURCE.into()));
                        match p.skills.apply(generation, Ok(Page::new(items, next))) {
                            Applied::Stale => return None,
                            Applied::Continue(fetch) => self.discover_page(fetch),
                            Applied::Replaced => {
                                p.skills.items.sort_by(|a, b| {
                                    a.name
                                        .0
                                        .cmp(&b.name.0)
                                        .then(a.id.as_str().cmp(b.id.as_str()))
                                });
                                // Keep the selection's record current.
                                if let Some(sel) = &mut p.selected
                                    && sel.kind == Kind::Skill
                                    && let Some(r) =
                                        p.skills.items.iter().find(|r| r.id.as_str() == sel.id)
                                {
                                    sel.record = r.clone();
                                }
                                if p.reload_selected
                                    && p.selected.as_ref().is_some_and(|s| s.kind == Kind::Skill)
                                {
                                    p.reload_selected = false;
                                    self.load_instruction_detail();
                                }
                            }
                            _ => {}
                        }
                    }
                    Err(e) => {
                        if self.panel().skills.apply(generation, Err(e.describe()))
                            == Applied::Stale
                        {
                            return None;
                        }
                    }
                }
            }
            (InstructionsSlot::InstructionRole(id), Reply::Query(q)) => {
                let result = iop::record(q).map_err(|e| e.describe());
                let p = self.panel();
                if let (Ok(r), Some(sel)) = (&result, &mut p.selected)
                    && sel.kind == Kind::Role
                    && sel.id == id
                {
                    sel.record = r.clone();
                }
                let reload = p.reload_selected
                    && p.selected
                        .as_ref()
                        .is_some_and(|s| s.kind == Kind::Role && s.id == id);
                p.roles.insert(id, result);
                if reload {
                    self.panel().reload_selected = false;
                    self.load_instruction_detail();
                }
            }
            (InstructionsSlot::InstructionHistory(generation), Reply::Query(q)) => {
                let page = iop::records_page(q)
                    .map(|page| Page::new(page.items, page.next))
                    .map_err(|e| e.describe());
                match self.panel().detail.history.apply(generation, page) {
                    Applied::Stale => return None,
                    Applied::Continue(fetch) => {
                        if let Some(sel) = self.panel().selected.clone() {
                            self.history_page(sel.kind, sel.id, fetch);
                        }
                    }
                    Applied::Replaced => {
                        self.panel()
                            .detail
                            .history
                            .items
                            .sort_by_key(|r| std::cmp::Reverse(r.revision.get()));
                        self.load_current_from_history();
                    }
                    Applied::Failed => self.load_current_from_history(),
                    Applied::Appended | Applied::Fenced => {}
                }
            }
            (InstructionsSlot::InstructionResources(generation), Reply::Query(q)) => {
                let page = iop::resources(q)
                    .map(|page| Page::new(page.items, page.next))
                    .map_err(|e| e.describe());
                match self.panel().detail.resources.apply(generation, page) {
                    Applied::Continue(fetch) => {
                        let revision = self.panel().detail.resources_revision;
                        if let (Some(sel), Some(revision)) =
                            (self.panel().selected.clone(), revision)
                        {
                            self.resources_page(sel.id, revision, fetch);
                        }
                    }
                    Applied::Stale => return None,
                    _ => {}
                }
            }
            (InstructionsSlot::InstructionText(generation), Reply::Task(result)) => {
                let d = &mut self.panel().detail;
                if generation != d.text_generation {
                    return None;
                }
                d.text = Some(match result {
                    Ok(TaskOutput::Instruction(TaskResult::Text(text))) => Ok(text),
                    Ok(_) => Err("The text reply did not match this request.".into()),
                    Err(e) => Err(e),
                });
            }
            (InstructionsSlot::InstructionPrepare(token), Reply::Task(result)) => {
                let Some(_) = self.pending_for(token) else {
                    // Prepared for a change no longer pending: release its staged file.
                    if let Ok(TaskOutput::Instruction(TaskResult::Prepared { staged, .. })) = result
                    {
                        self.remove_owned(Some(&staged));
                    }
                    return None;
                };
                match result {
                    Ok(TaskOutput::Instruction(TaskResult::Prepared { frame, staged })) => {
                        if let Some(p) = &mut self.panel().pending {
                            p.staged = Some(staged);
                            p.stage = Stage::Storing;
                        }
                        if !self.command(
                            InstructionsSlot::InstructionArtifact(token),
                            "Storing instruction text",
                            "artifacts.save",
                            move |_| frame,
                        ) {
                            self.fail_instruction(token, "Not saved: DevWorks is offline.".into());
                        }
                    }
                    Ok(_) => self.fail_instruction(
                        token,
                        "The prepared text did not match this change.".into(),
                    ),
                    Err(e) => self.fail_instruction(token, e),
                }
            }
            (InstructionsSlot::InstructionArtifact(token), Reply::Command(outcome)) => {
                let Some(pending) = self.pending_for(token) else {
                    // A restored, confirmed text save without its draft (after a restart).
                    if !is_unknown(&outcome) {
                        self.panel().message = Some((
                            Tone::Attention,
                            "An earlier instruction text save was confirmed, but its draft was \
                             not kept. Edit and save again to apply it."
                                .into(),
                        ));
                    }
                    return None;
                };
                if is_unknown(&outcome) {
                    if let Some(p) = &mut self.panel().pending {
                        p.stage = Stage::Unknown;
                    }
                    self.panel().message = Some((Tone::Attention, UNKNOWN.into()));
                    return None;
                }
                self.remove_owned(pending.staged.as_deref());
                if let Some(p) = &mut self.panel().pending {
                    p.staged = None;
                }
                match ops::saved_artifact(outcome) {
                    Ok(record) => {
                        let Some((target, name)) = pending.draft else {
                            self.fail_instruction(token, "No draft to save.".into());
                            return None;
                        };
                        let draft_kind = pending.kind;
                        let op = draft_kind.draft_op();
                        let project = pending.project.clone();
                        let content = record.artifact;
                        if let Some(p) = &mut self.panel().pending {
                            p.stage = Stage::Saving;
                        }
                        if !self.command(
                            InstructionsSlot::InstructionCommand(token),
                            "Saving instructions",
                            op,
                            move |b| {
                                let target = target.as_ref().map(|(id, r)| (id.as_str(), *r));
                                iop::command(
                                    b,
                                    iop::draft(
                                        draft_kind,
                                        project.as_deref(),
                                        target,
                                        &name,
                                        &content,
                                    ),
                                )
                            },
                        ) {
                            self.fail_instruction(token, "Not saved: DevWorks is offline.".into());
                        }
                    }
                    Err(e) => self.fail_instruction(
                        token,
                        format!("The text was not stored: {}", e.describe()),
                    ),
                }
            }
            (InstructionsSlot::InstructionCommand(token), Reply::Command(outcome)) => {
                let pending = self.pending_for(token);
                let known = pending.is_some();
                let toggle = pending.as_ref().and_then(|p| p.toggle);
                let restore = pending.as_ref().is_some_and(|p| p.draft.is_none());
                if is_unknown(&outcome) {
                    if let Some(p) = &mut self.panel().pending
                        && p.token == token
                    {
                        p.stage = Stage::Unknown;
                    }
                    self.panel().message = Some((Tone::Attention, UNKNOWN.into()));
                    return None;
                }
                if known {
                    self.panel().pending = None;
                }
                match iop::changed(outcome) {
                    Ok(record) => {
                        let p = self.panel();
                        p.editor = None;
                        p.new_skill = None;
                        p.new_skill_project = None;
                        p.message = Some((
                            Tone::Good,
                            match toggle {
                                Some(enabled) => format!(
                                    "{} {}. Running agents adopt the change at their next safe \
                                     step.",
                                    if enabled { "Enabled" } else { "Disabled" },
                                    record.name.0
                                ),
                                None => format!(
                                    "{} revision {} of {}. Running agents adopt it at their \
                                     next safe step.",
                                    if restore { "Restored" } else { "Saved" },
                                    record.revision.get(),
                                    record.name.0
                                ),
                            },
                        ));
                        let kind = if record.id.as_str().starts_with("role-") {
                            Kind::Role
                        } else {
                            Kind::Skill
                        };
                        // A shared skill's draft lands on its harness override: select it.
                        if kind == Kind::Skill {
                            p.select(InstructionsPanel::skill_selection(&record));
                        } else if let Some(role) = AgentRole::ALL
                            .into_iter()
                            .find(|r| iop::role_id(*r) == record.id.as_str())
                        {
                            p.select(InstructionsPanel::role_selection(role, &record));
                        }
                        self.load_instructions();
                    }
                    Err(e) if iop::is_conflict(&e) => {
                        self.panel().message = Some((
                            Tone::Attention,
                            "This item changed since it was loaded (stale revision). It was \
                             refreshed; your edit is kept — review and save again."
                                .into(),
                        ));
                        self.load_instructions();
                    }
                    Err(e) => {
                        self.panel().message =
                            Some((Tone::Bad, format!("Not saved. {}", e.describe())));
                    }
                }
            }
            (slot, reply) => return Some((slot, reply)),
        }
        None
    }
}
