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
    route::Route,
    screen::{Cx, Screen, ScreenContext, ScreenId, controller},
    widgets::Tone,
};
use devworks_core::settings::catalog::AgentRole;
use eframe::egui::Ui;

const UNKNOWN: &str = "The outcome is not confirmed yet. Use Confirm outcomes; the exact \
                       request is resubmitted and nothing is applied twice.";

controller!(InstructionsCtl, instructions: InstructionsPanel);

/// Settings draws the panel among its items and hands its actions to [`InstructionsPanel::act`].
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
    /// The panel on its own.
    fn ui(&mut self, cx: &mut Cx, _route: Route, ui: &mut Ui) {
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
                target,
                name,
                text,
            } => self.save_instruction(kind, target, name, text),
            InstructionAction::Restore {
                kind,
                id,
                expected,
                target,
            } => self.restore_instruction(kind, id, expected, target),
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
        p.skills_generation = p.skills_generation.wrapping_add(1);
        p.skills_gathering.clear();
        let generation = p.skills_generation;
        if p.skills.as_ref().is_some_and(|s| s.is_err()) {
            p.skills = None;
        }
        self.discover_page(generation, None);
        for role in AgentRole::ALL {
            let id = iop::role_id(role);
            let submitted = self.query(
                InstructionsSlot::InstructionRole(id.clone()),
                "Reading role instructions",
                "roles.read",
                {
                    let id = id.clone();
                    move |b| iop::query(b, "roles.read", iop::read_body(&id, None))
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
            p.detail.generation = p.detail.generation.wrapping_add(1);
            p.detail.text_generation = p.detail.text_generation.wrapping_add(1);
            p.detail.resources_generation = p.detail.resources_generation.wrapping_add(1);
            p.detail.history = None;
            p.detail.history_gathering.clear();
            p.detail.resources = None;
            p.detail.resources_gathering.clear();
            p.detail.text = None;
        }
    }

    fn discover_page(&mut self, generation: u64, cursor: Option<String>) {
        if !self.query(
            InstructionsSlot::InstructionSkills(generation),
            "Discovering skills",
            "skills.discover",
            move |b| iop::query(b, "skills.discover", iop::discover_body(cursor.as_deref())),
        ) {
            self.panel().skills = Some(Err("Not requested: DevWorks is offline.".into()));
        }
    }

    fn load_instruction_detail(&mut self) {
        let Some(sel) = self.panel().selected.clone() else {
            return;
        };
        let p = self.panel();
        p.detail.generation = p.detail.generation.wrapping_add(1);
        p.detail.text_generation = p.detail.text_generation.wrapping_add(1);
        p.detail.resources_generation = p.detail.resources_generation.wrapping_add(1);
        let generation = p.detail.generation;
        p.detail.history = None;
        p.detail.history_gathering.clear();
        p.detail.resources = None;
        p.detail.resources_gathering.clear();
        p.detail.resources_revision = None;
        p.detail.text = None;
        p.detail.shown = Some(sel.record.revision.get());
        // The listed record may predate the history (which observes sources first): the text
        // and resources load from the history's current revision once it arrives.
        p.detail.follow_history = true;
        self.history_page(sel.kind, sel.id.clone(), generation, None);
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
        let current = match &p.detail.history {
            Some(Ok(h)) => iop::current_in(h).cloned(),
            Some(Err(e)) => {
                let e = e.clone();
                p.detail.text = Some(Err(e.clone()));
                p.detail.resources = Some(Err(e));
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
        let (text_generation, resources_generation) =
            (p.detail.text_generation, p.detail.resources_generation);
        self.load_instruction_text(text_generation, current.content.clone());
        if sel.kind == Kind::Skill {
            self.resources_page(sel.id, revision, resources_generation, None);
        }
    }

    fn history_page(&mut self, kind: Kind, id: String, generation: u64, cursor: Option<String>) {
        let op = kind.history_op();
        if !self.query(
            InstructionsSlot::InstructionHistory(generation),
            "Reading instruction history",
            op,
            move |b| iop::query(b, op, iop::history_body(&id, cursor.as_deref())),
        ) {
            self.panel().detail.history = Some(Err("Not requested: DevWorks is offline.".into()));
        }
    }

    fn resources_page(
        &mut self,
        id: String,
        revision: i64,
        generation: u64,
        cursor: Option<String>,
    ) {
        if !self.query(
            InstructionsSlot::InstructionResources(generation),
            "Reading skill resources",
            "skills.resources",
            move |b| iop::resources_query(b, iop::resources_body(&id, revision, cursor.as_deref())),
        ) {
            self.panel().detail.resources = Some(Err("Not requested: DevWorks is offline.".into()));
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
        let record = match &self.panel().detail.history {
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
            d.resources_generation = d.resources_generation.wrapping_add(1);
            d.resources = None;
            d.resources_gathering.clear();
            d.resources_revision = Some(revision);
            let resources_generation = d.resources_generation;
            self.resources_page(id, revision, resources_generation, None);
        }
    }

    fn save_instruction(
        &mut self,
        kind: Kind,
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
                stage: Stage::Preparing,
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
            move |b| iop::command(b, op, iop::rollback_body(&id, expected, target)),
        );
        let p = self.panel();
        if submitted {
            p.pending = Some(Pending {
                token,
                kind,
                draft: None,
                stage: Stage::Saving,
                staged: None,
            });
            p.message = None;
        } else {
            p.message = Some((Tone::Attention, "Not restored: DevWorks is offline.".into()));
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
                if generation != self.panel().skills_generation {
                    return None;
                }
                match iop::records_page(q) {
                    Ok(page) => {
                        let p = self.panel();
                        p.skills_gathering.extend(page.items);
                        match page.next {
                            Some(next) => self.discover_page(generation, Some(next)),
                            None => {
                                let mut all = std::mem::take(&mut p.skills_gathering);
                                all.sort_by(|a, b| {
                                    a.name
                                        .0
                                        .cmp(&b.name.0)
                                        .then(a.id.as_str().cmp(b.id.as_str()))
                                });
                                // Keep the selection's record current.
                                if let Some(sel) = &mut p.selected
                                    && sel.kind == Kind::Skill
                                    && let Some(r) = all.iter().find(|r| r.id.as_str() == sel.id)
                                {
                                    sel.record = r.clone();
                                }
                                p.skills = Some(Ok(all));
                                if p.reload_selected
                                    && p.selected.as_ref().is_some_and(|s| s.kind == Kind::Skill)
                                {
                                    p.reload_selected = false;
                                    self.load_instruction_detail();
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let p = self.panel();
                        p.skills_gathering.clear();
                        p.skills = Some(Err(e.describe()));
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
                if generation != self.panel().detail.generation {
                    return None;
                }
                match iop::records_page(q) {
                    Ok(page) => {
                        self.panel().detail.history_gathering.extend(page.items);
                        match page.next {
                            Some(next) => {
                                if let Some(sel) = self.panel().selected.clone() {
                                    self.history_page(sel.kind, sel.id, generation, Some(next));
                                }
                            }
                            None => {
                                let d = &mut self.panel().detail;
                                let mut all = std::mem::take(&mut d.history_gathering);
                                all.sort_by_key(|r| std::cmp::Reverse(r.revision.get()));
                                d.history = Some(Ok(all));
                                self.load_current_from_history();
                            }
                        }
                    }
                    Err(e) => {
                        let d = &mut self.panel().detail;
                        d.history_gathering.clear();
                        d.history = Some(Err(e.describe()));
                        self.load_current_from_history();
                    }
                }
            }
            (InstructionsSlot::InstructionResources(generation), Reply::Query(q)) => {
                if generation != self.panel().detail.resources_generation {
                    return None;
                }
                match iop::resources(q) {
                    Ok(page) => {
                        self.panel().detail.resources_gathering.extend(page.items);
                        match page.next {
                            Some(next) => {
                                let revision = self.panel().detail.resources_revision;
                                if let (Some(sel), Some(revision)) =
                                    (self.panel().selected.clone(), revision)
                                {
                                    self.resources_page(sel.id, revision, generation, Some(next));
                                }
                            }
                            None => {
                                let d = &mut self.panel().detail;
                                d.resources = Some(Ok(std::mem::take(&mut d.resources_gathering)));
                            }
                        }
                    }
                    Err(e) => {
                        let d = &mut self.panel().detail;
                        d.resources_gathering.clear();
                        d.resources = Some(Err(e.describe()));
                    }
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
                        let op = pending.kind.draft_op();
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
                                iop::command(b, op, iop::draft_body(target, &name, &content))
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
                        p.message = Some((
                            Tone::Good,
                            format!(
                                "{} revision {} of {}. Running agents adopt it at their next \
                                 safe step.",
                                if restore { "Restored" } else { "Saved" },
                                record.revision.get(),
                                record.name.0
                            ),
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
