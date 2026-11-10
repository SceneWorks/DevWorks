//! Settings → Roles and skills (DW-34: SKILL-01/02/04/08, CAP-ROLES). File-backed skills and
//! per-role instructions with their revision history, human drafts and restores. Every read
//! and change is a gateway operation the shell submits for this panel; agent proposals are
//! decided in Approvals, never here.
use crate::instructions_ops::{self as iop, Kind, Origin, State};
use crate::theme;
use crate::widgets::{self, Tone, ViewState, chip};
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::InstructionRecord;
use devworks_core::settings::catalog::AgentRole;
use eframe::egui::{self, RichText, Ui};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// What the panel asks the shell to do.
#[derive(Clone, Debug, PartialEq)]
pub enum InstructionAction {
    /// Discover every skill (all pages) and read every role's active record.
    Load,
    /// Load the selected item's history, shown text and resources.
    LoadDetail,
    /// Load the text of one revision (from the loaded history) of the selected item.
    View(i64),
    /// A human draft: `target` is `(instructionId, latest revision)`, `None` a new harness skill.
    Save {
        kind: Kind,
        target: Option<(String, i64)>,
        name: String,
        text: String,
    },
    /// Restore (activate) an approved revision.
    Restore {
        kind: Kind,
        id: String,
        expected: i64,
        target: i64,
    },
    OpenApprovals,
}

/// The item whose detail is shown.
#[derive(Clone, Debug)]
pub struct Selection {
    pub kind: Kind,
    pub id: String,
    /// The `name` a draft of this item states (skill name, or role key).
    pub draft_name: String,
    pub title: String,
    pub record: InstructionRecord,
}

#[derive(Default)]
pub struct Detail {
    /// Fences history replies (and everything on a new selection).
    pub generation: u64,
    /// Fences text replies on their own, so viewing another revision never strands the
    /// history or resource listing in flight.
    pub text_generation: u64,
    /// Fences resource listing replies on their own.
    pub resources_generation: u64,
    /// The revision the resource listing belongs to (its pages continue with it).
    pub resources_revision: Option<i64>,
    /// `None` while loading.
    pub history: Option<Result<Vec<InstructionRecord>, String>>,
    pub history_gathering: Vec<InstructionRecord>,
    /// The revision whose text is shown and its text (`None` while loading).
    pub shown: Option<i64>,
    pub text: Option<Result<String, String>>,
    pub resources: Option<Result<Vec<ArtifactRef>, String>>,
    pub resources_gathering: Vec<ArtifactRef>,
}
impl Detail {
    pub fn latest(&self) -> Option<i64> {
        match &self.history {
            Some(Ok(h)) => iop::latest_revision(h),
            _ => None,
        }
    }
}

/// A change submitted for this panel and not yet settled.
#[derive(Clone, Debug)]
pub struct Pending {
    pub token: u64,
    pub kind: Kind,
    /// The draft (target, name) and its text, or `None` for a restore.
    pub draft: Option<(Option<(String, i64)>, String)>,
    pub stage: Stage,
    /// The staged text file, owned until its `artifacts.save` has a definite outcome.
    pub staged: Option<PathBuf>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Preparing,
    Storing,
    Saving,
    /// The outcome is unknown; the shell's Confirm outcomes resubmits the exact frame.
    Unknown,
}

#[derive(Default)]
pub struct InstructionsPanel {
    pub requested: bool,
    pub skills_generation: u64,
    /// `None` while the first discovery is loading.
    pub skills: Option<Result<Vec<InstructionRecord>, String>>,
    pub skills_gathering: Vec<InstructionRecord>,
    /// Active role records by role item ID (absent while loading).
    pub roles: BTreeMap<String, Result<InstructionRecord, String>>,
    pub selected: Option<Selection>,
    pub detail: Detail,
    /// The edited text of the selected item.
    pub editor: Option<String>,
    /// A new harness skill being written (name, text).
    pub new_skill: Option<(String, String)>,
    pub pending: Option<Pending>,
    pub message: Option<(Tone, String)>,
    pub sequence: u64,
    /// A refresh is re-reading the selected item's record: its details load once that record
    /// arrives, so the shown text and the expected revision come from the same observation.
    pub reload_selected: bool,
}

impl InstructionsPanel {
    pub fn next_token(&mut self) -> u64 {
        self.sequence = self.sequence.wrapping_add(1);
        self.sequence
    }
    /// Selects an item and resets its detail (a new generation fences older replies).
    pub fn select(&mut self, selection: Selection) {
        let generation = self.detail.generation.wrapping_add(1);
        self.detail = Detail {
            generation,
            text_generation: self.detail.text_generation.wrapping_add(1),
            resources_generation: self.detail.resources_generation.wrapping_add(1),
            shown: Some(selection.record.revision.get()),
            ..Detail::default()
        };
        self.editor = None;
        self.selected = Some(selection);
    }
    pub fn skill_selection(record: &InstructionRecord) -> Selection {
        Selection {
            kind: Kind::Skill,
            id: record.id.as_str().to_owned(),
            draft_name: record.name.0.clone(),
            title: record.name.0.clone(),
            record: record.clone(),
        }
    }
    pub fn role_selection(role: AgentRole, record: &InstructionRecord) -> Selection {
        Selection {
            kind: Kind::Role,
            id: record.id.as_str().to_owned(),
            draft_name: role.as_str().to_owned(),
            title: iop::role_title(role).to_owned(),
            record: record.clone(),
        }
    }
    fn busy(&self) -> bool {
        self.pending.is_some()
    }

    pub fn ui(&mut self, ui: &mut Ui, online: bool, actions: &mut Vec<InstructionAction>) {
        egui::Frame::new()
            .stroke(theme::soft_line())
            .corner_radius(theme::radius())
            .inner_margin(theme::margin(12, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("Roles and skills").strong());
                widgets::caption(
                    ui,
                    "Saved and restored changes are adopted by running agents at their next \
                     safe step. Skills and role instructions never grant tools or permissions; \
                     harness policies and selected tools still apply.",
                );
                widgets::caption(
                    ui,
                    "Shared skills: ~/.agents/skills (never written here). Harness skills: \
                     <data folder>/skills.",
                );
                ui.horizontal_wrapped(|ui| {
                    widgets::caption(
                        ui,
                        "Agent-proposed changes wait in Approvals (skills.rollback / \
                         roles.rollback) and are activated only there.",
                    );
                    if widgets::secondary_button(ui, "Open Approvals").clicked() {
                        actions.push(InstructionAction::OpenApprovals);
                    }
                });
                if !online {
                    widgets::state_block(
                        ui,
                        &ViewState::Offline {
                            reason: "Roles and skills need the running application.".into(),
                        },
                    );
                    return;
                }
                if !self.requested {
                    self.requested = true;
                    actions.push(InstructionAction::Load);
                }
                ui.horizontal(|ui| {
                    if widgets::secondary_button(ui, "Refresh roles and skills").clicked() {
                        actions.push(InstructionAction::Load);
                    }
                    if let Some(p) = &self.pending {
                        ui.spinner();
                        widgets::caption(
                            ui,
                            match p.stage {
                                Stage::Preparing => "Preparing text…",
                                Stage::Storing => "Storing text…",
                                Stage::Saving => "Saving…",
                                Stage::Unknown => "Outcome not confirmed; use Confirm outcomes.",
                            },
                        );
                    }
                });
                if let Some((tone, text)) = &self.message {
                    chip(ui, text.clone(), *tone);
                }
                ui.add_space(6.0);
                self.skills_ui(ui, actions);
                ui.add_space(8.0);
                self.roles_ui(ui, actions);
                if self.selected.is_some() {
                    ui.add_space(8.0);
                    widgets::divider(ui);
                    self.detail_ui(ui, actions);
                }
            });
        ui.add_space(8.0);
    }

    fn skills_ui(&mut self, ui: &mut Ui, actions: &mut Vec<InstructionAction>) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Skills").strong());
            if self.new_skill.is_none()
                && ui
                    .add_enabled(!self.busy(), egui::Button::new("New harness skill"))
                    .clicked()
            {
                self.new_skill = Some((String::new(), String::new()));
            }
        });
        self.new_skill_ui(ui, actions);
        match &self.skills {
            None => {
                widgets::state_block(
                    ui,
                    &ViewState::Loading {
                        what: "skills".into(),
                    },
                );
            }
            Some(Err(e)) => {
                if widgets::state_block(
                    ui,
                    &ViewState::Failed {
                        what: "skills".into(),
                        error: e.clone(),
                    },
                ) == widgets::StateAction::Retry
                {
                    actions.push(InstructionAction::Load);
                }
            }
            Some(Ok(list)) if list.is_empty() => {
                widgets::state_block(
                    ui,
                    &ViewState::Empty {
                        title: "No skills found".into(),
                        body: "Add a skill directory with a SKILL.md under ~/.agents/skills, or \
                           create a harness skill."
                            .into(),
                    },
                );
            }
            Some(Ok(list)) => {
                let mut pick = None;
                for record in list {
                    let s = iop::record_status(record);
                    let selected = self
                        .selected
                        .as_ref()
                        .is_some_and(|sel| sel.kind == Kind::Skill && sel.id == record.id.as_str());
                    ui.horizontal_wrapped(|ui| {
                        let r = ui.selectable_label(selected, record.name.0.as_str());
                        if r.clicked() {
                            pick = Some(record.clone());
                        }
                        status_chips(ui, &s, record.revision.get(), true);
                        if let Some(loc) = iop::location(record) {
                            widgets::caption(ui, loc);
                        }
                    });
                }
                if let Some(record) = pick {
                    self.select(Self::skill_selection(&record));
                    actions.push(InstructionAction::LoadDetail);
                }
            }
        }
    }

    fn new_skill_ui(&mut self, ui: &mut Ui, actions: &mut Vec<InstructionAction>) {
        let busy = self.busy();
        let Some((name, text)) = &mut self.new_skill else {
            return;
        };
        let mut close = false;
        egui::Frame::new()
            .stroke(theme::soft_line())
            .corner_radius(theme::radius())
            .inner_margin(theme::margin(8, 6))
            .show(ui, |ui| {
                ui.label("New harness skill");
                widgets::caption(
                    ui,
                    "The name is the skill's directory under <data folder>/skills (one path \
                     segment).",
                );
                let r = ui.add(egui::TextEdit::singleline(name).hint_text("skill-name"));
                labelled(r, egui::Role::TextInput, "New skill name");
                let r = ui.add(
                    egui::TextEdit::multiline(text)
                        .desired_rows(6)
                        .desired_width(f32::INFINITY)
                        .code_editor(),
                );
                labelled(r, egui::Role::TextInput, "New skill text");
                let name_ok = iop::valid_skill_name(name);
                let text_ok = iop::savable_text(text);
                if !name.is_empty() && !name_ok {
                    widgets::caption(ui, "Use one directory name without separators.");
                }
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            name_ok && text_ok && !busy,
                            egui::Button::new("Create skill"),
                        )
                        .clicked()
                    {
                        actions.push(InstructionAction::Save {
                            kind: Kind::Skill,
                            target: None,
                            name: name.clone(),
                            text: text.clone(),
                        });
                    }
                    if widgets::secondary_button(ui, "Cancel").clicked() {
                        close = true;
                    }
                });
            });
        if close {
            self.new_skill = None;
        }
    }

    fn roles_ui(&mut self, ui: &mut Ui, actions: &mut Vec<InstructionAction>) {
        ui.label(RichText::new("Roles").strong());
        let mut pick = None;
        for role in AgentRole::ALL {
            let id = iop::role_id(role);
            let selected = self
                .selected
                .as_ref()
                .is_some_and(|s| s.kind == Kind::Role && s.id == id);
            ui.horizontal_wrapped(|ui| {
                match self.roles.get(&id) {
                    None => {
                        ui.label(iop::role_title(role));
                        ui.spinner();
                    }
                    Some(Err(e)) => {
                        ui.label(iop::role_title(role));
                        chip(ui, e.clone(), Tone::Bad);
                    }
                    Some(Ok(record)) => {
                        if ui
                            .selectable_label(selected, iop::role_title(role))
                            .clicked()
                        {
                            pick = Some((role, record.clone()));
                        }
                        status_chips(
                            ui,
                            &iop::record_status(record),
                            record.revision.get(),
                            false,
                        );
                    }
                }
                if role.read_only() {
                    widgets::caption(ui, "Always read-only (harness policy)");
                }
            });
        }
        if let Some((role, record)) = pick {
            self.select(Self::role_selection(role, &record));
            actions.push(InstructionAction::LoadDetail);
        }
    }

    fn detail_ui(&mut self, ui: &mut Ui, actions: &mut Vec<InstructionAction>) {
        let Some(sel) = self.selected.clone() else {
            return;
        };
        let busy = self.busy();
        let status = iop::record_status(&sel.record);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(&sel.title).strong());
            status_chips(
                ui,
                &status,
                sel.record.revision.get(),
                sel.kind == Kind::Skill,
            );
        });
        if let Some(loc) = iop::location(&sel.record) {
            widgets::caption(ui, format!("Location: {loc}"));
        }
        let shared = status.origin == Origin::Shared;
        if shared {
            widgets::caption(
                ui,
                "Saving creates a harness override; the shared original is not changed.",
            );
        }
        let latest = self.detail.latest();

        // History
        ui.add_space(4.0);
        ui.label(RichText::new("History").strong());
        match &self.detail.history {
            None => {
                widgets::state_block(
                    ui,
                    &ViewState::Loading {
                        what: "history".into(),
                    },
                );
            }
            Some(Err(e)) => {
                if widgets::state_block(
                    ui,
                    &ViewState::Failed {
                        what: "history".into(),
                        error: e.clone(),
                    },
                ) == widgets::StateAction::Retry
                {
                    actions.push(InstructionAction::LoadDetail);
                }
            }
            Some(Ok(history)) if history.is_empty() => {
                widgets::state_block(
                    ui,
                    &ViewState::Empty {
                        title: "No revisions".into(),
                        body: "This item has no recorded revisions yet.".into(),
                    },
                );
            }
            Some(Ok(history)) => {
                for record in history {
                    let s = iop::record_status(record);
                    let revision = record.revision.get();
                    ui.horizontal_wrapped(|ui| {
                        ui.label(format!("Revision {revision}"));
                        chip(ui, s.state.label(), state_tone(s.state));
                        if record.approved {
                            chip(ui, "approved", Tone::Good);
                        } else {
                            chip(ui, "not approved", Tone::Neutral);
                        }
                        if s.active {
                            chip(ui, "active", Tone::Accent);
                        }
                        if self.detail.shown == Some(revision) {
                            widgets::caption(ui, "shown");
                        } else if widgets::secondary_button(
                            ui,
                            &format!("View revision {revision}"),
                        )
                        .clicked()
                        {
                            actions.push(InstructionAction::View(revision));
                        }
                        if iop::restorable(record) {
                            let label = format!("Restore revision {revision}");
                            if ui
                                .add_enabled(!busy && latest.is_some(), egui::Button::new(label))
                                .clicked()
                                && let Some(expected) = latest
                            {
                                actions.push(InstructionAction::Restore {
                                    kind: sel.kind,
                                    id: sel.id.clone(),
                                    expected,
                                    target: revision,
                                });
                            }
                        } else if !record.approved && !s.active && s.state == State::Proposed {
                            widgets::caption(ui, "Decided in Approvals");
                        }
                    });
                }
            }
        }

        // Text
        ui.add_space(4.0);
        ui.label(
            RichText::new(match self.detail.shown {
                Some(r) => format!("Text (revision {r})"),
                None => "Text".into(),
            })
            .strong(),
        );
        let mut start_edit = None;
        match &self.detail.text {
            None => {
                widgets::state_block(
                    ui,
                    &ViewState::Loading {
                        what: "instruction text".into(),
                    },
                );
            }
            Some(Err(e)) => {
                if widgets::state_block(
                    ui,
                    &ViewState::Failed {
                        what: "instruction text".into(),
                        error: e.clone(),
                    },
                ) == widgets::StateAction::Retry
                {
                    actions.push(InstructionAction::LoadDetail);
                }
            }
            Some(Ok(text)) if self.editor.is_none() => {
                let mut shown = text.as_str();
                let r = ui.add(
                    egui::TextEdit::multiline(&mut shown)
                        .desired_rows(6)
                        .desired_width(f32::INFINITY)
                        .code_editor(),
                );
                labelled(r, egui::Role::TextInput, &format!("{} text", sel.title));
                if ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(if shared {
                            "Edit as harness override"
                        } else {
                            "Edit"
                        }),
                    )
                    .clicked()
                {
                    start_edit = Some(text.clone());
                }
            }
            Some(Ok(_)) => {}
        }
        if let Some(text) = start_edit {
            self.editor = Some(text);
        }
        let mut close = false;
        if let Some(text) = &mut self.editor {
            let r = ui.add(
                egui::TextEdit::multiline(text)
                    .desired_rows(10)
                    .desired_width(f32::INFINITY)
                    .code_editor(),
            );
            labelled(
                r,
                egui::Role::TextInput,
                &format!("Edit {} text", sel.title),
            );
            if !iop::savable_text(text) {
                widgets::caption(ui, "Instruction text must not be blank.");
            }
            if latest.is_none() {
                widgets::caption(ui, "Saving waits for the item's history to load.");
            }
            ui.horizontal(|ui| {
                let enabled = !busy && latest.is_some() && iop::savable_text(text);
                if ui.add_enabled(enabled, egui::Button::new("Save")).clicked()
                    && let Some(expected) = latest
                {
                    actions.push(InstructionAction::Save {
                        kind: sel.kind,
                        target: Some((sel.id.clone(), expected)),
                        name: sel.draft_name.clone(),
                        text: text.clone(),
                    });
                }
                if widgets::secondary_button(ui, "Cancel edit").clicked() {
                    close = true;
                }
            });
        }
        // Reset saves the role's catalog default text as a new revision; an earlier revision
        // is not assumed to hold it (it may be a customization recorded first).
        if sel.kind == Kind::Role
            && let Some(default) = iop::role_default_text(&sel.draft_name)
        {
            let enabled = !busy && latest.is_some();
            if ui
                .add_enabled(enabled, egui::Button::new("Reset to default"))
                .clicked()
                && let Some(expected) = latest
            {
                actions.push(InstructionAction::Save {
                    kind: Kind::Role,
                    target: Some((sel.id.clone(), expected)),
                    name: sel.draft_name.clone(),
                    text: default,
                });
            }
        }
        if close {
            self.editor = None;
        }

        // Resources
        if sel.kind == Kind::Skill {
            ui.add_space(4.0);
            ui.label(RichText::new("Resources").strong());
            match &self.detail.resources {
                None => {
                    widgets::state_block(
                        ui,
                        &ViewState::Loading {
                            what: "resources".into(),
                        },
                    );
                }
                Some(Err(e)) => {
                    widgets::state_block(
                        ui,
                        &ViewState::Failed {
                            what: "resources".into(),
                            error: e.clone(),
                        },
                    );
                }
                Some(Ok(list)) if list.is_empty() => {
                    widgets::caption(ui, "No resource files beside the skill text.");
                }
                Some(Ok(list)) => {
                    widgets::caption(
                        ui,
                        format!(
                            "{} resource file{} (read-only; paths are kept with the skill \
                             directory)",
                            list.len(),
                            if list.len() == 1 { "" } else { "s" }
                        ),
                    );
                    for a in list {
                        widgets::caption(
                            ui,
                            format!(
                                "{} · {} bytes · sha256 {}",
                                a.media_type,
                                a.byte_length.get(),
                                &a.sha256.as_str()[..12]
                            ),
                        );
                    }
                }
            }
        }
    }
}

fn state_tone(state: State) -> Tone {
    match state {
        State::Approved => Tone::Good,
        State::Proposed => Tone::Attention,
        State::Rejected => Tone::Bad,
        State::Draft | State::Unknown => Tone::Neutral,
    }
}

fn status_chips(ui: &mut Ui, s: &iop::Status, revision: i64, skill: bool) {
    if skill {
        chip(ui, s.origin.label(), Tone::Neutral);
    }
    chip(ui, format!("revision {revision}"), Tone::Neutral);
    if s.active {
        chip(ui, "active", Tone::Accent);
    }
    match s.effective {
        Some(true) => {
            chip(ui, "effective", Tone::Good);
        }
        Some(false) => {
            chip(ui, "overridden", Tone::Attention);
        }
        None => {}
    }
    if s.disabled {
        chip(ui, "disabled", Tone::Attention);
    }
    if s.missing {
        chip(ui, "missing", Tone::Bad);
    }
}

fn labelled(r: egui::Response, role: egui::Role, label: &str) -> egui::Response {
    let enabled = r.enabled();
    let label = label.to_owned();
    r.widget_info(|| egui::WidgetInfo::labeled(role, enabled, label.clone()));
    r
}
